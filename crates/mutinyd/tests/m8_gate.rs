#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
//! **The M8 maintenance gate, composed** (docs/M8-MAINTENANCE.md, issue #12): awake maintenance
//! bounds the durable queue and changes no answer; a crash on every maintenance seam (S1–S6)
//! recovers to the never-crashed twin through the real binary; full replay of a collapsed store
//! refuses by name; taint heals identically across maintenance; and tooth (b) — a GC that
//! reclaims a sleeping tenant's pages — is caught by the wake gate, loudly.

use mutinyd::{Config, MutinyServer};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

const OPERATOR: &str = "m8-operator";

struct Http {
    address: SocketAddr,
}

struct HttpAnswer {
    status: u16,
    body: String,
}

impl Http {
    fn try_request(
        &self,
        method: &str,
        path: &str,
        body: &[u8],
        bearer: Option<&str>,
    ) -> Result<HttpAnswer, String> {
        let mut stream = TcpStream::connect_timeout(&self.address, Duration::from_secs(10))
            .map_err(|e| e.to_string())?;
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
        Ok(HttpAnswer {
            status,
            body: text
                .split_once("\r\n\r\n")
                .map(|(_, body)| body.to_owned())
                .unwrap_or_default(),
        })
    }

    fn request(&self, method: &str, path: &str, body: &[u8], bearer: Option<&str>) -> HttpAnswer {
        self.try_request(method, path, body, bearer)
            .expect("request")
    }

    fn ok(&self, method: &str, path: &str, body: &[u8], bearer: Option<&str>) -> String {
        let answer = self.request(method, path, body, bearer);
        assert_eq!(answer.status, 200, "{method} {path}: {}", answer.body);
        answer.body
    }
}

fn tenant_config(name: &str, maintenance_every: u64) -> String {
    format!(
        r#"{{
      "name": "{name}",
      "maintenance_every": {maintenance_every},
      "quota": {{"requests_per_sec": 100000, "bytes_per_sec": 268435456, "queue_depth": 128}},
      "tables": [
        {{"name": "telemetry",
         "columns": [["event_id","utf8"],["branch","utf8"],["body","utf8"],["cost_micros","int64"],["error","bool"],["event_time","int64"]],
         "key_column": "event_id", "branch_column": "branch", "plane": "events",
         "semantic": {{"body_column":"body","event_time_column":"event_time","cost_micros_column":"cost_micros","error_column":"error"}}}}
      ],
      "semantic_standing": {{
        "topk": [{{"id": "incident-similar", "text": "urgent credential compromise investigation", "k": 3}}]
      }}
    }}"#
    )
}

fn config_json(data_dir: &std::path::Path, name: &str, maintenance_every: u64) -> String {
    format!(
        r#"{{
  "listen": "127.0.0.1:0",
  "operator_token": "{OPERATOR}",
  "data_dir": {data_dir},
  "embedding": {{"dim": 16, "version": "m4-v1"}},
  "tenants": [{tenant}]
}}"#,
        data_dir = serde_json::json!(data_dir),
        tenant = tenant_config(name, maintenance_every),
    )
}

struct World {
    dir: tempfile::TempDir,
    http: Http,
}

fn start(name: &str, maintenance_every: u64) -> World {
    let dir = tempfile::tempdir().expect("dir");
    let config = Config::from_json(&config_json(
        &dir.path().join("data"),
        name,
        maintenance_every,
    ))
    .expect("config");
    let server = MutinyServer::bind(&config).expect("binds");
    let address = server.address().expect("address");
    std::thread::spawn(move || {
        let _ = server.serve();
    });
    World {
        dir,
        http: Http { address },
    }
}

struct BinaryWorld {
    dir: tempfile::TempDir,
    child: Child,
    http: Http,
}

fn spawn_binary(
    dir: tempfile::TempDir,
    name: &str,
    maintenance_every: u64,
    env: &[(&str, &str)],
) -> BinaryWorld {
    let config_path = dir.path().join("config.json");
    if !config_path.exists() {
        std::fs::write(
            &config_path,
            config_json(&dir.path().join("data"), name, maintenance_every),
        )
        .expect("config");
    }
    let mut command = Command::new(env!("CARGO_BIN_EXE_mutinyd"));
    command
        .arg(&config_path)
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    for (key, value) in env {
        command.env(key, value);
    }
    let mut child = command.spawn().expect("mutinyd spawns");
    let stdout = child.stdout.take().expect("stdout");
    let mut lines = BufReader::new(stdout).lines();
    let address: SocketAddr = loop {
        let line = lines.next().expect("address").expect("readable");
        if let Some(rest) = line.strip_prefix("listening ") {
            break rest.parse().expect("address");
        }
    };
    std::thread::spawn(move || for _ in lines {});
    BinaryWorld {
        dir,
        child,
        http: Http { address },
    }
}

fn write_event(http: &Http, tenant: &str, key: &str, source: (&str, &str)) -> bool {
    let request = serde_json::json!({
        "actor": "analyst", "session": "sess-a", "branch": "sess-a",
        "intent": format!("record {key}"),
        "sources": [{"system": source.0, "record": source.1}],
        "table": "telemetry",
        "rows": [[key, "sess-a", format!("routine sample {key}"), 1_000_000, false, 1000]],
    });
    matches!(
        http.try_request(
            "POST",
            &format!("/v1/{tenant}/write"),
            request.to_string().as_bytes(),
            None,
        ),
        Ok(answer) if answer.status == 200
    )
}

fn taint(http: &Http, tenant: &str, source: (&str, &str)) -> bool {
    matches!(
        http.try_request(
            "POST",
            &format!("/v1/{tenant}/taint"),
            serde_json::json!({"system": source.0, "record": source.1})
                .to_string()
                .as_bytes(),
            Some(OPERATOR),
        ),
        Ok(answer) if answer.status == 200
    )
}

const ROLLUP_SQL: &str = "SELECT telemetry.branch AS branch, SUM(telemetry.cost_micros) AS \
                          total_cost, COUNT(*) AS events FROM telemetry GROUP BY telemetry.branch";

fn rollup(http: &Http, tenant: &str) -> String {
    strip_epoch(&http.ok(
        "GET",
        &format!("/v1/{tenant}/sql/oneshot?sql={}", urlencode(ROLLUP_SQL)),
        b"",
        None,
    ))
}

fn semantic(http: &Http, tenant: &str) -> String {
    http.ok(
        "GET",
        &format!("/v1/{tenant}/semantic/answer?branch=sess-a&query=incident-similar"),
        b"",
        None,
    )
}

fn strip_epoch(answer: &str) -> String {
    answer
        .lines()
        .filter(|line| !line.starts_with("epoch "))
        .collect::<Vec<_>>()
        .join("\n")
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

fn storage_files_and_bytes(tenant_dir: &std::path::Path, sub: &str) -> (u64, u64) {
    fn walk(path: &std::path::Path) -> (u64, u64) {
        let Ok(entries) = std::fs::read_dir(path) else {
            return (0, 0);
        };
        let mut files = 0;
        let mut bytes = 0;
        for entry in entries.flatten() {
            match entry.metadata() {
                Ok(meta) if meta.is_dir() => {
                    let (f, b) = walk(&entry.path());
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
    walk(&tenant_dir.join("storage").join(sub))
}

/// The identical retract-as-you-go workload on both worlds: `windows` windows of `per` writes,
/// each window's source tainted away one window later.
fn drive(http: &Http, tenant: &str, windows: usize, per: usize) {
    let mut key = 0;
    for window in 0..windows {
        for _ in 0..per {
            assert!(write_event(
                http,
                tenant,
                &format!("evt-{key:05}"),
                ("load", &format!("window-{window}")),
            ));
            key += 1;
        }
        if window > 0 {
            assert!(taint(
                http,
                tenant,
                ("load", &format!("window-{}", window - 1))
            ));
        }
    }
}

// ---- the gate ---------------------------------------------------------------------------------

/// **Maintenance bounds the queue and changes no answer.** The same workload runs against a
/// maintained world (every 8 commits) and a never-maintained twin; the answers are byte-equal
/// and the maintained world's storage is measurably bounded while the twin's grows O(commits).
#[test]
fn maintenance_bounds_the_queue_and_changes_no_answer() {
    let maintained = start("worker", 8);
    let unmaintained = start("worker", 0);
    drive(&maintained.http, "worker", 5, 8);
    drive(&unmaintained.http, "worker", 5, 8);

    assert_eq!(
        rollup(&maintained.http, "worker"),
        rollup(&unmaintained.http, "worker"),
        "maintenance must not change a standing answer"
    );
    assert_eq!(
        semantic(&maintained.http, "worker"),
        semantic(&unmaintained.http, "worker"),
        "maintenance must not change a semantic answer"
    );

    let a = maintained.dir.path().join("data").join("worker");
    let b = unmaintained.dir.path().join("data").join("worker");
    let (a_manifests, a_mbytes) = storage_files_and_bytes(&a, "manifests");
    let (b_manifests, _) = storage_files_and_bytes(&b, "manifests");
    let (_, a_pbytes) = storage_files_and_bytes(&a, "pages");
    let (_, b_pbytes) = storage_files_and_bytes(&b, "pages");
    println!(
        "queue bound: maintained {a_manifests} manifests / {} B vs unmaintained {b_manifests} \
         manifests / {} B",
        a_mbytes + a_pbytes,
        b_pbytes
    );
    assert!(
        a_manifests <= 2 * 8 + 4,
        "the maintained queue holds {a_manifests} manifests — not bounded by the policy"
    );
    assert!(
        b_manifests >= 40,
        "the unmaintained twin must show the growth the fix removes (got {b_manifests})"
    );
}

/// **A crash on every maintenance seam recovers to the twin.** The real binary aborts exactly at
/// S1..S6 via the documented crash-injection instrument; on restart, every acked write is
/// present and the answers equal a never-crashed twin fed the identical acked workload.
#[test]
fn a_crash_on_every_maintenance_seam_recovers_to_the_twin() {
    for seam in ["S1", "S2", "A", "S3", "S4", "S5", "S6"] {
        let world = spawn_binary(
            tempfile::tempdir().expect("dir"),
            "worker",
            10,
            &[("MUTINYD_MAINT_ABORT_AT", seam)],
        );
        let mut crashed = world;

        // 8 writes citing window-0, the taint, 2 writes citing window-1: commit 10 crosses the
        // policy, maintenance runs after its reply, and the process dies at the seam.
        for key in 0..8 {
            assert!(write_event(
                &crashed.http,
                "worker",
                &format!("evt-{key:05}"),
                ("load", "window-0"),
            ));
        }
        assert!(taint(&crashed.http, "worker", ("load", "window-0")));
        for key in 8..10 {
            assert!(write_event(
                &crashed.http,
                "worker",
                &format!("evt-{key:05}"),
                ("load", "window-1"),
            ));
        }
        // The abort lands after the last reply; wait for the process to actually die.
        let died = (0..100).any(|_| {
            std::thread::sleep(Duration::from_millis(100));
            matches!(crashed.child.try_wait(), Ok(Some(_)))
        });
        assert!(died, "seam {seam}: the crash instrument must fire");

        // Restart the same directory without the instrument; the crash path recovers it.
        let BinaryWorld { dir, .. } = crashed;
        let restarted = spawn_binary(dir, "worker", 10, &[]);

        // The never-crashed twin, fed exactly the acked workload.
        let twin = start("worker", 10);
        for key in 0..8 {
            assert!(write_event(
                &twin.http,
                "worker",
                &format!("evt-{key:05}"),
                ("load", "window-0")
            ));
        }
        assert!(taint(&twin.http, "worker", ("load", "window-0")));
        for key in 8..10 {
            assert!(write_event(
                &twin.http,
                "worker",
                &format!("evt-{key:05}"),
                ("load", "window-1")
            ));
        }

        assert_eq!(
            rollup(&restarted.http, "worker"),
            rollup(&twin.http, "worker"),
            "seam {seam}: recovery must equal the never-crashed twin"
        );
        assert_eq!(
            semantic(&restarted.http, "worker"),
            semantic(&twin.http, "worker"),
            "seam {seam}: semantic recovery must equal the never-crashed twin"
        );
        let mut restarted = restarted;
        let _ = restarted.child.kill();
        let _ = restarted.child.wait();
    }
}

/// **Fail closed at the boundary between the recovery paths.** A collapsed store whose plane
/// checkpoint has been removed cannot serve full replay and must refuse by name — never rebuild
/// a truncated history in silence.
#[test]
fn full_replay_of_a_collapsed_store_refuses_by_name() {
    let mut world = spawn_binary(tempfile::tempdir().expect("dir"), "worker", 4, &[]);
    for key in 0..6 {
        assert!(write_event(
            &world.http,
            "worker",
            &format!("evt-{key:05}"),
            ("load", "window-0")
        ));
    }
    // Maintenance ran at commit >= 4; the store is collapsed and the checkpoint exists. Kill the
    // process (an Awake registry row), then remove the checkpoint the crash path would use.
    world.child.kill().expect("kill");
    let _ = world.child.wait();
    let checkpoint = world
        .dir
        .path()
        .join("data")
        .join("worker")
        .join("plane-checkpoint.json");
    assert!(
        checkpoint.exists(),
        "maintenance must have left the checkpoint"
    );
    std::fs::remove_file(&checkpoint).expect("the tooth removes the checkpoint");

    let restarted = spawn_binary(world.dir, "worker", 4, &[]);
    let answer = restarted.http.request(
        "GET",
        &format!("/v1/worker/sql/oneshot?sql={}", urlencode(ROLLUP_SQL)),
        b"",
        None,
    );
    assert_ne!(
        answer.status, 200,
        "a silent partial rebuild is the failure mode"
    );
    assert!(
        answer
            .body
            .contains("refusing full replay of a collapsed store"),
        "the refusal must be named: {}",
        answer.body
    );
    let mut restarted = restarted;
    let _ = restarted.child.kill();
    let _ = restarted.child.wait();
}

/// **Taint composes with maintenance** (M4 × M8): healing after maintenance equals healing on a
/// never-maintained twin, byte for byte — retraction epochs are engine-native and the ledger
/// reapplies idempotently on every recovery path, so consuming capture history changes nothing.
#[test]
fn taint_after_maintenance_heals_like_the_never_maintained_twin() {
    let maintained = start("worker", 6);
    let unmaintained = start("worker", 0);
    for world in [&maintained, &unmaintained] {
        let mut key = 0;
        for window in 0..3 {
            for _ in 0..6 {
                assert!(write_event(
                    &world.http,
                    "worker",
                    &format!("evt-{key:05}"),
                    ("load", &format!("window-{window}")),
                ));
                key += 1;
            }
        }
        // Heal two full windows after the maintained world has collapsed their capture history.
        assert!(taint(&world.http, "worker", ("load", "window-0")));
        assert!(taint(&world.http, "worker", ("load", "window-1")));
    }
    assert_eq!(
        rollup(&maintained.http, "worker"),
        rollup(&unmaintained.http, "worker"),
        "healing across maintenance must equal the never-maintained twin"
    );
    assert_eq!(
        semantic(&maintained.http, "worker"),
        semantic(&unmaintained.http, "worker"),
        "semantic healing across maintenance must equal the never-maintained twin"
    );
}

/// **Tooth (b)** — a GC that reclaims a page a sleeping tenant still references. Constructed by
/// sweeping the slept tenant's store with the wrong live roots (none). The catching instrument
/// is the wake gate: the wake refuses loudly — it can never return a 200 with an answer built
/// from a store whose head was swept.
#[test]
fn tooth_b_a_gc_that_reclaims_a_sleeping_tenants_pages_is_caught() {
    let world = start("napper", 8);
    drive(&world.http, "napper", 3, 6);
    world.http.ok(
        "POST",
        "/fleet/sleep",
        serde_json::json!({"tenant": "napper"})
            .to_string()
            .as_bytes(),
        Some(OPERATOR),
    );

    // THE BUG, constructed: a sweep that forgets the sleeping tenant's head is live.
    {
        use substrate_pager::PageStore as _;
        let storage = world.dir.path().join("data").join("napper").join("storage");
        let store = substrate_wal::DurableStore::open(
            substrate_pager::std_vfs(),
            &storage,
            substrate_pager::StoreConfig::default(),
        )
        .expect("open slept store");
        store.recover().expect("recover");
        store.pager().gc(&[]).expect("the doctored sweep runs");
    }

    // The instrument: wake-on-delta must refuse loudly, not answer from a gutted store.
    let request = serde_json::json!({
        "actor": "analyst", "session": "sess-a", "branch": "sess-a",
        "intent": "wake", "sources": [{"system": "load", "record": "wake"}],
        "table": "telemetry",
        "rows": [["wake-1", "sess-a", "wake body", 1000, false, 1000]],
    });
    let answer = world.http.request(
        "POST",
        "/v1/napper/write",
        request.to_string().as_bytes(),
        None,
    );
    assert_ne!(
        answer.status, 200,
        "a wake served from a swept store is the failure mode: {}",
        answer.body
    );
}

/// **Maintenance archives the ledger and changes nothing observable** (docs/M4-TAINT.md § "The
/// archive tier"): after the policy fires, the cold tier exists with a manifest, a re-taint of
/// the archived source answers exactly like the never-maintained twin's (the union read), and
/// the tenant sleeps and wakes to twin-identical answers with its heals now covered by the
/// checkpoint instead of hot reapplication.
#[test]
fn maintenance_archives_the_ledger_and_changes_no_answer() {
    let maintained = start("worker", 8);
    let unmaintained = start("worker", 0);
    for world in [&maintained, &unmaintained] {
        for key in 0..8 {
            assert!(write_event(
                &world.http,
                "worker",
                &format!("evt-{key:05}"),
                ("load", "window-0"),
            ));
        }
        assert!(taint(&world.http, "worker", ("load", "window-0")));
        // Cross the policy so the maintained world runs a pass (archiving the recall above).
        for key in 8..18 {
            assert!(write_event(
                &world.http,
                "worker",
                &format!("evt-{key:05}"),
                ("load", "window-1"),
            ));
        }
    }

    let manifest = maintained
        .dir
        .path()
        .join("data")
        .join("worker")
        .join("taint-archive")
        .join("MANIFEST");
    assert!(
        manifest.exists(),
        "the maintenance pass must have archived the resolved recall — a missing cold tier \
         makes this gate vacuous"
    );

    // The union read: a re-taint of the ARCHIVED source reports like the twin's hot-ledger one.
    let archived_retaint = maintained.http.ok(
        "POST",
        "/v1/worker/taint",
        serde_json::json!({"system": "load", "record": "window-0"})
            .to_string()
            .as_bytes(),
        Some(OPERATOR),
    );
    let hot_retaint = unmaintained.http.ok(
        "POST",
        "/v1/worker/taint",
        serde_json::json!({"system": "load", "record": "window-0"})
            .to_string()
            .as_bytes(),
        Some(OPERATOR),
    );
    assert_eq!(
        strip_epoch(&archived_retaint),
        strip_epoch(&hot_retaint),
        "the archived ledger must regenerate the same report the hot ledger does"
    );

    // Sleep and wake with the archive in place: the checkpoint covers the archived heals.
    maintained.http.ok(
        "POST",
        "/fleet/sleep",
        serde_json::json!({"tenant": "worker"})
            .to_string()
            .as_bytes(),
        Some(OPERATOR),
    );
    assert_eq!(
        rollup(&maintained.http, "worker"),
        rollup(&unmaintained.http, "worker"),
        "wake over an archived ledger must equal the never-maintained twin"
    );
    assert_eq!(
        semantic(&maintained.http, "worker"),
        semantic(&unmaintained.http, "worker")
    );
}
