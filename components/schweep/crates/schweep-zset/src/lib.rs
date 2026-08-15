//! # schweep-zset
//!
//! Z-set batches over Arrow: the universal data representation in Current
//! (`ARCHITECTURE.md` §5.2, D-2).
//!
//! A **Z-set** is a multiset of rows in which each row carries an `i64` **weight**. `+1` means
//! one copy of the row exists, `+3` means three, `-1` means one copy is removed. All data moving
//! through Current — inputs, outputs, intermediate results — is Z-sets, and there is no separate
//! delete or update machinery anywhere: an update is a `-1` for the old row and a `+1` for the
//! new row, in the same Z-set.
//!
//! ## What lives here
//!
//! - [`Value`] and [`DataType`] — the value model (S-1, S-2) and the total order on values (S-7).
//! - [`Schema`] and [`Field`] — column lists, and their translation to Arrow.
//! - [`Row`] — a value per column, ordered lexicographically in schema order.
//! - [`ZSetBatch`] — the Arrow batch plus its aligned weight column, with `add`, `negate`, and
//!   `consolidate`.
//! - [`Canonical`] — the canonical form (S-8) that answer equality is defined on, and therefore
//!   the thing the differential harness compares (I-1).
//! - [`EpochDeltas`] — one epoch's input deltas, per table: the change that becomes visible when
//!   an epoch is sealed (S-6).
//!
//! Semantics referenced as `S-n` are defined in `docs/SEMANTICS.md`, which is the spec; this
//! crate implements it and does not decide it.

mod deltas;
mod error;
mod row;
mod schema;
mod value;
mod zset;

pub use deltas::EpochDeltas;
pub use error::{Result, ZSetError};
pub use row::Row;
pub use schema::{Field, Schema};
pub use value::{DataType, Value};
pub use zset::{Canonical, ZSetBatch};
