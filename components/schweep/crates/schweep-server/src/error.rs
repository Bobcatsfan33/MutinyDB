//! Server errors, each carrying the wire kind a client needs (D-23).
//!
//! The mapping from an error to an [`ErrorKind`] is here rather than at the socket, because *this* is
//! where the difference between "your request was wrong" and "back off and retry" is known. A handler
//! that guessed the kind from a string would be guessing about the one thing a client acts on.

use crate::wire::ErrorKind;

pub type ServerResult<T> = std::result::Result<T, ServerError>;

#[derive(Debug, thiserror::Error)]
pub enum ServerError {
    #[error("I/O failure: {0}")]
    Io(String),

    #[error("no standing query with handle {0}")]
    UnknownHandle(u64),

    #[error("no endpoint at {method} {path}")]
    UnknownPath { method: String, path: String },

    /// A persisted registration that could not be rebuilt (D-22): quarantined, not dropped.
    #[error("handle {handle} is quarantined: {reason}")]
    Quarantined { handle: u64, reason: String },

    /// The source's queue is full. **The one retryable kind** (D-23).
    #[error(
        "source {source_id:?} has {depth} batches queued against a bound of {bound}; back off and retry"
    )]
    Overloaded {
        source_id: String,
        depth: usize,
        bound: usize,
    },

    /// The source has too many *bytes* queued. Retryable: a seal frees them (D-23).
    #[error(
        "source {source_id:?} has {queued_bytes} bytes queued and this batch adds {batch_bytes}, past \
         the bound of {bound}; back off and retry"
    )]
    OverloadedBytes {
        source_id: String,
        queued_bytes: usize,
        batch_bytes: usize,
        bound: usize,
    },

    /// One batch is larger than a source's whole byte budget. **Not** retryable — the client must split it.
    #[error(
        "this batch is {batch_bytes} bytes, past the per-source budget of {bound}; retrying will not \
         make it fit, so split it"
    )]
    BatchTooLarge { batch_bytes: usize, bound: usize },

    /// A resume token behind the retained deltas. A gap is a refusal, never a silent re-baseline (D-23).
    #[error(
        "handle {handle}: resume token {token} is behind the oldest retained epoch {oldest}; the \
         deltas for those epochs are gone, and serving the answer instead would deliver it under the \
         wrong epoch number"
    )]
    TokenTooOld {
        handle: u64,
        token: u64,
        oldest: u64,
    },

    #[error("the registry is corrupt: {0}")]
    CorruptRegistry(&'static str),

    /// Something this sprint did not wire, refused at the point it would otherwise be wrong.
    ///
    /// A refusal here never reaches a client — it happens at startup — and that is the point: an
    /// arrangement `schweepd` cannot yet handle correctly must stop the process rather than serve
    /// answers that look fine.
    #[error("schweepd does not support this yet: {0}")]
    Unsupported(&'static str),

    #[error(transparent)]
    Log(#[from] schweep_log::LogError),

    #[error(transparent)]
    Sql(#[from] schweep_sql::SqlError),

    #[error(transparent)]
    Memo(#[from] schweep_memo::MemoError),

    #[error(transparent)]
    Circuit(#[from] schweep_circuit::CircuitError),

    #[error(transparent)]
    Batch(#[from] schweep_batch::BatchError),
}

impl ServerError {
    /// Which wire kind this failure is (D-23's taxonomy).
    #[must_use]
    pub fn kind(&self) -> ErrorKind {
        match self {
            ServerError::Overloaded { .. } | ServerError::OverloadedBytes { .. } => {
                ErrorKind::Overloaded
            }
            // Not `Overloaded`: that kind promises a retry can succeed, and this one never will (D-23).
            ServerError::BatchTooLarge { .. } => ErrorKind::Refused,
            ServerError::UnknownHandle(_) | ServerError::UnknownPath { .. } => ErrorKind::NotFound,
            // A conflict about *state*, not about the request: the client's request was well formed and
            // the server cannot satisfy it as asked.
            ServerError::Quarantined { .. }
            | ServerError::TokenTooOld { .. }
            | ServerError::Batch(schweep_batch::BatchError::ProvenanceUnavailable { .. })
            | ServerError::Log(schweep_log::LogError::TokenReused { .. }) => ErrorKind::Rejected,
            // Everything the dialect refuses is a statement about the request (S-12).
            ServerError::Sql(_) => ErrorKind::Refused,
            ServerError::Log(schweep_log::LogError::UnknownTable(_)) => ErrorKind::NotFound,
            ServerError::Log(_)
            | ServerError::Memo(_)
            | ServerError::Circuit(_)
            | ServerError::Batch(_)
            | ServerError::CorruptRegistry(_)
            | ServerError::Unsupported(_)
            | ServerError::Io(_) => ErrorKind::Internal,
        }
    }
}
