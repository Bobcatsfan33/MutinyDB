#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
//! **The M6 gate: one surface.** The three doors produce identical plans and identical counters
//! (the same-door law, composed); the admission boundary covers every door and its ledger is the
//! instrument that catches a bypass (tooth a); resume tokens are exact and the subscriber gate
//! catches an off-by-one (tooth b); the operator boundary is structural; the MD-3 extension
//! constructs are refused by name; a restart preserves the surface. `docs/M6-SURFACE.md` is the
//! contract under test.

use mutinyd::{Config, MutinyServer, TenantPlane};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;

// ---- a minimal client: one request per connection, exactly the wire the server speaks ---------

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
        stream.flush().expect("flush");
        let mut raw = Vec::new();
        stream.read_to_end(&mut raw).expect("read response");
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

    fn mcp(&self, tenant: &str, method: &str, params: serde_json::Value) -> serde_json::Value {
        let request =
            serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
        let body = self.ok(
            "POST",
            &format!("/v1/{tenant}/mcp"),
            request.to_string().as_bytes(),
            None,
        );
        serde_json::from_str(&body).expect("MCP response is JSON")
    }

    fn tool(&self, tenant: &str, name: &str, args: serde_json::Value) -> serde_json::Value {
        let response = self.mcp(
            tenant,
            "tools/call",
            serde_json::json!({"name": name, "arguments": args}),
        );
        assert!(response.get("error").is_none(), "tool {name}: {response}");
        response["result"]["structured"].clone()
    }
}

// ---- the world --------------------------------------------------------------------------------

const OPERATOR: &str = "test-operator-token";

fn config_json(data_dir: &std::path::Path, tenants: &[&str]) -> String {
    let tenant_blocks: Vec<String> = tenants
        .iter()
        .map(|name| {
            format!(
                r#"{{
      "name": "{name}",
      "quota": {{"requests_per_sec": 10000, "bytes_per_sec": 67108864, "queue_depth": 64}},
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
      }},
      "connectors": [{{"action_type": "identity.suspend_account", "compensating_action": "identity.reinstate_account", "receipt_prefix": "okta:suspend"}}]
    }}"#
            )
        })
        .collect();
    format!(
        r#"{{
  "listen": "127.0.0.1:0",
  "operator_token": "{OPERATOR}",
  "data_dir": {data_dir},
  "embedding": {{"dim": 16, "version": "m4-v1"}},
  "tenants": [{tenants}]
}}"#,
        data_dir = serde_json::json!(data_dir),
        tenants = tenant_blocks.join(",")
    )
}

struct World {
    _dir: tempfile::TempDir,
    http: Http,
    server_thread: std::thread::JoinHandle<()>,
}

fn start(tenants: &[&str]) -> World {
    let dir = tempfile::tempdir().expect("data dir");
    start_at(dir, tenants)
}

fn start_at(dir: tempfile::TempDir, tenants: &[&str]) -> World {
    let config = Config::from_json(&config_json(dir.path(), tenants)).expect("config parses");
    let server = MutinyServer::bind(&config).expect("server binds");
    let address = server.address().expect("address");
    let server_thread = std::thread::spawn(move || {
        let _ = server.serve();
    });
    World {
        _dir: dir,
        http: Http { address },
        server_thread,
    }
}

impl World {
    fn shutdown(self) -> tempfile::TempDir {
        // Graceful shutdown = checkpoint + drain, and the report says what drained (M6-SURFACE):
        // an operator asserts the drain rather than assuming it.
        let report = self.http.ok("POST", "/shutdown", b"", Some(OPERATOR));
        assert!(
            report.contains("epoch") && report.contains("registrations"),
            "the drain report must name what it drained: {report}"
        );
        let _ = self.server_thread.join();
        self._dir
    }
}

fn write_event(http: &Http, tenant: &str, branch: &str, key: &str, body: &str, cost: i64) {
    let request = serde_json::json!({
        "actor": "analyst", "session": branch, "branch": branch,
        "intent": format!("record {key}"),
        "sources": [{"system": "erp", "record": "ledger-9"}],
        "table": "telemetry",
        "rows": [[key, branch, body, cost, false, 1000]],
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

// ---- the gate ---------------------------------------------------------------------------------

#[test]
fn the_composed_surface_serves_and_stays_current() {
    let world = start(&["acme"]);
    let http = &world.http;

    write_event(
        http,
        "acme",
        "sess-a",
        "evt-1",
        "urgent credential compromise",
        12_000_000,
    );
    write_event(
        http,
        "acme",
        "sess-a",
        "evt-2",
        "routine reconciliation",
        3_000_000,
    );

    let handle_line = http.ok("POST", "/v1/acme/sql/register", ROLLUP_SQL.as_bytes(), None);
    let handle: u64 = handle_line.trim().parse().expect("handle");

    let before = http.ok(
        "GET",
        &format!("/v1/acme/sql/read?handle={handle}"),
        b"",
        None,
    );
    assert!(before.contains("15000000"), "{before}");

    // The answer stays current: another write moves it without re-registration.
    write_event(
        http,
        "acme",
        "sess-a",
        "evt-3",
        "routine settlement",
        2_000_000,
    );
    let after = http.ok(
        "GET",
        &format!("/v1/acme/sql/read?handle={handle}"),
        b"",
        None,
    );
    assert!(after.contains("17000000"), "{after}");

    // Branch lifecycle over the wire, with inheritance visible in the semantic door.
    let fork = serde_json::json!({"session": "sess-a", "from": "sess-a", "child": "hyp-a"});
    http.ok(
        "POST",
        "/v1/acme/branch/fork",
        fork.to_string().as_bytes(),
        None,
    );
    let inherited = http.ok(
        "GET",
        "/v1/acme/semantic/answer?branch=hyp-a&query=incident-similar",
        b"",
        None,
    );
    assert!(inherited.contains("evt-1"), "{inherited}");

    let health = http.ok("GET", "/v1/acme/health", b"", None);
    assert!(health.contains("surface v0") && health.contains("quarantine"));
    let metrics = http.ok("GET", "/metrics", b"", None);
    assert!(metrics.contains("mutiny_admitted_total{tenant=\"acme\",door=\"sql\"}"));
    assert!(metrics.contains("mutiny_admitted_total{tenant=\"acme\",door=\"typed\"}"));
    world.shutdown();
}

/// **The same-door law, composed.** Identical data; then the same query suite registered and
/// planned through the SQL door, the typed door, and the MCP door — identical plans, identical
/// engine counters, identical answers, and every door's operations present in the admission
/// ledger. The ledger comparison is the instrument tooth (a) relies on.
#[test]
fn the_same_door_law_holds_across_sql_typed_and_mcp() {
    let world = start(&["door-sql", "door-typed", "door-mcp"]);
    let http = &world.http;
    let suite = [
        ROLLUP_SQL,
        "SELECT telemetry.event_id AS event_id, telemetry.branch AS branch FROM telemetry",
        "SELECT telemetry.branch AS branch, COUNT(*) AS n FROM telemetry WHERE telemetry.error \
         = false GROUP BY telemetry.branch",
    ];

    // Identical data into each tenant through the same (typed) door.
    for tenant in ["door-sql", "door-typed", "door-mcp"] {
        write_event(http, tenant, "sess-a", "evt-1", "urgent alpha", 5_000_000);
        write_event(http, tenant, "sess-a", "evt-2", "routine beta", 1_000_000);
    }

    let mut per_door: Vec<(String, Vec<String>, Vec<String>, String)> = Vec::new();
    for (tenant, door) in [
        ("door-sql", "sql"),
        ("door-typed", "typed"),
        ("door-mcp", "mcp"),
    ] {
        let mut plans = Vec::new();
        let mut answers = Vec::new();
        for sql in suite {
            let handle: u64 = match door {
                "sql" => http
                    .ok(
                        "POST",
                        &format!("/v1/{tenant}/sql/register"),
                        sql.as_bytes(),
                        None,
                    )
                    .trim()
                    .parse()
                    .expect("handle"),
                "typed" => {
                    let body = serde_json::json!({"sql": sql});
                    http.ok(
                        "POST",
                        &format!("/v1/{tenant}/query/register"),
                        body.to_string().as_bytes(),
                        None,
                    )
                    .trim()
                    .parse()
                    .expect("handle")
                }
                _ => world
                    .http
                    .tool(tenant, "query.register", serde_json::json!({"sql": sql}))["handle"]
                    .as_u64()
                    .expect("handle"),
            };
            let plan = match door {
                "sql" => http.ok(
                    "GET",
                    &format!("/v1/{tenant}/sql/plan?handle={handle}"),
                    b"",
                    None,
                ),
                "typed" => http.ok(
                    "GET",
                    &format!("/v1/{tenant}/query/plan?handle={handle}"),
                    b"",
                    None,
                ),
                _ => world
                    .http
                    .tool(tenant, "query.plan", serde_json::json!({"handle": handle}))["plan"]
                    .as_str()
                    .expect("plan")
                    .to_owned(),
            };
            plans.push(plan);
            let answer = match door {
                "sql" => http.ok(
                    "GET",
                    &format!("/v1/{tenant}/sql/read?handle={handle}"),
                    b"",
                    None,
                ),
                "typed" => http.ok(
                    "GET",
                    &format!("/v1/{tenant}/query/read?handle={handle}"),
                    b"",
                    None,
                ),
                _ => {
                    let result = world.http.tool(
                        tenant,
                        "query.read",
                        serde_json::json!({"handle": handle}),
                    );
                    format!(
                        "epoch {}\n{}",
                        result["epoch"].as_u64().expect("epoch"),
                        result["answer"].as_str().expect("answer")
                    )
                }
            };
            answers.push(answer);
        }
        let counters = match door {
            "sql" => http.ok("GET", &format!("/v1/{tenant}/sql/counters"), b"", None),
            "typed" => http.ok("GET", &format!("/v1/{tenant}/query/counters"), b"", None),
            _ => http.ok("GET", &format!("/v1/{tenant}/query/counters"), b"", None),
        };
        per_door.push((door.to_owned(), plans, answers, counters));
    }

    let (_, reference_plans, reference_answers, reference_counters) = &per_door[0];
    for (door, plans, answers, counters) in &per_door[1..] {
        assert_eq!(
            plans, reference_plans,
            "door {door}: identical queries must compile to identical plans"
        );
        assert_eq!(
            answers, reference_answers,
            "door {door}: identical queries must produce identical answers"
        );
        assert_eq!(
            counters, reference_counters,
            "door {door}: identical operations must move identical engine counters"
        );
    }

    // Every door's operations are in the admission ledger: suite registrations + plan + read per
    // door, plus the two writes and the counters read. The exact per-door op count:
    let metrics = http.ok("GET", "/metrics", b"", None);
    let counter = |tenant: &str, door: &str| -> u64 {
        metrics
            .lines()
            .find(|line| {
                line.starts_with(&format!(
                    "mutiny_admitted_total{{tenant=\"{tenant}\",door=\"{door}\"}}"
                ))
            })
            .and_then(|line| line.split_whitespace().last())
            .and_then(|value| value.parse().ok())
            .unwrap_or(0)
    };
    // SQL-door tenant: 3 register + 3 plan + 3 read + 1 counters = 10 sql-door ops.
    assert_eq!(counter("door-sql", "sql"), 10);
    // Typed-door tenant: same ops through the typed door, plus its 2 writes.
    assert_eq!(counter("door-typed", "typed"), 12);
    // MCP tenant: 9 tool calls through the MCP door; writes + counters went through typed.
    assert_eq!(counter("door-mcp", "mcp"), 9);
    world.shutdown();
}

/// **Tooth (a): a door that bypasses the admission boundary is caught by the ledger.** A plane
/// driven directly — no server, no charge — performs engine operations that never appear in the
/// admission counters; the instrument (admitted ≥ operations, per door) fires.
#[test]
fn tooth_a_an_admission_bypass_is_caught_by_the_counter_ledger() {
    let dir = tempfile::tempdir().expect("dir");
    let config = Config::from_json(&config_json(dir.path(), &["bypass"])).expect("config");
    let metrics = Arc::new(mutinyd::Metrics::default());
    let mut plane = TenantPlane::open(
        &config.data_dir,
        &config.tenants[0],
        &config.embedding,
        config.checkpoint_every,
        Arc::clone(&metrics),
    )
    .expect("plane opens");

    // The bypassing "door": engine operations with no admission charge.
    let handle = plane.register(ROLLUP_SQL, None).expect("register");
    let _ = plane.read(handle).expect("read");

    let admitted_sql = metrics.counter("mutiny_admitted_total{tenant=\"bypass\",door=\"sql\"}");
    let admitted_typed = metrics.counter("mutiny_admitted_total{tenant=\"bypass\",door=\"typed\"}");
    let admitted_mcp = metrics.counter("mutiny_admitted_total{tenant=\"bypass\",door=\"mcp\"}");
    let operations_performed = 2u64;
    assert!(
        admitted_sql + admitted_typed + admitted_mcp < operations_performed,
        "the instrument must fire: {operations_performed} engine operations happened but the \
         admission ledger only covers {}",
        admitted_sql + admitted_typed + admitted_mcp
    );
}

/// **Resume tokens are exact, and tooth (b): an off-by-one is caught.** The server holds no
/// cursor; the client's token is the cursor; delivered epochs are contiguous and strictly above
/// it — and a subscriber that resumes at token−1 receives a duplicate the instrument names.
#[test]
fn resume_tokens_are_exact_and_the_off_by_one_tooth_is_caught() {
    let world = start(&["acme"]);
    let http = &world.http;

    let handle: u64 = http
        .ok("POST", "/v1/acme/sql/register", ROLLUP_SQL.as_bytes(), None)
        .trim()
        .parse()
        .expect("handle");

    let mut cursor = 0u64;
    let mut seen: Vec<u64> = Vec::new();
    for round in 0..3 {
        for event in 0..2 {
            write_event(
                http,
                "acme",
                "sess-a",
                &format!("evt-{round}-{event}"),
                "routine work",
                1_000_000,
            );
        }
        let body = http.ok(
            "GET",
            &format!("/v1/acme/sql/subscribe?handle={handle}&from={cursor}"),
            b"",
            None,
        );
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
                *epoch > cursor,
                "delivered epoch {epoch} at or below the resume cursor {cursor}: a duplicate"
            );
        }
        for pair in epochs.windows(2) {
            assert_eq!(pair[1], pair[0] + 1, "an epoch gap inside one delivery");
        }
        if let (Some(first), Some(previous)) = (epochs.first(), seen.last()) {
            assert_eq!(*first, previous + 1, "an epoch gap across resumes");
        }
        seen.extend(epochs);
        cursor = next;
    }
    assert!(!seen.is_empty());

    // Tooth (b): the off-by-one resume. Asking from cursor−1 redelivers the last epoch; the
    // instrument (delivered > cursor-held-by-a-correct-client) fires.
    let body = http.ok(
        "GET",
        &format!("/v1/acme/sql/subscribe?handle={handle}&from={}", cursor - 1),
        b"",
        None,
    );
    let first_delivered: u64 = body
        .lines()
        .find(|line| line.starts_with("epoch "))
        .and_then(|line| line.split_whitespace().last()?.parse().ok())
        .expect("the off-by-one delivers something");
    assert!(
        first_delivered <= cursor,
        "the instrument must fire: epoch {first_delivered} is a duplicate below the true cursor \
         {cursor}"
    );
    assert!(
        seen.contains(&first_delivered),
        "and it is precisely an epoch the subscriber already consumed"
    );
    world.shutdown();
}

/// The MCP door: the tool registry has no execute and no taint, by construction; propose flows,
/// and the operator door completes it with the token. The M3 law, at the wire.
#[test]
fn the_mcp_door_has_no_execute_and_the_operator_door_requires_the_token() {
    let world = start(&["acme"]);
    let http = &world.http;

    let tools = http.mcp("acme", "tools/list", serde_json::json!({}));
    let names: Vec<&str> = tools["result"]["tools"]
        .as_array()
        .expect("tools")
        .iter()
        .filter_map(|tool| tool["name"].as_str())
        .collect();
    assert!(names.contains(&"action.propose"));
    assert!(
        !names.iter().any(|name| name.contains("execute")),
        "no execute tool may exist: {names:?}"
    );
    assert!(
        !names.iter().any(|name| name.contains("taint")),
        "taint is operator-door only: {names:?}"
    );

    let session = http.tool(
        "acme",
        "session.open",
        serde_json::json!({"session": "sess-a"}),
    );
    assert_eq!(session["branch"].as_str(), Some("sess-a"));

    let proposed = http.tool(
        "acme",
        "action.propose",
        serde_json::json!({
            "actor": "agent", "branch": "sess-a",
            "action_type": "identity.suspend_account", "target": "user-1",
            "idempotency_key": "suspend-user-1", "justified_by": ["clm-1"],
        }),
    );
    assert_eq!(proposed["proposed"].as_str(), Some("suspend-user-1"));

    // Without the operator token: rejected before admission can be consumed.
    let denied = http.request(
        "POST",
        "/v1/acme/action/execute",
        serde_json::json!({"proposal": "suspend-user-1"})
            .to_string()
            .as_bytes(),
        None,
    );
    assert_eq!(denied.status, 409, "{}", denied.body);

    // With it: the gateway executes, receipt and all.
    let executed = http.ok(
        "POST",
        "/v1/acme/action/execute",
        serde_json::json!({"proposal": "suspend-user-1"})
            .to_string()
            .as_bytes(),
        Some(OPERATOR),
    );
    assert!(executed.contains("okta:suspend:user-1"), "{executed}");
    world.shutdown();
}

/// The MD-3 extension constructs are refused by name, with the door that serves them — never
/// accepted and ignored.
#[test]
fn unshipped_sql_constructs_are_refused_by_name() {
    let world = start(&["acme"]);
    let http = &world.http;
    for (sql, needle) in [
        (
            "SELECT * FROM claims WHERE TAINTED BY 'web:p77'",
            "TAINTED BY",
        ),
        ("SELECT e FROM t AS OF BRANCH 'b'", "AS OF"),
        (
            "SELECT count(*) FROM t GROUP BY semantic_cluster(e, 8)",
            "semantic_cluster",
        ),
    ] {
        let answer = http.request(
            "GET",
            &format!("/v1/acme/sql/oneshot?sql={}", urlencode(sql)),
            b"",
            None,
        );
        assert_eq!(answer.status, 400, "{sql}: {}", answer.body);
        assert!(
            answer.body.contains("refused by name") && answer.body.contains(needle.trim()),
            "{sql}: {}",
            answer.body
        );
    }
    world.shutdown();
}

/// Quota admission answers `Overloaded` (429), the only retryable kind, and the refusal is in
/// the ledger.
#[test]
fn over_quota_is_overloaded_and_retryable() {
    let dir = tempfile::tempdir().expect("dir");
    let config_text = config_json(dir.path(), &["tiny"])
        .replace("\"requests_per_sec\": 10000", "\"requests_per_sec\": 3");
    let config = Config::from_json(&config_text).expect("config");
    let server = MutinyServer::bind(&config).expect("binds");
    let address = server.address().expect("address");
    let thread = std::thread::spawn(move || {
        let _ = server.serve();
    });
    let http = Http { address };

    let mut overloaded = 0;
    for _ in 0..8 {
        let answer = http.request("GET", "/v1/tiny/health", b"", None);
        if answer.status == 429 {
            overloaded += 1;
            assert!(answer.body.starts_with("Overloaded"), "{}", answer.body);
        }
    }
    assert!(
        overloaded > 0,
        "the quota window must have refused something"
    );
    let metrics = http.request("GET", "/metrics", b"", None).body;
    assert!(metrics.contains("kind=\"Overloaded\""), "{metrics}");
    http.ok("POST", "/shutdown", b"", Some(OPERATOR));
    let _ = thread.join();
}

/// A restart preserves the surface: standing registrations, answers, forks, and semantic state.
#[test]
fn a_restart_preserves_registrations_and_answers() {
    let world = start(&["acme"]);
    let http = &world.http;
    write_event(http, "acme", "sess-a", "evt-1", "urgent one", 5_000_000);
    let handle: u64 = http
        .ok("POST", "/v1/acme/sql/register", ROLLUP_SQL.as_bytes(), None)
        .trim()
        .parse()
        .expect("handle");
    let fork = serde_json::json!({"session": "sess-a", "from": "sess-a", "child": "hyp-a"});
    http.ok(
        "POST",
        "/v1/acme/branch/fork",
        fork.to_string().as_bytes(),
        None,
    );
    let before = http.ok(
        "GET",
        &format!("/v1/acme/sql/read?handle={handle}"),
        b"",
        None,
    );
    let semantic_before = http.ok(
        "GET",
        "/v1/acme/semantic/answer?branch=hyp-a&query=incident-similar",
        b"",
        None,
    );
    let dir = world.shutdown();

    let world = start_at(dir, &["acme"]);
    let http = &world.http;
    let after = http.ok(
        "GET",
        &format!("/v1/acme/sql/read?handle={handle}"),
        b"",
        None,
    );
    assert_eq!(
        after, before,
        "the standing answer must survive the restart"
    );
    let semantic_after = http.ok(
        "GET",
        "/v1/acme/semantic/answer?branch=hyp-a&query=incident-similar",
        b"",
        None,
    );
    assert_eq!(semantic_after, semantic_before);
    world.shutdown();
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
