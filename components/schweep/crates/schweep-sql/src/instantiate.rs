//! Circuit plan → running circuit (`ARCHITECTURE.md` §5.6, §5.7).
//!
//! The plan says what to build; this builds it. Nothing here makes a decision: every operator's
//! shape, every schema, and every resolved column position was fixed by the incrementalizer, so this
//! module is a constructor and reads like one.
//!
//! Keeping it separate from the incrementalizer is what makes I-6's comparison cheap and what makes
//! C6's memo possible: two standing queries can be compared, and shared, without either of them
//! having allocated a single byte of operator state.
//!
//! **State backends are allocated here, one per stateful operator.** Today every one is a
//! `MemBackend`; C8 swaps in the durable backend behind the frozen `StateBackend` trait (D-18, D-19)
//! and nothing above this line changes — which is the entire point of the trait, and the reason this
//! file is the only place in the SQL crate that names a backend at all.

use schweep_circuit::{Circuit, CircuitBuilder, NodeId};
use schweep_ops::Operator;
use schweep_ops::{Aggregate, Distinct, Filter, Join, Project};
use schweep_state::{BackendFactory, MemFactory};

use crate::circuit_plan::{CircuitNode, CircuitPlan};
use crate::error::{Result, SqlError};

/// Build a runnable circuit from a plan, with operator state in memory.
pub fn instantiate(plan: &CircuitPlan) -> Result<Circuit> {
    instantiate_with(plan, &mut MemFactory::new())
}

/// Build a runnable circuit from a plan, with operator state from `factory` (C8).
///
/// The backend choice enters here and nowhere else. Operators never name a store — see
/// `schweep_state::factory` for why that is the arrangement the backend-invariance gate needs.
pub fn instantiate_with(plan: &CircuitPlan, factory: &mut dyn BackendFactory) -> Result<Circuit> {
    let mut builder = CircuitBuilder::new();
    let root = add(&mut builder, &plan.root, factory)?;
    let circuit = builder.build(root).map_err(circuit_error)?;

    // The same wiring check the incrementalizer makes, at the other end of the pipe. Cheap, and it
    // fails loudly at build time rather than quietly at answer time.
    let emitted = circuit.output_schema().map_err(circuit_error)?;
    if emitted != &plan.output_schema {
        return Err(SqlError::PlanWiringMismatch {
            emitted: emitted.to_string(),
            expected: plan.output_schema.to_string(),
        });
    }
    Ok(circuit)
}

fn add(
    builder: &mut CircuitBuilder,
    node: &CircuitNode,
    factory: &mut dyn BackendFactory,
) -> Result<NodeId> {
    match node {
        CircuitNode::Source {
            table,
            alias,
            schema,
        } => builder
            .source(table.clone(), alias.clone(), schema.clone())
            .map_err(circuit_error),
        _ => {
            let mut inputs = Vec::with_capacity(2);
            for child in children(node) {
                inputs.push(add(builder, child, factory)?);
            }
            // The label a backend is handed out under: the node it belongs to, and what it is. Unique
            // within a circuit because node indices are, and readable because `EXPLAIN STATE` prints it.
            let label = format!("n{}-{}", builder.node_count(), node_kind(node));
            let op =
                operator_for_with(node, &label, factory)?.ok_or(SqlError::PlanWiringMismatch {
                    emitted: "a source with children".to_owned(),
                    expected: "an operator".to_owned(),
                })?;
            builder.add(op, inputs).map_err(circuit_error)
        }
    }
}

/// The word that goes in a backend label.
fn node_kind(node: &CircuitNode) -> &'static str {
    match node {
        CircuitNode::Source { .. } => "source",
        CircuitNode::Filter { .. } => "filter",
        CircuitNode::Project { .. } => "project",
        CircuitNode::Join { .. } => "join",
        CircuitNode::Aggregate { .. } => "aggregate",
        CircuitNode::Distinct { .. } => "distinct",
    }
}

/// The nodes feeding this one, left to right.
///
/// Public because the memo walks a plan node by node — attaching some nodes and reusing others — and
/// must agree with this crate about what a node's inputs *are*. Two answers to that question would be
/// two wirings.
#[must_use]
pub fn children(node: &CircuitNode) -> Vec<&CircuitNode> {
    match node {
        CircuitNode::Source { .. } => Vec::new(),
        CircuitNode::Filter { input, .. }
        | CircuitNode::Project { input, .. }
        | CircuitNode::Aggregate { input, .. }
        | CircuitNode::Distinct { input } => vec![input.as_ref()],
        CircuitNode::Join { left, right, .. } => vec![left.as_ref(), right.as_ref()],
    }
}

/// The operator one plan node describes, or `None` if the node is a source.
///
/// **The single place an operator is constructed from a plan.** `instantiate` walks a plan into a
/// fresh circuit; C6's memo walks one into a live shared dataflow, attaching only the nodes it does not
/// already have. Both call this. A second constructor would be a second set of decisions about which
/// operator a node means — and I-8 asks whether shared and unshared execution agree, a question that
/// is only meaningful if there is one answer to that.
pub fn operator_for(node: &CircuitNode) -> Result<Option<Box<dyn Operator>>> {
    operator_for_with(node, "unlabelled", &mut MemFactory::new())
}

/// The operator one plan node describes, with its state from `factory` (C8).
pub fn operator_for_with(
    node: &CircuitNode,
    label: &str,
    factory: &mut dyn BackendFactory,
) -> Result<Option<Box<dyn Operator>>> {
    Ok(match node {
        CircuitNode::Source { .. } => None,

        CircuitNode::Filter {
            input,
            naming,
            predicate,
        } => Some(Box::new(
            Filter::new(input.schema().clone(), *naming, predicate.clone()).map_err(ops_error)?,
        )),

        CircuitNode::Project {
            input,
            naming,
            items,
            schema: _,
        } => Some(Box::new(
            Project::new(input.schema().clone(), *naming, items.clone()).map_err(ops_error)?,
        )),

        CircuitNode::Join {
            left,
            right,
            keys,
            schema: _,
        } => Some(Box::new(
            // One backend per side (§6 C2): the join keeps an integral of each input, because that
            // is what rule 2 requires — see `incremental.rs`.
            Join::new(
                left.schema().clone(),
                right.schema().clone(),
                keys.clone(),
                factory
                    .create(&format!("{label}-left"))
                    .map_err(state_error)?,
                factory
                    .create(&format!("{label}-right"))
                    .map_err(state_error)?,
            )
            .map_err(ops_error)?,
        )),

        CircuitNode::Aggregate {
            input,
            keys,
            aggregates,
            schema,
        } => Some(Box::new(
            Aggregate::new(
                input.schema().clone(),
                schema.clone(),
                keys.clone(),
                aggregates.clone(),
                factory.create(label).map_err(state_error)?,
            )
            .map_err(ops_error)?,
        )),

        CircuitNode::Distinct { input } => Some(Box::new(Distinct::new(
            input.schema().clone(),
            factory.create(label).map_err(state_error)?,
        ))),
    })
}

/// A circuit-construction failure is a wiring bug, and it is reported as one rather than dressed up
/// as a refusal: the query bound, so nothing the user wrote is at fault.
pub fn circuit_error(error: schweep_circuit::CircuitError) -> SqlError {
    SqlError::PlanWiringMismatch {
        emitted: error.to_string(),
        expected: "a circuit the plan describes".to_owned(),
    }
}

/// A backend that could not be opened is a *wiring* failure, not a refusal: the query bound, so
/// nothing the user wrote is at fault.
pub fn state_error(error: schweep_state::StateError) -> SqlError {
    SqlError::PlanWiringMismatch {
        emitted: error.to_string(),
        expected: "a state backend the plan's operators can use".to_owned(),
    }
}

pub fn ops_error(error: schweep_ops::OpError) -> SqlError {
    SqlError::PlanWiringMismatch {
        emitted: error.to_string(),
        expected: "an operator the plan describes".to_owned(),
    }
}
