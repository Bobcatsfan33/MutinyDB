//! The MCP agent door (docs/M6-SURFACE.md §2), absorbing loomd's protocol and its law.
//!
//! JSON-RPC 2.0: `initialize`, `tools/list`, `tools/call`. The codes are loomd's, unchanged.
//! **There is no `action.execute` tool, by construction** — M3's propose-not-execute separation
//! at the MCP boundary; execution lives behind the operator HTTP door alone, and taint moved
//! there with it because MutinyDB's taint heals rather than dry-runs (the AT-024 argument,
//! restated in M6-SURFACE).

use crate::config::{QUARANTINE_NOTICE, SURFACE_VERSION};
use crate::plane::{PlaneError, TenantPlane, WriteRequest};
use schweep_server::wire::ErrorKind;
use serde_json::{json, Value};

mod codes {
    pub const INVALID_REQUEST: i64 = -32600;
    pub const METHOD_NOT_FOUND: i64 = -32601;
    pub const INVALID_PARAMS: i64 = -32602;
    pub const INTERNAL: i64 = -32603;
    /// A decision, not a malformed call (loomd's convention).
    pub const DENIED: i64 = -32000;
}

fn ok(id: Option<Value>, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn err(id: Option<Value>, code: i64, message: String) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

fn code_for(error: &PlaneError) -> i64 {
    match error.kind() {
        ErrorKind::Refused => codes::INVALID_PARAMS,
        ErrorKind::NotFound => codes::METHOD_NOT_FOUND,
        ErrorKind::Rejected => codes::DENIED,
        ErrorKind::Overloaded => -32001,
        ErrorKind::Internal => codes::INTERNAL,
    }
}

/// The tool registry. Read it and note what is absent: no execute, no taint.
fn tool_list() -> Value {
    let tool = |name: &str, description: &str| json!({"name": name, "description": description});
    json!({"tools": [
        tool("session.open", "Open (or reopen) a session; returns its branch and capability token."),
        tool("write", "One enveloped commit into a configured table: actor, session, branch, intent, sources, table, rows."),
        tool("branch.fork", "Durably fork a branch; the child inherits the parent's standing answers (O(state), MD-5)."),
        tool("branch.merge", "Merge a child's post-fork divergence into a branch, policy re-run, all-or-nothing."),
        tool("branch.rewind", "Durably rewind a branch; standing state torn down, history kept as audit."),
        tool("query.register", "Register a standing SQL query; returns its handle."),
        tool("query.read", "Read a standing answer at the latest sealed epoch."),
        tool("query.oneshot", "One-shot SQL through the same circuits."),
        tool("query.subscribe", "Per-epoch deltas from a resume token; returns the next token."),
        tool("query.plan", "The registered query's plan rendering."),
        tool("semantic.answer", "A branch's standing semantic top-k answer."),
        tool("semantic.groups", "A branch's standing semantic grouping summaries."),
        tool("action.propose", "PROPOSE an action. Does not execute; only the operator door acts."),
    ]})
}

/// Handle one JSON-RPC request against the plane. Runs on the tenant's worker thread, behind the
/// same admission boundary as every other door.
pub fn handle(plane: &mut TenantPlane, request: &Value) -> Value {
    let id = request.get("id").cloned();
    let Some(method) = request.get("method").and_then(Value::as_str) else {
        return err(id, codes::INVALID_REQUEST, "no method".to_owned());
    };
    match method {
        "initialize" => ok(
            id,
            json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {"tools": {}},
                "serverInfo": {
                    "name": "mutinyd",
                    "version": SURFACE_VERSION,
                    "notice": QUARANTINE_NOTICE,
                },
            }),
        ),
        "tools/list" => ok(id, tool_list()),
        "tools/call" => {
            let params = request.get("params").cloned().unwrap_or(Value::Null);
            let Some(name) = params.get("name").and_then(Value::as_str) else {
                return err(id, codes::INVALID_PARAMS, "no tool name".to_owned());
            };
            let args = params.get("arguments").cloned().unwrap_or(json!({}));
            match call(plane, name, &args) {
                Ok(result) => ok(
                    id,
                    json!({"content": [{"type": "text", "text": result.to_string()}], "structured": result}),
                ),
                Err(error) => err(id, code_for(&error), error.to_string()),
            }
        }
        other => err(
            id,
            codes::METHOD_NOT_FOUND,
            format!("unknown method {other:?}"),
        ),
    }
}

fn arg_str(args: &Value, name: &str) -> Result<String, PlaneError> {
    args.get(name)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| PlaneError::Refused(format!("the argument {name:?} is required")))
}

fn arg_u64(args: &Value, name: &str) -> Result<u64, PlaneError> {
    args.get(name)
        .and_then(Value::as_u64)
        .ok_or_else(|| PlaneError::Refused(format!("the argument {name:?} must be a number")))
}

fn call(plane: &mut TenantPlane, name: &str, args: &Value) -> Result<Value, PlaneError> {
    match name {
        "session.open" => {
            let session = arg_str(args, "session")?;
            let token = plane.session_open(&session)?;
            Ok(json!({"session": session, "branch": session, "token": token}))
        }
        "write" => {
            let sources = args
                .get("sources")
                .and_then(Value::as_array)
                .ok_or_else(|| {
                    PlaneError::Refused("the argument \"sources\" is required".to_owned())
                })?
                .iter()
                .map(|source| Ok((arg_str(source, "system")?, arg_str(source, "record")?)))
                .collect::<Result<Vec<_>, PlaneError>>()?;
            let rows = args
                .get("rows")
                .and_then(Value::as_array)
                .ok_or_else(|| PlaneError::Refused("the argument \"rows\" is required".to_owned()))?
                .iter()
                .map(|row| {
                    row.as_array()
                        .cloned()
                        .ok_or_else(|| PlaneError::Refused("each row is an array".to_owned()))
                })
                .collect::<Result<Vec<_>, PlaneError>>()?;
            let receipt = plane.write(&WriteRequest {
                actor: arg_str(args, "actor")?,
                session: arg_str(args, "session")?,
                branch: arg_str(args, "branch")?,
                intent: arg_str(args, "intent")?,
                sources,
                table: arg_str(args, "table")?,
                rows,
            })?;
            Ok(json!({"commit": receipt.commit_seq, "epoch": receipt.epoch, "rows": receipt.rows}))
        }
        "branch.fork" => {
            plane.fork(
                &arg_str(args, "session")?,
                &arg_str(args, "from")?,
                &arg_str(args, "child")?,
            )?;
            Ok(json!({"forked": arg_str(args, "child")?}))
        }
        "branch.merge" => {
            let merged = plane.merge(
                &arg_str(args, "session")?,
                &arg_str(args, "child")?,
                &arg_str(args, "into")?,
            )?;
            Ok(json!({"merged": merged}))
        }
        "branch.rewind" => {
            let freed = plane.rewind(&arg_str(args, "session")?, &arg_str(args, "child")?)?;
            Ok(json!({"freed_bytes": freed}))
        }
        "query.register" => {
            let sql = arg_str(args, "sql")?;
            let unbounded = args
                .get("unbounded")
                .and_then(Value::as_str)
                .map(str::to_owned);
            let handle = plane.register(&sql, unbounded.as_deref())?;
            Ok(json!({"handle": handle}))
        }
        "query.read" => {
            let (epoch, answer) = plane.read(arg_u64(args, "handle")?)?;
            Ok(json!({"epoch": epoch, "answer": answer}))
        }
        "query.oneshot" => {
            let answer = plane.oneshot(&arg_str(args, "sql")?)?;
            Ok(json!({"answer": answer}))
        }
        "query.subscribe" => {
            let (next, deltas) =
                plane.subscribe(arg_u64(args, "handle")?, arg_u64(args, "from")?)?;
            let rendered: Vec<Value> = deltas
                .into_iter()
                .map(|delta| json!({"epoch": delta.epoch, "delta": delta.rendered}))
                .collect();
            Ok(json!({"token": next, "epochs": rendered}))
        }
        "query.plan" => Ok(json!({"plan": plane.plan_of(arg_u64(args, "handle")?)?})),
        "semantic.answer" => {
            let hits =
                plane.semantic_answer(&arg_str(args, "branch")?, &arg_str(args, "query")?)?;
            let rendered: Vec<Value> = hits
                .into_iter()
                .map(|hit| json!({"rank": hit.rank, "key": hit.key, "score": hit.score}))
                .collect();
            Ok(json!({"hits": rendered}))
        }
        "semantic.groups" => {
            let groups =
                plane.semantic_groups(&arg_str(args, "branch")?, &arg_str(args, "group")?)?;
            let rendered: Vec<Value> = groups
                .into_iter()
                .map(|group| {
                    json!({
                        "group": group.group_id,
                        "count": group.count,
                        "avg_cost": group.avg_cost,
                        "error_rate": group.error_rate,
                        "exemplar": group.exemplar_key,
                        "members": group.member_keys,
                    })
                })
                .collect();
            Ok(json!({"groups": rendered}))
        }
        "action.propose" => {
            let justified: Vec<String> = args
                .get("justified_by")
                .and_then(Value::as_array)
                .map(|keys| {
                    keys.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default();
            let key = plane.propose(
                &arg_str(args, "actor")?,
                &arg_str(args, "branch")?,
                &arg_str(args, "action_type")?,
                &arg_str(args, "target")?,
                &arg_str(args, "idempotency_key")?,
                &justified,
            )?;
            Ok(json!({
                "proposed": key,
                "note": "proposal only — execution requires the operator door (M3's law)",
            }))
        }
        other => Err(PlaneError::NotFound(format!("unknown tool {other:?}"))),
    }
}
