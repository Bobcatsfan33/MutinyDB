//! # schweep-log — the input log
//!
//! `ARCHITECTURE.md` §5.4: "The write path and the only place time enters."
//!
//! A directory of files holding an append-only sequence of records: batches appended by sources, and
//! the seal records that group them into epochs (S-6). The log is the **source of truth** — operator
//! state is a cache of it — which is what makes recovery "load a checkpoint and replay the suffix"
//! rather than a bespoke repair procedure.
//!
//! ## What it guarantees
//!
//! - **I-4 · exactly-once ingest.** A `dedup_token` names one batch. Offered again with the same
//!   content it is acknowledged and dropped; offered with *different* content it is refused loudly.
//!   Never silently rewritten.
//! - **Torn tails are ordinary.** Every record carries a length and a CRC, and a reader stops at the
//!   first frame that is short or fails its checksum. A crash mid-write is the expected state of a
//!   log, not an error condition.
//!
//! ## Where the orderings live
//!
//! `docs/DURABILITY.md`, written before this crate existed, numbers every step of the ack, seal,
//! checkpoint and recovery sequences and names the instant between each pair. [`fault::Seam`] is that
//! list in code, and the crash harness lands on those seams. If you change an ordering here, change
//! the document first (§10).

pub mod dedup;
pub mod error;
pub mod fault;
pub mod log;
pub mod record;
pub mod stream;

pub use error::{LogError, Result};
pub use fault::{FaultInjector, FaultPlan, Seam};
pub use log::{Ack, Batch, Epoch, Log, Pointer, SyncPolicy};
pub use record::Record;
pub use stream::{Epochs, SealedEpoch};
