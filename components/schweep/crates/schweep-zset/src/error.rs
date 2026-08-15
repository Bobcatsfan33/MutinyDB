//! Errors from the Z-set layer.
//!
//! Every variant names what was wrong specifically enough to act on. "Invalid input" is not an
//! error message this codebase is allowed to produce.

use crate::value::DataType;

/// The result type of every fallible operation in this crate.
pub type Result<T> = std::result::Result<T, ZSetError>;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ZSetError {
    #[error("schema has no fields; a Z-set schema must have at least one column")]
    EmptySchema,

    #[error("duplicate column name {0:?} in schema")]
    DuplicateFieldName(String),

    #[error("no column named {0:?} in schema")]
    UnknownColumn(String),

    #[error("column {name:?} has type {declared} which may not be stored in a table (S-3)")]
    UnstorableColumnType { name: String, declared: DataType },

    #[error("row has {found} values but the schema has {expected} columns")]
    ArityMismatch { expected: usize, found: usize },

    #[error("column {column:?} expects {expected} but the value is {found}")]
    ValueTypeMismatch {
        column: String,
        expected: DataType,
        found: DataType,
    },

    #[error("column {column:?} is declared NOT NULL but the value is NULL")]
    NullInNonNullable { column: String },

    #[error("Z-sets have different schemas: {left} vs {right}")]
    SchemaMismatch { left: String, right: String },

    #[error("weights column has {weights} entries but the batch has {rows} rows")]
    WeightLengthMismatch { weights: usize, rows: usize },

    #[error("weight arithmetic overflowed i64 while {while_doing}")]
    WeightOverflow { while_doing: &'static str },

    #[error("column {column:?} could not be read as {expected} from the Arrow batch")]
    ArrowDowncast { column: String, expected: DataType },

    #[error("Arrow error: {0}")]
    Arrow(String),
}

impl From<arrow_schema::ArrowError> for ZSetError {
    fn from(e: arrow_schema::ArrowError) -> Self {
        ZSetError::Arrow(e.to_string())
    }
}
