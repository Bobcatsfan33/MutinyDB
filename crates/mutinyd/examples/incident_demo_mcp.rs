#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
//! **The flagship incident, in its supported form: a scripted agent drives it end-to-end over
//! MCP against `mutinyd`.** This replaces the retired M4 dev binary (`docs/M6-SURFACE.md`,
//! "Retired at M6, visibly"): same story — ingest, standing answers current, poison, taint,
//! heal, receipt first — through the product doors. The agent speaks MCP; the operator's two
//! moments (approving the suspension, running the taint) go through the operator HTTP door,
//! because that separation is the point, not a limitation.
//!
//! Dev-only in one sense that matters: every linked component is release-quarantined, so this
//! binary is NOT a supported artifact until M8 (the mutinyd quarantine notice governs).
//! Deterministic; asserts its own key moments, so a broken step is a loud step.

use mutinyd::{Config, MutinyServer};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};

const OPERATOR: &str = "demo-operator-token";

fn main() {
    if std::env::args().any(|a| a == "--help" || a == "-h") {
        println!(
            "incident_demo_mcp — the flagship incident over MCP against mutinyd.\n\n  {}\n\n  \
             A scripted agent (no LLM) speaks MCP; the operator approves and taints through the\n  \
             operator HTTP door. Runs in seconds; asserts its own moments.",
            mutinyd::QUARANTINE_NOTICE
        );
        return;
    }
    run().expect("the incident demo must complete");
}

fn hex(text: &str) -> String {
    text.bytes().fold(String::new(), |mut out, byte| {
        out.push_str(&format!("{byte:02x}"));
        out
    })
}

struct Doors {
    address: SocketAddr,
}

impl Doors {
    fn http(&self, method: &str, path: &str, body: &[u8], bearer: Option<&str>) -> (u16, String) {
        let mut stream = TcpStream::connect(self.address).expect("connect");
        let auth = bearer
            .map(|token| format!("Authorization: Bearer {token}\r\n"))
            .unwrap_or_default();
        let head = format!(
            "{method} {path} HTTP/1.1\r\nHost: mutinyd\r\n{auth}Content-Length: {}\r\n\r\n",
            body.len()
        );
        stream.write_all(head.as_bytes()).expect("write");
        stream.write_all(body).expect("write body");
        let mut raw = Vec::new();
        stream.read_to_end(&mut raw).expect("read");
        let text = String::from_utf8_lossy(&raw).into_owned();
        let status = text
            .split_whitespace()
            .nth(1)
            .and_then(|code| code.parse().ok())
            .expect("status");
        let body = text
            .split_once("\r\n\r\n")
            .map(|(_, body)| body.to_owned())
            .unwrap_or_default();
        (status, body)
    }

    /// The agent's door.
    fn mcp(&self, tool: &str, args: serde_json::Value) -> serde_json::Value {
        let request = serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": {"name": tool, "arguments": args},
        });
        let (status, body) =
            self.http("POST", "/v1/acme/mcp", request.to_string().as_bytes(), None);
        assert_eq!(status, 200, "{tool}: {body}");
        let response: serde_json::Value = serde_json::from_str(&body).expect("json");
        assert!(response.get("error").is_none(), "{tool}: {response}");
        response["result"]["structured"].clone()
    }
}

fn banner(step: usize, title: &str) {
    println!("\n── {step} · {title} ──────────────────────────────────────────────");
}

fn run() -> Result<(), String> {
    println!(
        "MutinyDB · the incident, end-to-end over MCP ({})",
        mutinyd::SURFACE_VERSION
    );
    println!("{}", mutinyd::QUARANTINE_NOTICE);

    let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
    let config_text = format!(
        r#"{{
  "listen": "127.0.0.1:0",
  "operator_token": "{OPERATOR}",
  "data_dir": {data_dir},
  "embedding": {{"dim": 16, "version": "m4-v1"}},
  "tenants": [{{
    "name": "acme",
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
  }}]
}}"#,
        data_dir = serde_json::json!(dir.path()),
    );
    let config = Config::from_json(&config_text).map_err(|e| e.to_string())?;
    let server = MutinyServer::bind(&config).map_err(|e| e.to_string())?;
    let address = server.address().map_err(|e| e.to_string())?;
    std::thread::spawn(move || {
        let _ = server.serve();
    });
    let doors = Doors { address };

    banner(
        1,
        "the agent opens its session and a hypothesis fork — over MCP",
    );
    doors.mcp("session.open", serde_json::json!({"session": "sess-a"}));
    doors.mcp("session.open", serde_json::json!({"session": "sess-b"}));
    doors.mcp(
        "branch.fork",
        serde_json::json!({"session": "sess-a", "from": "sess-a", "child": "hyp-a"}),
    );
    println!("   sessions sess-a, sess-b; hypothesis fork hyp-a (durable, O(state), MD-5).");

    banner(
        2,
        "ingest — every write an enveloped commit through the front door",
    );
    let write = |branch: &str, table: &str, sources: serde_json::Value, rows: serde_json::Value| {
        doors.mcp(
            "write",
            serde_json::json!({
                "actor": "analyst", "session": branch, "branch": branch,
                "intent": "record", "sources": sources, "table": table, "rows": rows,
            }),
        );
    };
    write(
        "sess-a",
        "telemetry",
        serde_json::json!([{"system": "web", "record": "scraped-page-77"}]),
        serde_json::json!([[
            "evt-a1",
            "sess-a",
            "urgent credential compromise reported by scraped page",
            12000000,
            true,
            1001
        ]]),
    );
    write(
        "sess-a",
        "telemetry",
        serde_json::json!([{"system": "erp", "record": "ledger-9"}]),
        serde_json::json!([[
            "evt-a2",
            "sess-a",
            "routine ledger reconciliation complete",
            3000000,
            false,
            1002
        ]]),
    );
    write(
        "sess-a",
        "claims",
        serde_json::json!([{"system": "web", "record": "scraped-page-77"}, {"system": "erp", "record": "ledger-9"}]),
        serde_json::json!([["clm-1", "sess-a", "user-4471", "is_compromised", 9900]]),
    );
    write(
        "hyp-a",
        "claims",
        serde_json::json!([{"system": "mutiny", "record": format!("claims/{}", hex("clm-1"))}]),
        serde_json::json!([["clm-2", "hyp-a", "user-4471", "requires_suspension", 9500]]),
    );
    write(
        "sess-b",
        "telemetry",
        serde_json::json!([{"system": "erp", "record": "ledger-9"}]),
        serde_json::json!([[
            "evt-b1",
            "sess-b",
            "urgent database security control alert",
            11000000,
            true,
            1003
        ]]),
    );
    println!("   5 commits; the multi-source claim and the two-hop derived claim included.");

    banner(3, "standing answers — registered once, current forever");
    let rollup = doors.mcp(
        "query.register",
        serde_json::json!({"sql": "SELECT telemetry.branch AS branch, SUM(telemetry.cost_micros) AS total_cost, COUNT(*) AS events FROM telemetry GROUP BY telemetry.branch"}),
    )["handle"].as_u64().expect("handle");
    let claims_view = doors.mcp(
        "query.register",
        serde_json::json!({"sql": "SELECT claims.claim_id AS claim_id, claims.branch AS branch, claims.subject AS subject FROM claims"}),
    )["handle"].as_u64().expect("handle");
    let poisoned = doors.mcp("query.read", serde_json::json!({"handle": claims_view}))["answer"]
        .as_str()
        .expect("answer")
        .to_owned();
    assert!(poisoned.contains("clm-1") && poisoned.contains("clm-2"));
    println!("{poisoned}");
    let semantic = doors.mcp(
        "semantic.answer",
        serde_json::json!({"branch": "sess-a", "query": "incident-similar"}),
    );
    println!("   semantic (sess-a): {semantic}");

    banner(
        4,
        "the agent PROPOSES; the operator executes — the separation is structural",
    );
    doors.mcp(
        "action.propose",
        serde_json::json!({
            "actor": "agent-responder", "branch": "hyp-a",
            "action_type": "identity.suspend_account", "target": "user-4471",
            "idempotency_key": "suspend-user-4471", "justified_by": ["clm-1", "clm-2"],
        }),
    );
    let (status, executed) = doors.http(
        "POST",
        "/v1/acme/action/execute",
        serde_json::json!({"proposal": "suspend-user-4471"})
            .to_string()
            .as_bytes(),
        Some(OPERATOR),
    );
    assert_eq!(status, 200, "{executed}");
    assert!(executed.contains("okta:suspend:user-4471"), "{executed}");
    println!("   MCP has no execute tool; the operator door answered:\n{executed}");

    banner(
        5,
        "the poison is discovered — the operator taints; the report leads with the receipt",
    );
    let (status, report) = doors.http(
        "POST",
        "/v1/acme/taint",
        serde_json::json!({"system": "web", "record": "scraped-page-77"})
            .to_string()
            .as_bytes(),
        Some(OPERATOR),
    );
    assert_eq!(status, 200, "{report}");
    let cannot = report.find("CANNOT BE UNDONE").expect("irreversible first");
    let healed_at = report.find("ALREADY HEALED").expect("healed section");
    assert!(cannot < healed_at, "the receipt must come first");
    assert!(report.contains("okta:suspend:user-4471"));
    println!("{report}");

    banner(
        6,
        "the agent reads again — every standing answer healed itself",
    );
    let healed = doors.mcp("query.read", serde_json::json!({"handle": claims_view}))["answer"]
        .as_str()
        .expect("answer")
        .to_owned();
    assert!(
        !healed.contains("clm-1") && !healed.contains("clm-2"),
        "{healed}"
    );
    let healed_rollup = doors.mcp("query.read", serde_json::json!({"handle": rollup}))["answer"]
        .as_str()
        .expect("answer")
        .to_owned();
    assert!(!healed_rollup.contains("15000000"), "{healed_rollup}");
    let healed_semantic = doors.mcp(
        "semantic.answer",
        serde_json::json!({"branch": "sess-a", "query": "incident-similar"}),
    );
    assert!(
        !healed_semantic.to_string().contains("evt-a1"),
        "{healed_semantic}"
    );
    println!("   claims:\n{healed}");
    println!("   rollup:\n{healed_rollup}");
    println!("   semantic (sess-a): {healed_semantic}");
    println!(
        "\nThat is the flagship through the product doors: the agent never held an execute \
         tool,\nthe operator's receipt led the report, and the same propagation that keeps \
         answers current\nun-touched what the poisoned source touched."
    );

    doors.http("POST", "/shutdown", b"", Some(OPERATOR));
    Ok(())
}
