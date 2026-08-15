//! Refusals that only SQL *text* can provoke (`docs/SEMANTICS.md` S-35).
//!
//! Everything about what a query *means* — types, names, groups, aggregates — is refused by
//! [`schweep_plan::PlanError`], because both doors must refuse it by the same name. What is left
//! here is the surface: constructs SQL has and this dialect does not, and syntax that carries no
//! meaning to translate.
//!
//! Every variant names its construct (S-12). A refusal that says only "unsupported" tells the
//! person writing the query nothing about which of the forty things they wrote was the problem,
//! and the SQL fuzzer gate asserts that no refusal is anonymous.

use schweep_plan::PlanError;

pub type Result<T> = std::result::Result<T, SqlError>;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SqlError {
    /// sqlparser could not parse the text at all. Its message is passed through verbatim: it knows
    /// where the syntax error is and we do not.
    #[error("SQL parse error: {0}")]
    Parse(String),

    #[error("expected exactly one statement, found {found}")]
    NotOneStatement { found: usize },

    #[error("{0} is not a query; Current answers SELECT statements (D-1: reads only)")]
    NotAQuery(&'static str),

    /// A construct SQL has that the v1 dialect does not. The `&'static str` is the construct's
    /// name as a person would say it — "ORDER BY", not "OrderBy".
    #[error("{0} is not in the v1 dialect (§8)")]
    NotInDialect(&'static str),

    #[error(
        "SELECT * is not supported: a standing query's output schema must not change because a \
         column was added to a table, so every output column is named explicitly (S-11)"
    )]
    SelectStarNotSupported,

    #[error(
        "output column {0} has no name: write `AS <name>`, because Current derives names only for \
         a bare column reference (S-11)"
    )]
    MissingOutputName(String),

    #[error(
        "CAST {0} is not supported: the only accepted cast is CAST(NULL AS <type>), which types a \
         null literal; a cast that converts is the implicit conversion S-19 forbids"
    )]
    UnsupportedCast(String),

    #[error("{0} is not one of the six aggregates and there are no scalar functions in v1 (§8)")]
    UnknownFunction(String),

    #[error("type name {0} is not one of BIGINT, TEXT, VARCHAR, BOOLEAN (S-2, S-3)")]
    UnsupportedTypeName(String),

    #[error("integer literal {0} does not fit in Int64 (S-1)")]
    NumberOutOfRange(String),

    #[error(
        "join condition {0} is not a conjunction of column equalities; an INNER equi-join is the \
         only join at rung 2 (S-26)"
    )]
    NotAnEquiJoin(String),

    #[error("join key {0} and {1} are on the same side of the join, so they join nothing (S-26)")]
    JoinKeysOnOneSide(String, String),

    #[error("table name {0} is qualified; v1 has one namespace (§8)")]
    QualifiedTableName(String),

    /// The incrementalizer emitted a plan whose answer schema is not the one binding proved. Not a
    /// user error at all: it means a stage was dropped or mis-wired between the two, and saying so
    /// out loud is better than letting it surface later as an unexplained I-1 divergence.
    #[error("internal: the plan emits {emitted} but the query's answer schema is {expected}")]
    PlanWiringMismatch { emitted: String, expected: String },

    /// Everything semantic, forwarded unchanged. The SQL door adds no meaning of its own (S-35).
    #[error(transparent)]
    Plan(#[from] PlanError),
}
