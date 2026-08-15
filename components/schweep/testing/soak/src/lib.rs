//! # schweep-soak — the soak harness (§6 C8)
//!
//! > a scenario with operator state 10× RAM completes with flat memory (the soak harness arrives here —
//! > RSS sampled across the run, leak fails the job)
//!
//! Two pieces, and the second is the interesting one:
//!
//! - [`rss`] — sampling resident memory, and **reading the memory ceiling from the cgroup** rather than
//!   assuming one is in force. A gate that cannot see its own ceiling is not a gate.
//! - the tests in `tests/` — the ceiling gate itself: a shape whose operator state is many times the
//!   ceiling, run to completion, with the RSS curve asserted for *shape* and the answers asserted
//!   arithmetically.
//!
//! ## Why the answers are checked arithmetically and not against the oracle
//!
//! The oracle recomputes from the whole log in memory. Under a ceiling that the *state* exceeds tenfold,
//! the oracle does not fit either — so at this size there is no from-scratch twin to compare against,
//! and pretending otherwise would mean running the gate at a size where nothing spills. The shape is
//! therefore built so that its answer is known by construction: every group's count and sum are
//! arithmetic in the row generator, and a dropped or truncated entry moves one of them.
//!
//! The general correctness claim stays where it belongs — the differential gates, which run the same
//! backend over 4,400 generated scenarios (`testing/differential/tests/c8_backends.rs`). This gate's job
//! is memory, and it says so.

pub mod rss;

pub use rss::{ceiling, rss_bytes, Ceiling, Curve};
