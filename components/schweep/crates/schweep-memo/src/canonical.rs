//! Canonicalization and structural hashing (`ARCHITECTURE.md` §5.7).
//!
//! Two plans that describe the same computation should hash the same, so the memo can share the
//! circuitry that computes it. That sentence hides a decision about how hard to try.
//!
//! ## v1 is conservative on purpose, and the cost is one-directional
//!
//! > the memo starts conservative (share only exact sub-tree matches; no cross-predicate cleverness
//! > in v1) — §5.7
//!
//! A canonicalization that is too *weak* costs sharing: two queries that could have shared a subtree
//! each build their own, the answers stay right, and the only symptom is more work and more memory. A
//! canonicalization that is too *strong* — one that declares two subtrees equivalent when they are not
//! — is a correctness catastrophe: one query reads another's answer. I-8 exists because that is the
//! failure mode of shared computation.
//!
//! Those two costs are not symmetric, so the rules here are the ones that are *obviously* safe, and
//! nothing else. `a = b` and `b = a` hash apart. That is a missed sharing opportunity, it is written
//! down as such below, and it will stay that way until someone has a reason better than "we could".
//!
//! ## The rules, in full
//!
//! | Rule | What is normalized | Why it is safe |
//! | --- | --- | --- |
//! | **Sorted join keys** | the key-pair list of every join, sorted by (left index, right index) | `ON a.x = b.x AND a.y = b.y` is a *conjunction*: the pairs are a set, and the join's output rows and schema do not depend on their order (S-26). Only the internal probe-key encoding does, and that is not observable. |
//!
//! That is the whole list. One rule, because one rule is what could be justified in a sentence.
//!
//! ## Deliberately **not** normalized, and what each costs
//!
//! | Not normalized | Sharing lost | Why not |
//! | --- | --- | --- |
//! | `a = b` vs `b = a`, `x AND y` vs `y AND x` | two queries whose predicates differ only in operand order | Commutativity of a *predicate* is not commutativity of its *evaluation*: `a / b = 0` and `0 = a / b` raise the same error today, but the reordering rule would have to be proven to preserve error identity (S-22c reports the *least* message) for every operator, not asserted. §5.7 says no cleverness; this is the cleverness it means. |
//! | source aliases (`FROM t AS a` vs `FROM t AS b`) | two queries over one table under different aliases | Renaming an alias means rewriting every column reference in the subtree above it, and the output schema carries the names (S-8). A rename that is wrong anywhere is a wrong answer, not lost sharing. |
//! | output column order | `SELECT a, n` vs `SELECT n, a` | These are *different answers* (S-8, S-36), not two spellings of one. Normalizing them would be a bug. |
//! | filters merged or reordered (`WHERE a` then `WHERE b`) | plans differing only in filter nesting | The incrementalizer emits one filter per clause of the query and performs no optimisation (§5.6). Merging them here would make the memo an optimiser, and C8 owns that with measurements. |
//!
//! ## The hash
//!
//! [`subtree_hash`] is FNV-1a over the canonical node's s-expression rendering — `CircuitPlan`'s own
//! `structural_hash`, applied to the canonical form. It is deliberately not `std::hash::Hash`, whose
//! output is explicitly unstable across releases; a memo keyed on an unstable hash would silently stop
//! sharing when the toolchain moved, which is exactly the failure this module's tests exist to catch.

use schweep_sql::{CircuitNode, CircuitPlan};

/// The canonical form of a plan: the same computation, written the one way the memo recognises.
#[must_use]
pub fn canonicalize(plan: &CircuitPlan) -> CircuitPlan {
    CircuitPlan {
        root: canonical_node(&plan.root),
        output_schema: plan.output_schema.clone(),
    }
}

/// The canonical form of one node and everything beneath it.
#[must_use]
pub fn canonical_node(node: &CircuitNode) -> CircuitNode {
    match node {
        CircuitNode::Source { .. } => node.clone(),
        CircuitNode::Filter {
            input,
            naming,
            predicate,
        } => CircuitNode::Filter {
            input: Box::new(canonical_node(input)),
            naming: *naming,
            predicate: predicate.clone(),
        },
        CircuitNode::Project {
            input,
            naming,
            items,
            schema,
        } => CircuitNode::Project {
            input: Box::new(canonical_node(input)),
            naming: *naming,
            items: items.clone(),
            schema: schema.clone(),
        },
        CircuitNode::Join {
            left,
            right,
            keys,
            schema,
        } => {
            // The one rule. `sort` rather than `sort_unstable` so that duplicate pairs — which a
            // query could legitimately write as `ON a.x = b.x AND a.x = b.x` — keep a fixed order
            // instead of one the sort happened to pick.
            let mut keys = keys.clone();
            keys.sort();
            CircuitNode::Join {
                left: Box::new(canonical_node(left)),
                right: Box::new(canonical_node(right)),
                keys,
                schema: schema.clone(),
            }
        }
        CircuitNode::Aggregate {
            input,
            keys,
            aggregates,
            schema,
        } => CircuitNode::Aggregate {
            input: Box::new(canonical_node(input)),
            keys: keys.clone(),
            aggregates: aggregates.clone(),
            schema: schema.clone(),
        },
        CircuitNode::Distinct { input } => CircuitNode::Distinct {
            input: Box::new(canonical_node(input)),
        },
    }
}

/// The hash the memo keys sharing on: the canonical subtree's structural hash.
///
/// Takes a node that is **already canonical**. Canonicalizing here instead would hide a caller that
/// forgot to, and the symptom would be sharing that quietly stops happening.
#[must_use]
pub fn subtree_hash(canonical: &CircuitNode) -> u64 {
    canonical.structural_hash()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use schweep_plan::bind::Catalog;
    use schweep_zset::{DataType, Field, Schema};

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

    fn hash_of(sql: &str) -> u64 {
        let plan = schweep_sql::compile(sql, &catalog()).expect(sql);
        subtree_hash(&canonicalize(&plan).root)
    }

    /// The baseline: the same query twice is one hash. If this ever fails, nothing shares.
    #[test]
    fn the_same_query_hashes_the_same() {
        assert_eq!(
            hash_of("SELECT t.n AS n FROM t WHERE t.k > 1"),
            hash_of("SELECT t.n AS n FROM t WHERE t.k > 1")
        );
    }

    /// **The rule, asserted as a hash hit.** Reordered join keys share.
    ///
    /// This is the test §6 C6's pitfall note asks for: a canonicalization rule that stopped firing
    /// would cost sharing silently, because every answer would stay right.
    #[test]
    fn reordered_join_keys_are_one_hash() {
        let a = hash_of("SELECT t.n AS n, u.m AS m FROM t JOIN u ON t.id = u.id AND t.k = u.k");
        let b = hash_of("SELECT t.n AS n, u.m AS m FROM t JOIN u ON t.k = u.k AND t.id = u.id");
        assert_eq!(
            a, b,
            "the key list is a conjunction, so its order is not part of the query"
        );
    }

    /// Sorting is idempotent and does not disturb a plan that is already canonical.
    #[test]
    fn canonicalizing_twice_changes_nothing() {
        let plan = schweep_sql::compile(
            "SELECT t.n AS n, u.m AS m FROM t JOIN u ON t.k = u.k AND t.id = u.id",
            &catalog(),
        )
        .unwrap();
        let once = canonicalize(&plan);
        let twice = canonicalize(&once);
        assert_eq!(once.structural_form(), twice.structural_form());
    }

    /// **The subtree property**, which is what makes partial sharing possible: two queries with a
    /// common prefix and different suffixes agree on the prefix's hash and disagree on the root's.
    #[test]
    fn a_common_prefix_hashes_equal_while_the_roots_differ() {
        let a = schweep_sql::compile("SELECT t.n AS n FROM t WHERE t.k > 1", &catalog()).unwrap();
        let b = schweep_sql::compile("SELECT DISTINCT t.n AS n FROM t WHERE t.k > 1", &catalog())
            .unwrap();
        let (a, b) = (canonicalize(&a), canonicalize(&b));

        assert_ne!(
            subtree_hash(&a.root),
            subtree_hash(&b.root),
            "one has a DISTINCT and the other does not"
        );

        let a_nodes = a.root.nodes();
        let b_nodes = b.root.nodes();
        let shared: Vec<u64> = a_nodes
            .iter()
            .map(|n| subtree_hash(n))
            .filter(|h| b_nodes.iter().any(|other| subtree_hash(other) == *h))
            .collect();
        assert_eq!(
            shared.len(),
            3,
            "source, filter and projection are common to both; only the DISTINCT is novel"
        );
    }

    /// Two genuinely different queries must not collide. A hash that ignored a field would.
    #[test]
    fn different_queries_hash_apart() {
        let hashes = [
            hash_of("SELECT t.n AS n FROM t WHERE t.k > 1"),
            hash_of("SELECT t.n AS n FROM t WHERE t.k > 2"),
            hash_of("SELECT t.n AS n FROM t WHERE t.k < 1"),
            hash_of("SELECT t.n AS x FROM t WHERE t.k > 1"),
            hash_of("SELECT t.k AS n FROM t WHERE t.k > 1"),
            hash_of("SELECT t.n AS n FROM t"),
            hash_of("SELECT DISTINCT t.n AS n FROM t"),
            hash_of("SELECT t.k AS k, COUNT(*) AS c FROM t GROUP BY t.k"),
            hash_of("SELECT t.k AS k, SUM(t.n) AS c FROM t GROUP BY t.k"),
            hash_of("SELECT t.n AS n, u.m AS m FROM t JOIN u ON t.id = u.id"),
            hash_of("SELECT t.n AS n, u.m AS m FROM t JOIN u ON t.k = u.k"),
        ];
        let mut seen = hashes.to_vec();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(
            seen.len(),
            hashes.len(),
            "every one of these is a different question and must hash apart"
        );
    }

    /// The documented cost, asserted so that it stays a *decision* rather than becoming folklore: an
    /// operand swap is not normalized, so it does not share.
    #[test]
    fn a_swapped_comparison_does_not_share_and_that_is_the_recorded_cost() {
        assert_ne!(
            hash_of("SELECT t.n AS n FROM t WHERE t.k = 1"),
            hash_of("SELECT t.n AS n FROM t WHERE 1 = t.k"),
            "v1 does not normalize operand order; this costs sharing and never costs correctness"
        );
    }
}
