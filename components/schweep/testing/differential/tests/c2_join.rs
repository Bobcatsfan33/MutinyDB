//! **The C2 exit gate** (`ARCHITECTURE.md` §6 C2).
//!
//! > **Exit gate:** differential harness green over join scenarios: multi-key batches, retractions
//! > of joined rows, updates (retract+insert same epoch), weight multiplicities >1, and the
//! > delta-delta term (both sides changing in the same epoch — write a scenario that isolates it).
//!
//! Two kinds of scenario appear here and they do different jobs.
//!
//! **Handwritten scenarios** isolate one behaviour each. The randomized sweep would probably cover
//! them, and "probably" is not what §6 asks for: it says *write a scenario that isolates it*. A
//! handwritten scenario also fails with a name rather than a seed, which is worth a great deal when
//! the thing that broke is one term of three.
//!
//! **The randomized sweep** does what a hand-written suite cannot: it finds the combinations nobody
//! thought to write down.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use schweep_differential::{
    compare, sweep_matching, CircuitEngine, EngineUnderTest, OracleEngine, Scenario,
};
use schweep_plan::plan::{BinOp, Named, Query, Source};
use schweep_plan::Expr;
use schweep_zset::{DataType, EpochDeltas, Field, Row, Schema, Value};

/// Seeds swept to reach a large rung-2 population. One family in four is a plain join.
const SEEDS: u64 = 4400;

// ---------------------------------------------------------------------------------------------
// Handwritten scenarios, each isolating one thing
// ---------------------------------------------------------------------------------------------

fn two_int_tables() -> Vec<(String, Schema)> {
    let table = |value: &str| {
        Schema::new_table(vec![
            Field::nullable("k", DataType::Int64),
            Field::nullable(value, DataType::Int64),
        ])
        .unwrap()
    };
    vec![("l".to_owned(), table("x")), ("r".to_owned(), table("y"))]
}

fn join_query() -> Query {
    Query::from(Source::join(
        Source::scan("l", "l"),
        Source::scan("r", "r"),
        vec![("l.k".to_owned(), "r.k".to_owned())],
    ))
}

fn row(a: Option<i64>, b: Option<i64>) -> Row {
    Row::new(vec![
        a.map_or(Value::Null, Value::Int),
        b.map_or(Value::Null, Value::Int),
    ])
}

/// A hand-built scenario. `Scenario`'s fields are public precisely so the harness can be driven by
/// something other than the generator when a specific shape has to be pinned.
fn handwritten(name_seed: u64, epochs: Vec<EpochDeltas>) -> Scenario {
    Scenario {
        seed: name_seed,
        tables: two_int_tables(),
        query: join_query(),
        epochs,
        family: schweep_differential::Family::Join,
    }
}

/// The answer, or the live error, as one comparable string.
///
/// From C3 a query may legitimately have no answer (S-22), so a test that unwrapped the answer would
/// be asserting the absence of errors rather than the property it is about. Rendering both means the
/// comparison covers error text too, which is what I-1 requires.
fn rendered(engine: &CircuitEngine) -> String {
    use schweep_differential::EngineUnderTest;
    match engine.answer() {
        Ok(answer) => answer.render(),
        Err(message) => format!("ERROR: {message}"),
    }
}

/// Run a handwritten scenario through engine-vs-oracle and report the divergence if any.
fn expect_agreement(label: &str, scenario: &Scenario) {
    match compare::<CircuitEngine, OracleEngine>(scenario) {
        Ok(report) => assert!(
            report.comparisons > 0,
            "{label}: nothing was compared, which cannot be right"
        ),
        Err(divergence) => panic!("{label}:\n{divergence}"),
    }
}

/// **The isolating ΔA⋈ΔB scenario** (§6 C2's pitfall).
///
/// One epoch. Both sides insert a row under the same key. Both indexes are empty when the epoch
/// starts, so `ΔA ⋈ B` and `A ⋈ ΔB` each probe nothing: the entire answer is the delta-delta term.
/// Drop that term and the circuit answers nothing while the oracle answers one row.
#[test]
fn both_sides_inserting_a_matching_row_in_one_epoch() {
    let mut first = EpochDeltas::new();
    first.push("l", row(Some(1), Some(10)), 1);
    first.push("r", row(Some(1), Some(20)), 1);

    let scenario = handwritten(90_001, vec![first]);

    // Stated positively as well as differentially: the answer is exactly one joined row. If this
    // read "empty" the test would pass against a broken engine and a broken oracle alike.
    let mut engine = CircuitEngine::build(&scenario.tables, &scenario.query).unwrap();
    engine.seal_epoch(&scenario.epochs[0]).unwrap();
    assert_eq!(
        engine.answer().unwrap().render(),
        "(l.k: Int64, l.x: Int64, r.k: Int64, r.y: Int64)\n(1, 10, 1, 20) => 1\n",
        "the delta-delta term is the only possible source of this row"
    );

    expect_agreement("delta-delta isolated", &scenario);
}

/// The same shape at weight > 1, so a missing term cannot hide behind a coincidence of ones.
#[test]
fn both_sides_inserting_together_with_multiplicities() {
    let mut first = EpochDeltas::new();
    first.push("l", row(Some(1), Some(10)), 3);
    first.push("r", row(Some(1), Some(20)), 2);

    let scenario = handwritten(90_002, vec![first]);
    let mut engine = CircuitEngine::build(&scenario.tables, &scenario.query).unwrap();
    engine.seal_epoch(&scenario.epochs[0]).unwrap();
    assert_eq!(
        engine.answer().unwrap().render(),
        "(l.k: Int64, l.x: Int64, r.k: Int64, r.y: Int64)\n(1, 10, 1, 20) => 6\n",
        "weights multiply: 3 × 2 = 6 (S-26)"
    );
    expect_agreement("delta-delta with multiplicities", &scenario);
}

/// Both sides change in an epoch where each *also* has history — so all three terms fire at once
/// and their sum has to be right, not merely non-zero.
#[test]
fn all_three_terms_fire_in_one_epoch() {
    let mut first = EpochDeltas::new();
    first.push("l", row(Some(1), Some(10)), 1);
    first.push("r", row(Some(1), Some(20)), 1);

    let mut second = EpochDeltas::new();
    second.push("l", row(Some(1), Some(11)), 1);
    second.push("r", row(Some(1), Some(21)), 1);

    let scenario = handwritten(90_003, vec![first, second]);
    let mut engine = CircuitEngine::build(&scenario.tables, &scenario.query).unwrap();
    for epoch in &scenario.epochs {
        engine.seal_epoch(epoch).unwrap();
    }
    // Two rows each side, all under one key: the full join is 2 × 2 = 4.
    assert_eq!(
        engine.answer().unwrap().len(),
        4,
        "two rows against two rows is four"
    );
    expect_agreement("all three terms", &scenario);
}

/// Retraction of joined rows (§6 C2's gate list).
#[test]
fn retracting_a_joined_row_retracts_the_output() {
    let mut first = EpochDeltas::new();
    first.push("l", row(Some(1), Some(10)), 1);
    first.push("r", row(Some(1), Some(20)), 1);
    first.push("r", row(Some(1), Some(21)), 1);

    let mut second = EpochDeltas::new();
    second.push("l", row(Some(1), Some(10)), -1);

    let scenario = handwritten(90_004, vec![first, second]);
    let mut engine = CircuitEngine::build(&scenario.tables, &scenario.query).unwrap();
    engine.seal_epoch(&scenario.epochs[0]).unwrap();
    assert_eq!(engine.answer().unwrap().len(), 2);
    engine.seal_epoch(&scenario.epochs[1]).unwrap();
    assert!(
        engine.answer().unwrap().is_empty(),
        "retracting the only left row leaves no joined rows"
    );
    expect_agreement("retraction of joined rows", &scenario);
}

/// An update — retract and insert in the same epoch — on one side, against history on the other.
#[test]
fn a_same_epoch_update_moves_the_joined_row() {
    let mut first = EpochDeltas::new();
    first.push("l", row(Some(1), Some(10)), 1);
    first.push("r", row(Some(1), Some(20)), 1);

    let mut second = EpochDeltas::new();
    second.push("l", row(Some(1), Some(10)), -1);
    second.push("l", row(Some(1), Some(11)), 1);

    let scenario = handwritten(90_005, vec![first, second]);
    let mut engine = CircuitEngine::build(&scenario.tables, &scenario.query).unwrap();
    for epoch in &scenario.epochs {
        engine.seal_epoch(epoch).unwrap();
    }
    assert_eq!(
        engine.answer().unwrap().render(),
        "(l.k: Int64, l.x: Int64, r.k: Int64, r.y: Int64)\n(1, 11, 1, 20) => 1\n"
    );
    expect_agreement("same-epoch update", &scenario);
}

/// An update on **both** sides in one epoch: four deltas, all three terms, half of them negative.
#[test]
fn a_same_epoch_update_on_both_sides() {
    let mut first = EpochDeltas::new();
    first.push("l", row(Some(1), Some(10)), 1);
    first.push("r", row(Some(1), Some(20)), 1);

    let mut second = EpochDeltas::new();
    second.push("l", row(Some(1), Some(10)), -1);
    second.push("l", row(Some(1), Some(11)), 1);
    second.push("r", row(Some(1), Some(20)), -1);
    second.push("r", row(Some(1), Some(21)), 1);

    let scenario = handwritten(90_006, vec![first, second]);
    let mut engine = CircuitEngine::build(&scenario.tables, &scenario.query).unwrap();
    for epoch in &scenario.epochs {
        engine.seal_epoch(epoch).unwrap();
    }
    assert_eq!(
        engine.answer().unwrap().render(),
        "(l.k: Int64, l.x: Int64, r.k: Int64, r.y: Int64)\n(1, 11, 1, 21) => 1\n",
        "both sides moved; exactly one joined row survives"
    );
    expect_agreement("update on both sides", &scenario);
}

/// Multi-key batches: several keys moving in one epoch, some matching and some not.
#[test]
fn a_multi_key_batch_joins_only_the_matching_keys() {
    let mut first = EpochDeltas::new();
    for (k, x) in [(1, 10), (2, 11), (3, 12)] {
        first.push("l", row(Some(k), Some(x)), 1);
    }
    for (k, y) in [(2, 20), (3, 21), (4, 22)] {
        first.push("r", row(Some(k), Some(y)), 1);
    }

    let scenario = handwritten(90_007, vec![first]);
    let mut engine = CircuitEngine::build(&scenario.tables, &scenario.query).unwrap();
    engine.seal_epoch(&scenario.epochs[0]).unwrap();
    assert_eq!(
        engine.answer().unwrap().render(),
        "(l.k: Int64, l.x: Int64, r.k: Int64, r.y: Int64)\n\
         (2, 11, 2, 20) => 1\n\
         (3, 12, 3, 21) => 1\n",
        "keys 1 and 4 have no partner"
    );
    expect_agreement("multi-key batch", &scenario);
}

/// Null keys never join, on either side, in any term (S-13, S-26).
#[test]
fn null_keys_join_nothing() {
    let mut first = EpochDeltas::new();
    first.push("l", row(None, Some(10)), 1);
    first.push("r", row(None, Some(20)), 1);
    first.push("l", row(Some(1), Some(11)), 1);
    first.push("r", row(Some(1), Some(21)), 1);

    let scenario = handwritten(90_008, vec![first]);
    let mut engine = CircuitEngine::build(&scenario.tables, &scenario.query).unwrap();
    engine.seal_epoch(&scenario.epochs[0]).unwrap();
    assert_eq!(
        engine.answer().unwrap().render(),
        "(l.k: Int64, l.x: Int64, r.k: Int64, r.y: Int64)\n(1, 11, 1, 21) => 1\n",
        "only the non-null key joins"
    );
    expect_agreement("null keys", &scenario);
}

/// A self-join: one table, two aliases, two source nodes.
///
/// The oracle has supported this since C0. Until C2 the circuit could not represent it, because
/// sources were keyed by table; they are keyed by alias now.
#[test]
fn a_table_joined_to_itself_agrees_with_the_oracle() {
    let tables = vec![(
        "t".to_owned(),
        Schema::new_table(vec![
            Field::nullable("k", DataType::Int64),
            Field::nullable("v", DataType::Int64),
        ])
        .unwrap(),
    )];
    let query = Query::from(Source::join(
        Source::scan("t", "a"),
        Source::scan("t", "b"),
        vec![("a.k".to_owned(), "b.k".to_owned())],
    ));

    let mut first = EpochDeltas::new();
    first.push("t", row(Some(1), Some(7)), 1);
    first.push("t", row(Some(1), Some(8)), 1);
    first.push("t", row(Some(2), Some(9)), 1);

    let scenario = Scenario {
        seed: 90_009,
        tables,
        query,
        epochs: vec![first],
        family: schweep_differential::Family::Join,
    };

    let mut engine = CircuitEngine::build(&scenario.tables, &scenario.query).unwrap();
    engine.seal_epoch(&scenario.epochs[0]).unwrap();
    // Key 1 has two rows on each side (the same two), so 4 pairs; key 2 has one, so 1 pair.
    assert_eq!(engine.answer().unwrap().len(), 5);
    expect_agreement("self-join", &scenario);
}

/// A filter and a projection over a join, so the join's output feeds a linear operator.
#[test]
fn a_filter_and_projection_over_a_join() {
    let query = Query::from(Source::join(
        Source::scan("l", "l"),
        Source::scan("r", "r"),
        vec![("l.k".to_owned(), "r.k".to_owned())],
    ))
    .filter(Expr::binary(BinOp::Gt, Expr::column("r.y"), Expr::int(20)))
    .project(vec![
        Named::new("k", Expr::column("l.k")),
        Named::new(
            "sum",
            Expr::binary(BinOp::Add, Expr::column("l.x"), Expr::column("r.y")),
        ),
    ]);

    let mut first = EpochDeltas::new();
    first.push("l", row(Some(1), Some(10)), 1);
    first.push("r", row(Some(1), Some(20)), 1);
    first.push("r", row(Some(1), Some(30)), 1);

    let scenario = Scenario {
        seed: 90_010,
        tables: two_int_tables(),
        query,
        epochs: vec![first],
        family: schweep_differential::Family::Join,
    };

    let mut engine = CircuitEngine::build(&scenario.tables, &scenario.query).unwrap();
    engine.seal_epoch(&scenario.epochs[0]).unwrap();
    assert_eq!(
        engine.answer().unwrap().render(),
        "(k: Int64, sum: Int64)\n(1, 40) => 1\n",
        "only r.y = 30 passes the filter, and 10 + 30 = 40"
    );
    expect_agreement("filter and projection over a join", &scenario);
}

// ---------------------------------------------------------------------------------------------
// The randomized gate
// ---------------------------------------------------------------------------------------------

/// The gate: engine against oracle over randomized **join** scenarios, every sealed epoch.
#[test]
fn engine_vs_oracle_over_randomized_join_scenarios() {
    let report =
        match sweep_matching::<CircuitEngine, OracleEngine>(0..SEEDS, CircuitEngine::claims_join) {
            Ok(report) => report,
            Err(divergence) => panic!("{divergence}"),
        };

    println!(
        "C2 differential gate: {} join scenarios of {} seeds considered ({} skipped as \
         outside rung 2), {} epochs, {} answer comparisons, 0 divergences",
        report.scenarios, report.considered, report.skipped, report.epochs, report.comparisons,
    );

    assert!(
        report.scenarios >= 1000,
        "the gate needs at least 1,000 join scenarios, got {}",
        report.scenarios
    );
    assert_eq!(report.comparisons, report.epochs + report.scenarios);
    // D-16 closed Q-2, so raising expressions are part of the population now. The fence that used
    // to stand here — `error_answers == 0` — asserted that none occurred, because the two
    // implementations disagreed about what an error meant. The claim is stronger now: the sweep
    // above passing means both sides agreed at every comparison, error text included (`compare`
    // treats two different messages as a divergence), and this asserts the population actually
    // contains some so that agreement is not vacuous.
    assert!(
        report.error_answers > 0,
        "no comparison raised, so agreement about errors was never tested (S-22, D-16)"
    );
}

/// The rung-2 gate population contains what §6 C2's list names.
///
/// Measured on the scenarios the gate actually ran. The delta-delta case is the one that matters
/// most, and it is counted directly: an epoch in which both tables receive a positive-weight entry
/// under the same join key.
#[test]
fn the_gate_population_contains_the_shapes_c2_names() {
    let mut scenarios = 0;
    let mut with_same_epoch_matching_inserts = 0;
    let mut with_both_sides_changing = 0;
    let mut with_retraction = 0;
    let mut with_weight_above_one = 0;

    for seed in 0..SEEDS {
        let scenario = Scenario::generate(seed).unwrap();
        if !CircuitEngine::claims_join(&scenario) {
            continue;
        }
        scenarios += 1;

        let mut same_epoch_match = false;
        let mut both_sides = false;
        let mut retraction = false;
        let mut big_weight = false;

        for epoch in &scenario.epochs {
            let tables = epoch.tables();
            let left = tables.get("t0");
            let right = tables.get("t1");
            if let (Some(left), Some(right)) = (left, right) {
                if !left.is_empty() && !right.is_empty() {
                    both_sides = true;
                }
                // The join key is column 0 of each table (the generator always joins on `id`).
                let keys = |entries: &Vec<(Row, i64)>| -> Vec<Value> {
                    entries
                        .iter()
                        .filter(|(_, w)| *w > 0)
                        .filter_map(|(row, _)| row.get(0).cloned())
                        .filter(|v| !v.is_null())
                        .collect()
                };
                let left_keys = keys(left);
                if keys(right).iter().any(|k| left_keys.contains(k)) {
                    same_epoch_match = true;
                }
            }
            for (_, weight) in tables.values().flatten() {
                if *weight < 0 {
                    retraction = true;
                }
                if weight.abs() > 1 {
                    big_weight = true;
                }
            }
        }
        with_same_epoch_matching_inserts += usize::from(same_epoch_match);
        with_both_sides_changing += usize::from(both_sides);
        with_retraction += usize::from(retraction);
        with_weight_above_one += usize::from(big_weight);
    }

    println!(
        "C2 gate population: {scenarios} join scenarios · {with_both_sides_changing} with both \
         sides changing in one epoch · {with_same_epoch_matching_inserts} with same-epoch matching \
         inserts (the delta-delta case) · {with_retraction} with a retraction · \
         {with_weight_above_one} with a weight above 1"
    );

    // The delta-delta case must be common in the randomized population, not merely present in the
    // handwritten scenario above. If the generator drifted so that both sides stopped moving
    // together, the sweep would stop exercising term 3 and nothing else here would notice.
    assert!(
        with_same_epoch_matching_inserts * 4 >= scenarios,
        "only {with_same_epoch_matching_inserts} of {scenarios} join scenarios insert matching \
         keys on both sides in one epoch; the delta-delta term would be barely exercised"
    );
    assert!(
        with_both_sides_changing * 2 >= scenarios,
        "only {with_both_sides_changing} of {scenarios} join scenarios change both sides in one \
         epoch"
    );
    assert!(
        with_retraction * 10 >= scenarios * 7,
        "only {with_retraction} of {scenarios} join scenarios contain a retraction"
    );
    assert!(
        with_weight_above_one * 10 >= scenarios * 5,
        "only {with_weight_above_one} of {scenarios} join scenarios use a weight above 1"
    );
}

/// **The I-2 gate for joins:** two runs produce byte-identical state and answers.
///
/// The join is the first operator with state of its own, so this is the first time the state half
/// of the I-2 comparison has anything to say. The fingerprint includes both indexes.
#[test]
fn i2_two_runs_of_a_join_scenario_produce_byte_identical_state_and_answers() {
    let mut checked = 0;
    for seed in 0..SEEDS {
        let scenario = Scenario::generate(seed).unwrap();
        if !CircuitEngine::claims_join(&scenario) {
            continue;
        }
        checked += 1;
        if checked > 300 {
            break;
        }

        let run = |scenario: &Scenario| -> (Vec<String>, Vec<String>) {
            let mut engine =
                CircuitEngine::build(&scenario.tables, &scenario.query).expect("rung 2 builds");
            let mut states = vec![engine.state_fingerprint().unwrap()];
            let mut answers = vec![rendered(&engine)];
            for epoch in &scenario.epochs {
                engine.seal_epoch(epoch).unwrap();
                states.push(engine.state_fingerprint().unwrap());
                answers.push(rendered(&engine));
            }
            (states, answers)
        };

        let (states_a, answers_a) = run(&scenario);
        let (states_b, answers_b) = run(&Scenario::generate(seed).unwrap());
        assert_eq!(answers_a, answers_b, "seed {seed}: answers differed");
        assert_eq!(states_a, states_b, "seed {seed}: state differed");
    }
    assert!(
        checked > 100,
        "expected many join scenarios, found {checked}"
    );
}

/// The join's state stays inside its declaration for every scenario the gate runs (I-9).
///
/// `Circuit::step` already refuses to advance an epoch whose accounting fails, so a violation would
/// surface as a divergence. This asserts the other direction — that the join really does declare a
/// bound and really is being measured against it — so that the accounting cannot quietly become a
/// no-op the way it was in C1.
#[test]
fn the_joins_state_is_accounted_against_its_declaration() {
    let mut checked = 0;
    let mut saw_a_join_holding_state = false;

    for seed in 0..SEEDS {
        let scenario = Scenario::generate(seed).unwrap();
        if !CircuitEngine::claims_join(&scenario) || scenario.is_empty_input() {
            continue;
        }
        checked += 1;
        if checked > 200 {
            break;
        }

        let mut engine = CircuitEngine::build(&scenario.tables, &scenario.query).unwrap();
        for epoch in &scenario.epochs {
            engine.seal_epoch(epoch).unwrap();
        }
        let fingerprint = engine.state_fingerprint().unwrap();
        for line in fingerprint.lines().filter(|l| l.contains(" join ")) {
            assert!(
                line.contains("proportional to left + right"),
                "the join must declare its bound: {line}"
            );
            let size = field(line, "state_size=");
            let budget = field(line, "budget=");
            assert!(
                size <= budget,
                "join state {size} exceeds its budget {budget}: {line}"
            );
            if size > 0 {
                saw_a_join_holding_state = true;
            }
        }
    }
    assert!(
        checked > 100,
        "expected many join scenarios, found {checked}"
    );
    assert!(
        saw_a_join_holding_state,
        "no join ever held any state, so the accounting proved nothing"
    );
}

fn field(line: &str, key: &str) -> usize {
    line.split(key)
        .nth(1)
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|value| value.parse().ok())
        .unwrap_or_else(|| panic!("no {key} in {line}"))
}

/// The harness can still fail against the join, not only against the linear operators.
#[test]
fn the_gate_would_catch_a_wrong_join() {
    use schweep_differential::SaboteurEngine;

    let mut examined = 0;
    for seed in 0..SEEDS {
        let scenario = Scenario::generate(seed).unwrap();
        if !CircuitEngine::claims_join(&scenario) {
            continue;
        }
        let truthful =
            compare::<CircuitEngine, OracleEngine>(&scenario).unwrap_or_else(|d| panic!("{d}"));
        if !truthful
            .answers
            .iter()
            .any(|a| a.lines().count() > 1 && !a.starts_with("ERROR"))
        {
            continue;
        }
        if examined >= 120 {
            break;
        }
        examined += 1;
        assert!(
            compare::<CircuitEngine, SaboteurEngine>(&scenario).is_err(),
            "seed {seed}: the harness accepted a wrong answer against the join"
        );
    }
    assert!(
        examined > 80,
        "expected many productive join scenarios, found {examined}"
    );
}
