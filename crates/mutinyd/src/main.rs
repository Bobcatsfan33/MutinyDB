//! The `mutinyd` binary: `mutinyd <config.json>` serves HTTP + MCP-over-HTTP;
//! `mutinyd --mcp-stdio <tenant> <config.json>` speaks MCP on stdin/stdout for standard clients.

use mutinyd::{banner, Config, MutinyServer};
use std::io::{BufRead, Read, Write};

/// The binary's allocator (docs/M8-MAINTENANCE.md, S7): glibc keeps freed arena pages resident —
/// the nightly soak measured resident ≈ 5× live data, all of it *freed* transients from the
/// engine's per-pass compaction hydration — while mimalloc purges freed pages back to the OS.
/// With it, resident memory means live data, which is exactly what the soak's residual gate
/// asserts. A genuine leak is unaffected by the allocator and still fires the gate.
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

const HELP: &str = "\
mutinyd — MutinyDB's one surface: SQL, typed, and MCP doors over one admission boundary.

  v0.1 DEVELOPER RELEASE. All four components are release-admitted. Supported for
  evaluation; not approved for production until the EXT-KMS custody gate clears.
  docs/M6-SURFACE.md is the wire contract.

USAGE:
  mutinyd <config.json>                     serve HTTP (and MCP at POST /v1/<tenant>/mcp)
  mutinyd init [config.json]                write a safe local starter config (never overwrites)
  mutinyd --mcp-stdio <tenant> <config.json>  speak MCP JSON-RPC on stdin/stdout
  mutinyd --help
";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--help" || a == "-h") || args.is_empty() {
        print!("{HELP}");
        return;
    }
    let result = if args[0] == "init" {
        init(&args[1..])
    } else if args[0] == "--mcp-stdio" {
        mcp_stdio(&args[1..])
    } else {
        serve(&args[0])
    };
    if let Err(error) = result {
        eprintln!("mutinyd failed: {error}");
        std::process::exit(1);
    }
}

fn init(args: &[String]) -> Result<(), String> {
    use std::os::unix::fs::OpenOptionsExt;

    if args.len() > 1 {
        return Err("usage: mutinyd init [config.json]".to_owned());
    }
    let path = std::path::Path::new(args.first().map_or("mutinydb.json", String::as_str));
    let mut random = [0_u8; 32];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut random))
        .map_err(|error| format!("could not obtain an OS-random operator token: {error}"))?;
    let token = random
        .iter()
        .fold(String::with_capacity(64), |mut text, byte| {
            use std::fmt::Write as _;
            let _ = write!(text, "{byte:02x}");
            text
        });
    let data_dir = path
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .join("mutiny-data");
    let config = serde_json::json!({
        "listen": "127.0.0.1:7654",
        "operator_token": token,
        "data_dir": data_dir,
        "embedding": {"dim": 16, "version": "mutiny-v0.1"},
        "tenants": [{
            "name": "agent",
            "tables": [{
                "name": "memory",
                "columns": [
                    ["memory_id", "utf8"], ["branch", "utf8"], ["body", "utf8"],
                    ["cost_micros", "int64"], ["error", "bool"], ["event_time", "int64"]
                ],
                "key_column": "memory_id", "branch_column": "branch", "plane": "memory",
                "semantic": {
                    "body_column": "body", "event_time_column": "event_time",
                    "cost_micros_column": "cost_micros", "error_column": "error"
                }
            }],
            "semantic_standing": {
                "topk": [{"id": "agent-recall", "text": "important facts and prior outcomes", "k": 10}],
                "groups": [{"id": "agent-themes", "anchors": ["user preference", "tool outcome", "unresolved task"]}]
            }
        }]
    });
    let bytes = serde_json::to_vec_pretty(&config).map_err(|error| error.to_string())?;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(|error| format!("refusing to overwrite {}: {error}", path.display()))?;
    file.write_all(&bytes).map_err(|error| error.to_string())?;
    file.write_all(b"\n").map_err(|error| error.to_string())?;
    println!("created {}", path.display());
    println!("start with: mutinyd {}", path.display());
    Ok(())
}

fn serve(config_path: &str) -> Result<(), String> {
    let config = Config::from_path(std::path::Path::new(config_path)).map_err(|e| e.to_string())?;
    let server = MutinyServer::bind(&config).map_err(|e| e.to_string())?;
    let address = server.address().map_err(|e| e.to_string())?;
    eprintln!("{}", banner());
    // The bound address on stdout, so a harness that started us on port 0 can read it.
    println!("listening {address}");
    let _ = std::io::stdout().flush();
    server.serve().map_err(|e| e.to_string())
}

fn mcp_stdio(args: &[String]) -> Result<(), String> {
    let [tenant, config_path] = args else {
        return Err("usage: mutinyd --mcp-stdio <tenant> <config.json>".to_owned());
    };
    let config = Config::from_path(std::path::Path::new(config_path)).map_err(|e| e.to_string())?;
    let tenant_config = config
        .tenants
        .iter()
        .find(|t| &t.name == tenant)
        .ok_or_else(|| format!("unknown tenant {tenant:?}"))?;
    let metrics = std::sync::Arc::new(mutinyd::Metrics::default());
    let mut quota = mutinyd::server::QuotaWindow::new(tenant_config.quota);
    let mut plane = mutinyd::TenantPlane::open(
        &config.data_dir,
        tenant_config,
        &config.embedding,
        config.checkpoint_every,
        std::sync::Arc::clone(&metrics),
    )
    .map_err(|e| e.to_string())?;
    eprintln!("{}", banner());

    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let line = line.map_err(|e| e.to_string())?;
        if line.trim().is_empty() {
            continue;
        }
        // The same admission boundary as every other door: stdio charges the tenant's window.
        if let Err(reason) = quota.charge(line.len() as u64) {
            let mut out = stdout.lock();
            let _ = writeln!(
                out,
                "{}",
                serde_json::json!({
                    "jsonrpc": "2.0", "id": null,
                    "error": {"code": -32001, "message": reason}
                })
            );
            continue;
        }
        metrics.inc(&format!(
            "mutiny_admitted_total{{tenant=\"{tenant}\",door=\"mcp\"}}"
        ));
        let request: serde_json::Value = match serde_json::from_str(&line) {
            Ok(value) => value,
            Err(error) => {
                let mut out = stdout.lock();
                let _ = writeln!(
                    out,
                    "{}",
                    serde_json::json!({
                        "jsonrpc": "2.0", "id": null,
                        "error": {"code": -32600, "message": format!("not JSON: {error}")}
                    })
                );
                continue;
            }
        };
        let response = mutinyd::mcp::handle(&mut plane, &request);
        let mut out = stdout.lock();
        let _ = writeln!(out, "{response}");
        let _ = out.flush();
    }
    Ok(())
}
