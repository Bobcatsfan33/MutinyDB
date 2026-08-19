#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
//! **The soak: flat memory, shape AND budget.** The real binary under sustained ingest + query +
//! subscribe load, with the flagship as the boundedness mechanism: every window of writes is
//! tainted away a window later, so standing state stays bounded while the write path never
//! stops. RSS is sampled throughout; the gate asserts an absolute budget and a flat shape (the
//! last third's average within tolerance of the first third's). `M6_SOAK_SECS` scales it: the PR
//! gate runs a short soak, the nightly runs the long one.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const OPERATOR: &str = "soak-operator";
const RSS_BUDGET_KB: u64 = 786_432; // 768 MiB — the absolute budget.
const SHAPE_TOLERANCE: f64 = 1.40; // last-third average ≤ first-third average × this.

fn config_text(data_dir: &std::path::Path) -> String {
    format!(
        r#"{{
  "listen": "127.0.0.1:0",
  "operator_token": "{OPERATOR}",
  "data_dir": {data_dir},
  "embedding": {{"dim": 16, "version": "m4-v1"}},
  "tenants": [{{
    "name": "soak",
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

fn request(
    address: SocketAddr,
    method: &str,
    path: &str,
    body: &[u8],
    bearer: Option<&str>,
) -> Result<(u16, String), String> {
    let mut stream =
        TcpStream::connect_timeout(&address, Duration::from_secs(5)).map_err(|e| e.to_string())?;
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
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
    Ok((
        status,
        text.split_once("\r\n\r\n")
            .map(|(_, body)| body.to_owned())
            .unwrap_or_default(),
    ))
}

fn rss_kb(pid: u32) -> Option<u64> {
    let output = Command::new("ps")
        .args(["-o", "rss=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    String::from_utf8_lossy(&output.stdout).trim().parse().ok()
}

#[test]
fn the_soak_keeps_memory_flat_in_shape_and_budget() {
    let seconds: u64 = std::env::var("M6_SOAK_SECS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(25);

    let dir = tempfile::tempdir().expect("dir");
    let config_path = dir.path().join("config.json");
    std::fs::write(&config_path, config_text(&dir.path().join("data"))).expect("config");

    let mut child = Command::new(env!("CARGO_BIN_EXE_mutinyd"))
        .arg(&config_path)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("mutinyd spawns");
    let pid = child.id();
    let stdout = child.stdout.take().expect("stdout");
    let mut lines = BufReader::new(stdout).lines();
    let address: SocketAddr = loop {
        let line = lines.next().expect("address line").expect("readable");
        if let Some(rest) = line.strip_prefix("listening ") {
            break rest.parse().expect("address");
        }
    };
    std::thread::spawn(move || for _ in lines {});

    let rollup = "SELECT telemetry.branch AS branch, SUM(telemetry.cost_micros) AS total, \
                  COUNT(*) AS n FROM telemetry GROUP BY telemetry.branch";
    let (status, handle_line) = request(
        address,
        "POST",
        "/v1/soak/sql/register",
        rollup.as_bytes(),
        None,
    )
    .expect("register");
    assert_eq!(status, 200, "{handle_line}");
    let handle: u64 = handle_line.trim().parse().expect("handle");

    let started = Instant::now();
    let mut samples: Vec<u64> = Vec::new();
    let mut key = 0usize;
    let mut window = 0usize;
    let mut token = 0u64;
    let mut writes = 0u64;
    let mut taints = 0u64;
    while started.elapsed().as_secs() < seconds {
        // A window of writes, all citing the window's source…
        for _ in 0..16 {
            let body = serde_json::json!({
                "actor": "soak", "session": "sess-a", "branch": "sess-a",
                "intent": "soak write",
                "sources": [{"system": "load", "record": format!("window-{window}")}],
                "table": "telemetry",
                "rows": [[format!("evt-{key:08}"), "sess-a", format!("soak body {key}"), 1000, false, 1000]],
            })
            .to_string();
            let (status, answer) =
                request(address, "POST", "/v1/soak/write", body.as_bytes(), None).expect("write");
            assert_eq!(status, 200, "{answer}");
            key += 1;
            writes += 1;
        }
        // …the standing answer read, the subscriber advanced…
        let (status, _) = request(
            address,
            "GET",
            &format!("/v1/soak/sql/read?handle={handle}"),
            b"",
            None,
        )
        .expect("read");
        assert_eq!(status, 200);
        if let Ok((200, body)) = request(
            address,
            "GET",
            &format!("/v1/soak/sql/subscribe?handle={handle}&from={token}"),
            b"",
            None,
        ) {
            if let Some(next) = body
                .lines()
                .next()
                .and_then(|line| line.split_whitespace().last())
                .and_then(|value| value.parse().ok())
            {
                token = next;
            }
        }
        // …and the previous window tainted away: the flagship as the boundedness mechanism.
        if window > 0 {
            let taint =
                serde_json::json!({"system": "load", "record": format!("window-{}", window - 1)})
                    .to_string();
            let (status, answer) = request(
                address,
                "POST",
                "/v1/soak/taint",
                taint.as_bytes(),
                Some(OPERATOR),
            )
            .expect("taint");
            assert_eq!(status, 200, "{answer}");
            taints += 1;
        }
        window += 1;
        if let Some(rss) = rss_kb(pid) {
            samples.push(rss);
        }
    }

    assert!(
        samples.len() >= 6,
        "the soak needs enough samples to have a shape"
    );
    let peak = *samples.iter().max().expect("samples");
    let third = samples.len() / 3;
    let first: f64 = samples[..third].iter().sum::<u64>() as f64 / third as f64;
    let last: f64 = samples[samples.len() - third..].iter().sum::<u64>() as f64 / third as f64;
    println!(
        "soak: {seconds}s · {writes} writes · {taints} taints · rss first-third {first:.0} KiB, \
         last-third {last:.0} KiB, peak {peak} KiB (budget {RSS_BUDGET_KB})"
    );
    assert!(
        peak < RSS_BUDGET_KB,
        "the budget: peak RSS {peak} KiB exceeded {RSS_BUDGET_KB} KiB"
    );
    assert!(
        last <= first * SHAPE_TOLERANCE,
        "the shape: last-third RSS {last:.0} KiB grew beyond {SHAPE_TOLERANCE}× the first third \
         {first:.0} KiB — memory is not flat under retract-as-you-go load"
    );

    let _ = request(address, "POST", "/shutdown", b"", Some(OPERATOR));
    let _ = child.wait();
}
