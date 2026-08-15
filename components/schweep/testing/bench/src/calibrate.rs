//! **Calibration: the instruments get tested first** (§6 C10, and C9's lesson made law).
//!
//! Three of C9's seven findings were flaws in its own instruments, and all three passed while being wrong.
//! A sprint whose entire output is numbers cannot start from instruments nobody checked, so this module is
//! what a benchmark must clear *before* it is allowed to report anything: synthetic workloads whose cost is
//! known, and bands the measurement has to land inside.
//!
//! ## What "known cost" can and cannot mean
//!
//! It cannot mean a known *duration*. How many nanoseconds a loop takes is a property of the machine, and
//! a band tight enough to be a real check on one machine is a false failure on another. Chasing that is how
//! benchmark suites acquire a reputation for crying wolf until somebody adds a retry.
//!
//! It can mean a known **ratio**, and a known **count**:
//!
//! - **Ratio.** [`spin`] with twice the work must measure about twice the time, on any machine. That is a
//!   statement about the timer and the harness, not about the CPU, and it is the check that catches a
//!   timer that reports garbage, a harness that measures the wrong region, and an optimiser that deleted
//!   the workload. The last is not hypothetical: a spin loop whose result is unused compiles to nothing,
//!   and the ratio then comes out at 1.0 rather than 2.0 — which is exactly what the test would see.
//! - **Count.** [`counted_work`] performs a number of counted operations that is known *exactly*, so the
//!   accounting instrument must report that number and not one near it. An off-by-one in a counter is
//!   invisible in a ratio and fatal in a per-operation cost.
//!
//! ## The bands, and why they are as wide as they are
//!
//! The ratio bands below are wide — ±20% on a doubling — and deliberately so. They are not measuring the
//! machine's precision; they are catching a timer or a harness that is *wrong*, which fails by a factor,
//! not by a percent. A band tight enough to catch a 5% error would fire on a busy CI runner every week and
//! be disabled within a month, which is a worse instrument than none.

use std::hint::black_box;

use crate::units::{Count, Nanos, Ratio};

/// The unit of synthetic work: one iteration of a dependent-arithmetic chain.
///
/// Dependent on purpose — each step needs the previous step's result — so the CPU cannot execute the loop
/// wider than one step at a time and the cost stays close to linear in `units`. An independent loop would
/// be vectorised and superscalar-executed, and doubling its length would not double its time, which would
/// make the linearity check test the optimiser rather than the timer.
#[must_use]
pub fn spin(units: u64) -> u64 {
    let mut state = 0x9E37_79B9_7F4A_7C15u64;
    for _ in 0..units {
        // Wrapping, not checked: this is a hash step, and an overflow is the intended behaviour rather
        // than an error to report.
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        state ^= state >> 31;
        black_box(state);
    }
    // Returned and black-boxed at the call site, so the whole loop cannot be elided.
    black_box(state)
}

/// A workload that performs **exactly** `operations` counted operations and returns the count it actually
/// performed — not the count it was asked for.
///
/// The distinction is the whole point. A workload that returns its argument cannot detect a miscount; one
/// that counts what it did can, and the calibration self-test compares the two.
#[must_use]
pub fn counted_work(operations: u64) -> (Count, u64) {
    let mut performed = 0u64;
    let mut state = 1u64;
    for _ in 0..operations {
        state = state.wrapping_mul(31).wrapping_add(7);
        performed += 1;
        black_box(state);
    }
    (Count(performed), black_box(state))
}

/// A band a measured ratio must land inside, with the reason it is this wide carried alongside it.
#[derive(Clone, Copy, Debug)]
pub struct Band {
    pub low: f64,
    pub high: f64,
    pub expected: f64,
    pub because: &'static str,
}

impl Band {
    /// A band around an expected ratio, as a fraction either side. `tolerance` of 0.2 on an expected 2.0
    /// admits 1.6 to 2.4.
    #[must_use]
    pub fn around(expected: f64, tolerance: f64, because: &'static str) -> Band {
        Band {
            low: expected * (1.0 - tolerance),
            high: expected * (1.0 + tolerance),
            expected,
            because,
        }
    }

    #[must_use]
    pub fn admits(&self, ratio: Ratio) -> bool {
        ratio.is_within(self.low, self.high)
    }

    /// The failure message, which names the band, the reading, and what a reading outside it means.
    #[must_use]
    pub fn complain(&self, ratio: Ratio, what: &str) -> String {
        format!(
            "{what}: measured {ratio}, outside the band {:.2}×..{:.2}× around {:.2}×.\n  \
             The band is this wide because: {}\n  \
             A reading outside it is an instrument fault, not a slow machine — a machine that is merely \
             slow moves both sides of a ratio.",
            self.low, self.high, self.expected, self.because
        )
    }
}

/// The multiple of the clock's resolution that a benchmark's unit of work must exceed.
///
/// A thousand: at a 40 ns resolution that is a 40 µs floor, which every workload in this suite clears by
/// orders of magnitude. Stated as a constant so the calibration test can enforce it rather than each
/// benchmark remembering to.
pub const MIN_WORK_OVER_CLOCK_RESOLUTION: u64 = 1_000;

/// Is `elapsed` far enough above the clock's resolution to be a measurement rather than a reading of the
/// clock itself?
#[must_use]
pub fn is_measurable(elapsed: Nanos, resolution: Nanos) -> bool {
    elapsed.0 >= resolution.0.saturating_mul(MIN_WORK_OVER_CLOCK_RESOLUTION)
}
