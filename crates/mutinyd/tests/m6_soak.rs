#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
//! **The soak: flat memory, shape AND budget.** The real binary under sustained ingest + query +
//! subscribe load, with the flagship as the boundedness mechanism: every window of writes is
//! tainted away a window later, so standing state stays bounded while the write path never
//! stops. Resident memory is sampled throughout (honestly per platform — see [`rss_kb`]); the
//! gate asserts an absolute budget, a flat steady-state shape (the last third's average within
//! tolerance of the **middle** third's — the first third is warmup), and since M8 a bound on the
//! storage the maintenance pass must keep consumed (docs/M8-MAINTENANCE.md). `M6_SOAK_SECS`
//! scales it: the PR gate runs a short soak, the nightly runs the long one.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const OPERATOR: &str = "soak-operator";
const RSS_BUDGET_KB: u64 = 786_432; // 768 MiB — the absolute budget.
/// Last-third average ≤ **middle**-third average × this, on the **residual** (resident memory
/// minus the measured live-data term). The middle third is the baseline, not the first: the
/// first third is the warmup ramp from a cold start to the working set, and a gate that
/// punishes reaching a working set fails flat processes. The residual is the honest subject:
/// the taint ledger is append-only by M4's law, so live data grows with every taint and
/// resident memory legitimately tracks it — what must NOT grow is memory beyond live data
/// (that is history, the #12 bug class). The pre-fix growth still fires this gate: its
/// consumer was capture-history storage, uncorrelated with the live-data term, so the
/// residual showed the full 1.4×+ linear climb.
const SHAPE_TOLERANCE: f64 = 1.30;

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

/// Resident memory in KiB — **what memory pressure actually sees**, measured honestly per
/// platform. Raw `ps rss` lies in both directions of reclaimable-but-resident pages: on macOS
/// the default allocator retains clean reclaimable pages that `ps` counts (footprint was flat
/// at ~21 MB while `ps` said 66 MB and climbing — measured 2026-08-21), so macOS uses
/// `footprint(1)`; on Linux an allocator purging with `MADV_FREE` leaves those pages in RSS
/// until pressure reclaims them (`LazyFree` in smaps_rollup counts exactly those — the second
/// post-#14 nightly's "growth" was this), so Linux uses `Rss − LazyFree`. A genuine leak is
/// never clean and never LazyFree, so it is fully visible to both instruments. `ps` is the
/// last-resort fallback.
fn rss_kb(pid: u32) -> Option<u64> {
    if cfg!(target_os = "linux") {
        // Same correction, Linux form: an allocator that purges freed pages with MADV_FREE
        // (mimalloc's default) leaves them IN RSS until memory pressure reclaims them —
        // `LazyFree` in smaps_rollup counts exactly those. Rss − LazyFree is what pressure
        // actually sees, allocator-agnostic; a genuine leak is never LazyFree.
        if let Ok(text) = std::fs::read_to_string(format!("/proc/{pid}/smaps_rollup")) {
            let field = |name: &str| -> Option<u64> {
                text.lines().find_map(|line| {
                    line.strip_prefix(name)?
                        .trim()
                        .strip_suffix("kB")
                        .and_then(|value| value.trim().parse().ok())
                })
            };
            if let Some(rss) = field("Rss:") {
                return Some(rss.saturating_sub(field("LazyFree:").unwrap_or(0)));
            }
        }
    }
    if cfg!(target_os = "macos") {
        if let Ok(output) = Command::new("footprint").arg(pid.to_string()).output() {
            let text = String::from_utf8_lossy(&output.stdout).into_owned();
            if let Some(kb) = text.lines().find_map(|line| {
                let rest = line.split("Footprint: ").nth(1)?;
                let mut parts = rest.split_whitespace();
                let value: f64 = parts.next()?.parse().ok()?;
                let unit = parts.next()?;
                match unit {
                    "KB" => Some(value as u64),
                    "MB" => Some((value * 1024.0) as u64),
                    "GB" => Some((value * 1024.0 * 1024.0) as u64),
                    _ => None,
                }
            }) {
                return Some(kb);
            }
        }
    }
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

    // The live-data term (docs/M8-MAINTENANCE.md): the taint ledger is append-only BY M4's LAW
    // (every taint's resolution is journaled with per-row envelopes, and RecallReports regenerate
    // from it), so the engine's live data — and its compaction snapshot on disk — grows with
    // every taint. That is a database holding data, not a process leaking. The shape gate
    // therefore subtracts the *measured* on-disk live-data snapshot (compute/log) from each
    // sample and requires the residual flat: resident memory may track live data; it must never
    // track history. Both terms are printed, so a reviewer can see exactly what was subtracted.
    let live_data_dir = dir
        .path()
        .join("data")
        .join("soak")
        .join("compute")
        .join("log");
    fn dir_kib(path: &std::path::Path) -> u64 {
        let Ok(entries) = std::fs::read_dir(path) else {
            return 0;
        };
        entries
            .flatten()
            .map(|entry| match entry.metadata() {
                Ok(meta) if meta.is_dir() => dir_kib(&entry.path()),
                Ok(meta) if meta.is_file() => meta.len() / 1024,
                _ => 0,
            })
            .sum()
    }

    let started = Instant::now();
    let mut samples: Vec<u64> = Vec::new();
    let mut live_samples: Vec<u64> = Vec::new();
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
            live_samples.push(dir_kib(&live_data_dir));
        }
    }

    assert!(
        samples.len() >= 6,
        "the soak needs enough samples to have a shape"
    );
    let peak = *samples.iter().max().expect("samples");
    let adjusted: Vec<u64> = samples
        .iter()
        .zip(&live_samples)
        .map(|(rss, live)| rss.saturating_sub(*live))
        .collect();
    let third = samples.len() / 3;
    let thirds = |values: &[u64]| -> (f64, f64, f64) {
        let avg = |slice: &[u64]| slice.iter().sum::<u64>() as f64 / third as f64;
        (
            avg(&values[..third]),
            avg(&values[third..2 * third]),
            avg(&values[values.len() - third..]),
        )
    };
    let (raw_first, raw_middle, raw_last) = thirds(&samples);
    let (live_first, live_middle, live_last) = thirds(&live_samples);
    let (first, middle, last) = thirds(&adjusted);
    println!(
        "soak: {seconds}s · {writes} writes · {taints} taints · rss thirds {raw_first:.0} / \
         {raw_middle:.0} / {raw_last:.0} KiB · live-data thirds {live_first:.0} / \
         {live_middle:.0} / {live_last:.0} KiB · residual thirds {first:.0} / {middle:.0} / \
         {last:.0} KiB · peak {peak} KiB (budget {RSS_BUDGET_KB})"
    );
    assert!(
        peak < RSS_BUDGET_KB,
        "the budget: peak RSS {peak} KiB exceeded {RSS_BUDGET_KB} KiB"
    );
    assert!(
        last <= middle * SHAPE_TOLERANCE,
        "the shape: last-third residual RSS {last:.0} KiB (raw {raw_last:.0} minus live-data \
         {live_last:.0}) grew beyond {SHAPE_TOLERANCE}× the middle third {middle:.0} KiB — \
         memory is tracking something other than live data"
    );

    // The storage bound (docs/M8-MAINTENANCE.md, issue #12): awake maintenance must keep the
    // durable queue consumed — manifests and pages measured, not described. Pre-fix, this
    // workload grew storage O(commits) (~75 MB by window 220); the bounds below are an order of
    // magnitude under that and hold at any duration.
    let storage = dir.path().join("data").join("soak").join("storage");
    // Manifests and pages are sharded into prefix directories; walk the subtree.
    fn count_and_bytes_walk(path: &std::path::Path) -> (u64, u64) {
        let Ok(entries) = std::fs::read_dir(path) else {
            return (0, 0);
        };
        let mut files = 0;
        let mut bytes = 0;
        for entry in entries.flatten() {
            match entry.metadata() {
                Ok(meta) if meta.is_dir() => {
                    let (f, b) = count_and_bytes_walk(&entry.path());
                    files += f;
                    bytes += b;
                }
                Ok(meta) if meta.is_file() => {
                    files += 1;
                    bytes += meta.len();
                }
                _ => {}
            }
        }
        (files, bytes)
    }
    let count_and_bytes = |sub: &str| -> (u64, u64) { count_and_bytes_walk(&storage.join(sub)) };
    let (manifest_files, manifest_bytes) = count_and_bytes("manifests");
    let (page_files, page_bytes) = count_and_bytes("pages");
    println!(
        "storage bound: {manifest_files} manifests ({manifest_bytes} B), {page_files} pages \
         ({page_bytes} B) after {writes} writes"
    );
    assert!(
        manifest_files <= 256,
        "storage/manifests holds {manifest_files} files — the queue is not being consumed"
    );
    assert!(
        manifest_bytes + page_bytes <= 32 * 1024 * 1024,
        "storage holds {} bytes — the queue is not being consumed",
        manifest_bytes + page_bytes
    );

    let _ = request(address, "POST", "/shutdown", b"", Some(OPERATOR));
    let _ = child.wait();
}
