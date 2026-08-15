//! # schweep-crash — the crash-injection harness
//!
//! `ARCHITECTURE.md` §6 C4 and `docs/DURABILITY.md` §5. A durable run of a scenario, a fault chosen
//! by seed, and a comparison of what recovered against a twin that never crashed.
//!
//! ## What makes this testable at all
//!
//! I-7 says recovery is byte-identical to a process that never crashed, and that it is *provable
//! because of I-2*. That is the whole design: everything downstream of a sealed epoch is a
//! deterministic function of the log, so "identical to an uncrashed twin" is a comparison rather than
//! a judgement. The harness runs the same scenario twice — once cleanly, once with a fault — and
//! compares state fingerprints and answers at every epoch.
//!
//! ## Determinism, doubly
//!
//! Which seam, which occurrence, which byte offset, and the scenario itself all come from one seed.
//! There is no sleep, no wall clock, and no thread in this crate. A flaky crash test is worse than
//! none, because it teaches people to press re-run.
//!
//! ## What is simulated
//!
//! Faults abort the operation and the harness then drops every in-memory object and recovers from
//! disk. That models loss of everything not yet written, at a named instant. It is **not** a process
//! kill, and the distinction is kept visible in `docs/PROGRESS.md` rather than blurred: the counts of
//! simulated and real kills are reported separately, never added together.

pub mod runtime;
pub mod scenario_fault;

pub use runtime::{
    run_clean, run_with_fault, without_emission_counts, Backend, Config, Durable, RunOutcome,
};
pub use scenario_fault::{Fault, FaultChoice};
