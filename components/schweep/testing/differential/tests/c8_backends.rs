//! **The C8 backend and accounting gates** (`ARCHITECTURE.md` §6 C8, D-18, D-19).
//!
//! Two claims, and they are the ones that let a second backend be trusted at all:
//!
//! 1. **Backend invariance.** The same scenarios on `MemBackend` and `RedbBackend` produce
//!    byte-identical answers *and* identical **logical** state fingerprints. An operator must not be
//!    able to tell which store it was handed — if it could, every gate from C1 onward would only have
//!    tested one configuration of the engine.
//! 2. **`EXPLAIN STATE` reconciles.** What the report claims about state is checked against what the
//!    backend actually occupies on disk. A reported number nobody checks is decoration.
//!
//! ## A note on "logical"
//!
//! The fingerprint was already logical and no change was needed, which is worth recording rather than
//! assuming: `Operator::render_state` prints **decoded keys and weights** obtained through the trait's
//! `iter_all`, never the store's bytes, its file name, or its byte count. So the same contents render
//! the same on either backend by construction. The gate below asserts it over 4,400 scenarios anyway,
//! because "by construction" is a claim and this file is where claims get checked.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use schweep_differential::{CircuitEngine, EngineUnderTest, OracleEngine, Scenario};
use schweep_memo::{Admission, CostModel, Memo, Sharing};
use schweep_plan::bind::Catalog;
use schweep_state::{BackendFactory, RedbFactory};
use schweep_zset::{DataType, EpochDeltas, Field, Row, Schema, Value};

fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("schweep-c8-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// **Backend invariance.** Identical answers and identical logical state, on both stores.
#[test]
fn the_two_backends_agree_on_every_answer_and_every_logical_state() {
    const SEEDS: u64 = 1_200;
    let root = scratch("invariance");

    let mut compared = 0usize;
    let mut states_compared = 0usize;
    for seed in 0..SEEDS {
        let scenario = Scenario::generate(seed).unwrap();

        let mut mem =
            match <CircuitEngine as EngineUnderTest>::build(&scenario.tables, &scenario.query) {
                Ok(engine) => engine,
                Err(e) => panic!("seed {seed}: the memory-backed engine failed to build: {e}"),
            };
        let mut factory = RedbFactory::new(root.join(format!("seed-{seed}")));
        let mut spilled =
            match CircuitEngine::build_with(&scenario.tables, &scenario.query, &mut factory) {
                Ok(engine) => engine,
                Err(e) => panic!("seed {seed}: the redb-backed engine failed to build: {e}"),
            };

        // Epoch 0 counts: an engine that is wrong on an empty input is wrong before it starts.
        for (index, deltas) in std::iter::once(&EpochDeltas::new())
            .chain(scenario.epochs.iter())
            .enumerate()
        {
            if index > 0 {
                let (a, b) = (mem.seal_epoch(deltas), spilled.seal_epoch(deltas));
                assert_eq!(
                    a.is_ok(),
                    b.is_ok(),
                    "seed {seed}, epoch {index}: one backend sealed and the other did not"
                );
            }
            match (mem.answer(), spilled.answer()) {
                (Ok(a), Ok(b)) => assert_eq!(
                    a.render(),
                    b.render(),
                    "seed {seed}, epoch {index}: the backends disagree about the answer"
                ),
                (Err(a), Err(b)) => assert_eq!(
                    a, b,
                    "seed {seed}, epoch {index}: the backends raised different errors"
                ),
                (a, b) => panic!("seed {seed}, epoch {index}: {a:?} vs {b:?}"),
            }
            assert_eq!(
                mem.state_fingerprint().unwrap(),
                spilled.state_fingerprint().unwrap(),
                "seed {seed}, epoch {index}: the backends hold different LOGICAL state.\n\
                 The fingerprint prints decoded keys and weights through the trait, so this can only \
                 differ if the stores hold different contents — or if something backend-specific has \
                 crept into the rendering."
            );
            states_compared += 1;
        }
        compared += 1;

        // The spill files are deleted as we go: 1,200 scenarios of redb files is a lot of disk, and the
        // gate is about agreement rather than accumulation.
        drop(spilled);
        let _ = std::fs::remove_dir_all(root.join(format!("seed-{seed}")));
    }

    println!(
        "backend invariance: {compared} scenarios, {states_compared} logical-state comparisons, \
         MemBackend vs RedbBackend"
    );
    assert_eq!(compared, SEEDS as usize);
    let _ = std::fs::remove_dir_all(&root);
}

/// And the spilled engine still agrees with the **oracle**, not merely with its twin.
///
/// Two backends could agree with each other and both be wrong. This is the differential comparison
/// that rules that out, run on the store that ships.
#[test]
fn the_spilled_engine_agrees_with_the_oracle() {
    const SEEDS: u64 = 800;
    let root = scratch("oracle");
    let mut compared = 0usize;
    let mut error_answers = 0usize;

    for seed in 0..SEEDS {
        let scenario = Scenario::generate(seed).unwrap();
        let mut factory = RedbFactory::new(root.join(format!("seed-{seed}")));
        let mut engine =
            CircuitEngine::build_with(&scenario.tables, &scenario.query, &mut factory).unwrap();
        let mut oracle =
            <OracleEngine as EngineUnderTest>::build(&scenario.tables, &scenario.query).unwrap();

        for deltas in std::iter::once(&EpochDeltas::new()).chain(scenario.epochs.iter()) {
            if !deltas.is_empty() || compared > 0 {
                // Epoch 0 is compared before anything is sealed; the empty delta below is a real epoch.
            }
            match (engine.answer(), oracle.answer()) {
                (Ok(a), Ok(b)) => assert_eq!(a.render(), b.render(), "seed {seed}"),
                (Err(a), Err(b)) => {
                    assert_eq!(a, b, "seed {seed}");
                    error_answers += 1;
                }
                (a, b) => panic!("seed {seed}: engine {a:?}, oracle {b:?}"),
            }
            let _ = engine.seal_epoch(deltas);
            let _ = oracle.seal_epoch(deltas);
        }
        compared += 1;
        drop(engine);
        let _ = std::fs::remove_dir_all(root.join(format!("seed-{seed}")));
    }

    println!("redb vs oracle: {compared} scenarios, {error_answers} error answers");
    assert_eq!(compared, SEEDS as usize);
    assert!(error_answers > 0, "S-22 must be exercised on redb too");
    let _ = std::fs::remove_dir_all(&root);
}

// ---- EXPLAIN STATE ------------------------------------------------------------------------------

fn catalog() -> Catalog {
    let t = Schema::new(vec![
        Field::new("id", DataType::Int64, false),
        Field::nullable("k", DataType::Int64),
        Field::nullable("s", DataType::Utf8),
    ])
    .unwrap();
    Catalog::from([("t".to_owned(), t)])
}

fn row(id: i64, k: i64, s: &str) -> Row {
    Row::new(vec![
        Value::Int(id),
        Value::Int(k),
        Value::Str(s.to_owned()),
    ])
}

/// `EXPLAIN STATE` reports every operator of every query, and says which are shared.
#[test]
fn explain_state_reports_each_operator_of_each_query() {
    let root = scratch("explain");
    let mut memo = Memo::with_backends(
        catalog(),
        Sharing::On,
        Box::new(RedbFactory::new(root.join("state"))),
    )
    .unwrap();

    let a = memo
        .register_sql("SELECT t.k AS k, COUNT(*) AS c FROM t GROUP BY t.k")
        .unwrap();
    let b = memo
        .register_sql("SELECT DISTINCT t.k AS k, COUNT(*) AS c FROM t GROUP BY t.k")
        .unwrap();

    let mut deltas = EpochDeltas::new();
    for id in 0..200i64 {
        deltas.push("t", row(id, id % 17, "some value"), 1);
    }
    memo.seal_epoch(&deltas).unwrap();

    let report = memo.explain_state(CostModel::redb()).unwrap();
    println!("{}", report.render());

    assert_eq!(report.queries.len(), 2, "both queries appear");
    let first = &report.queries[0];
    let second = &report.queries[1];
    assert_eq!(first.handle, a);
    assert_eq!(second.handle, b);

    // The aggregate is shared; the distinct is not.
    let aggregate = first
        .operators
        .iter()
        .find(|op| op.operator == "aggregate")
        .expect("query 0 has an aggregate");
    assert!(
        aggregate.entries > 0,
        "the aggregate holds state after 200 rows"
    );
    assert_eq!(
        aggregate.shared_with, 1,
        "one other query reads this operator"
    );
    let distinct = second
        .operators
        .iter()
        .find(|op| op.operator == "distinct")
        .expect("query 1 has a distinct");
    assert_eq!(
        distinct.shared_with, 0,
        "the DISTINCT is private to query 1"
    );

    // Budgets are the I-9 numbers the runtime enforced, not a second opinion.
    for query in &report.queries {
        for op in &query.operators {
            let budget = op.budget.expect("nothing here is admitted-unbounded");
            assert!(
                op.entries <= budget,
                "{} holds {} entries against a budget of {budget}",
                op.operator,
                op.entries
            );
        }
    }

    // The dataflow total counts a shared operator once; the per-query totals do not.
    let per_query: usize = report.queries.iter().map(|q| q.entries()).sum();
    assert!(
        report.distinct_entries < per_query,
        "sharing must not be counted twice and called usage: {} vs {per_query}",
        report.distinct_entries
    );
    assert!(report.backends.contains("RedbBackend"));

    let _ = std::fs::remove_dir_all(&root);
}

/// **The reconciliation gate.** The real footprint lies inside the reported envelope.
#[test]
fn explain_state_reconciles_with_what_the_backend_actually_occupies() {
    let root = scratch("reconcile");
    let spill = root.join("state");
    // The real footprint is read from the directory rather than from the factory, because the factory
    // was moved into the memo. Nothing else may write into this directory: an earlier version of this
    // test created a spare backend here to hold a second handle, and its megabyte-sized file made the
    // gate fail by 3% — a measurement is only a measurement of the thing you meant if nothing else is
    // in the frame.
    let mut memo =
        Memo::with_backends(catalog(), Sharing::On, Box::new(RedbFactory::new(&spill))).unwrap();

    memo.register_sql("SELECT t.k AS k, COUNT(*) AS c FROM t GROUP BY t.k")
        .unwrap();
    memo.register_sql("SELECT DISTINCT t.s AS s FROM t")
        .unwrap();

    // Several rounds, so the reconciliation is checked as state grows rather than once at the end: a
    // model that only fits at one size is a model that fits by accident.
    let mut checked = 0usize;
    for round in 0..6i64 {
        let mut deltas = EpochDeltas::new();
        for id in 0..2_000i64 {
            let key = round * 2_000 + id;
            deltas.push("t", row(key, key % 977, &format!("value-{key}")), 1);
        }
        memo.seal_epoch(&deltas).unwrap();

        let actual = directory_bytes(&spill);
        let reconciliation = memo.reconcile(CostModel::redb(), actual).unwrap();
        println!("round {round}: {}", reconciliation.render());
        assert!(
            reconciliation.agrees(),
            "round {round}: the reported envelope does not contain the real footprint.\n{}",
            reconciliation.render()
        );
        assert!(
            reconciliation.reported_entries > 0,
            "round {round}: EXPLAIN STATE reported no state at all while the backend holds {actual} bytes"
        );
        checked += 1;
    }
    assert_eq!(checked, 6);

    // And an in-memory memo reports honestly that there is nothing to reconcile, rather than
    // manufacturing a ratio.
    let mut in_memory = Memo::with_sharing(catalog(), Sharing::On).unwrap();
    in_memory
        .register_sql("SELECT t.k AS k, COUNT(*) AS c FROM t GROUP BY t.k")
        .unwrap();
    let reconciliation = in_memory.reconcile(CostModel::memory(), 0).unwrap();
    assert!(reconciliation.agrees());
    assert!(reconciliation.render().contains("nothing on disk"));

    let _ = std::fs::remove_dir_all(&root);
}

/// Total bytes in a directory tree.
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

/// A memo on redb answers what a memo in memory answers — the invariance claim, one level up.
#[test]
fn a_memo_on_redb_answers_as_a_memo_in_memory_does() {
    let root = scratch("memo-invariance");
    let queries = [
        "SELECT t.k AS k, COUNT(*) AS c, SUM(t.id) AS s FROM t GROUP BY t.k",
        "SELECT DISTINCT t.s AS s FROM t",
        "SELECT t.id AS id FROM t WHERE t.k > 3",
        "SELECT COUNT(*) AS c, MIN(t.s) AS lo FROM t",
    ];

    let mut memory = Memo::with_sharing(catalog(), Sharing::On).unwrap();
    let mut spilled = Memo::with_backends(
        catalog(),
        Sharing::On,
        Box::new(RedbFactory::new(root.join("state"))),
    )
    .unwrap();
    let mut handles = Vec::new();
    for sql in queries {
        let plan = schweep_sql::compile(sql, &catalog()).unwrap();
        handles.push((
            memory.register(&plan, Admission::bounded()).unwrap(),
            spilled.register(&plan, Admission::bounded()).unwrap(),
        ));
    }

    for round in 0..5i64 {
        let mut deltas = EpochDeltas::new();
        for id in 0..50i64 {
            let key = round * 50 + id;
            deltas.push("t", row(key, key % 7, &format!("s{}", key % 11)), 1);
            if key % 3 == 0 {
                // Retractions, so the comparison covers state leaving as well as arriving (I-5).
                deltas.push("t", row(key, key % 7, &format!("s{}", key % 11)), -1);
            }
        }
        memory.seal_epoch(&deltas).unwrap();
        spilled.seal_epoch(&deltas).unwrap();

        for (in_memory, on_disk) in &handles {
            assert_eq!(
                memory.read(*in_memory).unwrap().1.render(),
                spilled.read(*on_disk).unwrap().1.render(),
                "round {round}: a memo on redb answered differently from one in memory"
            );
        }
    }
    memory.audit().unwrap();
    spilled.audit().unwrap();
    let _ = std::fs::remove_dir_all(&root);
}

/// The factory hands each operator its own labelled file, and the labels are readable.
#[test]
fn every_operator_gets_its_own_labelled_file() {
    let root = scratch("labels");
    let spill = root.join("state");
    let mut factory = RedbFactory::new(&spill);
    let plan = schweep_sql::compile(
        "SELECT t.k AS k, COUNT(*) AS c FROM t GROUP BY t.k",
        &catalog(),
    )
    .unwrap();
    let _circuit = schweep_sql::instantiate_with(&plan, &mut factory).unwrap();

    let handed = factory.handed_out();
    assert_eq!(handed.len(), 1, "one aggregate, one store");
    let (label, path) = &handed[0];
    assert!(
        label.contains("aggregate"),
        "the label names the operator: {label}"
    );
    let path = path.as_ref().unwrap();
    assert!(path.exists());
    assert!(
        path.to_string_lossy().contains("aggregate"),
        "a person reading the spill directory can tell which file is which: {path:?}"
    );

    // A join takes two, and they are distinguishable.
    let mut factory = RedbFactory::new(root.join("join"));
    let two = Schema::new(vec![Field::new("id", DataType::Int64, false)]).unwrap();
    let catalog = Catalog::from([("a".to_owned(), two.clone()), ("b".to_owned(), two)]);
    let plan =
        schweep_sql::compile("SELECT a.id AS x FROM a JOIN b ON a.id = b.id", &catalog).unwrap();
    let _circuit = schweep_sql::instantiate_with(&plan, &mut factory).unwrap();
    let labels: Vec<String> = factory
        .handed_out()
        .into_iter()
        .map(|(label, _)| label)
        .collect();
    assert_eq!(labels.len(), 2);
    assert!(
        labels[0].ends_with("-left") && labels[1].ends_with("-right"),
        "{labels:?}"
    );

    let _ = std::fs::remove_dir_all(&root);
}
