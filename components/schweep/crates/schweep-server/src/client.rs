//! A client, because a wire contract with one implementation is a guess (D-23).
//!
//! Every endpoint the server exposes is reached from here, which is what makes the contract *tested*
//! rather than described: the network differential gate, the I-6 network door, the kill -9 matrix and the
//! subscriber-crash gate all drive the server through this type, so a request the server accepts but a
//! client cannot form would fail a gate rather than sit in a document.
//!
//! **One connection per request.** No pooling, no keep-alive. That costs a handshake per call and buys
//! something the gates need badly: a client that is killed mid-stream leaves nothing behind for the next
//! one to inherit, so "resume from a token" is genuinely a fresh client with only its token — which is
//! the property the subscriber-crash gate exists to test.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};

use schweep_zset::Row;

use crate::server::encode_batch;
use crate::wire::ErrorKind;

/// What a request returned: a body, or a kind and a message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Response {
    Ok(String),
    Failed { kind: ErrorKind, message: String },
}

impl Response {
    /// The body, or a panic-free error for a caller that wants `Result`.
    pub fn body(&self) -> Result<&str, (ErrorKind, &str)> {
        match self {
            Response::Ok(body) => Ok(body),
            Response::Failed { kind, message } => Err((*kind, message.as_str())),
        }
    }

    #[must_use]
    pub fn is_ok(&self) -> bool {
        matches!(self, Response::Ok(_))
    }

    #[must_use]
    pub fn kind(&self) -> Option<ErrorKind> {
        match self {
            Response::Ok(_) => None,
            Response::Failed { kind, .. } => Some(*kind),
        }
    }
}

/// A response before it is read as text: the status, and the body's bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RawResponse {
    pub status: u16,
    pub body: Vec<u8>,
}

/// A client for one `schweepd`.
#[derive(Clone, Debug)]
pub struct Client {
    address: SocketAddr,
}

impl Client {
    #[must_use]
    pub fn new(address: SocketAddr) -> Client {
        Client { address }
    }

    #[must_use]
    pub fn address(&self) -> SocketAddr {
        self.address
    }

    /// One request, one connection. The body comes back as bytes, because one endpoint's does
    /// (`/read?format=frames`).
    pub fn request_raw(
        &self,
        method: &str,
        target: &str,
        body: &[u8],
    ) -> std::io::Result<RawResponse> {
        let mut stream = TcpStream::connect(self.address)?;
        let head = format!(
            "{method} {target} HTTP/1.1\r\nHost: schweepd\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        stream.write_all(head.as_bytes())?;
        stream.write_all(body)?;
        stream.flush()?;

        let mut raw = Vec::new();
        stream.read_to_end(&mut raw)?;
        let split = raw
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .map(|at| at + 4);
        let (head, body) = match split {
            Some(at) => (
                String::from_utf8_lossy(raw.get(..at).unwrap_or_default()).into_owned(),
                raw.get(at..).unwrap_or_default().to_vec(),
            ),
            None => (String::from_utf8_lossy(&raw).into_owned(), Vec::new()),
        };
        let status: u16 = head
            .lines()
            .next()
            .and_then(|line| line.split(' ').nth(1))
            .and_then(|code| code.parse().ok())
            .unwrap_or(0);
        Ok(RawResponse { status, body })
    }

    /// One request, with a text body — every endpoint but `/read?format=frames`.
    pub fn request(&self, method: &str, target: &str, body: &[u8]) -> std::io::Result<Response> {
        let raw = self.request_raw(method, target, body)?;
        let body = String::from_utf8_lossy(&raw.body).into_owned();
        Ok(match raw.status {
            200 => Response::Ok(body),
            other => Response::Failed {
                kind: kind_of(other),
                // The kind is the body's first line (see `wire::respond_error`), so the message is what
                // follows it.
                message: body
                    .split_once('\n')
                    .map_or(body.as_str(), |(_, rest)| rest)
                    .trim()
                    .to_owned(),
            },
        })
    }

    // ---- the endpoints -------------------------------------------------------------------------

    pub fn ingest(
        &self,
        source: &str,
        table: &str,
        token: &str,
        entries: &[(Row, i64)],
    ) -> std::io::Result<Response> {
        self.request(
            "POST",
            &format!("/ingest?source={source}&table={table}&token={token}"),
            &encode_batch(table, token, entries),
        )
    }

    pub fn seal(&self) -> std::io::Result<Response> {
        self.request("POST", "/seal", &[])
    }

    /// Append several batches and seal, all-or-nothing at the epoch boundary (MD-2 ask 3).
    pub fn transaction(
        &self,
        source: &str,
        batches: &[crate::WireBatch],
    ) -> std::io::Result<Response> {
        let mut body = Vec::new();
        for (table, token, entries) in batches {
            body.extend_from_slice(&encode_batch(table, token, entries));
        }
        self.request("POST", &format!("/txn?source={source}"), &body)
    }

    pub fn register(&self, sql: &str) -> std::io::Result<Response> {
        self.request("POST", "/register", sql.as_bytes())
    }

    pub fn register_unbounded(&self, sql: &str, reason: &str) -> std::io::Result<Response> {
        self.request(
            "POST",
            &format!("/register?unbounded={}", encode_query(reason)),
            sql.as_bytes(),
        )
    }

    pub fn deregister(&self, handle: u64) -> std::io::Result<Response> {
        self.request("POST", &format!("/deregister?handle={handle}"), &[])
    }

    pub fn read(&self, handle: u64) -> std::io::Result<Response> {
        self.request("GET", &format!("/read?handle={handle}"), &[])
    }

    /// The answer as the log's own frame: its schema, its epoch, and its entries in the order the
    /// server emitted them (D-23). What the network differential door decodes.
    pub fn read_frames(
        &self,
        handle: u64,
    ) -> std::io::Result<Result<Vec<u8>, (ErrorKind, String)>> {
        let raw = self.request_raw("GET", &format!("/read?handle={handle}&format=frames"), &[])?;
        Ok(match raw.status {
            200 => Ok(raw.body),
            other => {
                let text = String::from_utf8_lossy(&raw.body).into_owned();
                Err((
                    kind_of(other),
                    text.split_once('\n')
                        .map_or(text.as_str(), |(_, rest)| rest)
                        .trim()
                        .to_owned(),
                ))
            }
        })
    }

    pub fn fingerprint(&self) -> std::io::Result<Response> {
        self.request("GET", "/fingerprint", &[])
    }

    pub fn oneshot(&self, sql: &str) -> std::io::Result<Response> {
        self.request("GET", "/oneshot", sql.as_bytes())
    }

    pub fn subscribe(&self, handle: u64, from: u64) -> std::io::Result<Response> {
        self.request(
            "GET",
            &format!("/subscribe?handle={handle}&from={from}"),
            &[],
        )
    }

    pub fn plan(&self, handle: u64) -> std::io::Result<Response> {
        self.request("GET", &format!("/plan?handle={handle}"), &[])
    }

    pub fn counters(&self) -> std::io::Result<Response> {
        self.request("GET", "/counters", &[])
    }

    pub fn explain_state(&self) -> std::io::Result<Response> {
        self.request("GET", "/explain-state", &[])
    }

    pub fn explain_maintenance(&self) -> std::io::Result<Response> {
        self.request("GET", "/explain-maintenance", &[])
    }

    pub fn health(&self) -> std::io::Result<Response> {
        self.request("GET", "/health", &[])
    }

    pub fn shutdown(&self) -> std::io::Result<Response> {
        self.request("POST", "/shutdown", &[])
    }

    /// Is the server accepting connections? Used to wait for a subprocess to be ready **by asking**
    /// rather than by sleeping — the zero-flake rule with force.
    #[must_use]
    pub fn reachable(&self) -> bool {
        TcpStream::connect(self.address).is_ok()
    }

    /// The answer's rendered form, with the `epoch N` line stripped — what the differential harness
    /// compares (S-8).
    pub fn answer(&self, handle: u64) -> std::io::Result<Result<String, (ErrorKind, String)>> {
        Ok(match self.read(handle)? {
            Response::Ok(body) => Ok(body
                .split_once('\n')
                .map_or_else(String::new, |(_, rest)| rest.to_owned())),
            Response::Failed { kind, message } => Err((kind, message)),
        })
    }

    /// The epoch a read reported, so a caller can honour I-3 across two reads.
    pub fn epoch_of(&self, handle: u64) -> std::io::Result<Option<u64>> {
        Ok(match self.read(handle)? {
            Response::Ok(body) => crate::server::first_number(&body),
            Response::Failed { .. } => None,
        })
    }
}

fn kind_of(status: u16) -> ErrorKind {
    match status {
        400 => ErrorKind::Refused,
        404 => ErrorKind::NotFound,
        409 => ErrorKind::Rejected,
        429 => ErrorKind::Overloaded,
        _ => ErrorKind::Internal,
    }
}

/// Percent-encode the characters a query value cannot carry.
fn encode_query(raw: &str) -> String {
    let mut out = String::new();
    for byte in raw.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}
