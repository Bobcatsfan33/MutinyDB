//! **The C8 ceiling gate**: operator state many times the memory ceiling, completing with flat memory.
//!
//! > **Exit gate:** a scenario with operator state 10× RAM completes with flat memory (the soak harness
//! > arrives here — RSS sampled across the run, leak fails the job) … **Pitfalls:** this is where "it
//! > worked on the laptop" dies; the gate runs in CI at a fixed memory ceiling (cgroup), not on whatever
//! > the runner has free. — §6 C8
//!
//! ## How the ceiling is established
//!
//! The CI job applies it (`systemd-run --scope -p MemoryMax=…`) and **this test reads it back from the
//! cgroup**. With no ceiling in force the test does not claim the gate: it prints that it is a smoke
//! test, runs a reduced shape, and passes without asserting the multiplier. `CURRENT_CEILING_REQUIRED=1`
//! — which the CI job sets — turns that into a failure, so the gate cannot silently degrade into a test
//! that proves nothing on a machine with memory to spare.
//!
//! ## The shape, and why it is this shape
//!
//! Three things must be true at once: **operator state far exceeds the ceiling**, **the answer does
//! not**, and **no single step materialises more than a little**. Getting all three took two failed
//! attempts, and what they ruled out is worth recording because both are real limits of the engine as it
//! stands:
//!
//! 1. **`GROUP BY` with a group per row** — state is large, but the *answer* is one row per group, and a
//!    result store is a maintained integral **in memory**. A million groups is a million rows of RSS.
//!    The first attempt reached 1.5 GiB of resident memory with 85 MiB of state on disk.
//! 2. **`GROUP BY` with few groups and many rows each** — the answer is tiny, but `Aggregate` folds a
//!    changed group's whole ordered multiset to recompute it (S-30, and §5.3 requires the multiset so
//!    MIN/MAX survive retraction). A group with a million rows costs a million entries *per epoch that
//!    touches it*. That is a known cost with C10's name on it.
//!
//! What satisfies all three is a **join with near-unique keys behind a selective filter**:
//!
//! ```text
//!   SELECT a.id AS id FROM a JOIN b ON a.id = b.id WHERE a.id < 100
//!          │                    │                        │
//!          │                    │                        └─ the answer stays under 100 rows
//!          │                    └─ both integrals grow without bound: THE STATE
//!          └─ each probe scans one key: one entry
//! ```
//!
//! ## What this gate does *not* prove
//!
//! - Not correctness in general — `testing/differential/tests/c8_backends.rs` runs the same backend over
//!   4,400 generated scenarios against the oracle. Here the answer is checked *arithmetically*, because
//!   at this size a from-scratch oracle would not fit under the ceiling either.
//! - Not that a *`Memo`* runs under a ceiling. A memo keeps the accumulated input in memory for C7's
//!   mid-history catch-up, so its footprint tracks the *data*, not the state. This gate drives a circuit
//!   directly for that reason, and `docs/PROGRESS.md` names the limitation.
//! - Not that a **checkpoint** of spilled state fits: `StateBackend::snapshot` returns `Vec<u8>` by the
//!   frozen trait, so checkpointing materialises everything. This gate takes none.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use schweep_memo::{explain_circuit, CostModel};
use schweep_plan::bind::Catalog;
use schweep_soak::{ceiling, Ceiling, Curve};
use schweep_state::{BackendFactory, RedbFactory};
use schweep_zset::{DataType, EpochDeltas, Field, Row, Schema, Value};

/// The multiplier §6 C8 asks for: state at least this many times the ceiling.
const MULTIPLIER: u64 = 10;

/// Rows added to each table per sealed epoch.
///
/// Small enough that one epoch's transient batches are a few megabytes, large enough that the run
/// reaches its target in a few hundred epochs. The RSS curve needs many samples, and every epoch is one.
const ROWS_PER_EPOCH: i64 = 750;

/// How wide the padding column is.
///
/// State is bytes, and a wide row costs more bytes per row — which is how the gate reaches ten times the
/// ceiling in minutes rather than in an hour. The join stores each row on both sides, so a row costs
/// roughly twice this.
const PADDING: usize = 480;

/// Only ids below this reach the answer, so the result store stays small however large the state grows.
const ANSWER_KEYS: i64 = 100;

/// The resident-memory budget the **smoke** run holds itself to, when no cgroup ceiling is in force.
///
/// **This is the instrument that catches a leak, and the shape check is not.** Measured: the clean run
/// peaks at 38 MB of resident memory whether operator state is 538 MB or 1.08 GB — the footprint does not
/// track the state at all. An injected per-step leak took the peak to 214 MB, and the *shape* check
/// passed it, because the machine was under memory pressure and the OS reclaimed the leaked pages
/// half-way through the run: the curve climbed 6 → 214 MiB and then fell back to 160, so the quartile
/// means came out 1% apart. A shape can be flattened by the kernel. An absolute budget cannot.
///
/// 96 MiB is two and a half times the measured peak — room for a slower allocator and a different
/// machine — and less than half the leak's. In the real gate the cgroup ceiling plays this role and plays
/// it harder: 214 MB against a 128 MiB limit is an OOM kill, not an assertion.
const SMOKE_RSS_BUDGET_BYTES: u64 = 96 * 1024 * 1024;

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
        if meta.is_dir() {
            total += directory_bytes(&entry.path());
        } else {
            total += meta.len();
        }
    }
    total
}

#[test]
fn operator_state_many_times_the_ceiling_completes_with_flat_memory() {
    let ceiling = ceiling();
    let required = std::env::var("CURRENT_CEILING_REQUIRED").is_ok_and(|value| value == "1");
    let (target_state_bytes, ceiling_bytes) = match (&ceiling, required) {
        (Ceiling::Cgroup { bytes, .. }, _) => (bytes * MULTIPLIER, Some(*bytes)),
        (Ceiling::Unlimited, true) => panic!(
            "CURRENT_CEILING_REQUIRED=1 but no cgroup memory ceiling is in force. The CI job must \
             apply one — this test reads it back rather than assuming it, because a ceiling gate on a \
             machine with free memory proves nothing (§6 C8's pitfall)."
        ),
        (Ceiling::Unlimited, false) => {
            println!(
                "NOT A GATE: {} — running a reduced shape as a smoke test. The C8 ceiling gate is the \
                 CI job that applies a cgroup limit and sets CURRENT_CEILING_REQUIRED=1.",
                ceiling.describe()
            );
            (384 * 1024 * 1024, None)
        }
    };

    println!(
        "C8 ceiling gate: ceiling {} · target operator state {target_state_bytes} bytes ({MULTIPLIER}x)",
        ceiling.describe()
    );

    let root = std::env::temp_dir().join(format!("schweep-c8-ceiling-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let spill = root.join("state");

    let catalog = catalog();
    let sql = format!("SELECT a.id AS id FROM a JOIN b ON a.id = b.id WHERE a.id < {ANSWER_KEYS}");
    let plan = schweep_sql::compile(&sql, &catalog).unwrap();
    let mut factory = RedbFactory::new(&spill);
    let mut circuit = schweep_sql::instantiate_with(&plan, &mut factory).unwrap();

    let mut curve = Curve::default();
    let mut rows_written = 0i64;
    let mut epochs = 0u64;
    let mut retracted = 0i64;
    curve.sample();

    let mut state_bytes = 0u64;
    // The state at the moment warm-up ends, so the run can compare how fast memory grows against how fast
    // *state* grows. That ratio is the sprint's actual claim.
    let mut state_at_warm_up = 0u64;
    while state_bytes < target_state_bytes {
        let mut deltas = EpochDeltas::new();
        for index in 0..ROWS_PER_EPOCH {
            let id = rows_written + index;
            deltas.push("a", row(id), 1);
            deltas.push("b", row(id), 1);
        }
        // Retract a slice of what came before, so state leaves as well as arrives (I-5): a leak that
        // only appears when entries are removed would otherwise hide behind monotone growth. The
        // retracted ids are chosen above `ANSWER_KEYS` so the arithmetic below stays simple.
        if rows_written > ANSWER_KEYS + ROWS_PER_EPOCH {
            for index in 0..(ROWS_PER_EPOCH / 10) {
                let id = rows_written - ROWS_PER_EPOCH + ANSWER_KEYS + index;
                deltas.push("a", row(id), -1);
                deltas.push("b", row(id), -1);
                retracted += 1;
            }
        }
        circuit.step(&deltas).unwrap();
        rows_written += ROWS_PER_EPOCH;
        epochs += 1;
        curve.sample();
        state_bytes = directory_bytes(&spill);
        if curve.len() == schweep_soak::Curve::WARM_UP_SAMPLES {
            state_at_warm_up = state_bytes;
        }

        if epochs % 25 == 0 {
            println!(
                "  epoch {epochs}: {rows_written} rows · state {state_bytes} bytes · {}",
                curve.render()
            );
        }
        assert!(
            epochs < 20_000,
            "the shape is not reaching {target_state_bytes} bytes of state; it stalled at {state_bytes}"
        );
    }

    println!(
        "reached {state_bytes} bytes of operator state over {epochs} epochs \
         ({rows_written} rows inserted, {retracted} retracted)"
    );
    println!("RSS curve: {}", curve.render());
    println!(
        "  samples (MiB): {}",
        curve
            .samples
            .iter()
            .map(|b| format!("{}", b / (1024 * 1024)))
            .collect::<Vec<_>>()
            .join(" ")
    );

    // ---- the claim: resident memory stayed inside its budget -------------------------------------
    //
    // Under a ceiling the kernel enforces this and the assertion is a courtesy. Without one it is the
    // only thing standing between a leak and a green smoke run, because the shape check can be flattened
    // by the OS reclaiming what leaked.
    let budget = ceiling_bytes.unwrap_or(SMOKE_RSS_BUDGET_BYTES);
    assert!(
        curve.peak() <= budget,
        "peak resident memory {} exceeded the budget of {budget} bytes while operator state was on \
         disk. Memory is not supposed to track the state — that is what spilling is for: {}",
        curve.peak(),
        curve.render()
    );

    // ---- the claim: state exceeded the ceiling by the multiplier ---------------------------------
    if let Some(ceiling_bytes) = ceiling_bytes {
        assert!(
            state_bytes >= ceiling_bytes * MULTIPLIER,
            "operator state reached {state_bytes} bytes against a ceiling of {ceiling_bytes}; the gate \
             claims {MULTIPLIER}x"
        );
    }

    // ---- the claim: flat memory, by shape ---------------------------------------------------------
    assert!(
        curve.steady_state().len() >= 32,
        "too few post-warm-up RSS samples ({}) to say anything about the shape of the curve — the run \
         needs more epochs, not a smaller threshold",
        curve.steady_state().len()
    );
    let growth = curve
        .growth()
        .expect("a curve with 8 or more samples has quartile means");
    // The threshold, and where it comes from. Measured on this shape after the stated warm-up: resident
    // memory plateaus at 33–36 MiB and drifts +4.5% across the remaining 47 samples. Ten per cent is
    // above that with room for a slower machine and a different allocator, and far below what a leak
    // proportional to the state — which grows twentyfold across the same window — would produce.
    assert!(
        growth <= 0.10,
        "RSS grew {:+.1}% across the run after warm-up, while operator state was on disk. \
         Flat means flat: {}",
        growth * 100.0,
        curve.render()
    );
    // **The claim, stated as a ratio.** "Flat" cannot mean "never rises": the sampler's resolution is a
    // mebibyte, and a bounded cache converging on its limit drifts within that. What flat *does* mean is
    // that memory does not track the state — so the gate compares the two growth rates over the same
    // window, and requires memory to grow at most a fifth as fast.
    //
    // Measured on this shape: across 282 post-warm-up samples, operator state grew from 110 MB to 1.08 GB
    // (+880%) while resident memory went from 36.2 MiB to 37.8 MiB (+4.2%) — two hundred times slower. A
    // leak proportional to state would show the two rates equal, and fail here even though its total
    // growth might sit under the threshold above.
    let state_growth = if state_at_warm_up > 0 {
        (state_bytes as f64 - state_at_warm_up as f64) / state_at_warm_up as f64
    } else {
        0.0
    };
    println!(
        "  state grew {:+.1}% after warm-up while RSS grew {:+.1}% · climbs every quarter: {}",
        state_growth * 100.0,
        growth * 100.0,
        curve.climbs_every_quarter()
    );
    assert!(
        state_growth > 1.0,
        "the run must at least double its state after warm-up for the comparison below to mean \
         anything; it grew {:+.1}%",
        state_growth * 100.0
    );
    assert!(
        growth * 5.0 <= state_growth,
        "resident memory grew {:+.1}% while operator state grew {:+.1}% — memory is tracking the state, \
         which is what spilling exists to prevent: {}",
        growth * 100.0,
        state_growth * 100.0,
        curve.render()
    );

    // ---- the claim: the answer is right, arithmetically -------------------------------------------
    //
    // Every id below `ANSWER_KEYS` was inserted into both tables exactly once and never retracted, so
    // the join matches each one once and the filter admits all of them.
    let answer = circuit.answer().unwrap();
    assert_eq!(
        answer.entries().len() as i64,
        ANSWER_KEYS,
        "the answer must hold one row per id below {ANSWER_KEYS}: {} rows",
        answer.entries().len()
    );
    for (row, weight) in answer.entries() {
        assert_eq!(
            *weight, 1,
            "each id matches exactly once, so its weight is 1"
        );
        match row.get(0) {
            Some(Value::Int(id)) => assert!(
                *id >= 0 && *id < ANSWER_KEYS,
                "the filter admitted {id}, which is not below {ANSWER_KEYS}"
            ),
            other => panic!("the answer's first column should be the id, found {other:?}"),
        }
    }

    // ---- the claim: the state itself is right, entry for entry ------------------------------------
    //
    // The answer alone cannot see a dropped entry here: each id arrives in both tables in the same epoch,
    // so a row lost from one side's integral is never probed again. What does see it is the **count**: the
    // join keeps one entry per live row per side, and the generator knows exactly how many that is. A
    // spill that silently drops or truncates an entry under pressure moves this number and nothing else,
    // which is why the check is here and not left to the answer.
    let expected_entries = 2 * (rows_written - retracted) as usize;
    let held = circuit.total_state_size();
    assert_eq!(
        held, expected_entries,
        "the join holds {held} entries; the generator inserted {rows_written} rows and retracted \
         {retracted}, so both integrals together must hold {expected_entries}. A difference is state \
         the backend was given and did not keep."
    );

    // ---- and EXPLAIN STATE still reconciles, at this size -----------------------------------------
    let reconciliation = schweep_memo::reconcile_circuit(
        &circuit,
        CostModel::redb(),
        factory.describe(),
        state_bytes,
    )
    .unwrap();
    let report = explain_circuit(&circuit, CostModel::redb(), factory.describe()).unwrap();
    println!("{}", report.render());
    println!("reconciliation: {}", reconciliation.render());
    assert!(
        reconciliation.agrees(),
        "EXPLAIN STATE does not reconcile at {state_bytes} bytes: {}",
        reconciliation.render()
    );
    let (floor, typical) = report.byte_floor_and_typical();
    assert!(
        report.distinct_entries > 0,
        "EXPLAIN STATE reported no state while the spill directory holds {state_bytes} bytes"
    );
    assert!(
        state_bytes >= floor,
        "at {state_bytes} bytes of state, EXPLAIN STATE reported a floor of {floor}, which the store is \
         below — so the report claims more entries than the disk could be holding"
    );
    // The shape's padding column is 480 characters, so its keys are far wider than the ones the cost
    // model was measured on: the *typical* figure is expected to undershoot badly here, and it is
    // reported rather than asserted precisely because it is not a bound.
    println!(
        "  keys here are wide: {state_bytes} actual vs {typical} typical \
         ({:.0} bytes/entry against a measured 67..205 for ordinary keys)",
        state_bytes as f64 / report.distinct_entries as f64
    );

    drop(circuit);
    let _ = std::fs::remove_dir_all(&root);
}
