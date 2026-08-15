//! Property tests for the Z-set algebra (`ARCHITECTURE.md` §5.2, §7).
//!
//! The four properties §5.2 names are `addition_is_commutative`, `addition_is_associative`,
//! `consolidate_is_idempotent`, and `double_negation_is_identity`. The rest of this file pins
//! the algebraic facts the engine will lean on from C1 onward — above all
//! `a_plus_negative_a_is_empty`, which is invariant I-5 stated as arithmetic.
//!
//! **On "up to canonical form".** `add` is multiset union: it concatenates entries rather than
//! merging them, so `a.add(b)` and `b.add(a)` hold the same entries in a different physical
//! order. A Z-set *is* its canonical form (S-8) — consolidated, zero weights dropped, sorted —
//! so equality of Z-sets is equality of canonical forms, and that is what these properties
//! assert. Where the stronger byte-level statement is also true, it is asserted as well
//! (see `commutativity_is_byte_identical_after_consolidate`), so that "up to canonical form"
//! can never quietly become a way of excusing a real difference.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use proptest::prelude::*;
use schweep_zset::{DataType, Field, Row, Schema, Value, ZSetBatch};

/// A fixed three-column schema: an integer, a string, and a boolean, all nullable, so that
/// generated data exercises null ordering (S-7) and every Arrow array kind this crate builds.
fn test_schema() -> Schema {
    Schema::new(vec![
        Field::nullable("k", DataType::Int64),
        Field::nullable("s", DataType::Utf8),
        Field::nullable("b", DataType::Boolean),
    ])
    .unwrap()
}

/// Small value domains on purpose: collisions between generated rows are what make
/// consolidation do any work at all. A wide domain would produce Z-sets of distinct rows, and
/// every one of these properties would pass vacuously.
fn any_row() -> impl Strategy<Value = Row> {
    (
        prop_oneof![Just(Value::Null), (-3_i64..4).prop_map(Value::Int)],
        prop_oneof![
            Just(Value::Null),
            prop::sample::select(vec!["", "a", "b"]).prop_map(|s| Value::Str(s.to_owned()))
        ],
        prop_oneof![Just(Value::Null), any::<bool>().prop_map(Value::Bool)],
    )
        .prop_map(|(k, s, b)| Row::new(vec![k, s, b]))
}

/// Weights are bounded so that no sum in these tests can approach `i64` overflow: at most 24
/// entries of magnitude ≤ 1000 per Z-set, and at most three Z-sets summed.
fn any_zset() -> impl Strategy<Value = ZSetBatch> {
    prop::collection::vec((any_row(), -1000_i64..1001), 0..24)
        .prop_map(|entries| ZSetBatch::from_entries(test_schema(), entries).unwrap())
}

proptest! {
    // §5.2: "Z-set addition is commutative".
    #[test]
    fn addition_is_commutative(a in any_zset(), b in any_zset()) {
        prop_assert_eq!(
            a.add(&b).unwrap().canonical().unwrap(),
            b.add(&a).unwrap().canonical().unwrap()
        );
    }

    // The same statement at the byte level, so that "up to canonical form" above is a
    // description of `add`'s layout and not a weakened claim about the algebra.
    #[test]
    fn commutativity_is_byte_identical_after_consolidate(a in any_zset(), b in any_zset()) {
        let ab = a.add(&b).unwrap().consolidate().unwrap().canonical().unwrap();
        let ba = b.add(&a).unwrap().consolidate().unwrap().canonical().unwrap();
        prop_assert_eq!(ab.render(), ba.render());
    }

    // §5.2: "Z-set addition is associative".
    #[test]
    fn addition_is_associative(a in any_zset(), b in any_zset(), c in any_zset()) {
        let left = a.add(&b).unwrap().add(&c).unwrap();
        let right = a.add(&b.add(&c).unwrap()).unwrap();
        prop_assert_eq!(left.canonical().unwrap(), right.canonical().unwrap());
    }

    // §5.2: "consolidate is idempotent".
    #[test]
    fn consolidate_is_idempotent(a in any_zset()) {
        let once = a.consolidate().unwrap();
        let twice = once.consolidate().unwrap();
        prop_assert_eq!(once.canonical().unwrap(), twice.canonical().unwrap());
        // Idempotence at the byte level too: consolidating a consolidated batch changes nothing.
        prop_assert_eq!(once.entries().unwrap(), twice.entries().unwrap());
    }

    // §5.2: "negate ∘ negate = identity".
    #[test]
    fn double_negation_is_identity(a in any_zset()) {
        let back = a.negate().unwrap().negate().unwrap();
        prop_assert_eq!(a.entries().unwrap(), back.entries().unwrap());
    }

    // I-5, as arithmetic: a retraction of everything cancels everything, by the same code path
    // that added it. If this ever fails, deletion has grown a special case.
    #[test]
    fn a_plus_negative_a_is_empty(a in any_zset()) {
        let sum = a.add(&a.negate().unwrap()).unwrap();
        prop_assert!(sum.canonical().unwrap().is_empty());
    }

    // The empty Z-set is the additive identity.
    #[test]
    fn empty_is_the_additive_identity(a in any_zset()) {
        let zero = ZSetBatch::empty(test_schema()).unwrap();
        prop_assert_eq!(
            a.add(&zero).unwrap().canonical().unwrap(),
            a.canonical().unwrap()
        );
        prop_assert_eq!(
            zero.add(&a).unwrap().canonical().unwrap(),
            a.canonical().unwrap()
        );
    }

    // Negation distributes over addition: -(a + b) = (-a) + (-b).
    #[test]
    fn negation_distributes_over_addition(a in any_zset(), b in any_zset()) {
        let left = a.add(&b).unwrap().negate().unwrap();
        let right = a.negate().unwrap().add(&b.negate().unwrap()).unwrap();
        prop_assert_eq!(left.canonical().unwrap(), right.canonical().unwrap());
    }

    // Canonical form is what S-8 says it is: no duplicate rows, no zero weights, sorted
    // ascending by all columns in schema order.
    #[test]
    fn canonical_form_is_sorted_deduplicated_and_zero_free(a in any_zset()) {
        let c = a.canonical().unwrap();
        for window in c.entries().windows(2) {
            let (prev, next) = (window.first().unwrap(), window.get(1).unwrap());
            prop_assert!(prev.0 < next.0, "canonical entries must be strictly ascending");
        }
        prop_assert!(c.entries().iter().all(|(_, w)| *w != 0));
    }

    // Canonical form depends on the data, not on the order the entries arrived in (I-2).
    #[test]
    fn canonical_form_is_invariant_under_permutation(
        entries in prop::collection::vec((any_row(), -1000_i64..1001), 0..24),
        rotation in 0_usize..24,
    ) {
        let straight = ZSetBatch::from_entries(test_schema(), entries.clone()).unwrap();
        let mut rotated = entries;
        let n = rotated.len();
        if n > 0 {
            rotated.rotate_left(rotation % n);
        }
        let rotated = ZSetBatch::from_entries(test_schema(), rotated).unwrap();
        prop_assert_eq!(
            straight.canonical().unwrap().render(),
            rotated.canonical().unwrap().render()
        );
    }

    // Consolidation preserves the total weight of the Z-set: it merges and cancels, it never
    // invents or loses quantity.
    #[test]
    fn consolidate_preserves_total_weight(a in any_zset()) {
        let before: i64 = a.entries().unwrap().iter().map(|(_, w)| *w).sum();
        let after: i64 = a.consolidate().unwrap().entries().unwrap().iter().map(|(_, w)| *w).sum();
        prop_assert_eq!(before, after);
    }

    // The Arrow representation and the row view never disagree: what goes in comes out, in
    // order, with its weight (D-2).
    #[test]
    fn arrow_round_trip_preserves_entries(
        entries in prop::collection::vec((any_row(), -1000_i64..1001), 0..24)
    ) {
        let z = ZSetBatch::from_entries(test_schema(), entries.clone()).unwrap();
        prop_assert_eq!(z.entries().unwrap(), entries);
        prop_assert_eq!(z.record_batch().num_rows(), z.weights().len());
    }

    // Re-adopting a Z-set's own Arrow batch and weights reconstructs the same Z-set: the
    // zero-copy door (`from_arrow`) agrees with the validating door (`from_entries`).
    #[test]
    fn from_arrow_agrees_with_from_entries(a in any_zset()) {
        let rebuilt = ZSetBatch::from_arrow(
            a.schema().clone(),
            a.record_batch().clone(),
            a.weights().clone(),
        )
        .unwrap();
        prop_assert_eq!(a.entries().unwrap(), rebuilt.entries().unwrap());
    }
}
