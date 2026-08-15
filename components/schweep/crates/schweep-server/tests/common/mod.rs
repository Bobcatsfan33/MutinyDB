//! Shared plumbing for the server's integration tests: a server on a thread, and a client for it.
//!
//! Kept in one place because four test files drive a `schweepd` the same way, and a second copy of a
//! start/stop dance is a second place for a test to leak a process.

#![allow(dead_code)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;

use schweep_log::SyncPolicy;
use schweep_plan::bind::Catalog;
use schweep_server::admission::Policy;
use schweep_server::{Client, Server, ServerConfig};
use schweep_zset::{DataType, Field, Row, Schema, Value};

/// A server on a thread, its client, and the directory both share.
pub struct Harness {
    pub client: Client,
    pub dir: PathBuf,
    running: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Harness {
    /// Start on `dir`, keeping whatever is already there — which is how the restart tests work.
    pub fn start(dir: PathBuf, config: ServerConfig) -> Harness {
        let mut server = Server::bind(&dir, catalog(), config).unwrap();
        let address = server.address().unwrap();
        let running = server.running_flag();
        let thread = std::thread::spawn(move || {
            let _ = server.serve();
        });
        Harness {
            client: Client::new(address),
            dir,
            running,
            thread: Some(thread),
        }
    }

    pub fn fresh(name: &str) -> Harness {
        let dir = std::env::temp_dir().join(format!("schweep-c9-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Harness::start(dir, config())
    }

    /// Stop this server and start another on the same directory — a graceful restart.
    pub fn restart(mut self) -> Harness {
        let dir = self.dir.clone();
        self.stop();
        Harness::start(dir, config())
    }

    pub fn stop(&mut self) {
        let _ = self.client.shutdown();
        self.running.store(false, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        // The server is stopped; the directory is **not** removed. Several tests restart onto it, and a
        // failing test's directory is worth keeping to look at. `Harness::fresh` clears it on the way in
        // instead — the same discipline `testing/differential/tests/c8_backends.rs` uses.
        self.stop();
    }
}

pub fn config() -> ServerConfig {
    ServerConfig {
        policy: Policy::default(),
        sync: SyncPolicy::Deferred,
        checkpoint_every: 4,
    }
}

pub fn catalog() -> Catalog {
    let schema = Schema::new_table(vec![
        Field::not_null("k", DataType::Int64),
        Field::not_null("n", DataType::Int64),
    ])
    .unwrap();
    let mut catalog = Catalog::new();
    catalog.insert("t".to_owned(), schema);
    catalog
}

pub fn row(k: i64, n: i64) -> Row {
    Row::new(vec![Value::Int(k), Value::Int(n)])
}

/// A row of the two-column test table.
pub fn entry(k: i64, n: i64, weight: i64) -> (Row, i64) {
    (row(k, n), weight)
}

/// The standing query the tests register: one group per key, summing the values.
pub const SUM: &str = "SELECT t.k AS k, SUM(t.n) AS s FROM t GROUP BY t.k";
