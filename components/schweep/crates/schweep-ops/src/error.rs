//! Errors from operators.

use schweep_plan::PlanError;
use schweep_state::StateError;
use schweep_zset::{DataType, ZSetError};

pub type Result<T> = std::result::Result<T, OpError>;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum OpError {
    /// A binding refusal or an evaluation error — the same semantics the oracle raises (D-14).
    #[error(transparent)]
    Plan(#[from] PlanError),

    #[error(transparent)]
    ZSet(#[from] ZSetError),

    #[error("operator {op} takes {expected} input(s) but was given {found}")]
    Arity {
        op: &'static str,
        expected: usize,
        found: usize,
    },

    #[error(
        "operator {op} was given a delta with schema {found}, but its input schema is {expected}"
    )]
    InputSchemaMismatch {
        op: &'static str,
        expected: String,
        found: String,
    },

    #[error("{op} requires a Boolean predicate but the expression has type {found} (S-17)")]
    PredicateNotBoolean { op: &'static str, found: DataType },

    #[error(transparent)]
    State(#[from] StateError),

    #[error("join key names column {index} on the {side} side, which has no such column")]
    JoinKeyOutOfRange { side: &'static str, index: usize },

    #[error("join produced a weight outside the Int64 range")]
    JoinWeightOverflow,

    /// An index key shorter than its declared join key. Unreachable — every key this operator
    /// writes is built as `[key values…, row values…]` — and reported rather than assumed, because
    /// an operator that assumes its own state is well-formed cannot say when it is not.
    #[error("internal: a join index key is shorter than its join key")]
    CorruptJoinIndex,

    /// An aggregate state key with no value component. Unreachable — every multiset key this
    /// operator writes ends in the argument's value — and reported rather than assumed.
    #[error("internal: an aggregate state key is missing its value component")]
    CorruptAggregateState,
}
