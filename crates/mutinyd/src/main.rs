//! The `mutinyd` binary: `mutinyd <config.json>` serves HTTP + MCP-over-HTTP;
//! `mutinyd --mcp-stdio <tenant> <config.json>` speaks MCP on stdin/stdout for standard clients.

use mutinyd::{banner, Config, MutinyServer};
use std::io::{BufRead, Write};

/// The binary's allocator (docs/M8-MAINTENANCE.md, S7): glibc keeps freed arena pages resident —
/// the nightly soak measured resident ≈ 5× live data, all of it *freed* transients from the
/// engine's per-pass compaction hydration — while mimalloc purges freed pages back to the OS.
/// With it, resident memory means live data, which is exactly what the soak's residual gate
/// asserts. A genuine leak is unaffected by the allocator and still fires the gate.
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

const HELP: &str = "\
mutinyd — MutinyDB's one surface: SQL, typed, and MCP doors over one admission boundary.

  COMPOSED-DEVELOPMENT BUILD. Every linked component is release-quarantined
  (components.lock.json); this binary is NOT a supported or distributable artifact
  until M8's release gates clear. docs/M6-SURFACE.md is the wire contract.

USAGE:
  mutinyd <config.json>                     serve HTTP (and MCP at POST /v1/<tenant>/mcp)
  mutinyd --mcp-stdio <tenant> <config.json>  speak MCP JSON-RPC on stdin/stdout
  mutinyd --help
";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--help" || a == "-h") || args.is_empty() {
        print!("{HELP}");
        return;
    }
    let result = if args[0] == "--mcp-stdio" {
        mcp_stdio(&args[1..])
    } else {
        serve(&args[0])
    };
    if let Err(error) = result {
        eprintln!("mutinyd failed: {error}");
        std::process::exit(1);
    }
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
