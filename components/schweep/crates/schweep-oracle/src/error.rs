//! Errors from the oracle's own machinery.
//!
//! Everything about *semantics* — binding, refusals, three-valued evaluation, checked arithmetic —
//! lives in [`schweep_plan::PlanError`] and reaches here through [`OracleError::Plan`]. The oracle
//! and the engine must refuse the same queries by the same names and fail on the same data with
//! the same message, so those errors belong to the shared plan, not to either implementation
//! (D-14).
//!
//! What is left here is what only the oracle can say: its catalog, its epochs, its judgement that
//! a *history* is malformed, and its internal assertions.

use schweep_plan::PlanError;
use schweep_zset::ZSetError;

pub type Result<T> = std::result::Result<T, OracleError>;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum OracleError {
    /// A binding refusal or an evaluation error — shared with the engine (D-14).
    #[error(transparent)]
    Plan(#[from] PlanError),

    #[error(transparent)]
    ZSet(#[from] ZSetError),

    #[error("table {0:?} is declared more than once")]
    DuplicateTable(String),

    #[error(
        "table {table:?} would hold row {row} at weight {weight} after epoch {epoch}: \
         a table's contents may never go negative (S-5). This is a malformed history — \
         something retracted a row that was not there."
    )]
    NegativeIntegral {
        table: String,
        row: String,
        weight: i64,
        epoch: u64,
    },

    #[error("epoch {requested} requested but only {sealed} epochs have been sealed")]
    EpochOutOfRange { requested: u64, sealed: u64 },

    #[error("join produced a weight outside the Int64 range")]
    JoinWeightOverflow,

    /// A negative weight where the oracle's own reasoning says one cannot occur. This is an
    /// assertion against an oracle bug, not a user-facing condition: table integrals are
    /// non-negative (S-5), filter preserves weights, and join multiplies non-negatives.
    #[error("internal: negative weight {weight} reached {stage}, which should be impossible")]
    NegativeIntermediate { stage: &'static str, weight: i64 },
}

impl OracleError {
    /// The table the oracle does not know about, whichever layer noticed.
    ///
    /// `UnknownTable` is raised by the binder (it owns the catalog lookup) but is also natural for
    /// the oracle to raise directly; this keeps one spelling for both.
    pub fn unknown_table(name: impl Into<String>) -> OracleError {
        OracleError::Plan(PlanError::UnknownTable(name.into()))
    }
}
