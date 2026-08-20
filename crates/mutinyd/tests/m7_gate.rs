#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
//! **The M7 gate: the fleet plane.** Bounded wake beats full replay and matches a never-slept
//! twin byte for byte; tenants register, sleep, wake, and are removed without a restart, with
//! byte accounting; a sleeping tenant is bytes plus a registry row; wake-on-delta is selective
//! at tenant and circuit granularity with the mapping cross-checked; taint composes with sleep;
//! the fleet survives a real SIGKILL; and all three teeth are caught by their named instruments.
//! `docs/M7-FLEET.md` is the contract under test.

use mutinyd::{Config, MutinyServer, TenantPlane};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::time::Instant;

const OPERATOR: &str = "fleet-operator";

struct Http {
    address: SocketAddr,
}

struct HttpAnswer {
    status: u16,
    body: String,
}

impl Http {
    fn request(&self, method: &str, path: &str, body: &[u8], bearer: Option<&str>) -> HttpAnswer {
        let mut stream = TcpStream::connect(self.address).expect("connect");
        let auth = bearer
            .map(|token| format!("Authorization: Bearer {token}\r\n"))
            .unwrap_or_default();
        let head = format!(
            "{method} {path} HTTP/1.1\r\nHost: mutinyd\r\n{auth}Content-Length: {}\r\n\r\n",
            body.len()
        );
        stream.write_all(head.as_bytes()).expect("write head");
        stream.write_all(body).expect("write body");
        let mut raw = Vec::new();
        stream.read_to_end(&mut raw).expect("read");
        let text = String::from_utf8_lossy(&raw).into_owned();
        let status: u16 = text
            .split_whitespace()
            .nth(1)
            .and_then(|code| code.parse().ok())
            .expect("status");
        let body = text
            .split_once("\r\n\r\n")
            .map(|(_, body)| body.to_owned())
            .unwrap_or_default();
        HttpAnswer { status, body }
    }

    fn ok(&self, method: &str, path: &str, body: &[u8], bearer: Option<&str>) -> String {
        let answer = self.request(method, path, body, bearer);
        assert_eq!(answer.status, 200, "{method} {path}: {}", answer.body);
        answer.body
    }

    fn resident(&self) -> usize {
        self.ok("GET", "/fleet/status", b"", Some(OPERATOR))
            .lines()
            .find_map(|line| line.strip_prefix("resident ")?.trim().parse().ok())
            .expect("resident count")
    }
}

fn tenant_config(name: &str) -> String {
    format!(
        r#"{{
      "name": "{name}",
      "quota": {{"requests_per_sec": 100000, "bytes_per_sec": 268435456, "queue_depth": 128}},
      "tables": [
        {{"name": "telemetry",
         "columns": [["event_id","utf8"],["branch","utf8"],["body","utf8"],["cost_micros","int64"],["error","bool"],["event_time","int64"]],
         "key_column": "event_id", "branch_column": "branch", "plane": "events",
         "semantic": {{"body_column":"body","event_time_column":"event_time","cost_micros_column":"cost_micros","error_column":"error"}}}},
        {{"name": "claims",
         "columns": [["claim_id","utf8"],["branch","utf8"],["subject","utf8"],["asserts","utf8"],["confidence_bp","int64"]],
         "key_column": "claim_id", "branch_column": "branch", "plane": "memory"}}
      ],
      "semantic_standing": {{
        "topk": [{{"id": "incident-similar", "text": "urgent credential compromise investigation", "k": 3}}],
        "groups": [{{"id": "incident-groups", "anchors": ["urgent security incident", "routine operations"]}}]
      }}
    }}"#
    )
}

/// A tenant with an explicit maintenance policy (`0` disables — the dev knob that preserves the
/// full-capture-history crash path this gate uses as its baseline; docs/M8-MAINTENANCE.md).
fn tenant_config_every(name: &str, maintenance_every: u64) -> String {
    let base = tenant_config(name);
    base.replacen(
        "\"quota\"",
        &format!("\"maintenance_every\": {maintenance_every}, \"quota\""),
        1,
    )
}

fn config_json_blocks(data_dir: &std::path::Path, blocks: &[String]) -> String {
    format!(
        r#"{{
  "listen": "127.0.0.1:0",
  "operator_token": "{OPERATOR}",
  "data_dir": {data_dir},
  "embedding": {{"dim": 16, "version": "m4-v1"}},
  "tenants": [{tenants}]
}}"#,
        data_dir = serde_json::json!(data_dir),
        tenants = blocks.join(",")
    )
}

fn start_blocks(blocks: &[String]) -> World {
    let dir = tempfile::tempdir().expect("dir");
    let config = Config::from_json(&config_json_blocks(dir.path(), blocks)).expect("config");
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

fn config_json(data_dir: &std::path::Path, tenants: &[&str]) -> String {
    let blocks: Vec<String> = tenants.iter().map(|name| tenant_config(name)).collect();
    format!(
        r#"{{
  "listen": "127.0.0.1:0",
  "operator_token": "{OPERATOR}",
  "data_dir": {data_dir},
  "embedding": {{"dim": 16, "version": "m4-v1"}},
  "tenants": [{tenants}]
}}"#,
        data_dir = serde_json::json!(data_dir),
        tenants = blocks.join(",")
    )
}

struct World {
    dir: tempfile::TempDir,
    http: Http,
}

fn start(tenants: &[&str]) -> World {
    start_at(tempfile::tempdir().expect("dir"), tenants)
}

fn start_at(dir: tempfile::TempDir, tenants: &[&str]) -> World {
    let config = Config::from_json(&config_json(dir.path(), tenants)).expect("config");
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

fn write_event(http: &Http, tenant: &str, key: &str, body: &str, cost: i64, source: (&str, &str)) {
    let request = serde_json::json!({
        "actor": "analyst", "session": "sess-a", "branch": "sess-a",
        "intent": format!("record {key}"),
        "sources": [{"system": source.0, "record": source.1}],
        "table": "telemetry",
        "rows": [[key, "sess-a", body, cost, false, 1000]],
    });
    http.ok(
        "POST",
        &format!("/v1/{tenant}/write"),
        request.to_string().as_bytes(),
        None,
    );
}

const ROLLUP_SQL: &str = "SELECT telemetry.branch AS branch, SUM(telemetry.cost_micros) AS \
                          total_cost, COUNT(*) AS events FROM telemetry GROUP BY telemetry.branch";
const CLAIMS_SQL: &str = "SELECT claims.claim_id AS claim_id, claims.branch AS branch FROM claims";

fn sleep_tenant(http: &Http, tenant: &str) -> String {
    http.ok(
        "POST",
        "/fleet/sleep",
        serde_json::json!({"tenant": tenant}).to_string().as_bytes(),
        Some(OPERATOR),
    )
}

// ---- the gate ---------------------------------------------------------------------------------

/// **The prerequisite: bounded wake.** A tenant with a long history (many commits, several
/// taints — history >> current state) wakes in O(checkpoint + suffix): measurably cheaper than
/// the full-replay path the kill matrix gated (whose 1,000-cycle run cost 5,513 s largely in
/// replay), and byte-identical to a never-slept twin.
#[test]
fn bounded_wake_beats_full_replay_and_matches_the_never_slept_twin() {
    let world = start(&["sleeper"]);
    let twin_world = start(&["sleeper"]);
    // The full-replay baseline (M8 changed recovery semantics, docs/M8-MAINTENANCE.md): a tenant
    // that has never been maintained and never slept is the only shape full replay still serves,
    // so the baseline is built with maintenance disabled and measured from a quiescent copy —
    // exactly the crash image a SIGKILL would leave.
    let replay_world = start_blocks(&[tenant_config_every("sleeper", 0)]);
    let http = &world.http;
    let twin = &twin_world.http;
    let replayer = &replay_world.http;

    // A long history: 8 source windows x 15 writes, each window tainted away — so the capture
    // history is ~128 commits while the current state is one window.
    for target in [http, twin, replayer] {
        for window in 0..8 {
            for event in 0..15 {
                write_event(
                    target,
                    "sleeper",
                    &format!("evt-{window}-{event}"),
                    &format!("routine sample {window}-{event}"),
                    1_000_000,
                    ("load", &format!("window-{window}")),
                );
            }
            if window > 0 {
                target.ok(
                    "POST",
                    "/v1/sleeper/taint",
                    serde_json::json!({"system": "load", "record": format!("window-{}", window - 1)})
                        .to_string()
                        .as_bytes(),
                    Some(OPERATOR),
                );
            }
        }
    }
    let handle_http: u64 = http
        .ok(
            "POST",
            "/v1/sleeper/sql/register",
            ROLLUP_SQL.as_bytes(),
            None,
        )
        .trim()
        .parse()
        .expect("handle");
    let handle_twin: u64 = twin
        .ok(
            "POST",
            "/v1/sleeper/sql/register",
            ROLLUP_SQL.as_bytes(),
            None,
        )
        .trim()
        .parse()
        .expect("handle");

    let expected_rollup = twin.ok(
        "GET",
        &format!("/v1/sleeper/sql/read?handle={handle_twin}"),
        b"",
        None,
    );
    let expected_semantic = twin.ok(
        "GET",
        "/v1/sleeper/semantic/answer?branch=sess-a&query=incident-similar",
        b"",
        None,
    );

    sleep_tenant(http, "sleeper");
    assert_eq!(http.resident(), 0, "a slept tenant must not be resident");

    // Plane-level cost comparison on copies of the same durable state, so neither measurement
    // disturbs the server's live path.
    let config: mutinyd::config::TenantConfig =
        serde_json::from_str(&tenant_config("sleeper")).expect("tenant config");
    let embedding = mutinyd::config::EmbeddingConfig {
        dim: 16,
        version: "m4-v1".to_owned(),
    };
    // The baseline copy comes from the never-maintained tenant while it is quiescent — a
    // crash-consistent image (every commit is fsync'd), with no plane checkpoint, so open()
    // takes the true full-replay crash path.
    let full_dir = world.dir.path().join("copy-full");
    std::fs::create_dir_all(&full_dir).expect("copy dir");
    copy_dir(
        &replay_world.dir.path().join("sleeper"),
        &full_dir.join("sleeper"),
    );
    let started = Instant::now();
    let full = TenantPlane::open(
        &full_dir,
        &config,
        &embedding,
        8,
        Arc::new(mutinyd::Metrics::default()),
    )
    .expect("full replay opens");
    let full_replay = started.elapsed();
    drop(full);

    let copy_for = |suffix: &str| {
        let copy = world.dir.path().join(format!("copy-{suffix}"));
        std::fs::create_dir_all(&copy).expect("copy dir");
        copy_dir(&world.dir.path().join("sleeper"), &copy.join("sleeper"));
        copy
    };
    let wake_dir = copy_for("wake");
    let started = Instant::now();
    let woken = TenantPlane::wake(
        &wake_dir,
        &config,
        &embedding,
        8,
        Arc::new(mutinyd::Metrics::default()),
    )
    .expect("bounded wake opens");
    let bounded = started.elapsed();
    drop(woken);

    println!(
        "wake cost: full replay {full_replay:?} vs bounded {bounded:?} ({}x) over ~128 commits",
        full_replay.as_micros().max(1) / bounded.as_micros().max(1)
    );
    assert!(
        bounded < full_replay,
        "the bounded wake must beat full replay: bounded {bounded:?} vs full {full_replay:?}"
    );
    assert!(
        bounded.as_millis() < 2_000,
        "the bounded wake budget: {bounded:?} exceeded 2 s at this scale"
    );

    // And the server's own wake-on-delta path answers byte-identically to the never-slept twin.
    let woken_rollup = http.ok(
        "GET",
        &format!("/v1/sleeper/sql/read?handle={handle_http}"),
        b"",
        None,
    );
    assert_eq!(strip_epoch(&woken_rollup), strip_epoch(&expected_rollup));
    let woken_semantic = http.ok(
        "GET",
        "/v1/sleeper/semantic/answer?branch=sess-a&query=incident-similar",
        b"",
        None,
    );
    assert_eq!(woken_semantic, expected_semantic);
    assert_eq!(http.resident(), 1, "the delta woke exactly the one tenant");
}

fn copy_dir(from: &std::path::Path, to: &std::path::Path) {
    std::fs::create_dir_all(to).expect("mkdir");
    for entry in std::fs::read_dir(from).expect("read dir").flatten() {
        let target = to.join(entry.file_name());
        if entry.path().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), &target).expect("copy");
        }
    }
}

/// Register, write, sleep, wake-on-delta, and remove — all without restarting the process, with
/// removal returning the byte accounting to baseline (the M5 rewind discipline, fleet edition).
#[test]
fn the_fleet_lifecycle_runs_without_a_restart() {
    let world = start(&["anchor"]);
    let http = &world.http;

    let registered = http.ok(
        "POST",
        "/fleet/register",
        tenant_config("newcomer").as_bytes(),
        Some(OPERATOR),
    );
    assert!(registered.contains("registered newcomer"));

    write_event(
        http,
        "newcomer",
        "evt-1",
        "urgent first write",
        5_000_000,
        ("erp", "ledger-9"),
    );
    let handle: u64 = http
        .ok(
            "POST",
            "/v1/newcomer/sql/register",
            ROLLUP_SQL.as_bytes(),
            None,
        )
        .trim()
        .parse()
        .expect("handle");
    assert!(http
        .ok(
            "GET",
            &format!("/v1/newcomer/sql/read?handle={handle}"),
            b"",
            None
        )
        .contains("5000000"));

    sleep_tenant(http, "newcomer");
    let status = http.ok("GET", "/fleet/status", b"", Some(OPERATOR));
    assert!(
        status.contains("tenant newcomer state Asleep resident false"),
        "{status}"
    );

    // Wake-on-delta: the next write wakes it; the answer stays current across the sleep.
    write_event(
        http,
        "newcomer",
        "evt-2",
        "routine second write",
        2_000_000,
        ("erp", "ledger-9"),
    );
    assert!(http
        .ok(
            "GET",
            &format!("/v1/newcomer/sql/read?handle={handle}"),
            b"",
            None
        )
        .contains("7000000"));

    // Removal: teardown with byte accounting.
    let tenant_dir = world.dir.path().join("newcomer");
    assert!(tenant_dir.exists());
    let removed = http.ok(
        "POST",
        "/fleet/remove",
        serde_json::json!({"tenant": "newcomer"})
            .to_string()
            .as_bytes(),
        Some(OPERATOR),
    );
    assert!(removed.contains("freed_bytes"), "{removed}");
    let freed: u64 = removed
        .lines()
        .find_map(|line| line.strip_prefix("freed_bytes ")?.trim().parse().ok())
        .expect("freed bytes");
    assert!(freed > 0, "removal must account for the bytes it freed");
    assert!(
        !tenant_dir.exists(),
        "the tenant's bytes must return to zero"
    );
    let after = http.request("GET", "/v1/newcomer/health", b"", None);
    assert_eq!(after.status, 404, "a removed tenant is not found");
    assert!(!http
        .ok("GET", "/fleet/status", b"", Some(OPERATOR))
        .contains("newcomer"));
}

/// A sleeping tenant is bytes on the storage backend plus a registry row: the resident gauge and
/// the status agree, the checkpoint is on disk, and nothing else is held for it.
#[test]
fn a_sleeping_tenant_is_bytes_plus_a_registry_row() {
    let world = start(&["dormant"]);
    let http = &world.http;
    write_event(http, "dormant", "evt-1", "routine", 1_000_000, ("erp", "l"));
    assert_eq!(http.resident(), 1);
    sleep_tenant(http, "dormant");
    assert_eq!(http.resident(), 0);
    let metrics = http.ok("GET", "/metrics", b"", None);
    assert!(metrics.contains("mutiny_fleet_resident 0"), "{metrics}");
    assert!(metrics.contains("mutiny_fleet_registered 1"), "{metrics}");
    assert!(world
        .dir
        .path()
        .join("dormant")
        .join("plane-checkpoint.json")
        .exists());
}

/// **Wake-on-delta selectivity, both granularities, counter-asserted.** A delta for tenant A
/// wakes A and provably not B or C; within A, the delta's epoch moves only the circuits that
/// read its table — the unrelated standing query's subscription delta is empty — and the
/// mapping observed from the compute plane predicts exactly which circuits could move.
#[test]
fn wake_on_delta_is_selective_and_the_mapping_predicts_it() {
    let world = start(&["ten-a", "ten-b", "ten-c"]);
    let http = &world.http;

    // Standing queries over DIFFERENT tables in tenant A, then everyone sleeps.
    write_event(
        http,
        "ten-a",
        "seed",
        "routine seed",
        1_000_000,
        ("erp", "l"),
    );
    let rollup: u64 = http
        .ok(
            "POST",
            "/v1/ten-a/sql/register",
            ROLLUP_SQL.as_bytes(),
            None,
        )
        .trim()
        .parse()
        .expect("handle");
    let claims_view: u64 = http
        .ok(
            "POST",
            "/v1/ten-a/sql/register",
            CLAIMS_SQL.as_bytes(),
            None,
        )
        .trim()
        .parse()
        .expect("handle");
    for tenant in ["ten-a", "ten-b", "ten-c"] {
        sleep_tenant(http, tenant);
    }
    assert_eq!(http.resident(), 0);

    // The mapping is served WITHOUT waking anyone (the observation needs no resident plane).
    let mapping = http.ok("GET", "/fleet/mapping?tenant=ten-a", b"", Some(OPERATOR));
    assert_eq!(http.resident(), 0, "reading the mapping must wake nothing");
    assert!(
        mapping.contains(&format!("handle-{rollup}: telemetry")),
        "{mapping}"
    );
    assert!(
        mapping.contains(&format!("handle-{claims_view}: claims")),
        "{mapping}"
    );
    assert!(
        mapping.contains("semantic-incident-similar: telemetry"),
        "{mapping}"
    );

    // One delta for A's telemetry.
    write_event(
        http,
        "ten-a",
        "evt-2",
        "urgent alpha",
        2_000_000,
        ("erp", "l"),
    );

    // Tenant-granular: exactly one tenant woke.
    assert_eq!(
        http.resident(),
        1,
        "the delta must wake its tenant and provably not the others"
    );
    let status = http.ok("GET", "/fleet/status", b"", Some(OPERATOR));
    assert!(
        status.contains("tenant ten-b state Asleep resident false"),
        "{status}"
    );
    assert!(
        status.contains("tenant ten-c state Asleep resident false"),
        "{status}"
    );

    // Circuit-granular: the epoch moved the telemetry circuit and left the claims circuit's
    // subscription delta empty — exactly what the mapping predicted. (The sleep compacted the
    // log, so the subscription starts at the delta's predecessor, not zero.)
    let epoch: u64 = http
        .ok(
            "GET",
            &format!("/v1/ten-a/sql/read?handle={rollup}"),
            b"",
            None,
        )
        .lines()
        .next()
        .and_then(|line| line.strip_prefix("epoch ")?.trim().parse().ok())
        .expect("epoch");
    let rollup_sub = http.ok(
        "GET",
        &format!("/v1/ten-a/sql/subscribe?handle={rollup}&from={}", epoch - 1),
        b"",
        None,
    );
    let claims_sub = http.ok(
        "GET",
        &format!(
            "/v1/ten-a/sql/subscribe?handle={claims_view}&from={}",
            epoch - 1
        ),
        b"",
        None,
    );
    let non_empty = |body: &str| {
        body.lines()
            .filter(|line| line.starts_with('+') || line.starts_with('-'))
            .count()
    };
    assert!(
        non_empty(&rollup_sub) > 0,
        "the fed circuit must emit: {rollup_sub}"
    );
    assert_eq!(
        non_empty(&claims_sub),
        0,
        "the unrelated circuit must emit nothing: {claims_sub}"
    );
}

/// **M4 × M7: taint composes with sleep.** taint(S) against a sleeping tenant wakes it, heals
/// it, and the tenant re-sleeps and wakes still healed — byte-identical to a tenant that never
/// ingested the source (the M4 oracle discipline, across the sleep boundary).
#[test]
fn taint_against_a_sleeping_tenant_wakes_heals_and_resleeps() {
    let world = start(&["victim", "oracle"]);
    let http = &world.http;

    // The victim ingests the poison; the oracle world never does (same clean rows).
    write_event(
        http,
        "victim",
        "evt-p",
        "urgent credential compromise from scraped page",
        12_000_000,
        ("web", "scraped-page-77"),
    );
    let mut handles = std::collections::BTreeMap::new();
    for tenant in ["victim", "oracle"] {
        write_event(
            http,
            tenant,
            "evt-c1",
            "routine reconciliation",
            3_000_000,
            ("erp", "ledger-9"),
        );
        write_event(
            http,
            tenant,
            "evt-c2",
            "routine settlement",
            2_000_000,
            ("erp", "ledger-9"),
        );
        let handle: u64 = http
            .ok(
                "POST",
                &format!("/v1/{tenant}/sql/register"),
                ROLLUP_SQL.as_bytes(),
                None,
            )
            .trim()
            .parse()
            .expect("handle");
        handles.insert(tenant, handle);
    }
    sleep_tenant(http, "victim");
    assert_eq!(http.resident(), 1, "only the oracle remains awake");

    // The operator taints the SLEEPING tenant: the request wakes it, the heal runs.
    let report = http.ok(
        "POST",
        "/v1/victim/taint",
        serde_json::json!({"system": "web", "record": "scraped-page-77"})
            .to_string()
            .as_bytes(),
        Some(OPERATOR),
    );
    assert!(report.contains("ALREADY HEALED"), "{report}");

    // Re-sleep, wake again: still healed, and equal to the never-poisoned oracle.
    sleep_tenant(http, "victim");
    let healed = http.ok(
        "GET",
        &format!("/v1/victim/sql/read?handle={}", handles["victim"]),
        b"",
        None,
    );
    let oracle = http.ok(
        "GET",
        &format!("/v1/oracle/sql/read?handle={}", handles["oracle"]),
        b"",
        None,
    );
    assert_eq!(
        strip_epoch(&healed),
        strip_epoch(&oracle),
        "across sleep → taint → re-sleep → wake, the healed answers must equal the world that \
         never ingested the source"
    );
    let semantic = http.ok(
        "GET",
        "/v1/victim/semantic/answer?branch=sess-a&query=incident-similar",
        b"",
        None,
    );
    assert!(!semantic.contains("evt-p"), "{semantic}");
}

/// **The fleet survives a real SIGKILL** (tooth (c)'s instrument, on the honest path): a binary
/// with a dynamically registered, slept tenant is killed and restarted, and the registry
/// enumerates and wakes everything it promised.
#[test]
fn the_fleet_recovers_its_registry_across_a_real_kill() {
    let dir = tempfile::tempdir().expect("dir");
    let config_path = dir.path().join("config.json");
    std::fs::write(
        &config_path,
        config_json(&dir.path().join("data"), &["static-one"]),
    )
    .expect("config");

    let spawn = || {
        let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_mutinyd"))
            .arg(&config_path)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("mutinyd spawns");
        let stdout = child.stdout.take().expect("stdout");
        let mut lines = BufReader::new(stdout).lines();
        let address: SocketAddr = loop {
            let line = lines.next().expect("address").expect("readable");
            if let Some(rest) = line.strip_prefix("listening ") {
                break rest.parse().expect("address");
            }
        };
        std::thread::spawn(move || for _ in lines {});
        (child, Http { address })
    };

    let (mut child, http) = spawn();
    http.ok(
        "POST",
        "/fleet/register",
        tenant_config("dynamic-two").as_bytes(),
        Some(OPERATOR),
    );
    write_event(
        &http,
        "dynamic-two",
        "evt-1",
        "routine",
        4_000_000,
        ("erp", "l"),
    );
    let dyn_handle: u64 = http
        .ok(
            "POST",
            "/v1/dynamic-two/sql/register",
            ROLLUP_SQL.as_bytes(),
            None,
        )
        .trim()
        .parse()
        .expect("handle");
    sleep_tenant(&http, "dynamic-two");
    write_event(
        &http,
        "static-one",
        "evt-1",
        "routine",
        1_000_000,
        ("erp", "l"),
    );

    child.kill().expect("SIGKILL");
    let _ = child.wait();

    let (mut child, http) = spawn();
    let status = http.ok("GET", "/fleet/status", b"", Some(OPERATOR));
    assert!(
        status.contains("dynamic-two") && status.contains("static-one"),
        "the registry must enumerate every registered tenant after the kill: {status}"
    );
    assert!(
        status.contains("tenant dynamic-two state Asleep"),
        "the slept tenant survives asleep: {status}"
    );
    // The slept tenant wakes bounded; the crashed-awake tenant wakes by full replay.
    assert!(http
        .ok(
            "GET",
            &format!("/v1/dynamic-two/sql/read?handle={dyn_handle}"),
            b"",
            None
        )
        .contains("4000000"));
    let oneshot = http.ok(
        "GET",
        &format!("/v1/static-one/sql/oneshot?sql={}", urlencode(ROLLUP_SQL)),
        b"",
        None,
    );
    assert!(oneshot.contains("1000000"), "{oneshot}");
    let _ = child.kill();
    let _ = child.wait();
}

// ---- teeth ------------------------------------------------------------------------------------

/// **Tooth (a): a wake-on-delta that wakes every tenant.** Answers stay right, so the catching
/// instrument is the counter half of the selectivity gate: resident must equal baseline + the
/// tenants actually addressed. The tooth wakes all and the instrument fires.
#[test]
fn tooth_a_a_wake_everything_bug_is_caught_by_the_resident_counter() {
    let world = start(&["wa", "wb", "wc"]);
    let http = &world.http;
    for tenant in ["wa", "wb", "wc"] {
        write_event(http, tenant, "seed", "routine", 1_000_000, ("erp", "l"));
        sleep_tenant(http, tenant);
    }

    // The bug: a broken wake-on-delta path that wakes the whole fleet for one delta.
    for tenant in ["wa", "wb", "wc"] {
        http.ok(
            "POST",
            "/fleet/wake",
            serde_json::json!({"tenant": tenant}).to_string().as_bytes(),
            Some(OPERATOR),
        );
    }

    // The instrument: after one delta's worth of addressing, resident must be exactly 1.
    let expected_after_one_delta = 1;
    assert_ne!(
        http.resident(),
        expected_after_one_delta,
        "the instrument must fire: every tenant is resident though one delta arrived"
    );
}

/// **Tooth (b): a sleep that skips the checkpoint.** Both failure shapes are caught: a missing
/// checkpoint is refused by name (fail closed), and a checkpoint written from incomplete state
/// wakes into answers the byte-compare instrument rejects.
#[test]
fn tooth_b_a_checkpoint_skipping_sleep_is_caught() {
    // Arm 1: the checkpoint is gone entirely — the wake refuses by name.
    let world = start(&["gone"]);
    let http = &world.http;
    write_event(http, "gone", "evt-1", "routine", 1_000_000, ("erp", "l"));
    sleep_tenant(http, "gone");
    std::fs::remove_file(world.dir.path().join("gone").join("plane-checkpoint.json"))
        .expect("simulate the skipped checkpoint");
    let refused = http.request("GET", "/v1/gone/health", b"", None);
    assert_eq!(refused.status, 409, "{}", refused.body);
    assert!(
        refused.body.contains("refusing the wake"),
        "the refusal must name itself: {}",
        refused.body
    );

    // Arm 2: the checkpoint exists but omits standing state — the woken answers differ from the
    // never-slept twin, and the byte-compare instrument catches it.
    let world = start(&["partial"]);
    let twin_world = start(&["partial"]);
    for target in [&world.http, &twin_world.http] {
        write_event(
            target,
            "partial",
            "evt-1",
            "urgent one",
            5_000_000,
            ("erp", "l"),
        );
        write_event(
            target,
            "partial",
            "evt-2",
            "routine two",
            1_000_000,
            ("erp", "l"),
        );
    }
    let expected = twin_world.http.ok(
        "GET",
        "/v1/partial/semantic/answer?branch=sess-a&query=incident-similar",
        b"",
        None,
    );
    sleep_tenant(&world.http, "partial");
    let path = world
        .dir
        .path()
        .join("partial")
        .join("plane-checkpoint.json");
    let text = std::fs::read_to_string(&path).expect("checkpoint");
    let mut value: serde_json::Value = serde_json::from_str(&text).expect("json");
    let held = value["membership"]["sess-a"]
        .as_array_mut()
        .expect("membership");
    let before = held.len();
    held.retain(|entry| entry[1].as_str() != Some("evt-1"));
    assert!(
        held.len() < before,
        "the tooth must actually remove membership"
    );
    std::fs::write(&path, value.to_string()).expect("doctor");
    let woken = world.http.ok(
        "GET",
        "/v1/partial/semantic/answer?branch=sess-a&query=incident-similar",
        b"",
        None,
    );
    assert_ne!(
        woken, expected,
        "the instrument must fire: the incomplete checkpoint woke into different answers"
    );
    assert!(!woken.contains("evt-1"), "{woken}");
}

/// **Tooth (c): a registry that forgets a sleeping tenant on restart.** The fleet-recovery
/// enumeration is the instrument; the tooth drops the row and the enumeration fires.
#[test]
fn tooth_c_a_forgetful_registry_is_caught_by_the_enumeration() {
    let dir = tempfile::tempdir().expect("dir");
    let world = start_at(dir, &["keeper", "victim"]);
    let http = &world.http;
    for tenant in ["keeper", "victim"] {
        write_event(http, tenant, "evt-1", "routine", 1_000_000, ("erp", "l"));
        sleep_tenant(http, tenant);
    }
    http.ok("POST", "/shutdown", b"", Some(OPERATOR));

    // The bug: the registry forgets the sleeping victim.
    let registry_path = world.dir.path().join("fleet-registry.json");
    let text = std::fs::read_to_string(&registry_path).expect("registry");
    let value: serde_json::Value = serde_json::from_str(&text).expect("json");
    let mut doctored = value.clone();
    doctored["rows"]
        .as_object_mut()
        .expect("rows")
        .remove("victim")
        .expect("the tooth removes the victim");
    std::fs::write(&registry_path, doctored.to_string()).expect("doctor");

    // The instrument: restart and enumerate.
    // Reuse the same directory for the restarted fleet.
    let world = start_at(world.dir, &["keeper", "victim"]);
    let status = world.http.ok("GET", "/fleet/status", b"", Some(OPERATOR));
    let expected = ["keeper", "victim"];
    let missing: Vec<&str> = expected
        .iter()
        .filter(|name| !status.contains(&format!("tenant {name}")))
        .copied()
        .collect();
    assert_eq!(
        missing,
        vec!["victim"],
        "the instrument must fire and name the forgotten tenant: {status}"
    );
}

/// Standing reads render "epoch N" first; epochs legitimately differ between a tainted world
/// and its never-poisoned oracle (taint mints engine-native epochs), so equality is over the
/// answer body.
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
