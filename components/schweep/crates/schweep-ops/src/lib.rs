//! # schweep-ops — the circuit operators
//!
//! Each operator consumes one epoch's input deltas and produces one epoch's output delta
//! (`ARCHITECTURE.md` §5.3). C1 builds the linear ones; join arrives in C2, aggregates and
//! distinct in C3.
//!
//! | Operator | Kind | State | Sprint |
//! | --- | --- | --- | --- |
//! | [`Filter`] | linear | none | C1 |
//! | [`Project`] | linear | none | C1 |
//! | [`Join`] | bilinear | O(\|A\| + \|B\|) | C2 |
//! | [`Aggregate`] | stateful | per group, per aggregate slot | C3 |
//! | [`Distinct`] | stateful | the input's integral | C3 |
//!
//! ## The rule that governs every operator in this crate
//!
//! **Never special-case a negative weight** (I-5). A retraction flows through the same code path
//! as an insertion. The linear operators do not read a weight at all — filter carries it through,
//! project carries it through and lets consolidation add it up. The join *does* read weights, and
//! it still needs no special case: it **multiplies** them, and multiplication does not care about
//! sign. A retraction on one side times an insertion on the other is a negative output weight,
//! which is exactly the retraction of the joined row.
//!
//! ## Declared state, checked state
//!
//! Every operator declares a [`StateBound`] (I-9) and reports its actual [`Operator::state_size`].
//! The circuit compares them after every step, so a linear operator that quietly started
//! remembering something fails the run rather than passing it slowly. In C1 both declarations are
//! [`StateBound::Stateless`] and both reports are zero — which is §6 C1's pitfall turned into an
//! assertion instead of a warning.

pub mod aggregate;
pub mod distinct;
pub mod error;
pub mod join;
pub mod linear;
pub mod operator;

pub use aggregate::Aggregate;
pub use distinct::Distinct;
pub use error::{OpError, Result};
pub use join::Join;
pub use linear::{Filter, Project};
pub use operator::{error_row, error_schema, Operator, StateBound, StepOutput, ERROR_COLUMN};
