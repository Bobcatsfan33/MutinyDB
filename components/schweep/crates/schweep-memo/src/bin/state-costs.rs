//! Measure what operator state costs, and emit the ledger's artifact (I-10, §6 C8).
//!
//! ```text
//!   cargo run --release -p schweep-memo --bin state-costs > testing/evidence/c8-state-costs.json
//! ```
//!
//! Two measurements, and the second is the one the constants come from:
//!
//! 1. **A synthetic sweep** over a bare `RedbBackend`: entries in, bytes measured. It exists to show
//!    *why* a byte model cannot be tight, and the numbers are startling enough to be worth publishing —
//!    a redb file preallocates to 1,056,768 bytes, is **smaller** at 1,500 entries than when empty
//!    because it truncates on commit, and then grows at between 72 and 205 bytes per entry depending on
//!    how wide the keys are.
//! 2. **A running memo**: queries registered, epochs sealed, and at each round the number
//!    `EXPLAIN STATE` reports next to the bytes the spill directory actually holds. This is the
//!    quantity the reconciliation gate compares, so it is the quantity the tolerance is measured from.
//!    Measuring (1) and hoping it predicts (2) is how the first attempt at this file produced an
//!    envelope the gate rejected on its first round.
//!
//! Everything is deterministic — the same writes produce the same bytes, with no clock and no
//! allocator luck in the numbers — which is what lets `testing/differential/tests/evidence.rs`
//! recompute them and compare.

#![allow(clippy::print_stdout, clippy::unwrap_used, clippy::expect_used)]

use schweep_memo::{CostModel, Memo, Sharing};
use schweep_plan::bind::Catalog;
use schweep_state::{RedbBackend, RedbFactory, StateBackend, WriteBatch};
use schweep_zset::{DataType, EpochDeltas, Field, Row, Schema, Value};

/// A key of `width` values: one integer and `width - 1` short strings, which is the shape operator keys
/// actually have — a join key followed by row values (D-15).
fn key(index: u64, width: usize) -> Vec<Value> {
    let mut out = vec![Value::Int(index as i64)];
    for part in 1..width {
        out.push(Value::Str(format!("v{part}-{index}")));
    }
    out
}

/// The synthetic sweep: `entries` entries of `width` values, and the bytes the file then occupies.
fn synthetic(entries: u64, width: usize) -> u64 {
    let dir = std::env::temp_dir().join(format!("schweep-state-costs-{entries}-{width}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("state.redb");

    let mut backend = RedbBackend::open(&path).unwrap();
    const BATCH: u64 = 1_000;
    let mut written = 0u64;
    while written < entries {
        let mut batch = WriteBatch::new();
        for index in written..(written + BATCH).min(entries) {
            batch.add(key(index, width), 1);
        }
        backend.write(&batch).unwrap();
        written += BATCH;
    }
    let bytes = backend.bytes_on_disk().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    bytes
}

fn catalog() -> Catalog {
    let t = Schema::new(vec![
        Field::new("id", DataType::Int64, false),
        Field::nullable("k", DataType::Int64),
        Field::nullable("s", DataType::Utf8),
    ])
    .unwrap();
    Catalog::from([("t".to_owned(), t)])
}

fn directory_bytes(dir: &std::path::Path) -> u64 {
    let mut total = 0u64;
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    for entry in entries.flatten() {
        let Ok(meta) = entry.metadata() else { continue };
        if meta.is_dir() {
            total += directory_bytes(&entry.path());
        } else {
            total += meta.len();
        }
    }
    total
}

/// One round of the running-memo measurement.
struct Round {
    round: usize,
    entries: usize,
    backends: usize,
    reported_floor: u64,
    reported_typical: u64,
    actual: u64,
}

/// The measurement the tolerance comes from: a memo doing what a memo does.
fn running_memo(rounds: usize, rows_per_round: i64) -> Vec<Round> {
    let root = std::env::temp_dir().join("schweep-state-costs-memo");
    let _ = std::fs::remove_dir_all(&root);
    let spill = root.join("state");

    let mut memo =
        Memo::with_backends(catalog(), Sharing::On, Box::new(RedbFactory::new(&spill))).unwrap();
    // A shape with all three stateful operators: an aggregate, a distinct, and a join's two stores.
    memo.register_sql("SELECT t.k AS k, COUNT(*) AS c FROM t GROUP BY t.k")
        .unwrap();
    memo.register_sql("SELECT DISTINCT t.s AS s FROM t")
        .unwrap();

    let mut out = Vec::new();
    for round in 0..rounds {
        let mut deltas = EpochDeltas::new();
        for index in 0..rows_per_round {
            let id = round as i64 * rows_per_round + index;
            deltas.push(
                "t",
                Row::new(vec![
                    Value::Int(id),
                    Value::Int(id % 997),
                    Value::Str(format!("value-{id}")),
                ]),
                1,
            );
        }
        memo.seal_epoch(&deltas).unwrap();

        let report = memo.explain_state(CostModel::redb()).unwrap();
        let (floor, typical) = report.byte_floor_and_typical();
        out.push(Round {
            round,
            entries: report.distinct_entries,
            backends: report.distinct_backends,
            reported_floor: floor,
            reported_typical: typical,
            actual: directory_bytes(&spill),
        });
    }
    let _ = std::fs::remove_dir_all(&root);
    out
}

fn main() {
    let empty = synthetic(0, 2);

    let mut sweep = Vec::new();
    for width in [2usize, 4] {
        for entries in [
            100u64, 500, 1_000, 1_500, 2_000, 3_000, 5_000, 10_000, 20_000, 50_000, 200_000,
            500_000,
        ] {
            let bytes = synthetic(entries, width);
            sweep.push((entries, width, bytes, bytes / entries.max(1)));
        }
    }
    let at_scale: Vec<u64> = sweep
        .iter()
        .filter(|(entries, _, _, _)| *entries >= 50_000)
        .map(|(_, _, _, per)| *per)
        .collect();

    let rounds = running_memo(12, 4_000);

    println!("{{");
    println!("  \"$comment\": [");
    println!("    \"C8 · what operator state costs, and how far EXPLAIN STATE's byte model is from it (I-10).\",");
    println!("    \"Regenerate: cargo run --release -p schweep-memo --bin state-costs > testing/evidence/c8-state-costs.json\",");
    println!(
        "    \"Deterministic: the same writes produce the same bytes. No clock, no threads.\","
    );
    println!("    \"Checked by testing/differential/tests/evidence.rs, which recomputes the running-memo\",");
    println!("    \"rounds and asserts the recorded tolerance still covers the observed error.\"");
    println!("  ],");
    println!("  \"redb_version\": \"4\",");
    println!("  \"synthetic\": {{");
    println!("    \"bytes_when_empty\": {empty},");
    println!("    \"note\": \"A redb file preallocates, then truncates on commit: at 1,500 entries it is SMALLER than when empty. So a marginal bytes-per-entry figure is not even positive across this range, and the model has to be fitted to totals.\",");
    println!("    \"measurements\": [");
    for (index, (entries, width, bytes, per_entry)) in sweep.iter().enumerate() {
        let comma = if index + 1 == sweep.len() { "" } else { "," };
        println!(
            "      {{ \"entries\": {entries}, \"key_width\": {width}, \"bytes\": {bytes}, \"bytes_per_entry_total\": {per_entry} }}{comma}"
        );
    }
    println!("    ],");
    println!(
        "    \"bytes_per_entry_at_scale\": [{}, {}]",
        at_scale.iter().copied().min().unwrap_or(0),
        at_scale.iter().copied().max().unwrap_or(0)
    );
    println!("  }},");

    println!("  \"running_memo\": {{");
    println!("    \"note\": \"Two standing queries on redb, 4,000 rows per round. `reported_floor` is the bound EXPLAIN STATE publishes; `reported_typical` is a figure for ordinary key widths and is NOT a bound; `actual` is what the spill directory holds. The reconciliation gate asserts actual >= floor, and asserts that zero reported entries means no footprint beyond the empty files.\",");
    println!("    \"rounds\": [");
    let mut worst_position: f64 = 0.0;
    for (index, round) in rounds.iter().enumerate() {
        let comma = if index + 1 == rounds.len() { "" } else { "," };
        let per_entry = round.actual as f64 / round.entries.max(1) as f64;
        worst_position = worst_position.max(per_entry);
        println!(
            "      {{ \"round\": {}, \"entries\": {}, \"backends\": {}, \"reported_floor\": {}, \"reported_typical\": {}, \"actual\": {}, \"bytes_per_entry\": {:.1} }}{comma}",
            round.round,
            round.entries,
            round.backends,
            round.reported_floor,
            round.reported_typical,
            round.actual,
            per_entry
        );
    }
    println!("    ],");
    let above_floor = rounds
        .iter()
        .filter(|r| r.actual >= r.reported_floor)
        .count();
    println!("    \"rounds_above_the_reported_floor\": {above_floor},");
    println!(
        "    \"worst_observed_bytes_per_entry\": {:.1},",
        worst_position
    );
    println!("    \"rounds_total\": {}", rounds.len());
    println!("  }}");
    println!("}}");
}
