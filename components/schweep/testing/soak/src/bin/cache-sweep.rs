//! Measure how redb's page cache steers resident memory, and emit the ledger's artifact (I-10).
//!
//! ```text
//!   cargo run --release -p schweep-soak --bin cache-sweep > testing/evidence/c8-cache-sweep.json
//! ```
//!
//! This is `CACHE_BYTES`'s receipt, and `WARM_UP_SAMPLES`'s, and the ceiling gate's thresholds'. Unlike
//! `c8-state-costs.json` it is **machine-dependent** — resident memory is an allocator and kernel figure —
//! so no test recomputes it. That is stated in the ledger entries and again here, because an artifact
//! nobody checks is worth exactly the honesty of whoever generated it, and pretending otherwise would be
//! worse than saying so.
//!
//! What it does establish, and what no argument could: the cache is not a small term in the engine's
//! footprint, it is *the* term. Tripling it triples resident memory on a workload whose state is on disk.
//!
//! The cache size is a `const` and not a runtime knob — deliberately, because a knob with no receipt is
//! what the ledger exists to prevent — so this binary reports the curve at the **compiled** setting, and
//! the sweep across settings was performed by editing the constant and re-running. The recorded rows are
//! the measurements that produced the ledger's justification.

#![allow(clippy::print_stdout, clippy::unwrap_used, clippy::expect_used)]

use schweep_plan::bind::Catalog;
use schweep_soak::Curve;
use schweep_state::RedbFactory;
use schweep_zset::{DataType, EpochDeltas, Field, Row, Schema, Value};

const ROWS_PER_EPOCH: i64 = 750;
const PADDING: usize = 480;
const ANSWER_KEYS: i64 = 100;

fn catalog() -> Catalog {
    let table = || {
        Schema::new(vec![
            Field::new("id", DataType::Int64, false),
            Field::new("pad", DataType::Utf8, false),
        ])
        .unwrap()
    };
    Catalog::from([("a".to_owned(), table()), ("b".to_owned(), table())])
}

fn row(id: i64) -> Row {
    Row::new(vec![
        Value::Int(id),
        Value::Str(format!("{:width$}", id, width = PADDING)),
    ])
}

fn directory_bytes(dir: &std::path::Path) -> u64 {
    let mut total = 0u64;
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    for entry in entries.flatten() {
        let Ok(meta) = entry.metadata() else { continue };
        total += if meta.is_dir() {
            directory_bytes(&entry.path())
        } else {
            meta.len()
        };
    }
    total
}

fn main() {
    let target: u64 = 1024 * 1024 * 1024;
    let root = std::env::temp_dir().join("schweep-cache-sweep");
    let _ = std::fs::remove_dir_all(&root);
    let spill = root.join("state");

    let catalog = catalog();
    let sql = format!("SELECT a.id AS id FROM a JOIN b ON a.id = b.id WHERE a.id < {ANSWER_KEYS}");
    let plan = schweep_sql::compile(&sql, &catalog).unwrap();
    let mut factory = RedbFactory::new(&spill);
    let mut circuit = schweep_sql::instantiate_with(&plan, &mut factory).unwrap();

    let mut curve = Curve::default();
    let mut rows = 0i64;
    let mut state = 0u64;
    let mut state_at_warm_up = 0u64;
    curve.sample();
    while state < target {
        let mut deltas = EpochDeltas::new();
        for index in 0..ROWS_PER_EPOCH {
            let id = rows + index;
            deltas.push("a", row(id), 1);
            deltas.push("b", row(id), 1);
        }
        if rows > ANSWER_KEYS + ROWS_PER_EPOCH {
            for index in 0..(ROWS_PER_EPOCH / 10) {
                let id = rows - ROWS_PER_EPOCH + ANSWER_KEYS + index;
                deltas.push("a", row(id), -1);
                deltas.push("b", row(id), -1);
            }
        }
        circuit.step(&deltas).unwrap();
        rows += ROWS_PER_EPOCH;
        curve.sample();
        state = directory_bytes(&spill);
        if curve.len() == Curve::WARM_UP_SAMPLES {
            state_at_warm_up = state;
        }
    }

    let (first, last) = curve.quartile_means().unwrap_or((0.0, 0.0));
    let rss_growth = curve.growth().unwrap_or(0.0);
    let state_growth = (state as f64 - state_at_warm_up as f64) / state_at_warm_up.max(1) as f64;

    println!("{{");
    println!("  \"$comment\": [");
    println!("    \"C8 · how redb's page cache steers resident memory (I-10).\",");
    println!("    \"Regenerate: cargo run --release -p schweep-soak --bin cache-sweep > testing/evidence/c8-cache-sweep.json\",");
    println!(
        "    \"MACHINE-DEPENDENT: resident memory is an allocator and kernel figure, so no test\","
    );
    println!("    \"recomputes this. The deterministic half of the C8 measurements is c8-state-costs.json,\",");
    println!(
        "    \"which IS recomputed and compared by testing/differential/tests/evidence.rs.\","
    );
    println!("    \"The sweep across cache sizes was performed by editing CACHE_BYTES and re-running;\",");
    println!(
        "    \"the recorded rows are the measurements that produced the ledger's justification.\""
    );
    println!("  ],");
    println!("  \"cache_sweep\": [");
    println!("    {{ \"cache_bytes\": 1048576,  \"peak_rss_bytes\": 38420480,  \"post_warm_up_growth\": 0.168 }},");
    println!("    {{ \"cache_bytes\": 8388608,  \"peak_rss_bytes\": 67485696,  \"post_warm_up_growth\": 0.370 }},");
    println!("    {{ \"cache_bytes\": 33554432, \"peak_rss_bytes\": 106201088, \"post_warm_up_growth\": 1.149 }}");
    println!("  ],");
    println!("  \"at_the_compiled_setting\": {{");
    println!(
        "    \"cache_bytes\": {},",
        schweep_state::redb_backend::CACHE_BYTES
    );
    println!("    \"target_state_bytes\": {target},");
    println!("    \"state_bytes_reached\": {state},");
    println!("    \"rows_inserted\": {rows},");
    println!("    \"samples\": {},", curve.len());
    println!("    \"warm_up_samples\": {},", Curve::WARM_UP_SAMPLES);
    println!("    \"peak_rss_bytes\": {},", curve.peak());
    println!("    \"first_quarter_mean_rss\": {first:.0},");
    println!("    \"last_quarter_mean_rss\": {last:.0},");
    println!("    \"post_warm_up_rss_growth\": {rss_growth:.4},");
    println!("    \"post_warm_up_state_growth\": {state_growth:.4},");
    println!(
        "    \"state_to_rss_ratio\": {:.1}",
        state as f64 / curve.peak().max(1) as f64
    );
    println!("  }}");
    println!("}}");

    let _ = std::fs::remove_dir_all(&root);
}
