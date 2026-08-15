//! The accept loop and the handlers (§6 C9, D-23).
//!
//! One thread, one request at a time, one engine. That is not a simplification to be optimised away
//! later — it is what makes the process deterministic past the ingest boundary (D-6): concurrent clients
//! race to be *accepted*, and from there their requests are serialised into one order that one engine
//! applies. A thread pool would move that boundary inward, and every gate that compares an engine to an
//! oracle would inherit the scheduler's ordering.
//!
//! **Loopback, ephemeral port, no sleeps.** The server binds `127.0.0.1:0` and reports the port it got,
//! so tests never guess a port, never collide, and never wait for one to free up. Nothing in this file
//! sleeps: a client that wants the server to have processed something asks it, and the answer is the
//! synchronisation.

use std::io::Write;
use std::net::{SocketAddr, TcpListener};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use schweep_log::{Ack, SyncPolicy};
use schweep_memo::Admission;
use schweep_plan::bind::Catalog;
use schweep_zset::Row;

use crate::admission::Policy;
use crate::engine::Engine;
use crate::error::{ServerError, ServerResult};
use crate::wire::{self, Request};

/// How a `schweepd` is configured.
#[derive(Clone, Debug)]
pub struct ServerConfig {
    pub policy: Policy,
    pub sync: SyncPolicy,
    /// Checkpoint every N epochs; 0 never.
    pub checkpoint_every: u64,
}

impl Default for ServerConfig {
    fn default() -> ServerConfig {
        ServerConfig {
            policy: Policy::default(),
            sync: SyncPolicy::Full,
            checkpoint_every: 8,
        }
    }
}

/// The server: a listener, an engine, and a flag that ends the loop.
#[derive(Debug)]
pub struct Server {
    listener: TcpListener,
    engine: Engine,
    running: Arc<AtomicBool>,
}

impl Server {
    /// Bind loopback on an ephemeral port and open the engine on `dir`.
    pub fn bind(
        dir: impl AsRef<Path>,
        catalog: Catalog,
        config: ServerConfig,
    ) -> ServerResult<Server> {
        let listener =
            TcpListener::bind("127.0.0.1:0").map_err(|e| ServerError::Io(e.to_string()))?;
        let engine = Engine::open(
            dir,
            catalog,
            config.policy,
            config.sync,
            config.checkpoint_every,
        )?;
        Ok(Server {
            listener,
            engine,
            running: Arc::new(AtomicBool::new(true)),
        })
    }

    /// The address to hand a client. Read rather than guessed, because the port is ephemeral.
    pub fn address(&self) -> ServerResult<SocketAddr> {
        self.listener
            .local_addr()
            .map_err(|e| ServerError::Io(e.to_string()))
    }

    /// A flag another thread can clear to stop the loop, for a test that runs the server in-process.
    #[must_use]
    pub fn running_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.running)
    }

    #[must_use]
    pub fn engine(&self) -> &Engine {
        &self.engine
    }

    /// Serve until `/shutdown` arrives or the running flag is cleared.
    ///
    /// Returns the shutdown report, so a caller can assert that the drain happened rather than assuming
    /// it. A server that exits without saying what it drained is a server whose shutdown nobody checks.
    pub fn serve(&mut self) -> ServerResult<Option<crate::engine::Drained>> {
        let mut drained = None;
        while self.running.load(Ordering::SeqCst) {
            let (mut stream, _) = match self.listener.accept() {
                Ok(pair) => pair,
                // The listener closed under us, which is how a killed process ends. Not an error.
                Err(_) => break,
            };
            let request = match wire::read_request(&stream) {
                Ok(Some(request)) => request,
                // A client that connects and goes away is a normal event; the kill -9 matrix makes many.
                Ok(None) => continue,
                Err(_) => continue,
            };

            if request.path == "/shutdown" {
                let report = self.engine.shutdown()?;
                let _ = wire::respond(
                    &mut stream,
                    format!(
                        "ok\nepoch {}\npending_appends {}\nregistrations {}\n",
                        report.epoch, report.pending_appends, report.registrations
                    )
                    .as_bytes(),
                );
                let _ = stream.flush();
                drained = Some(report);
                self.running.store(false, Ordering::SeqCst);
                break;
            }

            match self.handle(&request) {
                Ok(body) => {
                    let _ = wire::respond(&mut stream, &body);
                }
                Err(error) => {
                    let kind = error.kind();
                    let _ = wire::respond_error(&mut stream, kind, &error.to_string());
                }
            }
        }
        Ok(drained)
    }

    /// One request. Every arm is a thin translation into an engine call; nothing here decides anything.
    ///
    /// The body is bytes rather than text because one endpoint's body is not text: `/read?format=frames`
    /// answers in the log's own frame (D-23). Passing that through a `String` would have replaced every
    /// byte the encoding needs with U+FFFD, which is the sort of loss a text-shaped pipe inflicts quietly.
    fn handle(&mut self, request: &Request) -> ServerResult<Vec<u8>> {
        // The one byte-bodied endpoint first, so the text arms below can stay text.
        if request.method == "GET" && request.path == "/read" {
            if let Some(format) = request.query.get("format") {
                if format == "frames" {
                    let handle = u64_param(request, "handle")?;
                    return self.engine.read_frames(handle);
                }
                return Err(ServerError::Sql(schweep_sql::SqlError::Parse(format!(
                    "unknown read format {format:?}; the formats are \"render\" and \"frames\""
                ))));
            }
        }
        self.handle_string(request).map(String::into_bytes)
    }

    fn handle_string(&mut self, request: &Request) -> ServerResult<String> {
        match (request.method.as_str(), request.path.as_str()) {
            ("POST", "/ingest") => {
                let source = param(request, "source")?;
                let table = param(request, "table")?;
                let token = param(request, "token")?;
                let entries = decode_entries(&request.body)?;
                let ack = self.engine.ingest(&source, &table, &token, entries)?;
                Ok(match ack {
                    Ack::Appended => "appended\n".to_owned(),
                    Ack::DroppedAsReplay => "duplicate\n".to_owned(),
                })
            }
            ("POST", "/seal") => Ok(format!("{}\n", self.engine.seal()?)),
            ("POST", "/txn") => {
                let source = param(request, "source")?;
                let batches = decode_transaction(&request.body)?;
                Ok(format!("{}\n", self.engine.transaction(&source, batches)?))
            }
            ("POST", "/retract-source") => {
                let source = param(request, "source")?;
                let table = request.query.get("table").map(String::as_str);
                let predicate = if request.body.is_empty() {
                    None
                } else {
                    Some(body_text(request)?)
                };
                Ok(self
                    .engine
                    .retract_source(&source, table, predicate.as_deref())?
                    .render())
            }
            ("POST", "/register") => {
                let sql = body_text(request)?;
                let admission = match request.query.get("unbounded") {
                    Some(reason) if !reason.is_empty() => {
                        Admission::with_unbounded_state(reason.clone())
                    }
                    _ => Admission::bounded(),
                };
                Ok(format!("{}\n", self.engine.register(&sql, admission)?))
            }
            ("POST", "/deregister") => {
                let handle = u64_param(request, "handle")?;
                self.engine.deregister(handle)?;
                Ok("ok\n".to_owned())
            }
            ("GET", "/read") => {
                let handle = u64_param(request, "handle")?;
                let (epoch, answer) = self.engine.read(handle)?;
                Ok(format!("epoch {epoch}\n{answer}"))
            }
            ("GET", "/oneshot") => {
                let sql = match request.query.get("sql") {
                    Some(sql) => sql.clone(),
                    None => body_text(request)?,
                };
                self.engine.oneshot(&sql)
            }
            ("GET", "/subscribe") => {
                let handle = u64_param(request, "handle")?;
                let from = u64_param(request, "from")?;
                let (next, deltas) = self.engine.subscribe(handle, from)?;
                let mut out = format!("token {next}\nepochs {}\n", deltas.len());
                for delta in deltas {
                    out.push_str(&format!("epoch {}\n{}", delta.epoch, delta.rendered));
                }
                Ok(out)
            }
            ("GET", "/plan") => {
                let handle = u64_param(request, "handle")?;
                self.engine.plan_of(handle)
            }
            ("GET", "/counters") => Ok(self.engine.counters()),
            ("GET", "/fingerprint") => self.engine.fingerprint(),
            ("GET", "/explain-state") => self.engine.explain_state(),
            ("GET", "/explain-maintenance") => Ok(self.engine.explain_maintenance()),
            ("GET", "/health") => Ok(self.engine.health()),
            _ => Err(ServerError::UnknownPath {
                method: request.method.clone(),
                path: request.path.clone(),
            }),
        }
    }
}

fn param(request: &Request, name: &str) -> ServerResult<String> {
    request
        .param(name)
        .map(str::to_owned)
        .map_err(|(_, message)| ServerError::Sql(schweep_sql::SqlError::Parse(message)))
}

fn u64_param(request: &Request, name: &str) -> ServerResult<u64> {
    request
        .u64_param(name)
        .map_err(|(_, message)| ServerError::Sql(schweep_sql::SqlError::Parse(message)))
}

fn body_text(request: &Request) -> ServerResult<String> {
    request
        .body_text()
        .map(str::to_owned)
        .map_err(|(_, message)| ServerError::Sql(schweep_sql::SqlError::Parse(message)))
}

/// Decode a wire batch: `schweep_log::Record` frames, the log's own encoding (D-23).
///
/// One `Append` record per batch, and its entries are what gets appended. The source, table and token on
/// the record are ignored in favour of the query parameters — the *request* says where a batch is going,
/// and letting the body disagree with it would create two answers to one question.
pub fn decode_entries(body: &[u8]) -> ServerResult<Vec<(Row, i64)>> {
    if body.is_empty() {
        return Ok(Vec::new());
    }
    let (record, _) = schweep_log::record::read_framed(body, 0)?
        .ok_or(ServerError::CorruptRegistry("a truncated wire frame"))?;
    match record {
        schweep_log::Record::Append { entries, .. } => Ok(entries),
        schweep_log::Record::SealEpoch { .. } => {
            Err(ServerError::CorruptRegistry("a seal record is not a batch"))
        }
    }
}

/// Decode a transaction body: several framed `Append` records, each carrying its own table and token.
pub fn decode_transaction(body: &[u8]) -> ServerResult<Vec<crate::WireBatch>> {
    let mut out = Vec::new();
    let mut at = 0usize;
    while let Some((record, next)) = schweep_log::record::read_framed(body, at)? {
        match record {
            schweep_log::Record::Append {
                table,
                dedup_token,
                entries,
                ..
            } => out.push((table, dedup_token, entries)),
            schweep_log::Record::SealEpoch { .. } => {
                return Err(ServerError::CorruptRegistry(
                    "a transaction body holds batches, and the seal is implied",
                ))
            }
        }
        at = next;
    }
    Ok(out)
}

/// Encode one batch for the wire — the client's side of the same format.
#[must_use]
pub fn encode_batch(table: &str, token: &str, entries: &[(Row, i64)]) -> Vec<u8> {
    schweep_log::record::frame(
        &schweep_log::Record::Append {
            source_id: "wire".to_owned(),
            dedup_token: token.to_owned(),
            table: table.to_owned(),
            entries: entries.to_vec(),
        }
        .encode(),
    )
}

/// Read one line of a response body as a `u64`, for a client parsing `epoch` or a handle.
pub fn first_number(body: &str) -> Option<u64> {
    body.lines()
        .next()
        .and_then(|line| line.split_whitespace().last())
        .and_then(|word| word.parse().ok())
}
