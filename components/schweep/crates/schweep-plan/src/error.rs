//! Errors from binding and from scalar evaluation.
//!
//! Two kinds live here and the distinction matters:
//!
//! - **Refusals** — the query is outside the dialect, or does not bind. Every one names the
//!   construct it refused (S-12). A refusal is a statement about the *query*, and it is raised
//!   before any data is touched, so a badly-typed query fails the same way on an empty database
//!   as on a full one.
//! - **Evaluation errors** — overflow, division by zero (S-20, S-21, S-22). A statement about
//!   the *data*, raised deterministically.
//!
//! Everything here is about *semantics*, which is why it lives with the shared plan rather than
//! with either implementation: the oracle and the engine must refuse the same queries by the same
//! names and fail on the same data with the same message, or I-1 would be comparing two dialects.
//! Errors about an implementation's own machinery — catalogs, epochs, malformed history — belong
//! to that implementation and stay there.

use schweep_zset::{DataType, ZSetError};

pub type Result<T> = std::result::Result<T, PlanError>;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PlanError {
    #[error(transparent)]
    ZSet(#[from] ZSetError),

    #[error("no table named {0:?}")]
    UnknownTable(String),

    // ---- binding: refusals, each naming its construct (S-12) ------------------------------
    #[error("{0} is not in the v1 dialect at rungs 1-3")]
    NotInDialect(&'static str),

    #[error("alias {0:?} is used more than once in one query")]
    DuplicateAlias(String),

    #[error(
        "column reference {0:?} is unqualified; before a GROUP BY every column is written \
         as alias.column (S-10)"
    )]
    UnqualifiedColumn(String),

    #[error(
        "column reference {0:?} is qualified, but after a GROUP BY the only columns are the \
         declared output names, referenced unqualified (S-10, S-27)"
    )]
    QualifiedColumnAfterGroupBy(String),

    #[error("no column named {name:?} in scope {scope}")]
    UnknownColumn { name: String, scope: String },

    #[error("output name {0:?} is declared more than once")]
    DuplicateOutputName(String),

    #[error(
        "untyped NULL literal: write a null with its type, so that binding never has to infer \
         one (S-19)"
    )]
    UntypedNullLiteral,

    #[error("operator {op} does not accept operands of type {left} and {right} (S-19)")]
    TypeMismatch {
        op: &'static str,
        left: DataType,
        right: DataType,
    },

    #[error("operator {op} does not accept an operand of type {found} (S-19)")]
    UnaryTypeMismatch { op: &'static str, found: DataType },

    #[error("{context} requires a Boolean expression but found {found} (S-17)")]
    ExpectedBoolean {
        context: &'static str,
        found: DataType,
    },

    #[error("CASE branches must all have one type: found {expected} and {found} (S-18)")]
    CaseBranchTypeMismatch { expected: DataType, found: DataType },

    #[error("CASE has no WHEN branches")]
    EmptyCase,

    #[error(
        "a GROUP BY declares neither keys nor aggregates, so it computes nothing; a grand total \
         needs at least one aggregate (S-33)"
    )]
    EmptyGroupKeys,

    #[error("a join with no key pairs is a cross join, which is not supported at rung 2 (S-26)")]
    CrossJoinNotSupported,

    #[error("aggregate {func} does not accept an argument of type {ty} (S-30)")]
    AggregateTypeUnsupported { func: &'static str, ty: DataType },

    // ---- refusals only SQL text can provoke (S-32, S-33, S-35) -----------------------------
    //
    // These five are unreachable through the typed API, because `Expr` has no aggregate variant:
    // the illegal query fails to type-check in Rust, which is a stronger guarantee than refusing
    // it at bind time. They live here anyway, with every other named refusal, because they are
    // statements about what a query *means* rather than about how its text was written — and
    // because `docs/SEMANTICS.md` names them in the rules they belong to, not in a parser appendix.
    #[error(
        "{func} is an aggregate, and HAVING is evaluated after aggregation: declare it as an \
         output column and reference it by name instead (S-32)"
    )]
    AggregateInHaving { func: String },

    #[error(
        "{func} is an aggregate, and WHERE is evaluated before aggregation, so there is nothing \
         to aggregate yet (S-9, S-32)"
    )]
    AggregateInWhere { func: String },

    #[error(
        "{outer} is an aggregate and cannot take the aggregate {inner} as its argument (S-32)"
    )]
    NestedAggregate { outer: String, inner: String },

    #[error(
        "{func} is an aggregate, so it must be the whole output column: an expression *over* an \
         aggregate is not in the v1 dialect (S-32)"
    )]
    AggregateNotTopLevel { func: String },

    #[error(
        "{name} is neither a grouping expression nor an aggregate, so it belongs to no group \
         (S-27, S-33)"
    )]
    ColumnNotGrouped { name: String },

    // ---- evaluation errors (S-20, S-21, S-22) ---------------------------------------------
    #[error("arithmetic overflow in {op} (S-20)")]
    ArithmeticOverflow { op: &'static str },

    #[error("division by zero in {op} (S-21)")]
    DivisionByZero { op: &'static str },

    #[error("{func} overflowed the Int64 range (S-30)")]
    AggregateOverflow { func: &'static str },
}

impl PlanError {
    /// True for errors that are facts about the **data** rather than about the query (S-22).
    ///
    /// These are the ones that become *live errors*: they are recorded, the offending row or group
    /// is dropped, and the answer is an error while the offending data is present. Everything else
    /// here is a statement about the query itself — a refusal, a type mismatch — and a query that
    /// does not bind never runs at all, so those propagate immediately and are not data-dependent.
    #[must_use]
    pub fn is_evaluation_error(&self) -> bool {
        matches!(
            self,
            PlanError::ArithmeticOverflow { .. }
                | PlanError::DivisionByZero { .. }
                | PlanError::AggregateOverflow { .. }
        )
    }
}
