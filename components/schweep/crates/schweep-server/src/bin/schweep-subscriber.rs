//! `schweep-subscriber` — a subscriber that can be killed (§6 C9).
//!
//! ```text
//!   schweep-subscriber <port-file> <handle> <journal> --until <epoch>
//! ```
//!
//! It exists so that "kill the subscriber mid-stream and resume by token" is a real kill of a real process
//! rather than a dropped struct. What makes it a *fair* subscriber is the order of two writes:
//!
//! 1. the epoch is appended to the journal and `fsync`ed;
//! 2. only then does the subscriber's own token advance — and the token is not held in memory at all, it is
//!    **derived from the journal** on every start.
//!
//! So a kill between receiving an epoch and recording it loses the record, and the resume redelivers that
//! epoch: exactly-once *per epoch* by idempotent redelivery, which is D-23's phrasing, achieved by the
//! client keeping the cursor and the server keeping none. A kill *during* the write can leave a partial
//! line, which the resume ignores (a line is consumed only if it ends in a newline) — so a torn write costs
//! a redelivery, never a gap.
//!
//! The journal line is `epoch N crc C`, where C is the CRC of the delta's bytes. Recording the CRC is what
//! lets the gate assert that a redelivered epoch arrived *identically*, not merely again.

#![allow(clippy::print_stdout, clippy::print_stderr)]

use std::io::Write;
use std::path::{Path, PathBuf};

use schweep_server::Client;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (Some(port_file), Some(handle), Some(journal)) = (args.first(), args.get(1), args.get(2))
    else {
        eprintln!("usage: schweep-subscriber <port-file> <handle> <journal> [--until <epoch>]");
        std::process::exit(2);
    };
    let handle: u64 = match handle.parse() {
        Ok(handle) => handle,
        Err(_) => {
            eprintln!("schweep-subscriber: {handle:?} is not a handle");
            std::process::exit(2);
        }
    };
    let until: u64 = args
        .iter()
        .position(|arg| arg == "--until")
        .and_then(|at| args.get(at + 1))
        .and_then(|value| value.parse().ok())
        .unwrap_or(u64::MAX);

    let port: u16 = match std::fs::read_to_string(port_file)
        .ok()
        .and_then(|text| text.trim().parse().ok())
    {
        Some(port) => port,
        None => {
            eprintln!("schweep-subscriber: no port in {port_file}");
            std::process::exit(2);
        }
    };
    let client = Client::new(([127, 0, 0, 1], port).into());
    let journal = PathBuf::from(journal);

    // The cursor comes from the journal, not from memory: that is the whole design.
    let mut token = resume_token(&journal);

    loop {
        if token >= until {
            println!("schweep-subscriber: caught up at {token}");
            return;
        }
        let response = match client.subscribe(handle, token) {
            Ok(response) => response,
            // The server went away — a killed `schweepd`, or a closed listener. Not this process's
            // failure, and it must not be reported as consumption.
            Err(error) => {
                eprintln!("schweep-subscriber: server unreachable: {error}");
                std::process::exit(4);
            }
        };
        let body = match response.body() {
            Ok(body) => body.to_owned(),
            // A refusal is recorded in the journal, because "the server refused my token" is exactly
            // what the gate needs to distinguish from a gap (D-23).
            Err((kind, message)) => {
                append(&journal, &format!("refused {} {message}\n", kind.name()));
                eprintln!("schweep-subscriber: refused ({}): {message}", kind.name());
                std::process::exit(3);
            }
        };

        for (epoch, delta) in deltas(&body) {
            // Recorded, and durable, *before* the token moves. The order is the guarantee.
            append(
                &journal,
                &format!(
                    "epoch {epoch} crc {:08x}\n",
                    schweep_log::record::crc32(delta.as_bytes())
                ),
            );
            token = epoch;
        }
    }
}

/// The last **complete** journal line's epoch, or 0. A partial trailing line is not consumption.
fn resume_token(journal: &Path) -> u64 {
    let Ok(text) = std::fs::read_to_string(journal) else {
        return 0;
    };
    let mut token = 0u64;
    for line in text.split_inclusive('\n') {
        if !line.ends_with('\n') {
            // A torn tail. Ignoring it costs one redelivery and keeps the alternative — treating a
            // half-written line as consumed — from ever losing an epoch.
            break;
        }
        if let Some(epoch) = line
            .strip_prefix("epoch ")
            .and_then(|rest| rest.split_whitespace().next())
            .and_then(|number| number.parse::<u64>().ok())
        {
            token = epoch;
        }
    }
    token
}

fn append(journal: &Path, line: &str) {
    let mut file = match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(journal)
    {
        Ok(file) => file,
        Err(error) => {
            eprintln!("schweep-subscriber: cannot open the journal: {error}");
            std::process::exit(5);
        }
    };
    if file.write_all(line.as_bytes()).is_err() || file.sync_all().is_err() {
        eprintln!("schweep-subscriber: cannot record an epoch; refusing to advance");
        std::process::exit(5);
    }
}

/// Split a `/subscribe` body into `(epoch, delta)` pairs.
fn deltas(body: &str) -> Vec<(u64, String)> {
    let mut out: Vec<(u64, String)> = Vec::new();
    for line in body.lines().skip(2) {
        match line.strip_prefix("epoch ") {
            Some(number) => {
                if let Ok(epoch) = number.trim().parse() {
                    out.push((epoch, String::new()));
                }
            }
            None => {
                if let Some(last) = out.last_mut() {
                    last.1.push_str(line);
                    last.1.push('\n');
                }
            }
        }
    }
    out
}
