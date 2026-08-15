//! # schweep-bench — the measuring instruments (§6 C10)
//!
//! **The instruments get tested first.** That is this sprint's law, and it exists because of C9's least
//! comfortable result: three of its seven findings were flaws in its own instruments — two assertions that
//! confused a fraction with a percentage, and one that compared a one-quarter span against a two-quarter
//! span — and every one of them *passed* while being wrong. A performance sprint is nothing but numbers.
//! Numbers from uncalibrated instruments are worse than no numbers, because they get quoted.
//!
//! So: no benchmark in this repository reports a figure until `tests/calibration.rs` is green, and the
//! calibration suite is a gate in its own right rather than a helper the benchmarks call.
//!
//! - [`units`] — [`Nanos`], [`Bytes`], [`Count`], [`Ratio`]. A number carries its unit **in its type**, the
//!   units do not mix, and a ratio cannot be printed as a percentage by accident.
//! - [`timer`] — the clock, medians, the full range, and **paired alternating rounds** for comparisons.
//! - [`calibrate`] — synthetic workloads of known *ratio* and known *count*, and the bands they must land
//!   inside.
//! - [`report`] — a benchmark's result, in the form the ledger stores and the README may quote.
//!
//! ## What may be quoted, and in what form
//!
//! Worst-honest form, everywhere. A report prints the **median with the full range beside it**, and where
//! one number is quoted it is the **slowest** run, not the fastest — [`timer::Sample::slowest`]. Every
//! artifact records the machine. No README claim exists without an artifact in `testing/evidence/`, which
//! is I-10 and predates this sprint.
//!
//! ## This crate reads the wall clock
//!
//! It is the only one that may. D-6 and I-2 forbid a clock in *engine and oracle* code so that answers are
//! a function of the log; an instrument that could not read a clock could not measure. Nothing here is
//! linked by the engine, and no measurement feeds back into an answer.

pub mod calibrate;
pub mod report;
pub mod swarm;
pub mod timer;
pub mod units;

pub use calibrate::{counted_work, is_measurable, spin, Band, MIN_WORK_OVER_CLOCK_RESOLUTION};
pub use report::{Benchmark, Machine};
pub use swarm::{marginal_query, near_duplicate_queries};
pub use timer::{
    clock_is_monotonic, clock_resolution_nanos, interleaved, paired, round, sample, Round, Sample,
};
pub use units::{Bytes, Count, Nanos, PercentChange, Ratio};
