//! # schweep-server — `schweepd`, one process over the embedded engine (§6 C9)
//!
//! ```text
//!            HTTP/1.1 over loopback (D-23)
//!   client ──────────────────────────────────► schweepd
//!                                                 │
//!            ingest · seal · txn · register        │  admission (bounded queues, per source)
//!            read · oneshot · subscribe            │  the memo (C6), state spilled (C8)
//!            plan · counters · explain-state       │  the log + checkpoints (C4), snapshots (C7)
//!            health · shutdown                     │  the registry file (D-22)
//! ```
//!
//! The server adds **no engine behaviour and no nondeterminism**. Every durable mechanism it drives was
//! built and gated in an earlier sprint; what C9 adds is a door, an admission policy, and a subscription
//! cursor that lives on the client.
//!
//! - [`wire`] — HTTP/1.1 by hand, and the five-kind error taxonomy. Exactly one kind is retryable.
//! - [`admission`] — per-source bounded queues, in **batches and bytes**. A queue with no bound is a
//!   memory leak with a schedule, and a queue bounded only in batches is not bounded at all.
//! - [`registry_file`] — the persisted registry: a handle survives a restart (D-22).
//! - [`engine`] — the composition: log, checkpoints, memo, subscriptions.
//! - [`server`] — the accept loop and the handlers.
//! - [`client`] — a client, because a wire contract with only one implementation is a guess.
//! - [`costs`] — what the two bounds cost in bytes, measured. Both ledger entries cite it, and
//!   `testing/differential/tests/evidence.rs` recomputes it.
//!
//! ## Four properties worth knowing before reading the code
//!
//! **The wall clock is not consulted.** No timeouts that change behaviour, no time-based flush, no
//! expiry, not even a `Date` header. Epochs are sealed when a client says so. What is nondeterministic is
//! the order concurrent requests arrive in — the ingest boundary, where D-6 puts it — and past that
//! boundary the engine runs on one thread in one order.
//!
//! **Catch-up comes from disk, one epoch at a time.** The memo is built with no input cache, and every
//! registration is caught up by streaming the log's epochs (`Memo::register_from_chunks`). That closes the
//! gap C8 named — a memo's footprint no longer tracks the data — and it has a second effect worth as much:
//! because the chunks are the epochs in order, a recovered registration takes the same passes the live path
//! took, so a recovered server matches a never-crashed twin down to the emission counters.
//!
//! **Recovery is bootstrap, not checkpoint, and the state store is discarded on open.** A memo cannot be
//! restored through `Circuit::restore` — its shape is the set of queries registered at the time — so the
//! checkpoint `schweepd` writes is C7's compaction anchor and nothing else. The D-22 addendum records why,
//! and it exists because the first version of this crate hydrated on top of a surviving redb store and
//! applied every input twice.
//!
//! **A subscription has no server-side cursor.** The resume token is the epoch number and the client
//! holds it. The server cannot lose a cursor it never kept, which is what makes redelivery after a
//! subscriber crash exactly-once per epoch rather than hopefully-once.

/// One batch on the wire: the table it targets, its dedup token, and its entries.
///
/// Named because a transaction is a list of them and the bare tuple is unreadable at the call site.
pub type WireBatch = (String, String, Vec<(schweep_zset::Row, i64)>);

pub mod admission;
pub mod client;
pub mod costs;
pub mod engine;
pub mod error;
pub mod registry_file;
pub mod server;
pub mod wire;

pub use admission::{Policy, DEFAULT_SOURCE_QUEUE_BOUND, DEFAULT_SOURCE_QUEUE_BYTES};
pub use client::{Client, RawResponse, Response};
pub use engine::{Drained, Engine, EpochDelta, SUBSCRIPTION_RING, SUBSCRIPTION_RING_BYTES};
pub use error::{ServerError, ServerResult};
pub use registry_file::Registry;
pub use server::{Server, ServerConfig};
pub use wire::ErrorKind;
