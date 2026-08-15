//! Errors from one-shot execution, snapshots, and compaction.

pub type Result<T> = std::result::Result<T, BatchError>;

#[derive(Debug, thiserror::Error)]
pub enum BatchError {
    #[error("snapshot I/O failure: {0}")]
    Io(String),

    #[error("parquet failure: {0}")]
    Parquet(String),

    #[error("arrow failure: {0}")]
    Arrow(String),

    /// A snapshot that is present but does not verify. Distinct from an *absent* snapshot, which is
    /// not an error: a crash before compaction's P7 leaves the whole log authoritative and no snapshot
    /// live (`docs/DURABILITY.md` §4).
    #[error("the snapshot is corrupt: {what}")]
    CorruptSnapshot { what: &'static str },

    #[error(
        "column {column} is reserved: a snapshot carries Z-set weights in it, so a table cannot \
         have one of its own (S-4)"
    )]
    ReservedColumn { column: &'static str },

    #[error("no table named {table:?} in the catalog this snapshot was written against")]
    UnknownTable { table: String },

    #[error(
        "source provenance is unavailable for the compacted prefix through epoch {epoch}; this is a \
         snapshot-v1 database and ownership cannot be reconstructed after its log records were removed"
    )]
    ProvenanceUnavailable { epoch: u64 },

    #[error("compaction needs a published checkpoint to anchor to; there is none (P1)")]
    NoCheckpointToAnchorTo,

    #[error(transparent)]
    Log(#[from] schweep_log::LogError),

    #[error(transparent)]
    ZSet(#[from] schweep_zset::ZSetError),

    #[error(transparent)]
    Circuit(#[from] schweep_circuit::CircuitError),

    #[error(transparent)]
    Sql(#[from] schweep_sql::SqlError),

    #[error(transparent)]
    Memo(#[from] schweep_memo::MemoError),
}

impl From<std::io::Error> for BatchError {
    fn from(error: std::io::Error) -> BatchError {
        BatchError::Io(error.to_string())
    }
}
