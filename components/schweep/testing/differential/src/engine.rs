//! The seam the differential harness tests through.
//!
//! The harness knows nothing about how an answer is produced. It hands an implementation a
//! sequence of sealed epochs and asks for the answer after each one. In C0 the only
//! implementation was the oracle, on both sides of the comparison, which proved the harness rather
//! than the engine. From C1 `schweep-circuit` implements the same trait and the comparison starts
//! meaning something (I-1).
//!
//! ## What this trait is allowed to know
//!
//! Only the neutral layer: [`schweep_plan::Query`] for the question, [`schweep_zset::EpochDeltas`]
//! for the changes, and [`schweep_zset::Canonical`] for the answer. It names **no**
//! implementation's types — not the oracle's, not the circuit's — which is what lets one be
//! swapped for the other without the harness noticing (D-14).
//!
//! That was not true in C0: the trait imported the oracle's `Query`, while a comment here claimed
//! it imported nothing of the oracle's. The claim is now true because the types moved, not because
//! the comment was softened.

use schweep_plan::Query;
use schweep_zset::{Canonical, EpochDeltas, Schema};

/// Something that can be fed sealed epochs and asked for an answer.
///
/// Errors are `String` rather than a typed error because the harness compares implementations
/// that do not share an error type: what must agree is *whether* a query failed and *what it
/// said*, not which enum carried it. Comparing the message keeps error paths inside I-1 instead
/// of quietly outside it.
pub trait EngineUnderTest: Sized {
    /// Shown in failure reports, so that "these two disagreed" names which two.
    fn name() -> &'static str;

    /// Register the tables and the single standing query this run is about.
    ///
    /// An implementation that does not support the query — a C1 circuit handed a join, say —
    /// refuses here, by name. It does not silently answer something else.
    fn build(tables: &[(String, Schema)], query: &Query) -> Result<Self, String>;

    /// Seal one epoch (S-6). After this returns, the answer must reflect this epoch and no part
    /// of any later one (I-3).
    fn seal_epoch(&mut self, deltas: &EpochDeltas) -> Result<(), String>;

    /// The answer as of the latest sealed epoch, in canonical form (S-8).
    fn answer(&self) -> Result<Canonical, String>;

    /// A deterministic rendering of everything the implementation is holding: its epoch, its
    /// result store, and each operator's state.
    ///
    /// This is what the I-2 gate compares. Answers alone are not enough — two runs can agree on
    /// every answer while accumulating different state, and that difference becomes a wrong
    /// answer later, or a recovery that does not match its uncrashed twin (I-7). An
    /// implementation with no state of its own returns a rendering of its answer, which is
    /// honest: it says "this is all I hold".
    fn state_fingerprint(&self) -> Result<String, String>;
}
