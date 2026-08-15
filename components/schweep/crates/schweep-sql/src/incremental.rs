//! # The incrementalizer
//!
//! Logical plan in, circuit plan out. This is the intellectual heart of the engine
//! (`ARCHITECTURE.md` §5.6), and this file is where the reason each rewrite is *correct* is written
//! down, rule by rule.
//!
//! ## The problem
//!
//! A query is written as a function of a whole table:
//!
//! ```text
//! Q(T) = π( σ( T ) )
//! ```
//!
//! A standing computation must instead be a function of a *change*. Given the answer at epoch `n`
//! and the deltas sealed in epoch `n+1`, it must produce the change to the answer without looking at
//! `T` again — that is the entire pitch, `O(change)` instead of `O(data)`.
//!
//! Write `∫` for accumulation (the sum of every delta so far, so `∫ΔT = T`) and `δ` for
//! differentiation (the change from one epoch to the next). Incrementalizing `Q` means finding `Q^Δ`
//! with:
//!
//! ```text
//! Q^Δ(ΔT) = δ( Q( ∫ΔT ) )
//! ```
//!
//! The whole subject is: for which shapes of `Q` can `Q^Δ` be computed **cheaply**, and what state
//! must be kept when it cannot. There are exactly three answers in this dialect, and every node
//! carries which one applies to it as a [`Rule`], so the claim is data rather than commentary.
//!
//! ## Rule 1 — linear operators need no rewrite at all
//!
//! An operator `f` is **linear** over Z-sets when
//!
//! ```text
//! f(a + b) = f(a) + f(b)      and      f(-a) = -f(a)
//! ```
//!
//! For such an `f`, `δ(f(∫Δ)) = f(Δ)`: applying `f` directly to the delta gives the delta of the
//! answer. The rewrite is the identity, and the operator keeps **no state**.
//!
//! `WHERE` is linear because a predicate decides row by row: filtering the union of two Z-sets is the
//! union of the filtered ones, and filtering a retraction retracts the filtered row (S-24). Projection
//! is linear because it maps rows and *adds the weights* of rows that collide (S-25) — addition is
//! what makes it survive `+`, and it is why a projection that merged by "first one wins" instead
//! would break this rule and, with it, retraction.
//!
//! **The trap this rule sets.** Linearity is a property of the *operator*, not of the syntax. A
//! predicate whose truth depends on data outside its own row — a correlated subquery, an aggregate —
//! is not linear, which is exactly why `WHERE COUNT(*) > 1` is refused rather than compiled
//! (`AggregateInWhere`, S-32). The dialect's shape is not arbitrary; it is the set of operators this
//! rule and the next two cover.
//!
//! ## Rule 2 — bilinear operators expand to three terms
//!
//! An operator of two arguments is **bilinear** when it is linear in each argument separately. `⋈` is:
//! joining `(a + b)` on the left against a fixed right is the union of the two joins, and weights
//! multiply (S-26). For a bilinear `⊗`,
//!
//! ```text
//! δ(A ⊗ B) = ΔA ⊗ B + A ⊗ ΔB + ΔA ⊗ ΔB
//! ```
//!
//! and all three terms are needed. The third is the one that is always forgotten: it accounts for
//! rows that arrive on *both* sides in the *same* epoch, which no other term sees. The gate has a
//! scenario that isolates it, and dropping the term is one of the canonical mutations (C2).
//!
//! `A` and `B` here are the **pre-epoch** integrals — the accumulated contents *before* this epoch's
//! deltas were applied. So the join keeps one integral per side, probes both against the pre-epoch
//! state, and only then integrates. Probing after integrating would double-count the delta-delta
//! term, and the resulting bug would look like "weights slightly too large sometimes".
//!
//! State is therefore proportional to both inputs — `O(|A| + |B|)`, declared as such under I-9 — and
//! that is not an implementation shortcoming to be optimised away later. It is what an equi-join
//! *is*: to know what a new left row matches, you must remember the right rows.
//!
//! ## Rule 3 — everything else keeps what it needs and recomputes the part that moved
//!
//! `SUM` is linear over its input, but `MIN` is not, `AVG` is not, and `COUNT` interacts with group
//! existence (S-29), so aggregation as a whole is not. There is no algebraic shortcut: the value of
//! `MIN(x)` for a group is a function of the group's whole contents, and a retraction of the current
//! minimum can only be answered by knowing what else is in the group.
//!
//! So the rewrite is: keep the group contents, note which groups a delta **touched**, recompute
//! exactly those groups, and emit the difference — a retraction of the old row and an insertion of
//! the new one. Cost is proportional to the *changed* groups, not to the table, which is the property
//! that matters; state is proportional to the input, declared under I-9 (S-30, and the ordered
//! multiset per (slot, group) that lets MIN/MAX survive retraction).
//!
//! `DISTINCT` is the same shape one row at a time: presence is a step function of accumulated weight
//! (S-34), so the operator keeps the accumulated weight per row and emits only where presence
//! flipped. Non-monotone in the same way, stateful for the same reason.
//!
//! ## What this file does *not* do, stated plainly
//!
//! It does not perform a general `δ`/`∫` algebraic rewrite over an operator algebra. Each logical
//! operator in this dialect has exactly **one** incremental implementation, already written in
//! `schweep-ops`, and the incrementalizer's job is to choose it, resolve names to positions, and
//! record the rule that justifies the choice.
//!
//! That is a smaller job than the DBSP papers describe, and the difference is worth being honest
//! about: a general rewriter earns its keep when the operator set is open, when the same logical
//! operator has several incremental forms to choose between, or when nested time domains (recursion,
//! iteration) require lifting the whole circuit. v1 has none of those — recursion is a non-goal
//! (D-3), and there is one form per operator. Writing the general machinery now would be building a
//! rewrite engine to fire six rules, and each of the six would then be tested through a layer of
//! indirection instead of directly.
//!
//! It also performs **no optimisation**: no predicate pushdown, no join reordering, no common
//! subexpression elimination. The plan's shape follows the query's shape (with S-36's one exception,
//! which removes a node rather than moving one). C6's memo shares identical sub-plans between
//! queries; C8 is where measured, ledgered tuning belongs. An optimiser that changed the plan today
//! would change what I-6 compares and what the differential harness covers, for a benefit nobody has
//! measured — which is exactly the order this project refuses to work in (I-10).

use schweep_plan::bind::{Bound, Catalog, Naming};
use schweep_plan::plan::{Query, Source};
use schweep_zset::Schema;

use crate::circuit_plan::{CircuitNode, CircuitPlan};
use crate::error::{Result, SqlError};
use crate::select::BoundQuery;

/// Incrementalize a bound query into a circuit plan.
///
/// The query must already be bound: every name resolved, every type checked, every construct inside
/// the dialect (S-12). This function assumes that and does not re-check it — which is why it takes a
/// [`BoundQuery`] rather than a `Query`, so the assumption is in the type rather than in a comment.
pub fn incrementalize(bound_query: &BoundQuery, catalog: &Catalog) -> Result<CircuitPlan> {
    let BoundQuery { query, bound } = bound_query;
    plan_of(query, bound, catalog)
}

/// Incrementalize a query the typed API built, binding it first.
///
/// The **same** function the SQL door reaches, called with a `Query` that came from Rust rather than
/// from text. That is what makes I-6 a property of the code rather than a promise: there is one
/// incrementalizer, and neither door has a private path into it.
pub fn incrementalize_typed(query: &Query, catalog: &Catalog) -> Result<CircuitPlan> {
    let bound = schweep_plan::bind(query, catalog)?;
    plan_of(query, &bound, catalog)
}

fn plan_of(query: &Query, bound: &Bound, catalog: &Catalog) -> Result<CircuitPlan> {
    // The source tree first: scans and joins, bottom-up (rules 1 and 2).
    let mut node = source_node(&query.source, catalog)?;

    // `WHERE` — rule 1, linear, no state. It sees the source's qualified columns (S-10).
    if let Some(predicate) = &query.filter {
        node = CircuitNode::Filter {
            input: Box::new(node),
            naming: Naming::Qualified,
            predicate: predicate.clone(),
        };
    }

    // `GROUP BY` — rule 3, stateful per group. Grouping erases the input schema (S-27), so
    // everything after it is named unqualified.
    if let Some(group_by) = &query.group_by {
        node = CircuitNode::Aggregate {
            input: Box::new(node),
            keys: group_by.keys.clone(),
            aggregates: group_by.aggregates.clone(),
            schema: bound.grouped_schema.clone(),
        };

        // `HAVING` is a filter over the group output, so it needs no operator of its own (S-32).
        // That is not a shortcut: `HAVING` genuinely *is* `WHERE` at a different point in the
        // pipeline, and giving it its own operator would be two implementations of one rule.
        if let Some(having) = &group_by.having {
            node = CircuitNode::Filter {
                input: Box::new(node),
                naming: Naming::Unqualified,
                predicate: having.clone(),
            };
        }
    }

    // The projection — rule 1 again. Its naming depends on whether a GROUP BY came before it.
    if let Some(items) = &query.project {
        let naming = if query.group_by.is_some() {
            Naming::Unqualified
        } else {
            Naming::Qualified
        };
        node = CircuitNode::Project {
            input: Box::new(node),
            naming,
            items: items.clone(),
            schema: bound.output_schema.clone(),
        };
    }

    // `DISTINCT` last of all (S-34) — rule 3, stateful per row.
    if query.distinct {
        node = CircuitNode::Distinct {
            input: Box::new(node),
        };
    }

    // The plan's answer schema and the binder's must agree. They cannot disagree by construction —
    // both come from `bound` — so this checks the *wiring*: a stage dropped above would show up here
    // as a schema mismatch rather than later as an unexplained I-1 divergence.
    if node.schema() != &bound.output_schema {
        return Err(SqlError::PlanWiringMismatch {
            emitted: node.schema().to_string(),
            expected: bound.output_schema.to_string(),
        });
    }

    Ok(CircuitPlan {
        root: node,
        output_schema: bound.output_schema.clone(),
    })
}

/// The source tree: a scan is an input, a join is rule 2.
///
/// Recursive, so a join of joins needs no special case. Each scan is keyed by its **alias** rather
/// than by its table, which is what makes a self-join representable: two nodes over one table, each
/// receiving the same deltas.
fn source_node(source: &Source, catalog: &Catalog) -> Result<CircuitNode> {
    match source {
        Source::Scan { table, alias } => Ok(CircuitNode::Source {
            table: table.clone(),
            alias: alias.clone(),
            schema: schweep_plan::bind_source(source, catalog)?,
        }),
        Source::Join { left, right, on } => {
            let left_node = source_node(left, catalog)?;
            let right_node = source_node(right, catalog)?;
            let left_schema = left_node.schema().clone();
            let right_schema = right_node.schema().clone();

            // Names become positions here, once, so the operator never looks a name up per row. The
            // binder has already proved these names resolve and that the two types match (S-19,
            // S-26), so a failure here is a wiring bug, not a user error.
            let mut keys = Vec::with_capacity(on.len());
            for (left_name, right_name) in on {
                keys.push((
                    index_of(&left_schema, left_name)?,
                    index_of(&right_schema, right_name)?,
                ));
            }

            let mut fields = left_schema.fields().to_vec();
            fields.extend(right_schema.fields().iter().cloned());
            let schema = Schema::new(fields).map_err(schweep_plan::PlanError::from)?;

            Ok(CircuitNode::Join {
                left: Box::new(left_node),
                right: Box::new(right_node),
                keys,
                schema,
            })
        }
    }
}

fn index_of(schema: &Schema, name: &str) -> Result<usize> {
    schema.index_of(name).ok_or_else(|| {
        SqlError::Plan(schweep_plan::PlanError::UnknownColumn {
            name: name.to_owned(),
            scope: schema.to_string(),
        })
    })
}
