//! The **network door** wearing the [`EngineUnderTest`] costume (§6 C9).
//!
//! This is what makes "the differential harness runs over the network" a fact rather than a plan. Each
//! scenario gets its own `schweepd` — a real listener on loopback, a real log and state store on disk, a
//! real registration — and every epoch is delivered by `POST /ingest` + `POST /seal`, every answer read by
//! `GET /read`. The oracle sits on the other side of the comparison exactly as it does for the in-process
//! doors, so a bug the server introduces between the engine and the socket is a divergence with a seed.
//!
//! ## Why the answer crosses as frames and not as text
//!
//! [`EngineUnderTest::answer`] must return a [`Canonical`], and a rendered answer would have to be parsed
//! back into rows to produce one. A hand-written value parser is exactly the wrong thing to put inside a
//! correctness gate: its bugs would *hide* divergences. So the rows arrive in the log's own frame
//! (`GET /read?format=frames`, D-23), decoded by the log's own decoder, which C4 already tests — and the
//! reconstruction is then held to a stricter standard than equality of rows:
//!
//! > the decoded rows, re-rendered in the order the server sent them, must equal the canonical form's
//! > render byte for byte.
//!
//! That is what closes the ordering hole. Re-canonicalizing on the client would sort a badly-ordered
//! answer into a good one and pass; requiring the round trip means a server that emits rows out of
//! canonical order (D-7) fails here instead.
//!
//! ## Zero flake, by construction
//!
//! The port is ephemeral (`127.0.0.1:0`) and read back from the listener, never guessed. Nothing sleeps:
//! the server is bound *before* its thread starts, so there is no window to wait out, and every
//! synchronisation is a request whose response is the acknowledgement. The data directory is named from a
//! process-local counter, so two scenarios never share one, and it is removed on drop.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;

use schweep_plan::bind::Catalog;
use schweep_plan::plan::Query;
use schweep_server::admission::Policy;
use schweep_server::{Client, Server, ServerConfig};
use schweep_zset::{Canonical, EpochDeltas, Schema, ZSetBatch};

use crate::engine::EngineUnderTest;
use crate::sql_render::sql_form;

/// Names the data directories apart. Not a source of nondeterminism: no answer, no plan and no counter
/// depends on the path, and a run's *content* is fixed by its seed.
static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

/// An [`EngineUnderTest`] that talks to a `schweepd` over a socket.
#[derive(Debug)]
pub struct NetworkEngine {
    client: Client,
    handle: u64,
    /// The answer's schema as bound locally — needed to rebuild a Z-set from decoded rows. It is not
    /// *trusted*: the render comparison in [`NetworkEngine::answer`] fails if the server disagrees.
    schema: Schema,
    sql: String,
    epoch: u64,
    running: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    dir: PathBuf,
}

impl NetworkEngine {
    /// The SQL this door registered — the most useful line in a failure report.
    #[must_use]
    pub fn sql(&self) -> &str {
        &self.sql
    }

    #[must_use]
    pub fn client(&self) -> &Client {
        &self.client
    }

    #[must_use]
    pub fn handle(&self) -> u64 {
        self.handle
    }

    /// Start a server on loopback and register `sql` against `tables`.
    pub fn start(tables: &[(String, Schema)], sql: &str, schema: Schema) -> Result<Self, String> {
        let catalog: Catalog = tables.iter().cloned().collect();
        let ordinal = NEXT_DIRECTORY.fetch_add(1, Ordering::SeqCst);
        let dir =
            std::env::temp_dir().join(format!("schweep-network-{}-{ordinal}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        let config = ServerConfig {
            policy: Policy::default(),
            // Durability under a real crash is the kill -9 matrix's gate, not this one's; here the fsync
            // per append would buy nothing and cost the sweep its size (D-19's `Deferred` note).
            sync: schweep_log::SyncPolicy::Deferred,
            checkpoint_every: 16,
        };
        let mut server =
            Server::bind(&dir, catalog, config).map_err(|e| format!("could not bind: {e}"))?;
        let address = server
            .address()
            .map_err(|e| format!("could not read the bound address: {e}"))?;
        let running = server.running_flag();
        // Bound before the thread starts, so a client may connect immediately: there is no readiness
        // window to sleep through.
        let thread = std::thread::spawn(move || {
            let _ = server.serve();
        });

        let client = Client::new(address);
        // Built before the registration, so that a refusal drops this value and its `Drop` shuts the
        // server down and removes the directory. A registration that failed must not leak a process.
        let mut engine = NetworkEngine {
            client,
            handle: 0,
            schema,
            sql: sql.to_owned(),
            epoch: 0,
            running,
            thread: Some(thread),
            dir,
        };

        let handle = match engine.client.register(sql) {
            Ok(response) => match response.body() {
                Ok(body) => body
                    .trim()
                    .parse::<u64>()
                    .map_err(|_| format!("/register answered {body:?}, not a handle"))?,
                Err((kind, message)) => {
                    return Err(format!("{sql}\n  was refused ({}): {message}", kind.name()))
                }
            },
            Err(e) => return Err(format!("could not reach the server to register: {e}")),
        };
        engine.handle = handle;
        Ok(engine)
    }
}

impl EngineUnderTest for NetworkEngine {
    fn name() -> &'static str {
        "network"
    }

    fn build(tables: &[(String, Schema)], query: &Query) -> Result<Self, String> {
        // The same decline the SQL door gives, word for word, so a scenario with no SQL form reads as
        // "neither side built it" rather than as a disagreement between the doors.
        let sql = match sql_form(query) {
            Ok(sql) => sql,
            Err(reason) => return Err(format!("no SQL form: {}", reason.label())),
        };
        let catalog: Catalog = tables.iter().cloned().collect();
        let bound = schweep_plan::bind::bind(query, &catalog)
            .map_err(|e| format!("{sql}\n  did not bind: {e}"))?;
        NetworkEngine::start(tables, &sql, bound.output_schema)
    }

    fn seal_epoch(&mut self, deltas: &EpochDeltas) -> Result<(), String> {
        let epoch = self.epoch + 1;
        for (table, entries) in deltas.tables() {
            // The token is a function of the epoch and the table, so a retry after a crash carries the
            // same one and I-4 suppresses it. Nothing here is random.
            let token = format!("e{epoch}-{table}");
            let response = self
                .client
                .ingest("differential", table, &token, entries)
                .map_err(|e| format!("/ingest failed at epoch {epoch}: {e}"))?;
            match response.body() {
                Ok(body) if body.trim() == "appended" => {}
                Ok(body) => {
                    return Err(format!(
                        "/ingest answered {body:?} for token {token:?}; the batch was not appended"
                    ))
                }
                Err((kind, message)) => {
                    return Err(format!("/ingest was refused ({}): {message}", kind.name()))
                }
            }
        }
        let sealed = self
            .client
            .seal()
            .map_err(|e| format!("/seal failed at epoch {epoch}: {e}"))?;
        match sealed.body() {
            Ok(body) => {
                let reported: u64 = body
                    .trim()
                    .parse()
                    .map_err(|_| format!("/seal answered {body:?}, not an epoch"))?;
                if reported != epoch {
                    return Err(format!(
                        "/seal reported epoch {reported} where {epoch} was expected; epochs must be \
                         consecutive (I-3)"
                    ));
                }
            }
            Err((kind, message)) => {
                return Err(format!("/seal was refused ({}): {message}", kind.name()))
            }
        }
        self.epoch = epoch;
        Ok(())
    }

    fn answer(&self) -> Result<Canonical, String> {
        let frames = match self
            .client
            .read_frames(self.handle)
            .map_err(|e| format!("/read failed: {e}"))?
        {
            Ok(frames) => frames,
            // A refusal is the engine's error message, which is what the harness compares error answers
            // on. The `ERROR: ` prefix is added by the harness, not here.
            Err((_, message)) => return Err(message),
        };
        let (record, _) = schweep_log::record::read_framed(&frames, 0)
            .map_err(|e| format!("the answer frame did not decode: {e}"))?
            .ok_or_else(|| "the answer frame was empty".to_owned())?;
        let (schema_text, entries) = match record {
            schweep_log::Record::Append { table, entries, .. } => (table, entries),
            schweep_log::Record::SealEpoch { .. } => {
                return Err("the answer arrived as a seal record".to_owned())
            }
        };

        // What the server sent, rendered in the server's order.
        let mut as_sent = format!("{schema_text}\n");
        for (row, weight) in &entries {
            as_sent.push_str(&format!("{row} => {weight}\n"));
        }

        let canonical = ZSetBatch::from_entries(self.schema.clone(), entries)
            .and_then(|batch| batch.canonical())
            .map_err(|e| format!("the answer's rows did not fit the bound schema: {e}"))?;

        // The round trip: canonical form must re-render to exactly the bytes that arrived. Anything else
        // — a row out of order, a zero weight left in, a schema the server names differently — fails here
        // rather than being tidied away by the client (D-7, S-8).
        if canonical.render() != as_sent {
            return Err(format!(
                "the answer did not survive its own canonical form; the wire said:\n{as_sent}\nand \
                 canonicalizing it gives:\n{}",
                canonical.render()
            ));
        }
        Ok(canonical)
    }

    fn state_fingerprint(&self) -> Result<String, String> {
        match self
            .client
            .fingerprint()
            .map_err(|e| format!("/fingerprint failed: {e}"))?
            .body()
        {
            Ok(body) => Ok(body.to_owned()),
            Err((_, message)) => Err(message.to_owned()),
        }
    }
}

impl Drop for NetworkEngine {
    fn drop(&mut self) {
        // Graceful first, so the shutdown path (checkpoint, then drain) is exercised thousands of times
        // by the sweep rather than only by the test that names it.
        let _ = self.client.shutdown();
        self.running.store(false, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            // The loop has already left `accept`, because `/shutdown` broke it. Joining is what makes the
            // directory safe to remove: a server still holding its state store would race the removal.
            let _ = thread.join();
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}
