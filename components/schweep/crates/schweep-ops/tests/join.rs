//! The join operator, term by term (`ARCHITECTURE.md` §5.3, D-3; `docs/SEMANTICS.md` S-26).
//!
//! The differential harness proves the join *agrees with the oracle* over randomized histories.
//! These tests do something the harness cannot: they isolate each of the three terms, so that when
//! one is wrong the failure names it instead of pointing at a scenario.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use schweep_ops::{Join, Operator, StateBound};
use schweep_state::MemBackend;
use schweep_zset::{DataType, Field, Row, Schema, Value, ZSetBatch};

fn left_schema() -> Schema {
    Schema::new(vec![
        Field::nullable("l.k", DataType::Int64),
        Field::nullable("l.x", DataType::Int64),
    ])
    .unwrap()
}

fn right_schema() -> Schema {
    Schema::new(vec![
        Field::nullable("r.k", DataType::Int64),
        Field::nullable("r.y", DataType::Int64),
    ])
    .unwrap()
}

fn join_on_first_column() -> Join {
    Join::new(
        left_schema(),
        right_schema(),
        vec![(0, 0)],
        Box::new(MemBackend::new()),
        Box::new(MemBackend::new()),
    )
    .unwrap()
}

fn row(a: Option<i64>, b: Option<i64>) -> Row {
    Row::new(vec![
        a.map_or(Value::Null, Value::Int),
        b.map_or(Value::Null, Value::Int),
    ])
}

fn batch(schema: Schema, entries: Vec<(Row, i64)>) -> ZSetBatch {
    ZSetBatch::from_entries(schema, entries).unwrap()
}

fn empty(schema: Schema) -> ZSetBatch {
    ZSetBatch::empty(schema).unwrap()
}

/// Step the join and render its output delta.
fn step(join: &mut Join, left: &ZSetBatch, right: &ZSetBatch) -> String {
    let out = join.step(&[left, right]).unwrap();
    assert!(
        out.errors.is_empty(),
        "the join raises nothing data-dependent"
    );
    out.data.canonical().unwrap().render()
}

// ---------------------------------------------------------------------------------------------
// The three terms, one at a time
// ---------------------------------------------------------------------------------------------

/// **Term 3 in isolation: `ΔA ⋈ ΔB`.**
///
/// One epoch. Both sides insert a matching row. Both indexes are empty beforehand, so term 1
/// (`ΔA ⋈ B`) and term 2 (`A ⋈ ΔB`) each probe nothing and contribute nothing. Every row in the
/// answer comes from term 3.
///
/// This is the test §6 C2 asks for: "the harness must have a scenario that fails if it is missing
/// (both sides insert matching rows in the same epoch)". Drop the term and this sees an empty
/// answer.
#[test]
fn the_delta_delta_term_is_the_whole_answer_when_both_sides_insert_together() {
    let mut join = join_on_first_column();
    let out = step(
        &mut join,
        &batch(left_schema(), vec![(row(Some(1), Some(10)), 1)]),
        &batch(right_schema(), vec![(row(Some(1), Some(20)), 1)]),
    );
    assert_eq!(
        out, "(l.k: Int64, l.x: Int64, r.k: Int64, r.y: Int64)\n(1, 10, 1, 20) => 1\n",
        "with both indexes empty, only the delta-delta term can produce this row"
    );
}

/// **Term 1 in isolation: `ΔA ⋈ B`.** The right side arrives first and is indexed; the next epoch's
/// left delta probes it.
#[test]
fn the_left_delta_probes_the_right_integral() {
    let mut join = join_on_first_column();

    let first = step(
        &mut join,
        &empty(left_schema()),
        &batch(right_schema(), vec![(row(Some(1), Some(20)), 1)]),
    );
    assert_eq!(
        first, "(l.k: Int64, l.x: Int64, r.k: Int64, r.y: Int64)\n",
        "nothing on the left yet, so nothing joins"
    );

    let second = step(
        &mut join,
        &batch(left_schema(), vec![(row(Some(1), Some(10)), 1)]),
        &empty(right_schema()),
    );
    assert_eq!(
        second,
        "(l.k: Int64, l.x: Int64, r.k: Int64, r.y: Int64)\n(1, 10, 1, 20) => 1\n"
    );
}

/// **Term 2 in isolation: `A ⋈ ΔB`.** The mirror image.
#[test]
fn the_right_delta_probes_the_left_integral() {
    let mut join = join_on_first_column();
    step(
        &mut join,
        &batch(left_schema(), vec![(row(Some(1), Some(10)), 1)]),
        &empty(right_schema()),
    );
    let second = step(
        &mut join,
        &empty(left_schema()),
        &batch(right_schema(), vec![(row(Some(1), Some(20)), 1)]),
    );
    assert_eq!(
        second,
        "(l.k: Int64, l.x: Int64, r.k: Int64, r.y: Int64)\n(1, 10, 1, 20) => 1\n"
    );
}

/// The probes read the integrals **as they were before this epoch**, so a row is never counted
/// twice.
///
/// Both sides already hold a matching row; then both sides add another. If the probes ran against
/// the *updated* indexes, the new pairs would be counted twice — once by a term probing an index
/// that already contains this epoch's row, and once by the delta-delta term. The answer would be
/// 8 rows where it should be 4.
#[test]
fn probing_happens_before_integrating_so_nothing_is_counted_twice() {
    let mut join = join_on_first_column();
    step(
        &mut join,
        &batch(left_schema(), vec![(row(Some(1), Some(10)), 1)]),
        &batch(right_schema(), vec![(row(Some(1), Some(20)), 1)]),
    );

    // Now each side gains one more row under the same key. Afterwards each side holds 2 rows, so
    // the full join is 2×2 = 4 rows; three were already emitted, so this epoch must emit exactly 3.
    let out = step(
        &mut join,
        &batch(left_schema(), vec![(row(Some(1), Some(11)), 1)]),
        &batch(right_schema(), vec![(row(Some(1), Some(21)), 1)]),
    );
    assert_eq!(
        out,
        "(l.k: Int64, l.x: Int64, r.k: Int64, r.y: Int64)\n\
         (1, 10, 1, 21) => 1\n\
         (1, 11, 1, 20) => 1\n\
         (1, 11, 1, 21) => 1\n",
        "exactly the three new pairs: old×new, new×old, new×new"
    );
}

// ---------------------------------------------------------------------------------------------
// S-26: weights, nulls, retraction
// ---------------------------------------------------------------------------------------------

/// Multiplicities multiply (S-26).
#[test]
fn weights_multiply() {
    let mut join = join_on_first_column();
    let out = step(
        &mut join,
        &batch(left_schema(), vec![(row(Some(1), Some(10)), 3)]),
        &batch(right_schema(), vec![(row(Some(1), Some(20)), 2)]),
    );
    assert_eq!(
        out, "(l.k: Int64, l.x: Int64, r.k: Int64, r.y: Int64)\n(1, 10, 1, 20) => 6\n",
        "three copies against two copies is six, not five and not one"
    );
}

/// A null key never matches, not even another null (S-13, S-26).
#[test]
fn a_null_key_never_matches_even_another_null() {
    let mut join = join_on_first_column();
    let out = step(
        &mut join,
        &batch(left_schema(), vec![(row(None, Some(10)), 1)]),
        &batch(right_schema(), vec![(row(None, Some(20)), 1)]),
    );
    assert_eq!(
        out, "(l.k: Int64, l.x: Int64, r.k: Int64, r.y: Int64)\n",
        "NULL = NULL is NULL, not true"
    );

    // And a null-keyed row already in the index is not matched by a later null-keyed row either —
    // the index does hold it, so this checks the probe declines rather than the scan missing.
    let out = step(
        &mut join,
        &batch(left_schema(), vec![(row(None, Some(11)), 1)]),
        &batch(right_schema(), vec![(row(None, Some(21)), 1)]),
    );
    assert_eq!(out, "(l.k: Int64, l.x: Int64, r.k: Int64, r.y: Int64)\n");
}

/// Retracting one side retracts the joined rows, by the same code path that produced them (I-5).
#[test]
fn retracting_one_side_retracts_the_joined_rows() {
    let mut join = join_on_first_column();
    step(
        &mut join,
        &batch(left_schema(), vec![(row(Some(1), Some(10)), 1)]),
        &batch(
            right_schema(),
            vec![(row(Some(1), Some(20)), 1), (row(Some(1), Some(21)), 1)],
        ),
    );

    let out = step(
        &mut join,
        &batch(left_schema(), vec![(row(Some(1), Some(10)), -1)]),
        &empty(right_schema()),
    );
    assert_eq!(
        out,
        "(l.k: Int64, l.x: Int64, r.k: Int64, r.y: Int64)\n\
         (1, 10, 1, 20) => -1\n\
         (1, 10, 1, 21) => -1\n",
        "one retraction on the left retracts both joined rows"
    );
}

/// A same-epoch retract-and-insert on one side — an update — against a stable other side.
#[test]
fn a_same_epoch_update_on_one_side_moves_the_joined_row() {
    let mut join = join_on_first_column();
    step(
        &mut join,
        &batch(left_schema(), vec![(row(Some(1), Some(10)), 1)]),
        &batch(right_schema(), vec![(row(Some(1), Some(20)), 1)]),
    );

    let out = step(
        &mut join,
        &batch(
            left_schema(),
            vec![(row(Some(1), Some(10)), -1), (row(Some(1), Some(11)), 1)],
        ),
        &empty(right_schema()),
    );
    assert_eq!(
        out,
        "(l.k: Int64, l.x: Int64, r.k: Int64, r.y: Int64)\n\
         (1, 10, 1, 20) => -1\n\
         (1, 11, 1, 20) => 1\n"
    );
}

/// A retraction on one side and an insertion on the other, in one epoch: the delta-delta term
/// carries a *negative* product, and nothing special-cases it (I-5).
#[test]
fn the_delta_delta_term_handles_a_negative_product() {
    let mut join = join_on_first_column();
    step(
        &mut join,
        &batch(left_schema(), vec![(row(Some(1), Some(10)), 1)]),
        &empty(right_schema()),
    );

    // Left retracts its row; right inserts a matching one. Term 2 pairs the right insert with the
    // left integral (+1), and term 3 pairs the two deltas (-1). They cancel exactly.
    let out = step(
        &mut join,
        &batch(left_schema(), vec![(row(Some(1), Some(10)), -1)]),
        &batch(right_schema(), vec![(row(Some(1), Some(20)), 1)]),
    );
    assert_eq!(
        out, "(l.k: Int64, l.x: Int64, r.k: Int64, r.y: Int64)\n",
        "a row that arrives and leaves within one epoch nets to nothing"
    );
}

/// A row that churns within one epoch on both sides at once.
#[test]
fn same_epoch_churn_on_both_sides_nets_to_nothing() {
    let mut join = join_on_first_column();
    let out = step(
        &mut join,
        &batch(
            left_schema(),
            vec![(row(Some(1), Some(10)), 1), (row(Some(1), Some(10)), -1)],
        ),
        &batch(right_schema(), vec![(row(Some(1), Some(20)), 1)]),
    );
    assert_eq!(out, "(l.k: Int64, l.x: Int64, r.k: Int64, r.y: Int64)\n");
    assert_eq!(
        join.state_entries(),
        1,
        "the churned left row leaves no entry; the right row stays"
    );
}

/// A multi-column key: every pair must compare TRUE (S-26).
#[test]
fn a_multi_column_key_requires_every_pair_to_match() {
    let left = Schema::new(vec![
        Field::nullable("l.a", DataType::Int64),
        Field::nullable("l.b", DataType::Int64),
    ])
    .unwrap();
    let right = Schema::new(vec![
        Field::nullable("r.a", DataType::Int64),
        Field::nullable("r.b", DataType::Int64),
    ])
    .unwrap();
    let mut join = Join::new(
        left.clone(),
        right.clone(),
        vec![(0, 0), (1, 1)],
        Box::new(MemBackend::new()),
        Box::new(MemBackend::new()),
    )
    .unwrap();

    let out = join
        .step(&[
            &batch(
                left,
                vec![(row(Some(1), Some(2)), 1), (row(Some(1), Some(3)), 1)],
            ),
            &batch(right, vec![(row(Some(1), Some(2)), 1)]),
        ])
        .unwrap()
        .data
        .canonical()
        .unwrap()
        .render();
    assert_eq!(
        out, "(l.a: Int64, l.b: Int64, r.a: Int64, r.b: Int64)\n(1, 2, 1, 2) => 1\n",
        "only the row matching on both columns joins"
    );
}

// ---------------------------------------------------------------------------------------------
// I-9: the declaration, and the state behind it
// ---------------------------------------------------------------------------------------------

#[test]
fn the_join_declares_state_proportional_to_both_inputs() {
    let join = join_on_first_column();
    assert_eq!(
        join.state_bound(),
        StateBound::ProportionalToInputs {
            inputs: &["left", "right"],
            factor: 1,
            constant: 0
        }
    );
    assert_eq!(join.state_size(), 0, "an unfed join holds nothing");
}

#[test]
fn state_grows_with_the_inputs_and_shrinks_when_they_are_retracted() {
    let mut join = join_on_first_column();
    step(
        &mut join,
        &batch(
            left_schema(),
            vec![(row(Some(1), Some(10)), 1), (row(Some(2), Some(11)), 1)],
        ),
        &batch(right_schema(), vec![(row(Some(1), Some(20)), 1)]),
    );
    assert_eq!(join.state_size(), 3, "two left rows plus one right row");

    step(
        &mut join,
        &batch(left_schema(), vec![(row(Some(1), Some(10)), -1)]),
        &empty(right_schema()),
    );
    assert_eq!(
        join.state_size(),
        2,
        "a retracted row leaves the index entirely — no tombstone"
    );
}

/// A cross join is refused: at least one key pair is required (S-26).
#[test]
fn a_join_with_no_key_pairs_is_refused() {
    let error = Join::new(
        left_schema(),
        right_schema(),
        vec![],
        Box::new(MemBackend::new()),
        Box::new(MemBackend::new()),
    )
    .unwrap_err();
    assert!(
        error.to_string().contains("cross join"),
        "the refusal must name the construct: {error}"
    );
}

/// Key columns must have the same type — there are no coercions (S-19).
#[test]
fn a_join_across_types_is_refused() {
    let right = Schema::new(vec![
        Field::nullable("r.k", DataType::Utf8),
        Field::nullable("r.y", DataType::Int64),
    ])
    .unwrap();
    let error = Join::new(
        left_schema(),
        right,
        vec![(0, 0)],
        Box::new(MemBackend::new()),
        Box::new(MemBackend::new()),
    )
    .unwrap_err();
    assert!(
        error.to_string().contains("join key"),
        "the refusal must name the operator: {error}"
    );
}

/// The state rendering is ordered, so two identical histories fingerprint identically (I-2).
#[test]
fn the_state_rendering_is_deterministic() {
    let run = |order_flipped: bool| {
        let mut join = join_on_first_column();
        let entries = if order_flipped {
            vec![(row(Some(2), Some(11)), 1), (row(Some(1), Some(10)), 1)]
        } else {
            vec![(row(Some(1), Some(10)), 1), (row(Some(2), Some(11)), 1)]
        };
        step(
            &mut join,
            &batch(left_schema(), entries),
            &batch(right_schema(), vec![(row(Some(1), Some(20)), 1)]),
        );
        join.render_indexes().unwrap()
    };
    assert_eq!(
        run(false),
        run(true),
        "entry order within an epoch must not change the stored state"
    );
    assert!(run(false).contains("left:"), "both indexes are rendered");
    assert!(run(false).contains("right:"));
}
