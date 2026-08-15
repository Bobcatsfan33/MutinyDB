//! `schweepd` — the server binary (§6 C9).
//!
//! ```text
//!   schweepd <data-dir> <catalog.txt> [--port-file <path>]
//! ```
//!
//! The catalog is a file rather than a flag because a schema is not a command-line argument: it is data
//! the server must agree with its clients about, and a file can be diffed. Its format is one table per
//! line, `table: name:type:nullable, …`, which is enough for a server whose DDL story is not yet written
//! — and *saying* that is better than inventing a DDL endpoint C9 was not asked for.
//!
//! `--port-file` exists for the kill -9 matrix: the server binds an ephemeral port (D-23's zero-flake
//! rule — no guessed ports, no collisions) and writes it where the parent can read it. A parent that
//! polled for a port would be sleeping-as-synchronisation; a parent that reads a file the child wrote
//! after binding is waiting on the event itself.

#![allow(clippy::print_stdout, clippy::print_stderr)]

use std::path::PathBuf;

use schweep_log::SyncPolicy;
use schweep_plan::bind::Catalog;
use schweep_server::{Policy, Server, ServerConfig};
use schweep_zset::{DataType, Field, Schema};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (Some(dir), Some(catalog_path)) = (args.first(), args.get(1)) else {
        eprintln!(
            "usage: schweepd <data-dir> <catalog.txt> [--port-file <path>] [--queue-bound N]"
        );
        std::process::exit(2);
    };

    let mut port_file: Option<PathBuf> = None;
    let mut policy = Policy::default();
    let mut index = 2usize;
    while index < args.len() {
        match args.get(index).map(String::as_str) {
            Some("--port-file") => {
                port_file = args.get(index + 1).map(PathBuf::from);
                index += 2;
            }
            Some("--queue-bound") => {
                if let Some(bound) = args.get(index + 1).and_then(|v| v.parse().ok()) {
                    policy.queue_bound = bound;
                }
                index += 2;
            }
            _ => index += 1,
        }
    }

    let catalog = match read_catalog(catalog_path) {
        Ok(catalog) => catalog,
        Err(message) => {
            eprintln!("schweepd: {message}");
            std::process::exit(2);
        }
    };

    let config = ServerConfig {
        policy,
        sync: SyncPolicy::Full,
        checkpoint_every: 8,
    };
    let mut server = match Server::bind(dir, catalog, config) {
        Ok(server) => server,
        Err(error) => {
            eprintln!("schweepd: could not start: {error}");
            std::process::exit(1);
        }
    };

    let address = match server.address() {
        Ok(address) => address,
        Err(error) => {
            eprintln!("schweepd: no address: {error}");
            std::process::exit(1);
        }
    };
    // Written *after* the listener is bound, so its existence means the server is accepting.
    if let Some(path) = &port_file {
        if std::fs::write(path, format!("{}\n", address.port())).is_err() {
            eprintln!("schweepd: could not write the port file");
            std::process::exit(1);
        }
    }
    println!("schweepd listening on {address}");

    match server.serve() {
        Ok(Some(report)) => {
            println!(
                "schweepd drained at epoch {} · {} pending appends · {} registrations",
                report.epoch, report.pending_appends, report.registrations
            );
        }
        Ok(None) => println!("schweepd stopped"),
        Err(error) => {
            eprintln!("schweepd: {error}");
            std::process::exit(1);
        }
    }
}

/// One table per line: `name: column:type:nullable, column:type:nullable`.
fn read_catalog(path: &str) -> Result<Catalog, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("cannot read {path}: {e}"))?;
    let mut catalog = Catalog::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (name, columns) = line
            .split_once(':')
            .ok_or_else(|| format!("catalog line {line:?} has no table name"))?;
        let mut fields = Vec::new();
        for column in columns.split(',') {
            let parts: Vec<&str> = column.trim().split(':').collect();
            let (Some(field), Some(kind)) = (parts.first(), parts.get(1)) else {
                return Err(format!("catalog column {column:?} needs name:type[:null]"));
            };
            let data_type = match *kind {
                "int" | "Int64" => DataType::Int64,
                "text" | "Utf8" => DataType::Utf8,
                "bool" | "Boolean" => DataType::Boolean,
                other => return Err(format!("catalog type {other:?} is not int, text or bool")),
            };
            let nullable = parts.get(2).is_none_or(|flag| *flag != "notnull");
            fields.push(Field::new(*field, data_type, nullable));
        }
        let schema = Schema::new(fields).map_err(|e| format!("catalog table {name}: {e}"))?;
        catalog.insert(name.trim().to_owned(), schema);
    }
    if catalog.is_empty() {
        return Err(format!("catalog {path} declares no tables"));
    }
    Ok(catalog)
}
