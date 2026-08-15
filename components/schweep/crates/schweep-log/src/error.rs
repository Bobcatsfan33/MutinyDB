//! Errors from the log.

pub type Result<T> = std::result::Result<T, LogError>;

#[derive(Debug, thiserror::Error)]
pub enum LogError {
    #[error("log I/O failure: {0}")]
    Io(String),

    #[error("the log directory {0:?} is not a directory")]
    NotADirectory(String),

    /// A record that is present but malformed. Distinct from a *torn tail*, which is not an error:
    /// a short or CRC-failing frame is the expected state of a log whose process died mid-write and
    /// is discarded silently (`docs/DURABILITY.md` R5).
    #[error("the log is corrupt: {0}")]
    Corrupt(&'static str),

    #[error("no table named {0:?}")]
    UnknownTable(String),

    #[error(transparent)]
    ZSet(#[from] schweep_zset::ZSetError),

    /// **I-4.** The same dedup token was offered with different content.
    ///
    /// Not a replay: a replay carries the same bytes and is acknowledged-and-dropped. This is two
    /// different batches claiming one identity, which is a bug in the caller, and accepting either
    /// silently would leave the wrong one durable. §5.4: "refused loudly".
    #[error(
        "dedup token {token:?} from source {source_id:?} was already accepted with different \
         content; a token names one batch and may not be reused for another (I-4)"
    )]
    TokenReused { source_id: String, token: String },

    #[error("epoch {requested} requested but only {sealed} epochs have been sealed")]
    EpochOutOfRange { requested: u64, sealed: u64 },

    /// The epoch asked for is in the snapshot, not the log: compaction discarded its records.
    #[error("epoch {requested} was compacted away; the log retains epochs after {retained_from}")]
    EpochCompacted { requested: u64, retained_from: u64 },

    /// Compaction was asked to compact a prefix that is already gone.
    #[error("nothing to compact: anchor {anchor} is not past the retained prefix {retained_from}")]
    NothingToCompact { anchor: u64, retained_from: u64 },

    /// A fault the crash harness injected at a named seam (`docs/DURABILITY.md` §5).
    ///
    /// Only ever produced when a fault plan selects a seam, and only in tests. It carries the seam's
    /// name so that a harness can assert *which* fault fired rather than merely that one did.
    #[error("injected fault at seam {0}")]
    InjectedFault(&'static str),
}

impl From<std::io::Error> for LogError {
    fn from(e: std::io::Error) -> Self {
        LogError::Io(e.to_string())
    }
}
