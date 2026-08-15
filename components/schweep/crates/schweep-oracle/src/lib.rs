//! # schweep-oracle — the naive reference engine
//!
//! **This crate is the spec.** When a question arises about what a query should return, the
//! answer is what the oracle returns; and if the oracle is wrong, the oracle gets fixed first
//! (`ARCHITECTURE.md` §5.1, §10).
//!
//! It is a complete, naive, in-memory implementation of dialect rungs 1–3: tables are `Vec<Row>`,
//! epochs are replayed prefixes of the log, and every query is recomputed from scratch over the
//! full input at every epoch. No indexes. No incrementality. No cleverness. That is not a
//! shortcut taken to save time — it is the property that makes the oracle worth having. An
//! implementation with no state and no optimisation has almost nowhere to hide a bug, and it
//! cannot share one with the incremental engine it is used to check (I-1).
//!
//! It is also slow, deliberately and permanently. Answering at epoch N replays the whole log
//! prefix; the join is a nested loop. No performance claim is made for this crate and none will
//! be (I-10).
//!
//! ## Where the semantics live
//!
//! Not here. `docs/SEMANTICS.md` decides what a query means, rule by numbered rule (S-1…S-33),
//! and this crate implements those rules with the rule number cited at each site. The order is
//! always: document, then oracle, then engine (§10).
//!
//! ## A tour
//!
//! - [`engine`] — [`Oracle`]: seal epochs, replay prefixes, recompute answers.
//! - [`aggregate`] — the five aggregates, weight-aware and null-ignoring (S-30, S-31).
//!
//! The query IR, the binder, and the scalar expression library are **not** here: they live in
//! `schweep-plan`, because from C1 the engine needs them too and neither implementation may own
//! the definition of what a query is (D-14). They are re-exported from this crate so that a
//! reader of the oracle does not have to know that.
//!
//! ## Example
//!
//! ```
//! use schweep_oracle::{EpochDeltas, Expr, Oracle, Query, Source};
//! use schweep_zset::{DataType, Field, Row, Schema, Value};
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let schema = Schema::new_table(vec![Field::nullable("v", DataType::Int64)])?;
//! let mut oracle = Oracle::new([("t".to_owned(), schema)])?;
//!
//! // Epoch 1: two copies of (1) and one of (2).
//! let mut d1 = EpochDeltas::new();
//! d1.push("t", Row::new(vec![Value::Int(1)]), 2);
//! d1.push("t", Row::new(vec![Value::Int(2)]), 1);
//! oracle.seal_epoch(d1)?;
//!
//! // Epoch 2: retract one copy of (1). A retraction is just a negative weight.
//! let mut d2 = EpochDeltas::new();
//! d2.push("t", Row::new(vec![Value::Int(1)]), -1);
//! oracle.seal_epoch(d2)?;
//!
//! let query = Query::from(Source::scan("t", "t"))
//!     .filter(Expr::is_not_null(Expr::column("t.v")));
//! let answer = oracle.answer(&query)?;
//! assert_eq!(
//!     answer.canonical()?.render(),
//!     "(t.v: Int64)\n(1) => 1\n(2) => 1\n"
//! );
//! # Ok(())
//! # }
//! ```

pub mod aggregate;
pub mod engine;
pub mod error;
pub mod live_errors;

pub use engine::{Epoch, Oracle};
pub use error::{OracleError, Result};
pub use live_errors::LiveErrors;

// Re-exported so a reader of the oracle does not have to know where the shared layer lives.
// The definitions are in `schweep-plan` (D-14); these are the same types, not copies.
pub use schweep_plan::{
    bind, AggFunc, BinOp, Bound, Catalog, Expr, GroupBy, Named, PlanError, Query, Source,
};
pub use schweep_zset::EpochDeltas;
