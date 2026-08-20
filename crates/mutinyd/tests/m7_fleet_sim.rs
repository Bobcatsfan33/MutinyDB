#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
//! **The fleet simulation** (docs/M7-FLEET.md): N sleeping tenants on one host — the registry
//! holds them, resident memory stays flat and is measured, a random subset receives deltas and
//! wakes selectively, and p50/p99 wake-to-first-answer is published with the storage backend
//! named. `M7_SIM_TENANTS` scales it (PR gate small; the published run is 10,000 in CI under a
//! cgroup memory ceiling). The real binary is measured, not the test process.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const OPERATOR: &str = "sim-operator";

/// Resident cost a sleeping tenant is allowed to add (registry row + config), measured against
/// the RSS curve while the fleet grows. Generous on purpose: the claim is "flat", not "zero".
const MAX_RESIDENT_BYTES_PER_SLEEPER: u64 = 24 * 1024;

fn request(
    address: SocketAddr,
    method: &str,
    path: &str,
    body: &[u8],
    bearer: Option<&str>,
) -> Result<(u16, String), String> {
    let mut stream =
        TcpStream::connect_timeout(&address, Duration::from_secs(10)).map_err(|e| e.to_string())?;
    stream
        .set_read_timeout(Some(Duration::from_secs(60)))
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

fn ok(address: SocketAddr, method: &str, path: &str, body: &[u8], bearer: Option<&str>) -> String {
    let (status, text) = request(address, method, path, body, bearer).expect("request");
    assert_eq!(status, 200, "{method} {path}: {text}");
    text
}

fn rss_kb(pid: u32) -> Option<u64> {
    let output = Command::new("ps")
        .args(["-o", "rss=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    String::from_utf8_lossy(&output.stdout).trim().parse().ok()
}

fn tenant_config(name: &str) -> String {
    format!(
        r#"{{
      "name": "{name}",
      "quota": {{"requests_per_sec": 100000, "bytes_per_sec": 268435456, "queue_depth": 64}},
      "tables": [
        {{"name": "telemetry",
         "columns": [["event_id","utf8"],["branch","utf8"],["body","utf8"],["cost_micros","int64"],["error","bool"],["event_time","int64"]],
         "key_column": "event_id", "branch_column": "branch", "plane": "events"}}
      ]
    }}"#
    )
}

const ROLLUP_SQL: &str = "SELECT telemetry.branch AS branch, SUM(telemetry.cost_micros) AS \
                          total, COUNT(*) AS n FROM telemetry GROUP BY telemetry.branch";

#[test]
fn the_fleet_simulation_sleeps_n_tenants_and_wakes_selectively() {
    let tenants: usize = std::env::var("M7_SIM_TENANTS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(60);
    let wake_subset: usize = (tenants / 20).clamp(3, 200);

    let dir = tempfile::tempdir().expect("dir");
    let config_path = dir.path().join("config.json");
    std::fs::write(
        &config_path,
        format!(
            r#"{{
  "listen": "127.0.0.1:0",
  "operator_token": "{OPERATOR}",
  "data_dir": {data_dir},
  "embedding": {{"dim": 16, "version": "m4-v1"}},
  "tenants": [{seed}]
}}"#,
            data_dir = serde_json::json!(dir.path().join("data")),
            seed = tenant_config("seed-tenant"),
        ),
    )
    .expect("config");

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
        let line = lines.next().expect("address").expect("readable");
        if let Some(rest) = line.strip_prefix("listening ") {
            break rest.parse().expect("address");
        }
    };
    std::thread::spawn(move || for _ in lines {});

    // ---- build the sleeping fleet ---------------------------------------------------------------
    let mut rss_curve: Vec<(usize, u64)> = Vec::new();
    let mut handles: Vec<u64> = Vec::new();
    let started_build = Instant::now();
    for index in 0..tenants {
        let name = format!("sim-{index:05}");
        ok(
            address,
            "POST",
            "/fleet/register",
            tenant_config(&name).as_bytes(),
            Some(OPERATOR),
        );
        let write = serde_json::json!({
            "actor": "sim", "session": "sess-a", "branch": "sess-a",
            "intent": "seed", "sources": [{"system": "load", "record": "seed"}],
            "table": "telemetry",
            "rows": [[format!("evt-{index}"), "sess-a", "seed body", 1_000_000, false, 1000]],
        });
        ok(
            address,
            "POST",
            &format!("/v1/{name}/write"),
            write.to_string().as_bytes(),
            None,
        );
        let handle: u64 = ok(
            address,
            "POST",
            &format!("/v1/{name}/sql/register"),
            ROLLUP_SQL.as_bytes(),
            None,
        )
        .trim()
        .parse()
        .expect("handle");
        handles.push(handle);
        ok(
            address,
            "POST",
            "/fleet/sleep",
            serde_json::json!({"tenant": name}).to_string().as_bytes(),
            Some(OPERATOR),
        );
        if (index + 1) % (tenants / 3).max(1) == 0 {
            if let Some(rss) = rss_kb(pid) {
                rss_curve.push((index + 1, rss));
            }
        }
    }
    let build_elapsed = started_build.elapsed();

    let status = ok(address, "GET", "/fleet/status", b"", Some(OPERATOR));
    let resident: usize = status
        .lines()
        .find_map(|line| line.strip_prefix("resident ")?.trim().parse().ok())
        .expect("resident");
    let registered: usize = status
        .lines()
        .find_map(|line| line.strip_prefix("registered ")?.trim().parse().ok())
        .expect("registered");
    assert_eq!(
        registered,
        tenants + 1,
        "every registered tenant is a registry row"
    );
    assert_eq!(resident, 0, "a sleeping fleet holds no resident planes");

    // ---- resident memory is flat while they sleep ----------------------------------------------
    let final_rss = rss_kb(pid).expect("rss");
    println!("fleet rss curve (tenants, KiB): {rss_curve:?} final {final_rss} KiB");
    if let (Some((n1, first)), Some((n2, last))) = (rss_curve.first(), rss_curve.last()) {
        // Allocator noise dominates small spans; the per-sleeper claim is asserted where it
        // means something (the 10k CI run), and the absolute budget is asserted always.
        if n2 - n1 >= 500 {
            let per_sleeper = (last.saturating_sub(*first) * 1024) / ((n2 - n1) as u64);
            assert!(
                per_sleeper <= MAX_RESIDENT_BYTES_PER_SLEEPER,
                "a sleeping tenant costs {per_sleeper} resident bytes; the flat-memory claim \
                 allows at most {MAX_RESIDENT_BYTES_PER_SLEEPER}"
            );
        }
    }
    assert!(
        final_rss < 1_572_864,
        "resident memory {final_rss} KiB exceeded the 1.5 GiB sim budget"
    );

    // ---- a random subset receives deltas and wakes selectively ----------------------------------
    let mut state: u64 = 0x517c_c1b7_2722_0a95;
    let mut chosen = std::collections::BTreeSet::new();
    while chosen.len() < wake_subset {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        chosen.insert((state >> 16) as usize % tenants);
    }
    let mut wake_ns: Vec<u128> = Vec::new();
    for index in &chosen {
        let name = format!("sim-{index:05}");
        let started = Instant::now();
        // The waking delta: one write; the answer read immediately after is the first answer.
        let write = serde_json::json!({
            "actor": "sim", "session": "sess-a", "branch": "sess-a",
            "intent": "wake", "sources": [{"system": "load", "record": "wake"}],
            "table": "telemetry",
            "rows": [[format!("wake-{index}"), "sess-a", "wake body", 2_000_000, false, 2000]],
        });
        ok(
            address,
            "POST",
            &format!("/v1/{name}/write"),
            write.to_string().as_bytes(),
            None,
        );
        let answer = ok(
            address,
            "GET",
            &format!("/v1/{name}/sql/read?handle={}", handles[*index]),
            b"",
            None,
        );
        wake_ns.push(started.elapsed().as_nanos());
        assert!(answer.contains("3000000"), "{name}: {answer}");
    }
    let status = ok(address, "GET", "/fleet/status", b"", Some(OPERATOR));
    let resident_after: usize = status
        .lines()
        .find_map(|line| line.strip_prefix("resident ")?.trim().parse().ok())
        .expect("resident");
    assert_eq!(
        resident_after,
        chosen.len(),
        "selectivity: exactly the addressed tenants woke"
    );

    wake_ns.sort_unstable();
    let p50 = wake_ns[wake_ns.len() / 2];
    let p99 = wake_ns[(wake_ns.len() * 99 / 100).min(wake_ns.len() - 1)];
    let peak_rss = rss_kb(pid).expect("rss");
    println!(
        "m7-fleet-sim: {{\"tenants\": {tenants}, \"woken\": {}, \"wake_to_first_answer_ms\": \
         {{\"p50\": {:.1}, \"p99\": {:.1}}}, \"build_secs\": {:.0}, \"rss_after_wakes_kib\": \
         {peak_rss}, \"storage_backend\": \"local filesystem (see job for the exact runner \
         disk)\", \"mode\": \"{}\"}}",
        chosen.len(),
        p50 as f64 / 1e6,
        p99 as f64 / 1e6,
        build_elapsed.as_secs_f64(),
        if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        },
    );

    ok(address, "POST", "/shutdown", b"", Some(OPERATOR));
    let _ = child.wait();
}
