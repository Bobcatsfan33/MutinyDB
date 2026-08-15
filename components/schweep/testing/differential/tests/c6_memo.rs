//! **The C6 exit gate**: I-8, mid-history attach, and teardown (`ARCHITECTURE.md` §6 C6, §5.7).
//!
//! > **Exit gate:** I-8 gate: a battery of overlapping queries runs twice — sharing enabled and
//! > disabled — with byte-identical answers and a counter proof that sharing actually shared (fewer
//! > operator-steps executed); teardown gate: deregistering a query frees exactly its private suffix
//! > (state accounting returns to baseline); 1,000 register/deregister cycles leak nothing.
//!
//! ## The two failure modes, and which half of the gate catches each
//!
//! Sharing breaks silently in **both** directions, and the two need different instruments:
//!
//! | Failure | Symptom | Caught by |
//! | --- | --- | --- |
//! | sharing stops happening (a canonicalization rule that no longer fires) | every answer stays right; the engine is merely slower and fatter | the **counter** half: operator steps with sharing on must be strictly fewer |
//! | sharing happens when it must not (a hash that ignores a plan field) | one query reads another's answer | the **answer-equality** half, immediately |
//! | a refcount off by one | a node leaks, or a live query's input is freed | the **teardown/leak** half, by accounting |
//!
//! A gate with only the answer half would pass a memo that had quietly stopped sharing anything at
//! all. That is the pitfall §6 C6 names, and it is why every number below is asserted rather than
//! printed.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use schweep_differential::{sweep_matching, MemoEngine, OracleEngine};
use schweep_memo::{Memo, Sharing};
use schweep_oracle::Oracle;
use schweep_plan::bind::Catalog;
use schweep_zset::{DataType, EpochDeltas, Field, Row, Schema, Value};

/// The battery: overlapping standing queries over one dataflow.
///
/// Chosen so that sharing has something to find at every rung — a common scan, a common filter, a
/// common join, a common aggregate — and so that some queries are *prefixes* of others (`A` is `B`
/// without its DISTINCT), which is the case where the novel suffix is exactly one node.
const BATTERY: &[&str] = &[
    // A shared source and filter, three different suffixes.
    "SELECT t.n AS n FROM t WHERE t.k > 1",
    "SELECT DISTINCT t.n AS n FROM t WHERE t.k > 1",
    "SELECT t.n AS n, t.id AS id FROM t WHERE t.k > 1",
    // The same filter, aggregated two ways.
    "SELECT t.k AS k, COUNT(*) AS c FROM t WHERE t.k > 1 GROUP BY t.k",
    "SELECT t.k AS k, SUM(t.n) AS s FROM t WHERE t.k > 1 GROUP BY t.k",
    "SELECT t.k AS k, COUNT(*) AS c FROM t WHERE t.k > 1 GROUP BY t.k HAVING c > 1",
    // A shared join, two suffixes — and the reordered-key spelling, which canonicalization shares.
    "SELECT t.n AS n, u.m AS m FROM t JOIN u ON t.id = u.id AND t.k = u.k",
    "SELECT DISTINCT t.n AS n, u.m AS m FROM t JOIN u ON t.k = u.k AND t.id = u.id",
    "SELECT t.id AS id, COUNT(*) AS c FROM t JOIN u ON t.id = u.id AND t.k = u.k GROUP BY t.id",
    // A grand total over the shared scan: an answer that exists before any epoch (S-33, D-20).
    "SELECT COUNT(*) AS c, MIN(t.n) AS lo FROM t",
    // Queries that share nothing with the rest, so the gate is not only about overlap.
    "SELECT u.m AS m FROM u WHERE u.m IS NOT NULL",
    "SELECT t.n AS n FROM t WHERE t.k > 2",
];

fn catalog() -> Catalog {
    let t = Schema::new(vec![
        Field::new("id", DataType::Int64, false),
        Field::new("k", DataType::Int64, true),
        Field::new("n", DataType::Int64, true),
    ])
    .unwrap();
    let u = Schema::new(vec![
        Field::new("id", DataType::Int64, false),
        Field::new("k", DataType::Int64, true),
        Field::new("m", DataType::Int64, true),
    ])
    .unwrap();
    Catalog::from([("t".to_owned(), t), ("u".to_owned(), u)])
}

fn tables() -> Vec<(String, Schema)> {
    catalog().into_iter().collect()
}

fn t_row(id: i64, k: Option<i64>, n: Option<i64>) -> Row {
    Row::new(vec![
        Value::Int(id),
        k.map_or(Value::Null, Value::Int),
        n.map_or(Value::Null, Value::Int),
    ])
}

fn u_row(id: i64, k: Option<i64>, m: Option<i64>) -> Row {
    t_row(id, k, m)
}

/// A history with retractions, multiplicities, same-epoch churn, an empty epoch, and NULLs.
fn history() -> Vec<EpochDeltas> {
    let mut epochs = Vec::new();

    let mut first = EpochDeltas::new();
    first.extend(
        "t",
        vec![
            (t_row(1, Some(2), Some(10)), 1),
            (t_row(2, Some(2), Some(20)), 2),
            (t_row(3, Some(0), Some(30)), 1),
            (t_row(4, Some(3), None), 1),
        ],
    );
    first.extend(
        "u",
        vec![
            (u_row(1, Some(2), Some(100)), 1),
            (u_row(2, Some(2), None), 1),
        ],
    );
    epochs.push(first);

    // An empty epoch: nothing may move.
    epochs.push(EpochDeltas::new());

    let mut third = EpochDeltas::new();
    third.extend(
        "t",
        vec![
            // Same-epoch retract-and-reinsert, and a partial retraction of a multiplicity.
            (t_row(1, Some(2), Some(10)), -1),
            (t_row(1, Some(2), Some(11)), 1),
            (t_row(2, Some(2), Some(20)), -1),
            (t_row(5, Some(4), Some(50)), 3),
        ],
    );
    third.extend("u", vec![(u_row(3, Some(4), Some(300)), 1)]);
    epochs.push(third);

    let mut fourth = EpochDeltas::new();
    // Retract a whole row, and add one that joins nothing.
    fourth.extend(
        "t",
        vec![
            (t_row(3, Some(0), Some(30)), -1),
            (t_row(6, Some(9), Some(60)), 1),
        ],
    );
    fourth.extend("u", vec![(u_row(1, Some(2), Some(100)), -1)]);
    epochs.push(fourth);

    epochs
}

/// Register the whole battery into one memo and step it through a history, collecting every answer
/// at every epoch — plus the operator-step count the I-8 counter proof compares.
fn run_battery(sharing: Sharing) -> (Vec<String>, usize, schweep_memo::Accounting) {
    let mut memo = Memo::with_sharing(catalog(), sharing).unwrap();
    let handles: Vec<_> = BATTERY
        .iter()
        .map(|sql| memo.register_sql(sql).expect(sql))
        .collect();

    // Measured from *after* registration: registering costs one recomputation over the accumulated
    // input (here, nothing), and the claim being tested is about steady-state maintenance.
    let steps_before = memo.dataflow().operator_steps();

    let mut answers = Vec::new();
    let mut record = |memo: &Memo| {
        for (sql, handle) in BATTERY.iter().zip(&handles) {
            let rendered = match memo.read(*handle) {
                Ok((epoch, answer)) => format!("epoch {epoch} · {sql}\n{}", answer.render()),
                Err(error) => format!("epoch ? · {sql}\nERROR: {error}\n"),
            };
            answers.push(rendered);
        }
    };

    record(&memo);
    for deltas in history() {
        memo.seal_epoch(&deltas).unwrap();
        record(&memo);
    }
    memo.audit().unwrap();

    let steps = memo.dataflow().operator_steps() - steps_before;
    (answers, steps, memo.accounting())
}

/// **The I-8 gate, both halves.**
#[test]
fn i8_sharing_changes_the_counters_and_not_one_answer_byte() {
    let (shared_answers, shared_steps, shared) = run_battery(Sharing::On);
    let (private_answers, private_steps, private) = run_battery(Sharing::Off);

    // ---- half one: not one byte ----------------------------------------------------------------
    assert_eq!(
        shared_answers.len(),
        BATTERY.len() * (history().len() + 1),
        "every query must be read at every epoch, epoch 0 included"
    );
    for (index, (with, without)) in shared_answers.iter().zip(&private_answers).enumerate() {
        assert_eq!(
            with, without,
            "I-8 violated at reading {index}: sharing changed an answer\n--- shared ---\n{with}\n\
             --- private ---\n{without}"
        );
    }

    // ---- half two: the counters moved -----------------------------------------------------------
    println!(
        "I-8: {} operator steps shared vs {} private ({} nodes vs {})",
        shared_steps, private_steps, shared.live_nodes, private.live_nodes
    );
    assert!(
        shared_steps < private_steps,
        "sharing must do strictly less work: {shared_steps} vs {private_steps}"
    );
    assert!(
        shared.live_nodes < private.live_nodes,
        "and hold strictly fewer nodes: {} vs {}",
        shared.live_nodes,
        private.live_nodes
    );
    // A weak inequality would pass a memo that shared one node out of forty. This is the measured
    // figure, asserted as a floor so a regression in canonicalization cannot hide inside it.
    assert!(
        private_steps - shared_steps >= private_steps / 4,
        "sharing saved only {} of {private_steps} steps, which is less than the battery overlaps",
        private_steps - shared_steps
    );
    assert!(
        shared.shared_subtrees < private.live_nodes,
        "the share index must be smaller than the unshared node count"
    );
}

/// Every answer in the battery is the oracle's answer — under sharing, at every epoch.
///
/// The I-8 halves above compare the memo to *itself*. This is the comparison that says the memo is
/// right at all (I-1), and it is the one that would catch two colliding queries even if the sharing
/// switch were broken in both positions.
#[test]
fn every_query_in_the_battery_agrees_with_the_oracle_at_every_epoch() {
    let mut memo = Memo::with_sharing(catalog(), Sharing::On).unwrap();
    let mut oracle = Oracle::new(tables()).unwrap();
    let handles: Vec<_> = BATTERY
        .iter()
        .map(|sql| memo.register_sql(sql).expect(sql))
        .collect();
    let queries: Vec<_> = BATTERY
        .iter()
        .map(|sql| schweep_sql::bind_sql(sql, &catalog()).expect(sql).query)
        .collect();

    let mut compared = 0usize;
    let mut checkpoints = history();
    checkpoints.insert(0, EpochDeltas::new());
    for (index, deltas) in checkpoints.iter().enumerate() {
        if index > 0 {
            memo.seal_epoch(deltas).unwrap();
            oracle.seal_epoch(deltas.clone()).unwrap();
        }
        for ((sql, handle), query) in BATTERY.iter().zip(&handles).zip(&queries) {
            let from_memo = memo.read(*handle).map(|(_, answer)| answer.render());
            let from_oracle = oracle
                .canonical_answer_at(query, oracle.sealed_epoch())
                .map(|answer| answer.render());
            match (from_memo, from_oracle) {
                (Ok(a), Ok(b)) => assert_eq!(a, b, "{sql} at epoch {index}"),
                // Both refused. The memo forwards the circuit's wording and the oracle has its own,
                // so what must agree is the message *inside* — which for a live evaluation error is
                // the least message S-22c names.
                (Err(a), Err(b)) => assert!(
                    a.to_string().contains(&b.to_string())
                        || b.to_string().contains(&a.to_string()),
                    "{sql} at epoch {index}: memo said {a}, oracle said {b}"
                ),
                (a, b) => panic!("{sql} at epoch {index}: memo {a:?}, oracle {b:?}"),
            }
            compared += 1;
        }
    }
    println!("memo vs oracle: {compared} answers compared across the battery");
    assert_eq!(compared, BATTERY.len() * (history().len() + 1));
}

/// **Mid-history attach.** A query registered after N epochs answers as the oracle does, from its
/// first read onward — including when it attaches to a subtree another query is already using.
#[test]
fn a_query_attaching_mid_history_answers_as_though_it_had_always_been_there() {
    let mut memo = Memo::with_sharing(catalog(), Sharing::On).unwrap();
    let mut oracle = Oracle::new(tables()).unwrap();

    // The query that establishes the shared subtree, registered from the start.
    let early = memo
        .register_sql("SELECT t.n AS n FROM t WHERE t.k > 1")
        .unwrap();

    let epochs = history();
    for deltas in &epochs {
        memo.seal_epoch(deltas).unwrap();
        oracle.seal_epoch(deltas.clone()).unwrap();
    }

    let before = memo.accounting();

    // Three latecomers: a duplicate of the resident query, a suffix over its subtree, and one that
    // shares only the scan.
    let latecomers = [
        "SELECT t.n AS n FROM t WHERE t.k > 1",
        "SELECT DISTINCT t.n AS n FROM t WHERE t.k > 1",
        "SELECT t.k AS k, COUNT(*) AS c FROM t GROUP BY t.k",
    ];
    let mut handles = Vec::new();
    for sql in latecomers {
        handles.push((sql, memo.register_sql(sql).expect(sql)));
    }
    memo.audit().unwrap();

    let after = memo.accounting();
    assert_eq!(
        after.live_nodes - before.live_nodes,
        2,
        "the duplicate adds nothing at all; the DISTINCT adds one node over the resident query's \
         scan-filter-project; the aggregate adds one, because it reads the same scan"
    );

    for (sql, handle) in &handles {
        let query = schweep_sql::bind_sql(sql, &catalog()).unwrap().query;
        let from_memo = memo.read(*handle).unwrap().1.render();
        let from_oracle = oracle
            .canonical_answer_at(&query, oracle.sealed_epoch())
            .unwrap()
            .render();
        assert_eq!(
            from_memo,
            from_oracle,
            "{sql} registered at epoch {} must answer for the whole history",
            memo.registrations().get(handle).unwrap().registered_at
        );
    }

    // And they keep up from there: the shared prefix now feeds four queries.
    let mut more = EpochDeltas::new();
    more.extend(
        "t",
        vec![
            (t_row(7, Some(5), Some(70)), 2),
            (t_row(1, Some(2), Some(11)), -1),
        ],
    );
    memo.seal_epoch(&more).unwrap();
    oracle.seal_epoch(more).unwrap();

    for (sql, handle) in &handles {
        let query = schweep_sql::bind_sql(sql, &catalog()).unwrap().query;
        assert_eq!(
            memo.read(*handle).unwrap().1.render(),
            oracle
                .canonical_answer_at(&query, oracle.sealed_epoch())
                .unwrap()
                .render(),
            "{sql} after a further epoch"
        );
    }
    let early_query = schweep_sql::bind_sql(BATTERY[0], &catalog()).unwrap().query;
    assert_eq!(
        memo.read(early).unwrap().1.render(),
        oracle
            .canonical_answer_at(&early_query, oracle.sealed_epoch())
            .unwrap()
            .render(),
        "and the query that was there first is untouched by any of it"
    );
}

/// **Mid-history attach onto a core that currently holds an error** (S-22, D-16).
///
/// The shared subtree is raising a division-by-zero for a row that is present, so the resident query
/// has no answer at all. A query attaching to that subtree must inherit the error — not an empty
/// answer, and not somebody else's error — and must recover when the offending row is retracted,
/// because a live error is a property of the *contents* (S-22b).
#[test]
fn a_query_attaching_to_an_erroring_core_inherits_the_error_and_recovers_with_it() {
    let mut memo = Memo::with_sharing(catalog(), Sharing::On).unwrap();
    let mut oracle = Oracle::new(tables()).unwrap();

    // `t.n / t.k` raises when k is 0 (S-21).
    let core = "SELECT t.n AS n FROM t WHERE (t.n / t.k) > 0";
    let resident = memo.register_sql(core).unwrap();
    let core_query = schweep_sql::bind_sql(core, &catalog()).unwrap().query;

    let mut first = EpochDeltas::new();
    first.extend(
        "t",
        vec![
            (t_row(1, Some(2), Some(10)), 1),
            (t_row(2, Some(0), Some(20)), 1), // divides by zero
        ],
    );
    memo.seal_epoch(&first).unwrap();
    oracle.seal_epoch(first).unwrap();

    let resident_error = memo.read(resident).unwrap_err().to_string();
    assert!(
        resident_error.contains("division by zero"),
        "the resident query has no answer while the offending row is present: {resident_error}"
    );
    assert!(
        oracle
            .canonical_answer_at(&core_query, oracle.sealed_epoch())
            .is_err(),
        "and the oracle agrees there is no answer"
    );

    // Attach a suffix to the erroring core, mid-history.
    let suffix = "SELECT DISTINCT t.n AS n FROM t WHERE (t.n / t.k) > 0";
    let late = memo.register_sql(suffix).unwrap();
    let suffix_query = schweep_sql::bind_sql(suffix, &catalog()).unwrap().query;
    memo.audit().unwrap();

    let late_error = memo.read(late).unwrap_err().to_string();
    assert!(
        late_error.contains("division by zero"),
        "a query attaching to an erroring core inherits the error (S-22, I-8): {late_error}"
    );
    assert!(
        oracle
            .canonical_answer_at(&suffix_query, oracle.sealed_epoch())
            .is_err(),
        "which is what the oracle says too"
    );

    // A query that shares *nothing* with the erroring core must be unaffected — the error belongs to
    // the queries downstream of the node that raised it, and to no others.
    let bystander = memo.register_sql("SELECT u.m AS m FROM u").unwrap();
    assert!(
        memo.read(bystander).is_ok(),
        "one query's evaluation error must not become another's (I-8)"
    );

    // Retract the offending row: both queries get their answers back, and both match the oracle.
    let mut second = EpochDeltas::new();
    second.extend("t", vec![(t_row(2, Some(0), Some(20)), -1)]);
    memo.seal_epoch(&second).unwrap();
    oracle.seal_epoch(second).unwrap();

    for (sql, handle, query) in [(core, resident, &core_query), (suffix, late, &suffix_query)] {
        let from_memo = memo.read(handle).unwrap().1.render();
        let from_oracle = oracle
            .canonical_answer_at(query, oracle.sealed_epoch())
            .unwrap()
            .render();
        assert_eq!(
            from_memo, from_oracle,
            "{sql} recovers when the offending row goes (S-22b)"
        );
    }
}

/// **The teardown gate.** Deregistering frees exactly the private suffix.
#[test]
fn teardown_frees_exactly_the_private_suffix() {
    let mut memo = Memo::with_sharing(catalog(), Sharing::On).unwrap();
    let resident: Vec<_> = BATTERY
        .iter()
        .take(4)
        .map(|sql| memo.register_sql(sql).expect(sql))
        .collect();
    for deltas in history() {
        memo.seal_epoch(&deltas).unwrap();
    }
    let baseline = memo.accounting();
    let baseline_answers: Vec<String> = resident
        .iter()
        .map(|handle| memo.read(*handle).unwrap().1.render())
        .collect();

    // Register the rest of the battery, then take it away again.
    let transient: Vec<_> = BATTERY
        .iter()
        .skip(4)
        .map(|sql| memo.register_sql(sql).expect(sql))
        .collect();
    let peak = memo.accounting();
    assert!(
        peak.live_nodes > baseline.live_nodes,
        "the transient queries must actually have built something"
    );

    for handle in transient {
        memo.deregister(handle).unwrap();
    }
    memo.audit().unwrap();

    let after = memo.accounting();
    assert_eq!(
        after.holdings(),
        baseline.holdings(),
        "every holding must return to baseline:\n  baseline {baseline:?}\n  after    {after:?}"
    );

    // The queries that stayed are unharmed — the point of "exactly".
    for (handle, expected) in resident.iter().zip(&baseline_answers) {
        assert_eq!(
            &memo.read(*handle).unwrap().1.render(),
            expected,
            "a resident query's answer changed when another was deregistered"
        );
    }

    // And the dataflow still works.
    let mut more = EpochDeltas::new();
    more.extend("t", vec![(t_row(8, Some(6), Some(80)), 1)]);
    memo.seal_epoch(&more).unwrap();
    memo.audit().unwrap();
}

/// **The leak gate.** 1,000 register/deregister cycles, asserted by accounting.
#[test]
fn a_thousand_cycles_over_a_live_dataflow_leak_nothing() {
    let mut memo = Memo::with_sharing(catalog(), Sharing::On).unwrap();
    let resident: Vec<_> = BATTERY
        .iter()
        .take(3)
        .map(|sql| memo.register_sql(sql).expect(sql))
        .collect();
    for deltas in history() {
        memo.seal_epoch(&deltas).unwrap();
    }
    let baseline = memo.accounting();

    for round in 0..1_000u32 {
        // Rotate through the battery so the cycles exercise every shape, including the ones that
        // share nothing and the ones that are pure suffixes.
        let sql = BATTERY[(round as usize) % BATTERY.len()];
        let handle = memo.register_sql(sql).expect(sql);
        memo.deregister(handle).unwrap();
        if round % 100 == 0 {
            memo.audit().unwrap();
            assert_eq!(
                memo.accounting().holdings(),
                baseline.holdings(),
                "round {round} ({sql}) did not return to baseline"
            );
        }
    }

    memo.audit().unwrap();
    assert_eq!(memo.accounting().holdings(), baseline.holdings());
    // Still answering, and still correct.
    let mut oracle = Oracle::new(tables()).unwrap();
    for deltas in history() {
        oracle.seal_epoch(deltas).unwrap();
    }
    for (sql, handle) in BATTERY.iter().take(3).zip(&resident) {
        let query = schweep_sql::bind_sql(sql, &catalog()).unwrap().query;
        assert_eq!(
            memo.read(*handle).unwrap().1.render(),
            oracle
                .canonical_answer_at(&query, oracle.sealed_epoch())
                .unwrap()
                .render(),
            "{sql} after 1,000 register/deregister cycles"
        );
    }
}

/// The memo's plumbing under I-1: the whole generated population, one query at a time, through a
/// registration rather than a private circuit.
///
/// One query shares nothing, so this is not a sharing test. It is the check that everything sharing
/// stands on — wiring, catch-up, priming, per-sink answers and per-sink errors — is right across the
/// same 4,400 scenarios the earlier gates use.
#[test]
fn the_memo_answers_the_whole_generated_population_as_the_oracle_does() {
    let report = match sweep_matching::<MemoEngine, OracleEngine>(0..4_400, |_| true) {
        Ok(report) => report,
        Err(divergence) => panic!("{divergence}"),
    };
    println!(
        "memo vs oracle over the population: {} scenarios, {} comparisons, {} error answers",
        report.scenarios, report.comparisons, report.error_answers
    );
    assert_eq!(report.scenarios, 4_400);
    assert!(report.error_answers > 0, "S-22 must be exercised here too");
    assert_eq!(
        report.families.len(),
        4,
        "every dialect rung must reach the memo"
    );
}

/// A registration that fails leaves nothing behind.
#[test]
fn a_refused_registration_holds_nothing() {
    let mut memo = Memo::with_sharing(catalog(), Sharing::On).unwrap();
    memo.register_sql(BATTERY[0]).unwrap();
    let baseline = memo.accounting();

    for bad in [
        "SELECT * FROM t",
        "SELECT t.n + 1 FROM t",
        "SELECT v.n AS n FROM v",
        "SELECT COUNT(*) + 1 AS c FROM t",
    ] {
        let refused = memo.register_sql(bad);
        assert!(refused.is_err(), "{bad} was accepted");
        assert_eq!(
            memo.accounting().holdings(),
            baseline.holdings(),
            "{bad} left something behind"
        );
        memo.audit().unwrap();
    }
}
