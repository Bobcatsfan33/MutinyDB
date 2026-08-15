//! End-to-end tests of the rung-1–3 semantics (`docs/SEMANTICS.md`).
//!
//! Each test names the rule it pins. Together they are the executable half of the spec: the
//! document says what a query means, and these say it again in a form that fails when the oracle
//! stops meaning it. Every disputed edge case in the document has a test here — most of all the
//! ones C2 and C3 are going to fight with (null join keys, weight multiplication, drained
//! groups, MIN under retraction).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use schweep_oracle::{AggFunc, EpochDeltas, Expr, Oracle, OracleError, Query, Source};
use schweep_plan::plan::{BinOp, GroupBy, Named};
use schweep_plan::PlanError;
use schweep_zset::{DataType, Field, Row, Schema, Value};

fn int_table(columns: &[&str]) -> Schema {
    Schema::new_table(
        columns
            .iter()
            .map(|c| Field::nullable(*c, DataType::Int64))
            .collect(),
    )
    .unwrap()
}

fn i(v: i64) -> Value {
    Value::Int(v)
}

fn row(values: Vec<Value>) -> Row {
    Row::new(values)
}

/// An oracle over one table `t(a, b)` with the given epoch-1 contents.
fn oracle_with(entries: Vec<(Row, i64)>) -> Oracle {
    let mut oracle = Oracle::new([("t".to_owned(), int_table(&["a", "b"]))]).unwrap();
    let mut deltas = EpochDeltas::new();
    deltas.extend("t", entries);
    oracle.seal_epoch(deltas).unwrap();
    oracle
}

/// Binding refusals and evaluation errors are `PlanError`s now, reported through
/// `OracleError::Plan` (D-14). They are *semantics*, shared with the engine, so the tests assert
/// on the semantic error itself rather than on which enum carried it.
fn plan_error(error: OracleError) -> PlanError {
    match error {
        OracleError::Plan(e) => e,
        other => panic!("expected a plan error, got: {other}"),
    }
}

fn answer(oracle: &Oracle, query: &Query) -> String {
    oracle.answer(query).unwrap().canonical().unwrap().render()
}

// ---------------------------------------------------------------------------------------------
// S-6 epochs
// ---------------------------------------------------------------------------------------------

#[test]
fn s6_epochs_are_dense_from_one_and_an_answer_is_as_of_a_sealed_epoch() {
    let mut oracle = Oracle::new([("t".to_owned(), int_table(&["a", "b"]))]).unwrap();
    assert_eq!(oracle.sealed_epoch(), 0);

    let mut d1 = EpochDeltas::new();
    d1.push("t", row(vec![i(1), i(10)]), 1);
    assert_eq!(oracle.seal_epoch(d1).unwrap(), 1);

    let mut d2 = EpochDeltas::new();
    d2.push("t", row(vec![i(2), i(20)]), 1);
    assert_eq!(oracle.seal_epoch(d2).unwrap(), 2);

    let query = Query::from(Source::scan("t", "t"));
    // The past is still readable, and it is the past — not a mixture (I-3).
    assert_eq!(
        oracle
            .answer_at(&query, 1)
            .unwrap()
            .canonical()
            .unwrap()
            .render(),
        "(t.a: Int64, t.b: Int64)\n(1, 10) => 1\n"
    );
    assert_eq!(
        oracle
            .answer_at(&query, 2)
            .unwrap()
            .canonical()
            .unwrap()
            .render(),
        "(t.a: Int64, t.b: Int64)\n(1, 10) => 1\n(2, 20) => 1\n"
    );
}

#[test]
fn s6_an_empty_epoch_does_not_move_the_answer() {
    let mut oracle = oracle_with(vec![(row(vec![i(1), i(1)]), 1)]);
    let query = Query::from(Source::scan("t", "t"));
    let before = answer(&oracle, &query);

    let empty = EpochDeltas::new();
    assert!(empty.is_empty());
    oracle.seal_epoch(empty).unwrap();

    assert_eq!(oracle.sealed_epoch(), 2, "the epoch was still sealed");
    assert_eq!(answer(&oracle, &query), before);
}

#[test]
fn epoch_beyond_the_sealed_prefix_is_refused() {
    let oracle = oracle_with(vec![]);
    let query = Query::from(Source::scan("t", "t"));
    assert_eq!(
        oracle.answer_at(&query, 9).unwrap_err(),
        OracleError::EpochOutOfRange {
            requested: 9,
            sealed: 1
        }
    );
}

// ---------------------------------------------------------------------------------------------
// S-5 / D-12 malformed history
// ---------------------------------------------------------------------------------------------

#[test]
fn s5_retracting_a_row_that_is_not_there_is_a_malformed_history() {
    let mut oracle = Oracle::new([("t".to_owned(), int_table(&["a", "b"]))]).unwrap();
    let mut d = EpochDeltas::new();
    d.push("t", row(vec![i(1), i(1)]), -1);
    let err = oracle.seal_epoch(d).unwrap_err();
    assert!(
        matches!(
            err,
            OracleError::NegativeIntegral {
                epoch: 1,
                weight: -1,
                ..
            }
        ),
        "expected NegativeIntegral, got {err}"
    );
}

#[test]
fn s5_retracting_more_copies_than_exist_is_a_malformed_history() {
    let mut oracle = oracle_with(vec![(row(vec![i(1), i(1)]), 2)]);
    let mut d = EpochDeltas::new();
    d.push("t", row(vec![i(1), i(1)]), -3);
    assert!(matches!(
        oracle.seal_epoch(d).unwrap_err(),
        OracleError::NegativeIntegral { epoch: 2, .. }
    ));
}

#[test]
fn s5_a_retraction_within_the_same_epoch_is_fine_and_the_row_survives_by_its_remainder() {
    // +3 then -2 in one epoch nets +1. The generator produces exactly this shape (§7).
    let mut oracle = Oracle::new([("t".to_owned(), int_table(&["a", "b"]))]).unwrap();
    let mut d = EpochDeltas::new();
    d.push("t", row(vec![i(1), i(1)]), 3);
    d.push("t", row(vec![i(1), i(1)]), -2);
    oracle.seal_epoch(d).unwrap();
    assert_eq!(
        answer(&oracle, &Query::from(Source::scan("t", "t"))),
        "(t.a: Int64, t.b: Int64)\n(1, 1) => 1\n"
    );
}

#[test]
fn s5_a_row_retracted_to_zero_disappears_entirely() {
    let mut oracle = oracle_with(vec![(row(vec![i(1), i(1)]), 1), (row(vec![i(2), i(2)]), 1)]);
    let mut d = EpochDeltas::new();
    d.push("t", row(vec![i(1), i(1)]), -1);
    oracle.seal_epoch(d).unwrap();
    assert_eq!(
        answer(&oracle, &Query::from(Source::scan("t", "t"))),
        "(t.a: Int64, t.b: Int64)\n(2, 2) => 1\n"
    );
}

// ---------------------------------------------------------------------------------------------
// S-24 / S-25 filter and projection
// ---------------------------------------------------------------------------------------------

#[test]
fn s24_filter_preserves_weights_exactly() {
    let oracle = oracle_with(vec![
        (row(vec![i(1), i(10)]), 5),
        (row(vec![i(2), i(20)]), 3),
    ]);
    let query = Query::from(Source::scan("t", "t")).filter(Expr::binary(
        BinOp::Lt,
        Expr::column("t.a"),
        Expr::int(2),
    ));
    assert_eq!(
        answer(&oracle, &query),
        "(t.a: Int64, t.b: Int64)\n(1, 10) => 5\n"
    );
}

#[test]
fn s25_projection_merges_rows_and_sums_their_weights() {
    // (1,'a') and (1,'b') both project to (1): the result is (1) at weight 2, not two rows and
    // not one row at weight 1. SELECT without DISTINCT preserves duplicates.
    let oracle = oracle_with(vec![
        (row(vec![i(1), i(10)]), 1),
        (row(vec![i(1), i(20)]), 1),
    ]);
    let query =
        Query::from(Source::scan("t", "t")).project(vec![Named::new("a", Expr::column("t.a"))]);
    assert_eq!(answer(&oracle, &query), "(a: Int64)\n(1) => 2\n");
}

#[test]
fn s17_where_x_equals_x_drops_null_rows() {
    let oracle = oracle_with(vec![
        (row(vec![i(1), i(1)]), 1),
        (row(vec![Value::Null, i(2)]), 1),
    ]);
    let query = Query::from(Source::scan("t", "t")).filter(Expr::binary(
        BinOp::Eq,
        Expr::column("t.a"),
        Expr::column("t.a"),
    ));
    assert_eq!(
        answer(&oracle, &query),
        "(t.a: Int64, t.b: Int64)\n(1, 1) => 1\n"
    );
}

#[test]
fn s17_where_not_p_is_not_the_complement_of_where_p() {
    let oracle = oracle_with(vec![
        (row(vec![i(1), i(1)]), 1),
        (row(vec![i(2), i(2)]), 1),
        (row(vec![Value::Null, i(3)]), 1),
    ]);
    let source = || Source::scan("t", "t");
    let eq_one = || Expr::binary(BinOp::Eq, Expr::column("t.a"), Expr::int(1));

    let matching = oracle
        .answer(&Query::from(source()).filter(eq_one()))
        .unwrap();
    let complement = oracle
        .answer(&Query::from(source()).filter(!eq_one()))
        .unwrap();
    let all = oracle.answer(&Query::from(source())).unwrap();

    // 1 row matches, 1 row is in the complement, and 3 rows exist: the null row is in neither.
    assert_eq!(matching.canonical().unwrap().len(), 1);
    assert_eq!(complement.canonical().unwrap().len(), 1);
    assert_eq!(all.canonical().unwrap().len(), 3);
}

// ---------------------------------------------------------------------------------------------
// S-26 join
// ---------------------------------------------------------------------------------------------

fn join_oracle() -> Oracle {
    Oracle::new([
        ("l".to_owned(), int_table(&["k", "x"])),
        ("r".to_owned(), int_table(&["k", "y"])),
    ])
    .unwrap()
}

fn join_query() -> Query {
    Query::from(Source::join(
        Source::scan("l", "l"),
        Source::scan("r", "r"),
        vec![("l.k".to_owned(), "r.k".to_owned())],
    ))
}

#[test]
fn s26_join_multiplies_weights() {
    let mut oracle = join_oracle();
    let mut d = EpochDeltas::new();
    d.push("l", row(vec![i(1), i(100)]), 3);
    d.push("r", row(vec![i(1), i(200)]), 2);
    oracle.seal_epoch(d).unwrap();

    // Three copies against two copies is six copies, not five and not one.
    assert_eq!(
        answer(&oracle, &join_query()),
        "(l.k: Int64, l.x: Int64, r.k: Int64, r.y: Int64)\n(1, 100, 1, 200) => 6\n"
    );
}

#[test]
fn s26_a_null_join_key_never_matches_even_another_null() {
    let mut oracle = join_oracle();
    let mut d = EpochDeltas::new();
    d.push("l", row(vec![Value::Null, i(100)]), 1);
    d.push("r", row(vec![Value::Null, i(200)]), 1);
    oracle.seal_epoch(d).unwrap();

    // NULL = NULL is NULL, not true (S-13), so the pair does not join.
    assert_eq!(
        answer(&oracle, &join_query()),
        "(l.k: Int64, l.x: Int64, r.k: Int64, r.y: Int64)\n"
    );
}

#[test]
fn s26_retracting_one_side_retracts_the_joined_rows() {
    let mut oracle = join_oracle();
    let mut d = EpochDeltas::new();
    d.push("l", row(vec![i(1), i(100)]), 1);
    d.push("r", row(vec![i(1), i(200)]), 1);
    d.push("r", row(vec![i(1), i(201)]), 1);
    oracle.seal_epoch(d).unwrap();
    assert_eq!(
        oracle
            .answer(&join_query())
            .unwrap()
            .canonical()
            .unwrap()
            .len(),
        2
    );

    let mut d2 = EpochDeltas::new();
    d2.push("l", row(vec![i(1), i(100)]), -1);
    oracle.seal_epoch(d2).unwrap();
    assert!(oracle
        .answer(&join_query())
        .unwrap()
        .canonical()
        .unwrap()
        .is_empty());
}

#[test]
fn s26_a_cross_join_is_refused_by_name() {
    let oracle = join_oracle();
    let query = Query::from(Source::join(
        Source::scan("l", "l"),
        Source::scan("r", "r"),
        vec![],
    ));
    assert_eq!(
        plan_error(oracle.answer(&query).unwrap_err()),
        PlanError::CrossJoinNotSupported
    );
}

#[test]
fn s26_a_repeated_alias_is_refused() {
    let oracle = join_oracle();
    let query = Query::from(Source::join(
        Source::scan("l", "same"),
        Source::scan("r", "same"),
        vec![("same.k".to_owned(), "same.k".to_owned())],
    ));
    assert!(matches!(
        plan_error(oracle.answer(&query).unwrap_err()),
        PlanError::DuplicateAlias(_) | PlanError::ZSet(_)
    ));
}

#[test]
fn a_table_can_be_joined_to_itself_under_two_aliases() {
    let mut oracle = join_oracle();
    let mut d = EpochDeltas::new();
    d.push("l", row(vec![i(1), i(7)]), 1);
    d.push("l", row(vec![i(1), i(8)]), 1);
    oracle.seal_epoch(d).unwrap();

    let query = Query::from(Source::join(
        Source::scan("l", "a"),
        Source::scan("l", "b"),
        vec![("a.k".to_owned(), "b.k".to_owned())],
    ));
    // 2 rows on each side, all sharing key 1: four pairs.
    assert_eq!(oracle.answer(&query).unwrap().canonical().unwrap().len(), 4);
}

// ---------------------------------------------------------------------------------------------
// S-27 … S-32 aggregation
// ---------------------------------------------------------------------------------------------

fn group_query(aggregates: Vec<Named<AggFunc>>, having: Option<Expr>) -> Query {
    Query::from(Source::scan("t", "t")).group_by(GroupBy {
        keys: vec![Named::new("k", Expr::column("t.a"))],
        aggregates,
        having,
    })
}

#[test]
fn s27_each_group_produces_exactly_one_row_at_weight_one() {
    let oracle = oracle_with(vec![
        (row(vec![i(1), i(10)]), 5),
        (row(vec![i(1), i(11)]), 2),
        (row(vec![i(2), i(20)]), 9),
    ]);
    let query = group_query(vec![Named::new("n", AggFunc::CountStar)], None);
    // Weight 1 per group, whatever the input multiplicities were; COUNT(*) carries them.
    assert_eq!(
        answer(&oracle, &query),
        "(k: Int64, n: Int64)\n(1, 7) => 1\n(2, 9) => 1\n"
    );
}

#[test]
fn s28_grouping_puts_all_nulls_in_one_group() {
    let oracle = oracle_with(vec![
        (row(vec![Value::Null, i(1)]), 1),
        (row(vec![Value::Null, i(2)]), 1),
        (row(vec![i(1), i(3)]), 1),
    ]);
    let query = group_query(vec![Named::new("n", AggFunc::CountStar)], None);
    // Grouping uses "not distinct from", so the two null-keyed rows are one group (S-28) —
    // unlike a join key, where NULL never matches (S-26).
    assert_eq!(
        answer(&oracle, &query),
        "(k: Int64, n: Int64)\n(NULL, 2) => 1\n(1, 1) => 1\n"
    );
}

#[test]
fn s29_a_group_drained_to_zero_rows_vanishes_rather_than_zeroing() {
    let mut oracle = oracle_with(vec![
        (row(vec![i(1), i(10)]), 1),
        (row(vec![i(2), i(20)]), 1),
    ]);
    let query = group_query(vec![Named::new("n", AggFunc::CountStar)], None);
    assert_eq!(
        answer(&oracle, &query),
        "(k: Int64, n: Int64)\n(1, 1) => 1\n(2, 1) => 1\n"
    );

    let mut d = EpochDeltas::new();
    d.push("t", row(vec![i(1), i(10)]), -1);
    oracle.seal_epoch(d).unwrap();

    // No (1, 0) row. No (1, NULL) row. The row is gone.
    assert_eq!(
        answer(&oracle, &query),
        "(k: Int64, n: Int64)\n(2, 1) => 1\n"
    );
}

#[test]
fn s30_retracting_the_current_min_reveals_the_second_smallest() {
    // The C3 gate in miniature, decided here in C0 as §5.1 requires.
    let mut oracle = oracle_with(vec![
        (row(vec![i(1), i(5)]), 1),
        (row(vec![i(1), i(7)]), 1),
        (row(vec![i(1), i(9)]), 1),
    ]);
    let query = group_query(
        vec![
            Named::new("lo", AggFunc::Min(Expr::column("t.b"))),
            Named::new("hi", AggFunc::Max(Expr::column("t.b"))),
        ],
        None,
    );
    assert_eq!(
        answer(&oracle, &query),
        "(k: Int64, lo: Int64, hi: Int64)\n(1, 5, 9) => 1\n"
    );

    let mut d = EpochDeltas::new();
    d.push("t", row(vec![i(1), i(5)]), -1);
    d.push("t", row(vec![i(1), i(9)]), -1);
    oracle.seal_epoch(d).unwrap();

    assert_eq!(
        answer(&oracle, &query),
        "(k: Int64, lo: Int64, hi: Int64)\n(1, 7, 7) => 1\n"
    );
}

#[test]
fn s30_a_value_retracted_to_weight_zero_is_no_longer_the_min() {
    // Present at weight 2, retracted twice: gone, so MIN must move even though the row was
    // there in an earlier epoch.
    let mut oracle = oracle_with(vec![(row(vec![i(1), i(1)]), 2), (row(vec![i(1), i(4)]), 1)]);
    let mut d = EpochDeltas::new();
    d.push("t", row(vec![i(1), i(1)]), -2);
    oracle.seal_epoch(d).unwrap();

    let query = group_query(
        vec![Named::new("lo", AggFunc::Min(Expr::column("t.b")))],
        None,
    );
    assert_eq!(
        answer(&oracle, &query),
        "(k: Int64, lo: Int64)\n(1, 4) => 1\n"
    );
}

#[test]
fn s30_count_star_counts_weights_and_count_x_skips_nulls() {
    let oracle = oracle_with(vec![
        (row(vec![i(1), i(10)]), 3),
        (row(vec![i(1), Value::Null]), 2),
    ]);
    let query = group_query(
        vec![
            Named::new("all", AggFunc::CountStar),
            Named::new("some", AggFunc::Count(Expr::column("t.b"))),
            Named::new("total", AggFunc::Sum(Expr::column("t.b"))),
        ],
        None,
    );
    assert_eq!(
        answer(&oracle, &query),
        "(k: Int64, all: Int64, some: Int64, total: Int64)\n(1, 5, 3, 30) => 1\n"
    );
}

#[test]
fn s30_an_all_null_group_counts_zero_and_sums_to_null() {
    let oracle = oracle_with(vec![(row(vec![i(1), Value::Null]), 2)]);
    let query = group_query(
        vec![
            Named::new("n", AggFunc::Count(Expr::column("t.b"))),
            Named::new("s", AggFunc::Sum(Expr::column("t.b"))),
            Named::new("a", AggFunc::Avg(Expr::column("t.b"))),
        ],
        None,
    );
    assert_eq!(
        answer(&oracle, &query),
        "(k: Int64, n: Int64, s: Int64, a: Float64)\n(1, 0, NULL, NULL) => 1\n"
    );
}

#[test]
fn s31_avg_lands_exactly_on_the_weighted_quotient_under_retraction() {
    let mut oracle = oracle_with(vec![
        (row(vec![i(1), i(1)]), 1),
        (row(vec![i(1), i(2)]), 1),
        (row(vec![i(1), i(6)]), 1),
    ]);
    let query = group_query(
        vec![Named::new("a", AggFunc::Avg(Expr::column("t.b")))],
        None,
    );
    assert_eq!(
        answer(&oracle, &query),
        "(k: Int64, a: Float64)\n(1, 3.0) => 1\n"
    );

    let mut d = EpochDeltas::new();
    d.push("t", row(vec![i(1), i(6)]), -1);
    oracle.seal_epoch(d).unwrap();

    assert_eq!(
        answer(&oracle, &query),
        "(k: Int64, a: Float64)\n(1, 1.5) => 1\n"
    );
}

#[test]
fn s32_having_filters_groups_and_sees_only_declared_names() {
    let oracle = oracle_with(vec![
        (row(vec![i(1), i(1)]), 1),
        (row(vec![i(2), i(1)]), 4),
        (row(vec![i(3), i(1)]), 2),
    ]);
    let query = group_query(
        vec![Named::new("n", AggFunc::CountStar)],
        Some(Expr::binary(BinOp::Ge, Expr::column("n"), Expr::int(2))),
    );
    assert_eq!(
        answer(&oracle, &query),
        "(k: Int64, n: Int64)\n(2, 4) => 1\n(3, 2) => 1\n"
    );
}

#[test]
fn s32_having_that_is_null_rejects_the_group() {
    let oracle = oracle_with(vec![(row(vec![i(1), Value::Null]), 1)]);
    // SUM over an all-null group is NULL, and `NULL > 0` is NULL, which is not TRUE (S-17).
    let query = group_query(
        vec![Named::new("s", AggFunc::Sum(Expr::column("t.b")))],
        Some(Expr::binary(BinOp::Gt, Expr::column("s"), Expr::int(0))),
    );
    assert_eq!(answer(&oracle, &query), "(k: Int64, s: Int64)\n");
}

/// **S-33 (D-20): a grand total returns one row, even over an empty input.**
///
/// This replaces C0's `s33_a_group_by_with_no_keys_is_refused`, which asserted the refusal that stood
/// while the question was open. The decision went the other way, so the test did too.
#[test]
fn s33_a_grand_total_returns_one_row_even_over_an_empty_input() {
    let empty = Oracle::new([("t".to_owned(), int_table(&["a", "b"]))]).unwrap();
    let query = Query::from(Source::scan("t", "t")).group_by(GroupBy {
        keys: vec![],
        aggregates: vec![
            Named::new("n", AggFunc::CountStar),
            Named::new("c", AggFunc::Count(Expr::column("t.b"))),
            Named::new("s", AggFunc::Sum(Expr::column("t.b"))),
            Named::new("lo", AggFunc::Min(Expr::column("t.b"))),
            Named::new("a", AggFunc::Avg(Expr::column("t.b"))),
        ],
        having: None,
    });

    // Epoch 0: nothing has ever been inserted, and the answer is still one row.
    assert_eq!(
        empty
            .answer_at(&query, 0)
            .unwrap()
            .canonical()
            .unwrap()
            .render(),
        "(n: Int64, c: Int64, s: Int64, lo: Int64, a: Float64)\n(0, 0, NULL, NULL, NULL) => 1\n",
        "COUNT is 0 and the rest are NULL — S-30's empty-P rules over an empty group"
    );

    // And with data, it aggregates the lot.
    let with_data = oracle_with(vec![(row(vec![i(1), i(4)]), 2), (row(vec![i(2), i(6)]), 1)]);
    assert_eq!(
        answer(&with_data, &query),
        "(n: Int64, c: Int64, s: Int64, lo: Int64, a: Float64)\n(3, 3, 14, 4, 4.666666666666667) => 1\n",
        "one group over everything: 3 rows by weight, sum 2x4 + 6 = 14"
    );
}

/// A GROUP BY that computes nothing is still refused — the remaining `EmptyGroupKeys` case.
#[test]
fn s33_a_group_by_with_neither_keys_nor_aggregates_is_refused() {
    let oracle = oracle_with(vec![]);
    let query = Query::from(Source::scan("t", "t")).group_by(GroupBy {
        keys: vec![],
        aggregates: vec![],
        having: None,
    });
    assert_eq!(
        plan_error(oracle.answer(&query).unwrap_err()),
        PlanError::EmptyGroupKeys
    );
}

/// `HAVING` over a grand total composes without a special case: `COUNT(*) > 0` is false over an
/// empty input, so the row is filtered out and the answer *is* empty (S-32, S-17, S-33).
#[test]
fn s33_having_can_filter_a_grand_total_away() {
    let empty = Oracle::new([("t".to_owned(), int_table(&["a", "b"]))]).unwrap();
    let query = Query::from(Source::scan("t", "t")).group_by(GroupBy {
        keys: vec![],
        aggregates: vec![Named::new("n", AggFunc::CountStar)],
        having: Some(Expr::binary(BinOp::Gt, Expr::column("n"), Expr::int(0))),
    });
    assert_eq!(
        empty
            .answer_at(&query, 0)
            .unwrap()
            .canonical()
            .unwrap()
            .render(),
        "(n: Int64)\n",
        "the group exists, and HAVING rejects it"
    );
}

#[test]
fn s27_a_projection_after_a_group_by_sees_only_the_declared_names() {
    let oracle = oracle_with(vec![(row(vec![i(1), i(10)]), 2)]);
    let mut query = group_query(vec![Named::new("n", AggFunc::CountStar)], None);
    query = query.project(vec![Named::new(
        "doubled",
        Expr::binary(BinOp::Mul, Expr::column("n"), Expr::int(2)),
    )]);
    assert_eq!(answer(&oracle, &query), "(doubled: Int64)\n(4) => 1\n");

    // The input columns are no longer reachable, and saying so names the rule.
    let bad = group_query(vec![Named::new("n", AggFunc::CountStar)], None)
        .project(vec![Named::new("x", Expr::column("t.b"))]);
    assert!(matches!(
        plan_error(oracle.answer(&bad).unwrap_err()),
        PlanError::QualifiedColumnAfterGroupBy(_)
    ));
}

// ---------------------------------------------------------------------------------------------
// S-10 … S-19 binding refusals, each naming its construct
// ---------------------------------------------------------------------------------------------

#[test]
fn s10_an_unqualified_column_before_a_group_by_is_refused_as_unqualified() {
    let oracle = oracle_with(vec![]);
    let query = Query::from(Source::scan("t", "t")).filter(Expr::binary(
        BinOp::Eq,
        Expr::column("a"),
        Expr::int(1),
    ));
    assert_eq!(
        plan_error(oracle.answer(&query).unwrap_err()),
        PlanError::UnqualifiedColumn("a".to_owned())
    );
}

#[test]
fn s12_binding_fails_the_same_way_on_an_empty_database() {
    let empty = Oracle::new([("t".to_owned(), int_table(&["a", "b"]))]).unwrap();
    let populated = oracle_with(vec![(row(vec![i(1), i(1)]), 1)]);
    let query = Query::from(Source::scan("t", "t")).filter(Expr::column("t.a"));
    assert_eq!(
        empty.answer_at(&query, 0).unwrap_err(),
        populated.answer(&query).unwrap_err()
    );
}

#[test]
fn s17_where_requires_a_boolean() {
    let oracle = oracle_with(vec![]);
    let query = Query::from(Source::scan("t", "t")).filter(Expr::column("t.a"));
    assert_eq!(
        plan_error(oracle.answer(&query).unwrap_err()),
        PlanError::ExpectedBoolean {
            context: "WHERE",
            found: DataType::Int64
        }
    );
}

#[test]
fn s19_there_are_no_implicit_conversions() {
    let oracle = oracle_with(vec![]);
    let query = Query::from(Source::scan("t", "t")).filter(Expr::binary(
        BinOp::Eq,
        Expr::column("t.a"),
        Expr::string("1"),
    ));
    assert_eq!(
        plan_error(oracle.answer(&query).unwrap_err()),
        PlanError::TypeMismatch {
            op: "=",
            left: DataType::Int64,
            right: DataType::Utf8
        }
    );
}

#[test]
fn s19_an_untyped_null_literal_is_refused_and_a_typed_one_is_not() {
    let oracle = oracle_with(vec![]);
    let untyped = Query::from(Source::scan("t", "t")).filter(Expr::binary(
        BinOp::Eq,
        Expr::column("t.a"),
        Expr::Literal(Value::Null),
    ));
    assert_eq!(
        plan_error(oracle.answer(&untyped).unwrap_err()),
        PlanError::UntypedNullLiteral
    );

    let typed = Query::from(Source::scan("t", "t")).filter(Expr::binary(
        BinOp::Eq,
        Expr::column("t.a"),
        Expr::Null(DataType::Int64),
    ));
    assert!(oracle.answer(&typed).is_ok());
}

#[test]
fn s3_a_float_column_cannot_be_declared_and_a_float_literal_cannot_be_written() {
    let float_table = Schema::new(vec![Field::nullable("x", DataType::Float64)]).unwrap();
    assert!(Oracle::new([("t".to_owned(), float_table)]).is_err());

    let oracle = oracle_with(vec![]);
    let query = Query::from(Source::scan("t", "t"))
        .project(vec![Named::new("x", Expr::Literal(Value::Float(1.0)))]);
    assert_eq!(
        plan_error(oracle.answer(&query).unwrap_err()),
        PlanError::NotInDialect("a FLOAT literal")
    );
}

#[test]
fn s30_sum_and_avg_refuse_a_non_integer_argument() {
    let text = Schema::new_table(vec![
        Field::nullable("k", DataType::Int64),
        Field::nullable("s", DataType::Utf8),
    ])
    .unwrap();
    let oracle = Oracle::new([("t".to_owned(), text)]).unwrap();
    let query = Query::from(Source::scan("t", "t")).group_by(GroupBy {
        keys: vec![Named::new("k", Expr::column("t.k"))],
        aggregates: vec![Named::new("s", AggFunc::Sum(Expr::column("t.s")))],
        having: None,
    });
    assert_eq!(
        plan_error(oracle.answer_at(&query, 0).unwrap_err()),
        PlanError::AggregateTypeUnsupported {
            func: "SUM",
            ty: DataType::Utf8
        }
    );
}

#[test]
fn s11_a_duplicate_output_name_is_refused() {
    let oracle = oracle_with(vec![]);
    let query = Query::from(Source::scan("t", "t")).project(vec![
        Named::new("x", Expr::column("t.a")),
        Named::new("x", Expr::column("t.b")),
    ]);
    assert_eq!(
        plan_error(oracle.answer(&query).unwrap_err()),
        PlanError::DuplicateOutputName("x".to_owned())
    );
}

// ---------------------------------------------------------------------------------------------
// S-22 evaluation errors
// ---------------------------------------------------------------------------------------------

#[test]
fn s22d_an_evaluation_error_is_deterministic() {
    let oracle = oracle_with(vec![(row(vec![i(1), i(0)]), 1)]);
    let query = divide_a_by_b();
    let first = plan_error(oracle.answer(&query).unwrap_err());
    let second = plan_error(oracle.answer(&query).unwrap_err());
    assert_eq!(first, PlanError::DivisionByZero { op: "/" });
    assert_eq!(
        first, second,
        "the same query on the same data errs the same way"
    );
}

fn divide_a_by_b() -> Query {
    Query::from(Source::scan("t", "t")).project(vec![Named::new(
        "q",
        Expr::binary(BinOp::Div, Expr::column("t.a"), Expr::column("t.b")),
    )])
}

/// **S-22: an error is a property of the contents, not of the change.**
///
/// The offending row is inserted, and the query has no answer for as long as it is present — at
/// that epoch and at every epoch after. Retract it and the answer comes back. This is the rule
/// D-16 settled, and the behaviour C1 found the two implementations disagreeing about.
#[test]
fn s22_an_error_lasts_while_the_offending_data_is_present_and_no_longer() {
    let mut oracle = oracle_with(vec![(row(vec![i(1), i(1)]), 1)]);
    let query = divide_a_by_b();
    assert_eq!(answer(&oracle, &query), "(q: Int64)\n(1) => 1\n");

    // Epoch 2 brings a row that divides by zero.
    let mut d = EpochDeltas::new();
    d.push("t", row(vec![i(9), i(0)]), 1);
    oracle.seal_epoch(d).unwrap();
    assert_eq!(
        plan_error(oracle.answer(&query).unwrap_err()),
        PlanError::DivisionByZero { op: "/" }
    );

    // Epoch 3 changes something unrelated. The error is still live, because the row is still there.
    let mut d = EpochDeltas::new();
    d.push("t", row(vec![i(2), i(2)]), 1);
    oracle.seal_epoch(d).unwrap();
    assert!(
        oracle.answer(&query).is_err(),
        "an error is a property of the contents, so it does not expire on its own"
    );

    // Epoch 4 retracts the offending row. The answer returns, with the epoch-3 row included.
    let mut d = EpochDeltas::new();
    d.push("t", row(vec![i(9), i(0)]), -1);
    oracle.seal_epoch(d).unwrap();
    assert_eq!(
        answer(&oracle, &query),
        "(q: Int64)\n(1) => 2\n",
        "1/1 = 1 and 2/2 = 1, so one row at weight 2"
    );
}

/// **S-22c: with several live errors, the least message is reported.**
#[test]
fn s22c_the_least_live_error_message_is_reported() {
    // One row divides by zero; another overflows. Both are live at once.
    let oracle = oracle_with(vec![
        (row(vec![i(1), i(0)]), 1),
        (row(vec![Value::Int(i64::MAX), i(1)]), 1),
    ]);
    let query = Query::from(Source::scan("t", "t")).project(vec![
        Named::new(
            "q",
            Expr::binary(BinOp::Div, Expr::column("t.a"), Expr::column("t.b")),
        ),
        Named::new(
            "s",
            Expr::binary(BinOp::Add, Expr::column("t.a"), Expr::int(1)),
        ),
    ]);
    // "arithmetic overflow in + (S-20)" < "division by zero in / (S-21)" lexicographically.
    assert_eq!(
        plan_error(oracle.answer(&query).unwrap_err()),
        PlanError::ArithmeticOverflow { op: "+" },
        "the least message wins, and which one that is does not depend on scan order"
    );
}

/// **S-22a: for an aggregate the unit is the group.** A group whose SUM overflows produces no row,
/// and the error is live; other groups cannot rescue the answer.
#[test]
fn s22a_a_group_whose_aggregate_overflows_makes_the_answer_an_error() {
    let oracle = oracle_with(vec![
        (row(vec![i(1), Value::Int(i64::MAX)]), 2),
        (row(vec![i(2), i(5)]), 1),
    ]);
    let query = Query::from(Source::scan("t", "t")).group_by(GroupBy {
        keys: vec![Named::new("k", Expr::column("t.a"))],
        aggregates: vec![Named::new("s", AggFunc::Sum(Expr::column("t.b")))],
        having: None,
    });
    assert_eq!(
        plan_error(oracle.answer(&query).unwrap_err()),
        PlanError::AggregateOverflow { func: "SUM" },
        "group 1 sums to 2 x i64::MAX, which does not fit"
    );
}

/// **S-22d: how the history was batched into epochs does not change the error.**
#[test]
fn s22d_batching_does_not_change_the_answer_or_the_error() {
    let rows = vec![
        (row(vec![i(1), i(1)]), 1),
        (row(vec![i(9), i(0)]), 1),
        (row(vec![i(2), i(2)]), 1),
    ];
    let query = divide_a_by_b();

    // All at once.
    let one_epoch = oracle_with(rows.clone());

    // One row per epoch.
    let mut row_at_a_time = Oracle::new([("t".to_owned(), int_table(&["a", "b"]))]).unwrap();
    for entry in rows {
        let mut d = EpochDeltas::new();
        d.extend("t", vec![entry]);
        row_at_a_time.seal_epoch(d).unwrap();
    }

    assert_eq!(
        plan_error(one_epoch.answer(&query).unwrap_err()),
        plan_error(row_at_a_time.answer(&query).unwrap_err())
    );
}

// ---------------------------------------------------------------------------------------------
// I-2, at the oracle's own level
// ---------------------------------------------------------------------------------------------

#[test]
fn i2_two_oracles_fed_the_same_log_give_byte_identical_answers() {
    let build = || {
        let mut oracle = Oracle::new([("t".to_owned(), int_table(&["a", "b"]))]).unwrap();
        for epoch in 0..5_i64 {
            let mut d = EpochDeltas::new();
            d.push("t", row(vec![i(epoch % 3), i(epoch)]), 2);
            d.push("t", row(vec![i(epoch % 3), i(epoch)]), -1);
            if epoch > 0 {
                d.push("t", row(vec![i((epoch - 1) % 3), i(epoch - 1)]), -1);
            }
            oracle.seal_epoch(d).unwrap();
        }
        oracle
    };

    let a = build();
    let b = build();
    let query = group_query(
        vec![
            Named::new("n", AggFunc::CountStar),
            Named::new("s", AggFunc::Sum(Expr::column("t.b"))),
            Named::new("m", AggFunc::Min(Expr::column("t.b"))),
        ],
        None,
    );
    for epoch in 0..=a.sealed_epoch() {
        assert_eq!(
            a.canonical_answer_at(&query, epoch).unwrap().render(),
            b.canonical_answer_at(&query, epoch).unwrap().render(),
            "epoch {epoch}"
        );
    }
}
