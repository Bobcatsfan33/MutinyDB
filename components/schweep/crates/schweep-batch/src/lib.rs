//! # schweep-batch — one-shot queries, Parquet ground truth, and compaction
//!
//! ```text
//!             ┌───────────────────────────────────────────────┐
//!   log ─────►│  snapshot (Parquet)  +  retained suffix        │──► accumulated input
//!             └───────────────────────────────────────────────┘         │
//!                        published-then-swapped                         ├─► one-shot answer
//!                                                                       └─► a fresh standing query
//! ```
//!
//! Three things that are really one thing: **the log cannot grow forever**, and everything else follows.
//!
//! - [`snapshot`] — the input integrals as Parquet, plus a manifest and the dedup ledger. Ground truth,
//!   readable by tools that are not us.
//! - [`compact`] — the P-sequence from `docs/DURABILITY.md` §4, in order, in one function.
//!   Publish-then-swap, never in-place; one rename is the only commit point.
//! - [`hydrate`] — snapshot + suffix → the accumulated input. The function that makes a compaction
//!   invisible.
//! - [`oneshot`] — a query answered once through an ephemeral circuit: the same machinery, one big
//!   delta, torn down after.
//!
//! ## The edge that makes compaction dangerous
//!
//! Compaction discards log records, and the dedup index was rebuilt by scanning them (R7). A token
//! acknowledged in the discarded prefix and re-offered afterwards would look new, and the batch would be
//! applied a second time — **I-4 broken by a space optimisation**, with no error and no crash. So the
//! ledger of acknowledged tokens rides the snapshot, and `Log::open` seeds from it. It is the first
//! thing to check if compaction is ever changed, and one of C7's canonical mutations removes it.

pub mod compact;
pub mod error;
pub mod hydrate;
pub mod oneshot;
pub mod snapshot;
pub mod source;

pub use compact::{compact, snapshot_dir, Compacted};
pub use error::{BatchError, Result};
pub use hydrate::{accumulated, accumulated_upto, as_one_delta, one_delta_for};
pub use oneshot::{answer, answer_over_integrals, answer_over_log, answer_sql};
pub use snapshot::Manifest;
pub use source::{source_integral, source_integrals_upto, SourceIntegrals};
