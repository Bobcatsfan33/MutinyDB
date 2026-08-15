//! Errors from building and stepping a circuit.

use schweep_ops::OpError;
use schweep_plan::PlanError;
use schweep_zset::ZSetError;

pub type Result<T> = std::result::Result<T, CircuitError>;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CircuitError {
    #[error(transparent)]
    Op(#[from] OpError),

    #[error(transparent)]
    Plan(#[from] PlanError),

    #[error(transparent)]
    ZSet(#[from] ZSetError),

    #[error("no node with id {0}")]
    UnknownNode(usize),

    #[error("sink {0} does not exist in this circuit")]
    UnknownSink(usize),

    /// An epoch counter may only move forward. A reader that saw epoch 5 must never be told the world
    /// is now at epoch 3 (I-3).
    #[error("a circuit at epoch {held} cannot be declared to be at epoch {offered}")]
    EpochWouldGoBackwards { held: u64, offered: u64 },

    /// A node was freed while something still read it — a refcount bug in the memo, caught here
    /// rather than becoming an empty answer downstream (I-8).
    #[error("node {node} cannot be freed: node {consumer} still reads it")]
    NodeStillConsumed { node: usize, consumer: usize },

    /// A sink was repointed at a node emitting a different schema. Its answer store already holds
    /// rows of the old one, and a schema is part of an answer (S-8).
    #[error("a sink holding {held} cannot be repointed at a node emitting {offered}")]
    SinkSchemaMismatch { held: String, offered: String },

    #[error(
        "node {node} takes input from node {input}, which is not earlier in the circuit; \
         a circuit is a DAG and is built in dependency order"
    )]
    NodeOutOfOrder { node: usize, input: usize },

    #[error("operator {op} declares arity {expected} but was wired to {found} input(s)")]
    WiringArity {
        op: &'static str,
        expected: usize,
        found: usize,
    },

    #[error("table {0:?} is declared as a source more than once")]
    DuplicateSource(String),

    #[error("this circuit has no source for table {0:?}")]
    UnknownSourceTable(String),

    #[error("a circuit needs at least one node")]
    EmptyCircuit,

    #[error(
        "operator {op} declares its state bound as {declared} but is holding {actual} \
         entries between steps, against a budget of {budget} — an operator exceeding its \
         declaration is a bug, not a tuning problem (I-9)"
    )]
    StateBoundViolated {
        op: &'static str,
        declared: String,
        actual: usize,
        budget: usize,
    },

    #[error(
        "operator {op} declares its state as proportional to {declared} input(s) but is wired \
         to {arity} — a declaration that does not describe the operator cannot be checked (I-9)"
    )]
    StateDeclarationArityMismatch {
        op: &'static str,
        declared: usize,
        arity: usize,
    },

    #[error(
        "operator {op} declares unbounded state ({reason}); an unbounded-by-nature construct must \
         be admitted explicitly at query registration (I-9), and there is no registry until C6"
    )]
    UnboundedStateNotAdmissible {
        op: &'static str,
        reason: &'static str,
    },

    #[error("weight arithmetic overflowed i64 while {while_doing}")]
    WeightOverflow { while_doing: &'static str },

    /// An error store entry that is not a message. Unreachable — the store's schema has one
    /// non-null `Utf8` column — and reported rather than assumed.
    #[error("internal: the live-error store holds an entry that is not a message")]
    CorruptErrorStore,

    #[error("injected fault at seam {0}")]
    InjectedFault(&'static str),

    #[error("a checkpoint snapshot is malformed or does not match this circuit's shape")]
    CorruptSnapshot,

    #[error("snapshot failure: {0}")]
    Snapshot(String),

    /// The query has no answer because live errors are present (S-22).
    ///
    /// Displays as the message alone, with nothing added, so that it is byte-identical to the
    /// oracle's rendering of the same error. I-1 compares the text.
    #[error("{0}")]
    LiveEvaluationError(String),
}
