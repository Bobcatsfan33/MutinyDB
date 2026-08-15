//! Memo errors.
//!
//! Everything here is about the *registry* — handles, refcounts, the share index. Errors about a
//! query's meaning belong to `schweep-plan`, errors about its text to `schweep-sql`, and errors about
//! the dataflow to `schweep-circuit`; all three are forwarded unchanged, because the memo adds no
//! semantics of its own (I-8: sharing may change counters and cost, never a result byte).

use schweep_circuit::CircuitError;
use schweep_sql::SqlError;
use schweep_zset::ZSetError;

pub type Result<T> = std::result::Result<T, MemoError>;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MemoError {
    #[error("no standing query with handle {0}")]
    UnknownHandle(u64),

    #[error("no table named {0:?} in the catalog this memo was built with")]
    UnknownTable(String),

    #[error("a plan with no nodes cannot be registered")]
    EmptyPlan,

    /// `register` was called on a memo that keeps no input cache (C9).
    ///
    /// Refused rather than silently registering a query that would answer only for epochs after it
    /// arrived — which would be a query whose answer no oracle agrees with, and the quietest possible
    /// way to break I-1. The caller must use `register_from` with the accumulated input it holds.
    #[error(
        "this memo keeps no accumulated input, so it cannot catch a registration up on its own; \
         use register_from with the accumulated input (C7's snapshot plus the retained log suffix)"
    )]
    NoInputCache,

    /// A plan node's child was not found in the node list its parent was walked from. A bug in the
    /// registry's own walk, and one that would otherwise wire an operator to the wrong input.
    #[error("internal: a plan node's input is not in the plan's node list")]
    PlanNodeNotFound,

    /// The memo's maintained refcount for a node disagrees with the dataflow's wiring.
    ///
    /// This is the leak gate's error. A refcount too high leaks a node forever; one too low frees a
    /// node another query is still reading, which the dataflow refuses outright
    /// ([`CircuitError::NodeStillConsumed`]) rather than allowing a query's input to vanish.
    #[error("refcount for node {node} is {held} but the dataflow shows {actual}")]
    RefcountDisagrees {
        node: usize,
        held: usize,
        actual: usize,
    },

    /// The share index names a node that no longer exists — the next registration would attach to a
    /// freed node.
    #[error("the share index maps hash {hash:#x} to node {node}, which has been freed")]
    StaleShareIndex { hash: u64, node: usize },

    #[error(transparent)]
    Circuit(#[from] CircuitError),

    #[error(transparent)]
    Sql(#[from] SqlError),

    #[error(transparent)]
    ZSet(#[from] ZSetError),
}
