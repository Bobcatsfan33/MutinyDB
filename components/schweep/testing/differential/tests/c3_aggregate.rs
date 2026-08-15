//! **The C3 exit gate** (`ARCHITECTURE.md` §6 C3).
//!
//! > **Exit gate:** differential green over aggregate scenarios *heavy on retractions* —
//! > specifically: retract the current MIN and assert the second-smallest surfaces; drain a group to
//! > zero rows and assert the group row vanishes (not zeroes); AVG over retractions lands exactly on
//! > the oracle's value.
//!
//! Every cliff §6 and the semantics doc name gets a scenario that isolates it, so a failure names the
//! rule rather than a seed. The randomized sweep then covers the combinations nobody wrote down.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use schweep_differential::{
    compare, sweep_matching, CircuitEngine, EngineUnderTest, Family, OracleEngine, Scenario,
};
use schweep_plan::plan::{BinOp, GroupBy, Named, Query, Source};
use schweep_plan::{AggFunc, Expr};
use schweep_zset::{DataType, EpochDeltas, Field, Row, Schema, Value};

const SEEDS: u64 = 4400;

// ---------------------------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------------------------

/// One table `t(k, v, s)`: an integer group key, an integer measure, a string measure.
fn one_table() -> Vec<(String, Schema)> {
    vec![(
        "t".to_owned(),
        Schema::new_table(vec![
            Field::nullable("k", DataType::Int64),
            Field::nullable("v", DataType::Int64),
            Field::nullable("s", DataType::Utf8),
        ])
        .unwrap(),
    )]
}

fn row(k: Option<i64>, v: Option<i64>, s: Option<&str>) -> Row {
    Row::new(vec![
        k.map_or(Value::Null, Value::Int),
        v.map_or(Value::Null, Value::Int),
        s.map_or(Value::Null, |x| Value::Str(x.to_owned())),
    ])
}

fn grouped(aggregates: Vec<Named<AggFunc>>, having: Option<Expr>) -> Query {
    Query::from(Source::scan("t", "t")).group_by(GroupBy {
        keys: vec![Named::new("k", Expr::column("t.k"))],
        aggregates,
        having,
    })
}

fn scenario(seed: u64, query: Query, epochs: Vec<EpochDeltas>) -> Scenario {
    Scenario {
        seed,
        tables: one_table(),
        query,
        epochs,
        family: Family::Aggregate,
    }
}

fn epoch(entries: Vec<(Row, i64)>) -> EpochDeltas {
    let mut d = EpochDeltas::new();
    d.extend("t", entries);
    d
}

/// Step a scenario through the engine and return the answer (or the live error) at each epoch.
fn engine_answers(scenario: &Scenario) -> Vec<String> {
    let mut engine = CircuitEngine::build(&scenario.tables, &scenario.query)
        .unwrap_or_else(|e| panic!("the query must build: {e}"));
    let mut out = Vec::new();
    for epoch in &scenario.epochs {
        engine.seal_epoch(epoch).unwrap();
        out.push(match engine.answer() {
            Ok(answer) => answer.render(),
            Err(message) => format!("ERROR: {message}"),
        });
    }
    out
}

/// Assert the engine and the oracle agree at every sealed epoch, naming the scenario on failure.
fn expect_agreement(label: &str, scenario: &Scenario) {
    match compare::<CircuitEngine, OracleEngine>(scenario) {
        Ok(report) => assert!(report.comparisons > 0, "{label}: nothing was compared"),
        Err(divergence) => panic!("{label}:\n{divergence}"),
    }
}

// ---------------------------------------------------------------------------------------------
// Cliff 1 — MIN/MAX keep an ordered multiset (S-30, §5.3)
// ---------------------------------------------------------------------------------------------

/// **Retract the current MIN and the second-smallest must surface.**
///
/// This is the canonical C3 cliff. An implementation that kept a single value per group instead of
/// an ordered multiset passes every insert-only test and fails this one: having thrown away the 7,
/// it has nothing to fall back to when the 5 leaves.
#[test]
fn retracting_the_current_min_reveals_the_second_smallest() {
    let query = grouped(
        vec![
            Named::new("lo", AggFunc::Min(Expr::column("t.v"))),
            Named::new("hi", AggFunc::Max(Expr::column("t.v"))),
        ],
        None,
    );
    let scenario = scenario(
        93_001,
        query,
        vec![
            epoch(vec![
                (row(Some(1), Some(5), None), 1),
                (row(Some(1), Some(7), None), 1),
                (row(Some(1), Some(9), None), 1),
            ]),
            // Retract the minimum. 7 must surface.
            epoch(vec![(row(Some(1), Some(5), None), -1)]),
            // Retract the maximum. 7 must surface at the top too.
            epoch(vec![(row(Some(1), Some(9), None), -1)]),
        ],
    );

    let answers = engine_answers(&scenario);
    assert_eq!(
        answers[0],
        "(k: Int64, lo: Int64, hi: Int64)\n(1, 5, 9) => 1\n"
    );
    assert_eq!(
        answers[1], "(k: Int64, lo: Int64, hi: Int64)\n(1, 7, 9) => 1\n",
        "retracting the MIN must reveal the second-smallest (S-30)"
    );
    assert_eq!(
        answers[2], "(k: Int64, lo: Int64, hi: Int64)\n(1, 7, 7) => 1\n",
        "retracting the MAX must reveal the second-largest"
    );
    expect_agreement("min under retraction", &scenario);
}

/// A value present at weight 3 is not gone until all three copies are.
#[test]
fn a_multiplicity_must_be_drained_before_the_min_moves() {
    let query = grouped(
        vec![Named::new("lo", AggFunc::Min(Expr::column("t.v")))],
        None,
    );
    let scenario = scenario(
        93_002,
        query,
        vec![
            epoch(vec![
                (row(Some(1), Some(2), None), 3),
                (row(Some(1), Some(8), None), 1),
            ]),
            epoch(vec![(row(Some(1), Some(2), None), -2)]),
            epoch(vec![(row(Some(1), Some(2), None), -1)]),
        ],
    );
    let answers = engine_answers(&scenario);
    assert_eq!(answers[0], "(k: Int64, lo: Int64)\n(1, 2) => 1\n");
    assert_eq!(
        answers[1], "(k: Int64, lo: Int64)\n(1, 2) => 1\n",
        "one copy of the 2 remains, so it is still the minimum"
    );
    assert_eq!(
        answers[2], "(k: Int64, lo: Int64)\n(1, 8) => 1\n",
        "the last copy left, so the minimum moves"
    );
    expect_agreement("multiplicity drained before MIN moves", &scenario);
}

/// MIN/MAX over strings order byte-wise (S-7), and over booleans `false < true`.
#[test]
fn min_and_max_work_on_strings_and_use_the_total_order() {
    let query = grouped(
        vec![
            Named::new("lo", AggFunc::Min(Expr::column("t.s"))),
            Named::new("hi", AggFunc::Max(Expr::column("t.s"))),
        ],
        None,
    );
    let scenario = scenario(
        93_003,
        query,
        vec![
            epoch(vec![
                (row(Some(1), None, Some("b")), 1),
                (row(Some(1), None, Some("Z")), 1),
                (row(Some(1), None, None), 1),
            ]),
            epoch(vec![(row(Some(1), None, Some("Z")), -1)]),
        ],
    );
    let answers = engine_answers(&scenario);
    assert_eq!(
        answers[0], "(k: Int64, lo: Utf8, hi: Utf8)\n(1, \"Z\", \"b\") => 1\n",
        "byte-wise: uppercase Z precedes lowercase b, and the NULL is ignored (S-30)"
    );
    assert_eq!(
        answers[1],
        "(k: Int64, lo: Utf8, hi: Utf8)\n(1, \"b\", \"b\") => 1\n"
    );
    expect_agreement("min/max on strings", &scenario);
}

// ---------------------------------------------------------------------------------------------
// Cliff 2 — a drained group vanishes (S-29)
// ---------------------------------------------------------------------------------------------

/// **A group drained to total weight zero vanishes** — no `(key, 0)` row, no `(key, NULL)` row.
///
/// §6 C3's pitfall: "groups-vanish-at-zero is where naive implementations emit a phantom (key, 0)
/// row; the oracle decides, and the oracle says the row disappears."
#[test]
fn a_group_drained_to_zero_vanishes_leaving_no_phantom_row() {
    let query = grouped(
        vec![
            Named::new("n", AggFunc::CountStar),
            Named::new("s", AggFunc::Sum(Expr::column("t.v"))),
        ],
        None,
    );
    let scenario = scenario(
        93_004,
        query,
        vec![
            epoch(vec![
                (row(Some(1), Some(4), None), 1),
                (row(Some(2), Some(5), None), 1),
            ]),
            epoch(vec![(row(Some(1), Some(4), None), -1)]),
        ],
    );
    let answers = engine_answers(&scenario);
    assert_eq!(
        answers[0],
        "(k: Int64, n: Int64, s: Int64)\n(1, 1, 4) => 1\n(2, 1, 5) => 1\n"
    );
    assert_eq!(
        answers[1], "(k: Int64, n: Int64, s: Int64)\n(2, 1, 5) => 1\n",
        "group 1 is gone entirely: no (1, 0, 0) and no (1, 0, NULL)"
    );
    assert!(
        !answers[1].contains("(1,"),
        "no phantom row for the drained group: {}",
        answers[1]
    );
    expect_agreement("drained group vanishes", &scenario);
}

/// A group that vanishes and then comes back.
#[test]
fn a_group_can_vanish_and_return() {
    let query = grouped(vec![Named::new("n", AggFunc::CountStar)], None);
    let scenario = scenario(
        93_005,
        query,
        vec![
            epoch(vec![(row(Some(1), Some(1), None), 1)]),
            epoch(vec![(row(Some(1), Some(1), None), -1)]),
            epoch(vec![(row(Some(1), Some(2), None), 1)]),
        ],
    );
    let answers = engine_answers(&scenario);
    assert_eq!(answers[0], "(k: Int64, n: Int64)\n(1, 1) => 1\n");
    assert_eq!(answers[1], "(k: Int64, n: Int64)\n");
    assert_eq!(
        answers[2], "(k: Int64, n: Int64)\n(1, 1) => 1\n",
        "the group returns, and the engine must not have kept a stale row to retract"
    );
    expect_agreement("group vanishes and returns", &scenario);
}

/// A group whose rows churn within one epoch never appears at all.
#[test]
fn a_group_created_and_drained_in_one_epoch_never_appears() {
    let query = grouped(vec![Named::new("n", AggFunc::CountStar)], None);
    let scenario = scenario(
        93_006,
        query,
        vec![epoch(vec![
            (row(Some(1), Some(1), None), 2),
            (row(Some(1), Some(1), None), -2),
            (row(Some(2), Some(2), None), 1),
        ])],
    );
    assert_eq!(
        engine_answers(&scenario)[0],
        "(k: Int64, n: Int64)\n(2, 1) => 1\n"
    );
    expect_agreement("group churned within one epoch", &scenario);
}

// ---------------------------------------------------------------------------------------------
// Cliff 3 — SUM accumulates in i128 and lands in i64 (S-30)
// ---------------------------------------------------------------------------------------------

/// **A sum that transits outside the Int64 range but ends inside it is correct.**
#[test]
fn a_sum_that_transits_out_of_range_and_returns_is_correct() {
    let query = grouped(
        vec![Named::new("s", AggFunc::Sum(Expr::column("t.v")))],
        None,
    );
    let scenario = scenario(
        93_007,
        query,
        vec![epoch(vec![
            (row(Some(1), Some(i64::MAX), None), 2),
            (row(Some(1), Some(-i64::MAX), None), 1),
        ])],
    );
    assert_eq!(
        engine_answers(&scenario)[0],
        format!("(k: Int64, s: Int64)\n(1, {}) => 1\n", i64::MAX),
        "2 x i64::MAX - i64::MAX = i64::MAX; an i64 accumulator would overflow halfway"
    );
    expect_agreement("sum transits out of range", &scenario);
}

/// A sum that genuinely does not fit raises, and the group emits nothing (S-22a, S-30).
#[test]
fn a_sum_that_does_not_fit_raises_and_the_error_clears_when_the_data_leaves() {
    let query = grouped(
        vec![Named::new("s", AggFunc::Sum(Expr::column("t.v")))],
        None,
    );
    let scenario = scenario(
        93_008,
        query,
        vec![
            epoch(vec![(row(Some(1), Some(i64::MAX), None), 2)]),
            epoch(vec![(row(Some(1), Some(i64::MAX), None), -1)]),
        ],
    );
    let answers = engine_answers(&scenario);
    assert_eq!(
        answers[0], "ERROR: SUM overflowed the Int64 range (S-30)",
        "the group cannot be evaluated, so the query has no answer (S-22a)"
    );
    assert_eq!(
        answers[1],
        format!("(k: Int64, s: Int64)\n(1, {}) => 1\n", i64::MAX),
        "one copy retracted brings the sum back in range, and the answer returns (S-22)"
    );
    expect_agreement("sum overflow and recovery", &scenario);
}

// ---------------------------------------------------------------------------------------------
// Cliff 4 — AVG is one division of two exact integers (S-31)
// ---------------------------------------------------------------------------------------------

/// **AVG over retractions lands exactly on the oracle's value** (§6 C3's gate list).
#[test]
fn avg_over_retractions_lands_exactly_on_the_oracles_value() {
    let query = grouped(
        vec![Named::new("a", AggFunc::Avg(Expr::column("t.v")))],
        None,
    );
    let scenario = scenario(
        93_009,
        query,
        vec![
            epoch(vec![
                (row(Some(1), Some(1), None), 1),
                (row(Some(1), Some(2), None), 1),
                (row(Some(1), Some(6), None), 1),
            ]),
            epoch(vec![(row(Some(1), Some(6), None), -1)]),
            // A weighted average: after this the group holds 1 at weight 3 and 2 at weight 1.
            epoch(vec![(row(Some(1), Some(1), None), 2)]),
        ],
    );
    let answers = engine_answers(&scenario);
    assert_eq!(answers[0], "(k: Int64, a: Float64)\n(1, 3.0) => 1\n");
    assert_eq!(answers[1], "(k: Int64, a: Float64)\n(1, 1.5) => 1\n");
    assert_eq!(
        answers[2], "(k: Int64, a: Float64)\n(1, 1.25) => 1\n",
        "sum 5 over count 4 — one division of two exact integers, weights included (S-31)"
    );
    expect_agreement("avg under retraction", &scenario);
}

/// AVG of a group whose values are all null is NULL, and the division is never performed (S-31).
#[test]
fn avg_of_an_all_null_group_is_null() {
    let query = grouped(
        vec![
            Named::new("n", AggFunc::Count(Expr::column("t.v"))),
            Named::new("s", AggFunc::Sum(Expr::column("t.v"))),
            Named::new("a", AggFunc::Avg(Expr::column("t.v"))),
        ],
        None,
    );
    let scenario = scenario(
        93_010,
        query,
        vec![epoch(vec![(row(Some(1), None, None), 2)])],
    );
    assert_eq!(
        engine_answers(&scenario)[0],
        "(k: Int64, n: Int64, s: Int64, a: Float64)\n(1, 0, NULL, NULL) => 1\n",
        "COUNT(x) is 0 while SUM and AVG are NULL — S-30's asymmetry"
    );
    expect_agreement("all-null group", &scenario);
}

// ---------------------------------------------------------------------------------------------
// Cliff 5 — grouping uses not-distinct, ON uses = (S-28 vs S-26), in one query
// ---------------------------------------------------------------------------------------------

/// **Nulls group together, but a null join key never matches** — both in a single query.
///
/// The two rules look alike and are opposites, which is why they belong in one scenario: if an
/// implementation used one notion of equality for both, this fails whichever way it chose.
#[test]
fn grouping_groups_nulls_together_while_a_join_key_never_matches_a_null() {
    let tables = vec![
        (
            "l".to_owned(),
            Schema::new_table(vec![
                Field::nullable("id", DataType::Int64),
                Field::nullable("g", DataType::Int64),
            ])
            .unwrap(),
        ),
        (
            "r".to_owned(),
            Schema::new_table(vec![Field::nullable("id", DataType::Int64)]).unwrap(),
        ),
    ];
    // Join on id (nulls never match), then group by l.g (nulls group together).
    let query = Query::from(Source::join(
        Source::scan("l", "l"),
        Source::scan("r", "r"),
        vec![("l.id".to_owned(), "r.id".to_owned())],
    ))
    .group_by(GroupBy {
        keys: vec![Named::new("g", Expr::column("l.g"))],
        aggregates: vec![Named::new("n", AggFunc::CountStar)],
        having: None,
    });

    let mut first = EpochDeltas::new();
    // Two rows with a NULL group key but a matching join key: they join, then group together.
    first.push("l", Row::new(vec![Value::Int(1), Value::Null]), 1);
    first.push("l", Row::new(vec![Value::Int(1), Value::Null]), 1);
    // A row with a NULL join key: it joins nothing, whatever its group key.
    first.push("l", Row::new(vec![Value::Null, Value::Int(9)]), 1);
    first.push("l", Row::new(vec![Value::Int(1), Value::Int(9)]), 1);
    first.push("r", Row::new(vec![Value::Int(1)]), 1);
    first.push("r", Row::new(vec![Value::Null]), 1);

    let scenario = Scenario {
        seed: 93_011,
        tables,
        query,
        epochs: vec![first],
        family: Family::JoinAggregate,
    };

    let mut engine = CircuitEngine::build(&scenario.tables, &scenario.query).unwrap();
    engine.seal_epoch(&scenario.epochs[0]).unwrap();
    assert_eq!(
        engine.answer().unwrap().render(),
        "(g: Int64, n: Int64)\n(NULL, 2) => 1\n(9, 1) => 1\n",
        "the two NULL-group rows are ONE group (S-28); the NULL join key matched nothing (S-26)"
    );
    expect_agreement("grouping vs join-key equality", &scenario);
}

// ---------------------------------------------------------------------------------------------
// HAVING, DISTINCT
// ---------------------------------------------------------------------------------------------

/// HAVING filters groups after aggregation, and a NULL predicate rejects (S-32, S-17).
#[test]
fn having_filters_groups_and_a_null_predicate_rejects() {
    let query = grouped(
        vec![Named::new("n", AggFunc::CountStar)],
        Some(Expr::binary(BinOp::Ge, Expr::column("n"), Expr::int(2))),
    );
    let scenario = scenario(
        93_012,
        query,
        vec![
            epoch(vec![
                (row(Some(1), Some(1), None), 1),
                (row(Some(2), Some(1), None), 3),
            ]),
            // Group 1 grows past the threshold; group 2 falls below it.
            epoch(vec![
                (row(Some(1), Some(2), None), 1),
                (row(Some(2), Some(1), None), -2),
            ]),
        ],
    );
    let answers = engine_answers(&scenario);
    assert_eq!(answers[0], "(k: Int64, n: Int64)\n(2, 3) => 1\n");
    assert_eq!(
        answers[1], "(k: Int64, n: Int64)\n(1, 2) => 1\n",
        "HAVING is re-evaluated as groups change, both ways"
    );
    expect_agreement("having", &scenario);
}

/// DISTINCT collapses weights to one (S-34), and does so incrementally.
#[test]
fn distinct_collapses_weights_and_tracks_presence_incrementally() {
    let query = Query::from(Source::scan("t", "t"))
        .project(vec![Named::new("k", Expr::column("t.k"))])
        .distinct();
    let scenario = scenario(
        93_013,
        query,
        vec![
            // Three rows projecting to two distinct keys: without DISTINCT this would be (1)=>2.
            epoch(vec![
                (row(Some(1), Some(1), None), 1),
                (row(Some(1), Some(2), None), 1),
                (row(Some(2), Some(3), None), 1),
            ]),
            // Another copy of key 1. Already present, so the answer must not move.
            epoch(vec![(row(Some(1), Some(4), None), 1)]),
            // Drain key 1 down to one copy. Still present.
            epoch(vec![
                (row(Some(1), Some(1), None), -1),
                (row(Some(1), Some(2), None), -1),
            ]),
            // The last copy leaves: now it is absent.
            epoch(vec![(row(Some(1), Some(4), None), -1)]),
        ],
    );
    let answers = engine_answers(&scenario);
    assert_eq!(answers[0], "(k: Int64)\n(1) => 1\n(2) => 1\n");
    assert_eq!(
        answers[1], "(k: Int64)\n(1) => 1\n(2) => 1\n",
        "an already-present row gaining a copy must emit nothing"
    );
    assert_eq!(answers[2], "(k: Int64)\n(1) => 1\n(2) => 1\n");
    assert_eq!(
        answers[3], "(k: Int64)\n(2) => 1\n",
        "the last copy leaving is the only change that matters"
    );
    expect_agreement("distinct", &scenario);
}

// ---------------------------------------------------------------------------------------------
// The randomized gate
// ---------------------------------------------------------------------------------------------

/// The gate: engine against oracle over randomized **aggregate** scenarios, every sealed epoch.
#[test]
fn engine_vs_oracle_over_randomized_aggregate_scenarios() {
    let report = match sweep_matching::<CircuitEngine, OracleEngine>(
        0..SEEDS,
        CircuitEngine::claims_aggregate,
    ) {
        Ok(report) => report,
        Err(divergence) => panic!("{divergence}"),
    };

    println!(
        "C3 differential gate: {} aggregate scenarios of {} seeds considered ({} skipped as \
         non-aggregate), {} epochs, {} answer comparisons, {} of them a shared live error, \
         0 divergences",
        report.scenarios,
        report.considered,
        report.skipped,
        report.epochs,
        report.comparisons,
        report.error_answers,
    );

    assert!(
        report.scenarios >= 2000,
        "the gate needs at least 2,000 aggregate scenarios, got {}",
        report.scenarios
    );
    assert_eq!(report.comparisons, report.epochs + report.scenarios);
    assert!(
        report.error_answers > 0,
        "no comparison raised, so agreement about errors was never tested (S-22, D-16)"
    );
}

/// **Gate hygiene: the skipped-seed count fell.**
///
/// C2's gate skipped 3,310 of 4,400 seeds as outside rung 2. The engine now claims every family the
/// generator produces, so a sweep over `claims` should skip none — and the number is printed rather
/// than merely asserted, because "the gate covers more than it did" is the kind of claim that should
/// come with a figure.
#[test]
fn the_engine_now_claims_the_whole_generated_population() {
    let report =
        match sweep_matching::<CircuitEngine, OracleEngine>(0..SEEDS, CircuitEngine::claims) {
            Ok(report) => report,
            Err(divergence) => panic!("{divergence}"),
        };

    println!(
        "C3 whole-population sweep: {} scenarios of {} seeds considered, {} skipped \
         (C2 skipped 3310), {} epochs, {} answer comparisons, 0 divergences",
        report.scenarios, report.considered, report.skipped, report.epochs, report.comparisons,
    );

    assert!(
        report.skipped < 3310,
        "the skipped-seed count must have fallen from C2's 3310, got {}",
        report.skipped
    );
    assert_eq!(
        report.skipped, 0,
        "the engine claims every generated family now, so nothing should be skipped"
    );
    assert_eq!(report.scenarios, SEEDS as usize);
}

/// The aggregate gate population carries the shapes C3 depends on, measured on what it ran.
#[test]
fn the_gate_population_contains_the_shapes_c3_needs() {
    let mut scenarios = 0;
    let mut with_retraction = 0;
    let mut with_weight_above_one = 0;
    let mut with_distinct = 0;
    let mut join_aggregate = 0;
    let mut with_both_sides_changing = 0;

    for seed in 0..SEEDS {
        let scenario = Scenario::generate(seed).unwrap();
        if !CircuitEngine::claims_aggregate(&scenario) {
            continue;
        }
        scenarios += 1;
        if scenario.query.distinct {
            with_distinct += 1;
        }
        if scenario.family == Family::JoinAggregate {
            join_aggregate += 1;
        }

        let mut retraction = false;
        let mut big = false;
        let mut both = false;
        for epoch in &scenario.epochs {
            let tables = epoch.tables();
            if let (Some(l), Some(r)) = (tables.get("t0"), tables.get("t1")) {
                if !l.is_empty() && !r.is_empty() {
                    both = true;
                }
            }
            for (_, weight) in tables.values().flatten() {
                if *weight < 0 {
                    retraction = true;
                }
                if weight.abs() > 1 {
                    big = true;
                }
            }
        }
        with_retraction += usize::from(retraction);
        with_weight_above_one += usize::from(big);
        with_both_sides_changing += usize::from(both);
    }

    println!(
        "C3 gate population: {scenarios} aggregate scenarios ({join_aggregate} over a join) · \
         {with_retraction} with a retraction · {with_weight_above_one} with a weight above 1 · \
         {with_distinct} with DISTINCT · {with_both_sides_changing} with both join sides changing \
         in one epoch"
    );

    // §6 C3 asks for scenarios "heavy on retractions".
    assert!(
        with_retraction * 10 >= scenarios * 7,
        "only {with_retraction} of {scenarios} aggregate scenarios contain a retraction"
    );
    assert!(
        with_weight_above_one * 10 >= scenarios * 5,
        "only {with_weight_above_one} of {scenarios} use a weight above 1"
    );
    assert!(
        with_distinct * 8 >= scenarios,
        "only {with_distinct} of {scenarios} use DISTINCT"
    );
    // The delta-delta coverage assertion extends to the join-aggregate family: an aggregate over a
    // join still depends on the join's third term being right.
    assert!(
        with_both_sides_changing * 2 >= join_aggregate,
        "only {with_both_sides_changing} of {join_aggregate} join-aggregate scenarios change both \
         sides in one epoch"
    );
}

/// **I-2 for the stateful aggregates:** two runs produce byte-identical state and answers.
#[test]
fn i2_two_runs_of_an_aggregate_scenario_produce_byte_identical_state_and_answers() {
    let mut checked = 0;
    for seed in 0..SEEDS {
        let scenario = Scenario::generate(seed).unwrap();
        if !CircuitEngine::claims_aggregate(&scenario) {
            continue;
        }
        checked += 1;
        if checked > 300 {
            break;
        }

        let run = |scenario: &Scenario| -> (Vec<String>, Vec<String>) {
            let mut engine =
                CircuitEngine::build(&scenario.tables, &scenario.query).expect("rung 3 builds");
            let mut states = vec![engine.state_fingerprint().unwrap()];
            let mut answers = vec![match engine.answer() {
                Ok(a) => a.render(),
                Err(e) => format!("ERROR: {e}"),
            }];
            for epoch in &scenario.epochs {
                engine.seal_epoch(epoch).unwrap();
                states.push(engine.state_fingerprint().unwrap());
                answers.push(match engine.answer() {
                    Ok(a) => a.render(),
                    Err(e) => format!("ERROR: {e}"),
                });
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
        "expected many aggregate scenarios, found {checked}"
    );
}

/// The aggregate and distinct operators declare a bound and are accounted against it (I-9).
#[test]
fn stateful_operators_are_accounted_against_their_declarations() {
    let mut checked = 0;
    let mut saw_aggregate_state = false;
    let mut saw_distinct_state = false;

    for seed in 0..SEEDS {
        let scenario = Scenario::generate(seed).unwrap();
        if !CircuitEngine::claims_aggregate(&scenario) || scenario.is_empty_input() {
            continue;
        }
        checked += 1;
        if checked > 250 {
            break;
        }

        let mut engine = CircuitEngine::build(&scenario.tables, &scenario.query).unwrap();
        for epoch in &scenario.epochs {
            engine.seal_epoch(epoch).unwrap();
        }
        let fingerprint = engine.state_fingerprint().unwrap();
        for line in fingerprint.lines() {
            let stateful = line.contains(" aggregate ") || line.contains(" distinct ");
            if !stateful {
                continue;
            }
            assert!(
                line.contains("proportional to input"),
                "a stateful operator must declare its bound: {line}"
            );
            let size = field(line, "state_size=");
            let budget = field(line, "budget=");
            assert!(
                size <= budget,
                "state {size} exceeds budget {budget}: {line}"
            );
            if size > 0 && line.contains(" aggregate ") {
                saw_aggregate_state = true;
            }
            if size > 0 && line.contains(" distinct ") {
                saw_distinct_state = true;
            }
        }
    }
    assert!(
        checked > 100,
        "expected many aggregate scenarios, found {checked}"
    );
    assert!(
        saw_aggregate_state,
        "no aggregate ever held state, so the accounting proved nothing"
    );
    assert!(
        saw_distinct_state,
        "no distinct ever held state, so the accounting proved nothing"
    );
}

fn field(line: &str, key: &str) -> usize {
    line.split(key)
        .nth(1)
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|value| value.parse().ok())
        .unwrap_or_else(|| panic!("no {key} in {line}"))
}

/// The harness can still fail against the aggregates.
#[test]
fn the_gate_would_catch_a_wrong_aggregate() {
    use schweep_differential::SaboteurEngine;

    let mut examined = 0;
    for seed in 0..SEEDS {
        let scenario = Scenario::generate(seed).unwrap();
        if !CircuitEngine::claims_aggregate(&scenario) {
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
            "seed {seed}: the harness accepted a wrong answer against the aggregate"
        );
    }
    assert!(
        examined > 80,
        "expected many productive aggregate scenarios, found {examined}"
    );
}
