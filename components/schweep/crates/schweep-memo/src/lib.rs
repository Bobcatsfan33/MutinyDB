//! # schweep-memo — many queries, one dataflow
//!
//! ```text
//!   register("SELECT ... WHERE k > 1")            register("SELECT DISTINCT ... WHERE k > 1")
//!            │                                              │
//!            ▼                                              ▼
//!        ┌────────┐   ┌────────┐   ┌─────────┐          ┌──────────┐
//!        │ source │──►│ filter │──►│ project │──┬──────►│ distinct │──► sink B
//!        └────────┘   └────────┘   └─────────┘  └──────────────────────► sink A
//!                     shared by both queries          novel to B
//! ```
//!
//! Two standing queries with a common prefix share the circuitry that computes it: the prefix is
//! stepped **once** per epoch, not once per query, and each query keeps its own answer. That is the
//! whole point of the memo, and I-8 is the law that makes it safe to want:
//!
//! > **I-8 · Memo transparency.** Whether a subplan is shared or private may change counters and
//! > cost, never a result byte.
//!
//! ## Layout
//!
//! - [`canonical`] — canonicalization and structural hashing. Conservative by design: exact subtree
//!   matches only, with the rule inventory and the sharing each omission costs written out.
//! - [`registry`] — [`Memo`]: register, read, deregister; attach to live subtrees; refcounted
//!   teardown; the I-9 admission for unbounded state.
//! - [`explain`] — `EXPLAIN STATE` (C8): what every operator holds, per query, with a byte estimate
//!   that a gate reconciles against the backend's real footprint.
//! - [`costs`] — the measured constants that estimate turns on, each in the ledger with its artifact.
//!
//! ## The one thing to read before changing anything here
//!
//! **Sharing fails silently in both directions, and the two failures need different tests.**
//!
//! - Sharing that *stops happening* — a canonicalization rule that no longer fires, a hash that
//!   accidentally includes a node id — leaves every answer correct. No answer test can see it. What
//!   sees it is a **counter**: `Circuit::operator_steps` with sharing on must be strictly below the
//!   same battery with sharing off.
//! - Sharing that happens when it should *not* — a hash that ignores a plan field, so two different
//!   queries collide — is cross-contamination: one query reads the other's answer. What sees that is
//!   the **answer-equality** half of the I-8 gate, and it sees it immediately.
//!
//! So the gate has both halves, and the teeth exercise both: `docs/PROGRESS.md`'s C6 section records
//! which mutation each half caught.

pub mod canonical;
pub mod costs;
pub mod error;
pub mod explain;
pub mod maintenance;
pub mod registry;

pub use canonical::{canonicalize, subtree_hash};
pub use error::{MemoError, Result};
pub use explain::{
    explain_circuit, reconcile_circuit, CostModel, ExplainState, OperatorState, QueryState,
    Reconciliation,
};
pub use maintenance::{explain_maintenance, ExplainMaintenance, MaintenanceNode, QueryMaintenance};
pub use registry::{Accounting, Admission, CatchUp, Chunks, Handle, Memo, Registration, Sharing};
