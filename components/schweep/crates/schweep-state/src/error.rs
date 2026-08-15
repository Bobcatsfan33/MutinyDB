//! Errors from a state backend.

pub type Result<T> = std::result::Result<T, StateError>;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StateError {
    #[error("weight arithmetic overflowed i64 while {while_doing}")]
    WeightOverflow { while_doing: &'static str },

    /// A backend that talks to something outside the process — `RocksBackend` in C4 — reports
    /// failures here. `MemBackend` never produces one, and the variant exists so that operators
    /// are written against a fallible interface from the start rather than being retrofitted.
    #[error("state backend failure: {0}")]
    Backend(String),
}
