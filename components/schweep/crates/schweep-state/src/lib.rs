//! # schweep-state — operator state, behind an interface
//!
//! `ARCHITECTURE.md` §5.5. An ordered key-value store with prefix range scans and atomic
//! multi-key write batches. Operators reach their state through [`StateBackend`] and never through
//! a concrete container, which is the seam §2 insists on:
//!
//! > `schweep-log` and `schweep-state` must sit behind traits rather than being called concretely
//! > from operators.
//!
//! C2 uses it for the join's two indexes. C4 adds `RocksBackend` and the checkpoint protocol, and
//! **freezes the trait at its exit** — so until then this is allowed to grow, and what it is
//! missing is written down rather than left to be rediscovered (D-15).
//!
//! ## Keys are values, not bytes
//!
//! A key is a `Vec<Value>`, ordered by the total order on values (S-7); a stored value is an `i64`
//! weight. An order-preserving *byte* encoding is a storage concern and belongs inside a backend
//! that needs one, not in the interface every operator sees. `RocksBackend` will need it;
//! `MemBackend` is a `BTreeMap` and does not. The full argument is D-15.
//!
//! ## The two implementations, and why there are two
//!
//! [`MemBackend`] is a `BTreeMap`: the oracle's store, most tests', and the one every circuit got
//! before C8. [`RedbBackend`] (D-19, amending D-5) is a redb file per operator — the durable one, and
//! the one that made D-18's trait freeze **final**: the freeze's whole condition was that a second
//! implementation validate the trait, and this one did without a single change to it.
//!
//! Which one a circuit gets is decided by a [`BackendFactory`] threaded in from the caller, never by
//! the operators. An operator that knew which store it had could behave differently on each, and the
//! backend-invariance gate exists to assert that none of them can.

pub mod backend;
pub mod codec;
pub mod error;
pub mod factory;
pub mod mem;
pub mod redb_backend;

pub use backend::{Key, StateBackend, WriteBatch};
pub use codec::{decode_entries, decode_key, encode_entries, encode_key};
pub use error::{Result, StateError};
pub use factory::{BackendFactory, MemFactory, RedbFactory};
pub use mem::MemBackend;
pub use redb_backend::RedbBackend;
