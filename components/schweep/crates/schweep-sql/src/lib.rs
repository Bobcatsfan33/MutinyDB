//! # schweep-sql — the SQL door, and the incrementalizer behind it
//!
//! ```text
//!   SQL text ──parse──► AST ──bind──► Query ──incrementalize──► CircuitPlan ──instantiate──► Circuit
//!                                      ▲
//!   typed API ──────────────────────────┘
//! ```
//!
//! The two doors meet at [`schweep_plan::Query`] and never diverge again. Everything downstream of
//! that junction — the plan, the hash, the circuit — is reached by one code path, which is what makes
//! I-6 ("the typed API and SQL compile to structurally identical plans") a property of the code
//! rather than a promise about it.
//!
//! - [`parse`] — text → AST, and a refusal by name for every clause SQL has that this dialect does
//!   not. A construct parsing is never a reason to support it (S-35).
//! - [`select`] — the binder: AST → `Query`. Names (S-11), grouping (S-27, S-33), projection (S-36).
//! - [`expr`] — SQL expressions → `Expr`, including the `CAST(NULL AS T)` rule (S-19) and the three
//!   refusals for an aggregate met where a scalar belongs (S-32).
//! - [`incremental`] — the incrementalizer. **Read this one first if you read only one**: it is the
//!   intellectual heart of the engine (§5.6) and it documents the DBSP rules rule by rule.
//! - [`circuit_plan`] — the plan type, its s-expression rendering, and its structural hash.
//! - [`instantiate`] — plan → running circuit, allocating one state backend per stateful operator.
//!
//! ## The semantic gate for this crate is `tests/binder.rs`
//!
//! Not the differential harness. I-6 makes both doors compile to identical plans, so a binder that
//! turns SQL text into a **valid but wrong** plan is invisible to a differential sweep: both doors
//! produce that plan, and the oracle is asked the same wrong question, so the answers agree. What
//! catches it is `tests/binder.rs` — SQL text on one side, the plan the rule says it means written out
//! on the other — and `tests/dialect.rs`, which pins every refusal to the construct it names.
//!
//! **Every dialect change adds rows to both.** This is rule 11 in `CLAUDE.md`.
//!
//! ## The whole surface
//!
//! ```no_run
//! # use schweep_sql::compile;
//! # use schweep_plan::bind::Catalog;
//! # fn main() -> Result<(), schweep_sql::SqlError> {
//! # let catalog = Catalog::new();
//! let plan = compile("SELECT t.a AS a, COUNT(*) AS n FROM t GROUP BY t.a", &catalog)?;
//! let circuit = schweep_sql::instantiate::instantiate(&plan)?;
//! # Ok(())
//! # }
//! ```

pub mod circuit_plan;
pub mod error;
pub mod expr;
pub mod incremental;
pub mod instantiate;
pub mod parse;
pub mod select;

pub use circuit_plan::{CircuitNode, CircuitPlan, Rule};
pub use error::{Result, SqlError};
pub use incremental::{incrementalize, incrementalize_typed};
pub use instantiate::{children, instantiate, instantiate_with, operator_for, operator_for_with};
pub use select::BoundQuery;

use schweep_plan::bind::Catalog;
use schweep_plan::Expr;
use schweep_zset::Schema;

/// A C11 predicate and the qualified input scope it was bound against.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BoundPredicate {
    pub expression: Expr,
    pub scope: Schema,
}

/// SQL text → a bound query (S-9 … S-36). The whole front half of the pipeline.
pub fn bind_sql(sql: &str, catalog: &Catalog) -> Result<BoundQuery> {
    let parsed = parse::parse(sql)?;
    let statement = parse::select_of(&parsed)?;
    select::bind_select(&statement, catalog)
}

/// SQL text → a circuit plan. The whole pipeline, short of allocating state.
pub fn compile(sql: &str, catalog: &Catalog) -> Result<CircuitPlan> {
    let bound = bind_sql(sql, catalog)?;
    incrementalize(&bound, catalog)
}

/// Bind a standalone C11 source-retraction predicate with exactly the same rules as `WHERE`.
///
/// Wrapping the expression in a minimal SELECT intentionally reuses the public SQL pipeline. This
/// keeps qualification, three-valued logic, type checking, and refusals identical to a query filter.
pub fn bind_where_predicate(
    table: &str,
    predicate: &str,
    catalog: &Catalog,
) -> Result<BoundPredicate> {
    let schema = catalog
        .get(table)
        .ok_or_else(|| SqlError::Plan(schweep_plan::PlanError::UnknownTable(table.to_owned())))?;
    let field = schema.fields().first().ok_or_else(|| {
        SqlError::Parse(format!(
            "table {table:?} has no columns, so a predicate cannot be bound"
        ))
    })?;
    let quote = |name: &str| format!("\"{}\"", name.replace('"', "\"\""));
    let sql = format!(
        "SELECT {}.{} AS __source_retraction_probe FROM {} WHERE {predicate}",
        quote(table),
        quote(&field.name),
        quote(table)
    );
    let bound = bind_sql(&sql, catalog)?;
    let expression = bound.query.filter.ok_or_else(|| {
        SqlError::Parse("source-retraction predicate produced no WHERE expression".to_owned())
    })?;
    Ok(BoundPredicate {
        expression,
        scope: bound.bound.input_schema,
    })
}
