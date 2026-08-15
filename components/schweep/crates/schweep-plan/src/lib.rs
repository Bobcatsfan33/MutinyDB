//! # schweep-plan — the logical plan, the binder, and the scalar expression library
//!
//! The **neutral** layer between the oracle and the engine (D-14). Everything here is about what a
//! query *means*, and nothing here is about how it is *executed*: there is no state, no epoch, no
//! incrementality, and no table.
//!
//! - [`plan`] — the query IR: [`Query`], [`Source`], [`Expr`], [`AggFunc`], [`GroupBy`] (S-9…S-11).
//! - [`bind`] — resolve columns, type expressions, refuse everything else by name (S-12), and
//!   compute the answer's schema.
//! - [`eval`] — three-valued scalar evaluation (S-13…S-22).
//!
//! ## Why this crate exists
//!
//! From C1 there are two implementations of the query surface: `schweep-oracle` recomputes from
//! scratch, `schweep-circuit` maintains answers from deltas. They must agree byte for byte (I-1).
//! Two things they must *not* each own are the plan type and the answer schema — if the engine
//! had its own copy of either, a disagreement about what a query even *is* would surface as a
//! correctness failure, and I-6's claim that both doors compile to one plan would be unprovable.
//!
//! The scalar expression library lives here too, which §6 C5 requires ("implemented once, shared
//! by oracle and engine"). **That has a cost, and it is not hidden:** a bug inside this crate
//! produces the same wrong answer on both sides of the differential harness, which therefore
//! cannot see it. The mitigation is that this crate is pinned directly to `docs/SEMANTICS.md` by
//! its own unit tests — the Kleene truth tables, checked arithmetic, CASE short-circuiting — and
//! not to another implementation. See D-14 for the full argument.
//!
//! ## Where the semantics live
//!
//! Not here. `docs/SEMANTICS.md` decides what a query means, rule by numbered rule (S-1…S-33), and
//! this crate implements those rules with the rule number cited at each site. The order is always:
//! document, then oracle, then engine (§10).

pub mod bind;
pub mod error;
pub mod eval;
pub mod plan;

pub use bind::{bind, bind_source, projection_schema, type_of, Bound, Catalog, Naming, Scope};
pub use error::{PlanError, Result};
pub use eval::{eval, is_true};
pub use plan::{AggFunc, BinOp, Expr, GroupBy, Named, Query, Source};
