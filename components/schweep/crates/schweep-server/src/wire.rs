//! The wire: HTTP/1.1 by hand, and the error taxonomy (D-23).
//!
//! Hand-written because D-23 says so and because the alternative was a hundred crates and an async
//! runtime for a wire format the gates do not need. What is here is the smallest correct subset of
//! HTTP/1.1 that the endpoints require: a request line, headers, a `Content-Length` body, one response.
//! No chunked encoding, no keep-alive pipelining beyond one request per connection, no compression.
//!
//! **Bodies are the log's own encoding.** Append batches are `schweep_log::Record` frames and answers
//! are `Canonical::render()`. There is no new serialization format in this crate, which is the point:
//! a batch that survives the wire is one the log can already write, and an answer that crosses it is the
//! answer the differential harness already compares (S-8).

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;

/// How a request failed, and therefore what a client should do about it (D-23).
///
/// The kind *is* the status code, because a client's recovery differs per kind and a single "error"
/// would leave it guessing. `Overloaded` is the only retryable kind, and that is the whole point: a
/// client that retries everything turns a malformed request into a hot loop.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorKind {
    /// 400 — outside the dialect, or malformed. A statement about the request (S-12).
    Refused,
    /// 404 — unknown handle, table, or path.
    NotFound,
    /// 409 — a conflict: a dedup token reused with different content (I-4), a plan that cannot bind.
    Rejected,
    /// 429 — admission refused it; the source's queue is full. **The only retryable kind.**
    Overloaded,
    /// 500 — a bug, or an I/O failure.
    Internal,
}

impl ErrorKind {
    #[must_use]
    pub fn status(self) -> u16 {
        match self {
            ErrorKind::Refused => 400,
            ErrorKind::NotFound => 404,
            ErrorKind::Rejected => 409,
            ErrorKind::Overloaded => 429,
            ErrorKind::Internal => 500,
        }
    }

    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            ErrorKind::Refused => "Refused",
            ErrorKind::NotFound => "NotFound",
            ErrorKind::Rejected => "Rejected",
            ErrorKind::Overloaded => "Overloaded",
            ErrorKind::Internal => "Internal",
        }
    }

    /// Whether a client should retry. Exactly one kind says yes.
    #[must_use]
    pub fn retryable(self) -> bool {
        matches!(self, ErrorKind::Overloaded)
    }
}

/// A parsed request: the method, the path, the query parameters, and the body.
#[derive(Clone, Debug)]
pub struct Request {
    pub method: String,
    pub path: String,
    pub query: BTreeMap<String, String>,
    pub body: Vec<u8>,
}

impl Request {
    /// A query parameter, or a `Refused` naming it — so a missing parameter is never a default.
    pub fn param(&self, name: &str) -> Result<&str, (ErrorKind, String)> {
        self.query.get(name).map(String::as_str).ok_or_else(|| {
            (
                ErrorKind::Refused,
                format!("the query parameter {name:?} is required"),
            )
        })
    }

    /// A `u64` parameter.
    pub fn u64_param(&self, name: &str) -> Result<u64, (ErrorKind, String)> {
        let raw = self.param(name)?;
        raw.parse().map_err(|_| {
            (
                ErrorKind::Refused,
                format!("the query parameter {name:?} must be a number, not {raw:?}"),
            )
        })
    }

    pub fn body_text(&self) -> Result<&str, (ErrorKind, String)> {
        std::str::from_utf8(&self.body)
            .map_err(|_| (ErrorKind::Refused, "the body is not UTF-8".to_owned()))
    }
}

/// Read one request from a connection.
///
/// Returns `Ok(None)` when the peer closed without sending one, which is not an error: a client that
/// connects and goes away is a normal event, and the kill -9 matrix produces it constantly.
pub fn read_request(stream: &TcpStream) -> std::io::Result<Option<Request>> {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
        return Ok(None);
    }

    let mut parts = line.trim_end().split(' ');
    let method = parts.next().unwrap_or_default().to_owned();
    let target = parts.next().unwrap_or_default().to_owned();
    if method.is_empty() || target.is_empty() {
        return Ok(None);
    }

    let mut length = 0usize;
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header)? == 0 {
            break;
        }
        let header = header.trim_end();
        if header.is_empty() {
            break;
        }
        if let Some((name, value)) = header.split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                length = value.trim().parse().unwrap_or(0);
            }
        }
    }

    let mut body = vec![0u8; length];
    if length > 0 {
        reader.read_exact(&mut body)?;
    }

    let (path, raw_query) = match target.split_once('?') {
        Some((path, query)) => (path.to_owned(), query),
        None => (target.clone(), ""),
    };
    let mut query = BTreeMap::new();
    for pair in raw_query.split('&').filter(|p| !p.is_empty()) {
        let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
        query.insert(percent_decode(name), percent_decode(value));
    }

    Ok(Some(Request {
        method,
        path,
        query,
        body,
    }))
}

/// The subset of percent-decoding a query string needs. `+` is not a space here: this is a path query,
/// not a form body, and treating it as one would corrupt SQL sent as a parameter.
fn percent_decode(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0usize;
    while index < bytes.len() {
        match bytes.get(index) {
            Some(b'%') => {
                let hex = raw.get(index + 1..index + 3);
                match hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                    Some(byte) => {
                        out.push(byte);
                        index += 3;
                    }
                    None => {
                        out.push(b'%');
                        index += 1;
                    }
                }
            }
            Some(byte) => {
                out.push(*byte);
                index += 1;
            }
            None => break,
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Write a successful response.
pub fn respond(stream: &mut TcpStream, body: &[u8]) -> std::io::Result<()> {
    write_response(stream, 200, "OK", body)
}

/// Write a failure, with the kind named in the body's first line so a client that logs only the body
/// still learns which kind it was.
pub fn respond_error(
    stream: &mut TcpStream,
    kind: ErrorKind,
    message: &str,
) -> std::io::Result<()> {
    let body = format!("{}\n{message}\n", kind.name());
    write_response(stream, kind.status(), kind.name(), body.as_bytes())
}

fn write_response(
    stream: &mut TcpStream,
    status: u16,
    reason: &str,
    body: &[u8],
) -> std::io::Result<()> {
    // No `Date` header, and that is deliberate: D-23 says the server takes no wall-clock decision, and
    // a header read from the clock would put one in every response for tests to trip over.
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nContent-Type: text/plain; charset=utf-8\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]

    use super::*;

    #[test]
    fn a_query_string_is_decoded_without_treating_plus_as_a_space() {
        // SQL travels as a query parameter in one endpoint, and `a + 1` must survive it.
        assert_eq!(percent_decode("SELECT%20a%20%2B%201"), "SELECT a + 1");
        assert_eq!(percent_decode("a+b"), "a+b");
        assert_eq!(percent_decode("%zz"), "%zz", "a bad escape is left alone");
        assert_eq!(percent_decode("trailing%"), "trailing%");
    }

    #[test]
    fn exactly_one_error_kind_is_retryable() {
        let kinds = [
            ErrorKind::Refused,
            ErrorKind::NotFound,
            ErrorKind::Rejected,
            ErrorKind::Overloaded,
            ErrorKind::Internal,
        ];
        let retryable: Vec<&str> = kinds
            .iter()
            .filter(|kind| kind.retryable())
            .map(|kind| kind.name())
            .collect();
        assert_eq!(
            retryable,
            vec!["Overloaded"],
            "D-23: a client that retries everything turns a malformed request into a hot loop"
        );
        // And every kind maps to the status D-23 records.
        assert_eq!(ErrorKind::Refused.status(), 400);
        assert_eq!(ErrorKind::NotFound.status(), 404);
        assert_eq!(ErrorKind::Rejected.status(), 409);
        assert_eq!(ErrorKind::Overloaded.status(), 429);
        assert_eq!(ErrorKind::Internal.status(), 500);
    }
}
