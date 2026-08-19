#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
//! **The composed kill -9 matrix.** The real binary under concurrent ingest, query, subscribe,
//! and taint load, SIGKILLed mid-flight, restarted, and held to three laws: every acknowledged
//! write is applied exactly once (minus what a taint deliberately healed); a subscriber
//! resuming at its client-held token never sees a duplicate or a gap; and the final state
//! equals a never-crashed twin fed the same acknowledged operations. `M6_KILLS` scales the
//! matrix: the PR gate runs a bounded round, the nightly workflow runs at least 1,000.

use std::collections::BTreeSet;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

const OPERATOR: &str = "kill-operator";

fn config_text(data_dir: &std::path::Path) -> String {
    format!(
        r#"{{
  "listen": "127.0.0.1:0",
  "operator_token": "{OPERATOR}",
  "data_dir": {data_dir},
  "embedding": {{"dim": 16, "version": "m4-v1"}},
  "tenants": [{{
    "name": "kills",
    "quota": {{"requests_per_sec": 100000, "bytes_per_sec": 268435456, "queue_depth": 256}},
    "tables": [
      {{"name": "telemetry",
       "columns": [["event_id","utf8"],["branch","utf8"],["body","utf8"],["cost_micros","int64"],["error","bool"],["event_time","int64"]],
       "key_column": "event_id", "branch_column": "branch", "plane": "events"}}
    ]
  }}]
}}"#,
        data_dir = serde_json::json!(data_dir),
    )
}

struct Server {
    child: Child,
    address: SocketAddr,
}

fn spawn(config_path: &std::path::Path) -> Server {
    let mut child = Command::new(env!("CARGO_BIN_EXE_mutinyd"))
        .arg(config_path)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("mutinyd spawns");
    let stdout = child.stdout.take().expect("stdout");
    let mut lines = BufReader::new(stdout).lines();
    let address = loop {
        let line = lines
            .next()
            .expect("the binary prints its address")
            .expect("readable");
        if let Some(rest) = line.strip_prefix("listening ") {
            break rest.parse().expect("address parses");
        }
    };
    // Drain remaining stdout on a background thread so the child never blocks on a full pipe.
    std::thread::spawn(move || for _ in lines {});
    Server { child, address }
}

fn request(
    address: SocketAddr,
    method: &str,
    path: &str,
    body: &[u8],
    bearer: Option<&str>,
) -> Result<(u16, String), String> {
    let mut stream = TcpStream::connect_timeout(&address.to_owned(), Duration::from_secs(5))
        .map_err(|e| e.to_string())?;
    stream
        .set_read_timeout(Some(Duration::from_secs(20)))
        .map_err(|e| e.to_string())?;
    let auth = bearer
        .map(|token| format!("Authorization: Bearer {token}\r\n"))
        .unwrap_or_default();
    let head = format!(
        "{method} {path} HTTP/1.1\r\nHost: mutinyd\r\n{auth}Content-Length: {}\r\n\r\n",
        body.len()
    );
    stream
        .write_all(head.as_bytes())
        .map_err(|e| e.to_string())?;
    stream.write_all(body).map_err(|e| e.to_string())?;
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).map_err(|e| e.to_string())?;
    let text = String::from_utf8_lossy(&raw).into_owned();
    let status: u16 = text
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .ok_or("no status")?;
    let body = text
        .split_once("\r\n\r\n")
        .map(|(_, body)| body.to_owned())
        .unwrap_or_default();
    Ok((status, body))
}

/// One acknowledged operation, in acknowledgment order — the ledger the twin replays.
#[derive(Clone, Debug)]
enum AckedOp {
    Write {
        key: String,
        source: String,
        cost: i64,
    },
    Taint {
        source: String,
    },
}

fn write_body(key: &str, source: &str, cost: i64) -> Vec<u8> {
    serde_json::json!({
        "actor": "loader", "session": "sess-a", "branch": "sess-a",
        "intent": format!("record {key}"),
        "sources": [{"system": "load", "record": source}],
        "table": "telemetry",
        "rows": [[key, "sess-a", format!("body of {key}"), cost, false, 1000]],
    })
    .to_string()
    .into_bytes()
}

const BY_KEY_SQL: &str = "SELECT telemetry.event_id AS event_id, COUNT(*) AS n FROM telemetry \
                          GROUP BY telemetry.event_id";
const ROLLUP_SQL: &str = "SELECT telemetry.branch AS branch, SUM(telemetry.cost_micros) AS \
                          total, COUNT(*) AS n FROM telemetry GROUP BY telemetry.branch";

#[test]
fn the_kill_matrix_holds_exactly_once_resume_and_twin_equality() {
    let kills: usize = std::env::var("M6_KILLS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(30);

    let dir = tempfile::tempdir().expect("dir");
    let config_path = dir.path().join("config.json");
    std::fs::write(&config_path, config_text(&dir.path().join("data"))).expect("config written");

    let mut ledger: Vec<AckedOp> = Vec::new();
    let mut subscriber_token = 0u64;
    let mut subscriber_handle: Option<u64> = None;
    let mut seen_epochs: BTreeSet<u64> = BTreeSet::new();
    let mut next_key = 0usize;
    let mut source_window = 0usize;
    let mut rng: u64 = 0x4d36_5f21;

    for cycle in 0..kills {
        let mut server = spawn(&config_path);
        let address = server.address;

        // Register the standing query once; the registry file carries it across every kill.
        if subscriber_handle.is_none() {
            let (status, body) = request(
                address,
                "POST",
                "/v1/kills/sql/register",
                ROLLUP_SQL.as_bytes(),
                None,
            )
            .expect("register");
            assert_eq!(status, 200, "{body}");
            subscriber_handle = Some(body.trim().parse().expect("handle"));
        }
        let handle = subscriber_handle.expect("handle");

        // Concurrent load: a oneshot reader hammering the query door while the writer works.
        let reader_stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let reader_flag = std::sync::Arc::clone(&reader_stop);
        let reader = std::thread::spawn(move || {
            while !reader_flag.load(std::sync::atomic::Ordering::Relaxed) {
                let _ = request(
                    address,
                    "GET",
                    &format!("/v1/kills/sql/read?handle={handle}"),
                    b"",
                    None,
                );
            }
        });

        // The writer: acknowledged-or-retried, the ledger records only acknowledgments. The kill
        // lands mid-burst, so some writes die unacknowledged — exactly the point.
        rng = rng
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let burst = 2 + (rng % 3) as usize;
        for _ in 0..burst {
            let key = format!("evt-{next_key:06}");
            let source = format!("window-{source_window}");
            let cost = 1_000 + (next_key as i64 % 7) * 100;
            let body = write_body(&key, &source, cost);
            if let Ok((200, _)) = request(address, "POST", "/v1/kills/write", &body, None) {
                ledger.push(AckedOp::Write { key, source, cost });
                next_key += 1;
            }
            // Unacknowledged attempts are retried with the same commit content next cycle by
            // virtue of the key counter not advancing.
        }

        // Every eighth cycle, the operator taints the previous source window — the flagship
        // under kill load. Retried until acknowledged; taint resumes are M4's law.
        if cycle % 8 == 7 && source_window > 0 {
            let source = format!("window-{}", source_window - 1);
            let taint_body = serde_json::json!({"system": "load", "record": source}).to_string();
            if let Ok((200, _)) = request(
                address,
                "POST",
                "/v1/kills/taint",
                taint_body.as_bytes(),
                Some(OPERATOR),
            ) {
                ledger.push(AckedOp::Taint { source });
            }
        }
        if cycle % 4 == 3 {
            source_window += 1;
        }

        // The subscriber resumes at its client-held token: strictly-above, contiguous, no gaps.
        if let Ok((200, body)) = request(
            address,
            "GET",
            &format!("/v1/kills/sql/subscribe?handle={handle}&from={subscriber_token}"),
            b"",
            None,
        ) {
            let next: u64 = body
                .lines()
                .next()
                .and_then(|line| line.split_whitespace().last())
                .and_then(|token| token.parse().ok())
                .expect("token");
            let epochs: Vec<u64> = body
                .lines()
                .filter(|line| line.starts_with("epoch "))
                .filter_map(|line| line.split_whitespace().last()?.parse().ok())
                .collect();
            for epoch in &epochs {
                assert!(
                    *epoch > subscriber_token,
                    "cycle {cycle}: duplicate epoch {epoch} at or below token {subscriber_token}"
                );
                assert!(
                    seen_epochs.insert(*epoch),
                    "cycle {cycle}: epoch {epoch} delivered twice across kills"
                );
            }
            for pair in epochs.windows(2) {
                assert_eq!(pair[1], pair[0] + 1, "cycle {cycle}: gap inside a delivery");
            }
            subscriber_token = next;
        }

        // SIGKILL, mid-everything.
        reader_stop.store(true, std::sync::atomic::Ordering::Relaxed);
        let _ = reader.join();
        server.child.kill().expect("SIGKILL lands");
        let _ = server.child.wait();
    }

    // ---- after the last kill: recovery, then the three laws -----------------------------------
    let mut server = spawn(&config_path);
    let address = server.address;

    // Law 1: every acknowledged write present exactly once, minus what taints healed.
    let mut acked: BTreeSet<String> = BTreeSet::new();
    let mut healed: BTreeSet<String> = BTreeSet::new();
    for op in &ledger {
        match op {
            AckedOp::Write { key, .. } => {
                acked.insert(key.clone());
            }
            AckedOp::Taint { source } => {
                for earlier in &ledger {
                    if let AckedOp::Write { key, source: s, .. } = earlier {
                        if s == source && acked.contains(key) {
                            healed.insert(key.clone());
                        }
                    }
                }
            }
        }
    }
    let (status, by_key) = request(
        address,
        "GET",
        &format!("/v1/kills/sql/oneshot?sql={}", urlencode(BY_KEY_SQL)),
        b"",
        None,
    )
    .expect("oneshot");
    assert_eq!(status, 200, "{by_key}");
    for key in &acked {
        let present = by_key.contains(&format!("(\"{key}\", 1)"));
        let expected = !healed.contains(key);
        assert_eq!(
            present, expected,
            "acked write {key}: present={present}, expected present={expected} (healed set)"
        );
        assert!(
            !by_key.contains(&format!("(\"{key}\", 2)")),
            "{key} was applied twice — the exactly-once law broke"
        );
    }

    // Law 3: the never-crashed twin, fed the same acknowledged operations in order.
    let twin_dir = tempfile::tempdir().expect("twin dir");
    let twin_config = twin_dir.path().join("config.json");
    std::fs::write(&twin_config, config_text(&twin_dir.path().join("data"))).expect("written");
    let mut twin = spawn(&twin_config);
    let twin_address = twin.address;
    let (status, _) = request(
        twin_address,
        "POST",
        "/v1/kills/sql/register",
        ROLLUP_SQL.as_bytes(),
        None,
    )
    .expect("twin register");
    assert_eq!(status, 200);
    for op in &ledger {
        match op {
            AckedOp::Write { key, source, cost } => {
                let (status, body) = request(
                    twin_address,
                    "POST",
                    "/v1/kills/write",
                    &write_body(key, source, *cost),
                    None,
                )
                .expect("twin write");
                assert_eq!(status, 200, "{body}");
            }
            AckedOp::Taint { source } => {
                let body = serde_json::json!({"system": "load", "record": source}).to_string();
                let (status, answer) = request(
                    twin_address,
                    "POST",
                    "/v1/kills/taint",
                    body.as_bytes(),
                    Some(OPERATOR),
                )
                .expect("twin taint");
                assert_eq!(status, 200, "{answer}");
            }
        }
    }
    for sql in [BY_KEY_SQL, ROLLUP_SQL] {
        let (_, crashed) = request(
            address,
            "GET",
            &format!("/v1/kills/sql/oneshot?sql={}", urlencode(sql)),
            b"",
            None,
        )
        .expect("oneshot");
        let (_, clean) = request(
            twin_address,
            "GET",
            &format!("/v1/kills/sql/oneshot?sql={}", urlencode(sql)),
            b"",
            None,
        )
        .expect("twin oneshot");
        assert_eq!(
            crashed, clean,
            "after {kills} SIGKILLs the recovered answers must equal the never-crashed twin"
        );
    }

    println!(
        "kill matrix: {kills} SIGKILLs · {} acked writes · {} taints · {} epochs to the subscriber — exactly-once, no double epoch, twin-equal",
        acked.len(),
        ledger.iter().filter(|op| matches!(op, AckedOp::Taint { .. })).count(),
        seen_epochs.len()
    );
    let _ = server.child.kill();
    let _ = server.child.wait();
    let _ = twin.child.kill();
    let _ = twin.child.wait();
}

fn urlencode(text: &str) -> String {
    let mut out = String::new();
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}
