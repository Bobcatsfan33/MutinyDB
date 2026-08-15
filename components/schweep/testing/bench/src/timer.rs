//! The clock, and what a sample of it means.
//!
//! **This crate reads the wall clock, and that is allowed here and nowhere else.** The hard rule (D-6,
//! I-2) is that no *engine or oracle* code consults a clock; a measuring instrument that could not measure
//! time would be no instrument at all. Nothing in this crate is linked by the engine — it is a test-only
//! crate, like `schweep-crash` and `schweep-soak` — and no measurement it takes feeds back into any answer.
//!
//! `Instant` rather than `SystemTime`: monotonic, unaffected by NTP stepping, and the calibration
//! self-tests check the monotonicity rather than trusting the documentation.
//!
//! ## What a benchmark reports, and why it is the median of paired rounds
//!
//! A single timing is noise with a number attached. This crate reports the **median** of repeated rounds
//! and publishes the **full range** alongside it, because the median alone hides a bimodal machine and the
//! range is the honest part. For comparisons between two implementations it reports **paired** rounds —
//! A then B, B then A, alternating — so that a machine which slows down half-way through slows down both
//! sides equally. That is the method §6 C10 names for the DuckDB comparison, and it is used for every
//! comparison here rather than only that one.

use std::time::Instant;

use crate::units::{Count, Nanos, Ratio};

/// One timed run of something.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Round {
    pub elapsed: Nanos,
    /// What the round did, in whatever unit the workload counts — rows, epochs, queries.
    pub work: Count,
}

/// A set of rounds, and the statistics a report is allowed to quote from it.
#[derive(Clone, Debug, Default)]
pub struct Sample {
    pub label: String,
    pub rounds: Vec<Round>,
}

impl Sample {
    #[must_use]
    pub fn new(label: impl Into<String>) -> Sample {
        Sample {
            label: label.into(),
            rounds: Vec::new(),
        }
    }

    pub fn push(&mut self, round: Round) {
        self.rounds.push(round);
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.rounds.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.rounds.is_empty()
    }

    /// The median elapsed time. `None` for an empty sample — a benchmark that ran nothing reports nothing
    /// rather than zero, because zero is a number somebody will quote.
    #[must_use]
    pub fn median(&self) -> Option<Nanos> {
        if self.rounds.is_empty() {
            return None;
        }
        let mut times: Vec<u64> = self.rounds.iter().map(|r| r.elapsed.0).collect();
        times.sort_unstable();
        let middle = times.len() / 2;
        Some(Nanos(if times.len() % 2 == 1 {
            times.get(middle).copied().unwrap_or(0)
        } else {
            let a = times.get(middle - 1).copied().unwrap_or(0);
            let b = times.get(middle).copied().unwrap_or(0);
            (a + b) / 2
        }))
    }

    #[must_use]
    pub fn fastest(&self) -> Option<Nanos> {
        self.rounds.iter().map(|r| r.elapsed).min()
    }

    /// **The number this project quotes when it quotes one number.** §6 C10's rule, and the user's:
    /// worst-honest form, the slowest run, never the fastest.
    #[must_use]
    pub fn slowest(&self) -> Option<Nanos> {
        self.rounds.iter().map(|r| r.elapsed).max()
    }

    /// Spread, as slowest ÷ fastest. A sample whose spread is large is a sample whose median means less,
    /// and every report prints this next to the median rather than in a footnote.
    #[must_use]
    pub fn spread(&self) -> Option<Ratio> {
        self.slowest()?.ratio_to(self.fastest()?)
    }

    #[must_use]
    pub fn total_work(&self) -> Count {
        Count(self.rounds.iter().map(|r| r.work.0).sum())
    }

    /// A line for a report: median, range, spread, and what was done.
    #[must_use]
    pub fn render(&self) -> String {
        match (self.median(), self.fastest(), self.slowest(), self.spread()) {
            (Some(median), Some(fast), Some(slow), Some(spread)) => format!(
                "{}: median {} · range {}..{} · spread {} · {} rounds · work {}",
                self.label,
                median.describe(),
                fast.describe(),
                slow.describe(),
                spread,
                self.rounds.len(),
                self.total_work().0
            ),
            _ => format!("{}: no rounds", self.label),
        }
    }
}

/// Time one closure once. The closure returns what it did, so the count comes from the workload rather
/// than from the caller's belief about the workload.
pub fn round<T>(work: impl FnOnce() -> (Count, T)) -> (Round, T) {
    let start = Instant::now();
    let (count, value) = work();
    let elapsed = start.elapsed();
    (
        Round {
            // `as u64` cannot truncate anything a benchmark produces: it would take 584 years.
            elapsed: Nanos(elapsed.as_nanos().min(u128::from(u64::MAX)) as u64),
            work: count,
        },
        value,
    )
}

/// Time a closure `rounds` times, returning the sample.
pub fn sample<T>(
    label: impl Into<String>,
    rounds: usize,
    mut work: impl FnMut() -> (Count, T),
) -> Sample {
    let mut out = Sample::new(label);
    for _ in 0..rounds {
        let (round, _value) = round(&mut work);
        out.push(round);
    }
    out
}

/// Two samples measured **paired and alternating**: A, B, B, A, A, B, …
///
/// The alternation is the point. A machine that gets slower over a run — thermal throttling, a noisy
/// neighbour, a cache filling — biases whichever side ran first, and running all of A then all of B bakes
/// that bias into the comparison. Alternating, and reversing the order on alternate rounds, spreads it
/// across both sides.
pub fn paired<A, B>(
    label_a: impl Into<String>,
    label_b: impl Into<String>,
    rounds: usize,
    mut a: impl FnMut() -> (Count, A),
    mut b: impl FnMut() -> (Count, B),
) -> (Sample, Sample) {
    let mut sample_a = Sample::new(label_a);
    let mut sample_b = Sample::new(label_b);
    for index in 0..rounds {
        if index % 2 == 0 {
            let (ra, _) = round(&mut a);
            let (rb, _) = round(&mut b);
            sample_a.push(ra);
            sample_b.push(rb);
        } else {
            let (rb, _) = round(&mut b);
            let (ra, _) = round(&mut a);
            sample_b.push(rb);
            sample_a.push(ra);
        }
    }
    (sample_a, sample_b)
}

/// **N-way paired measurement**: every workload timed once per round, in a rotating order.
///
/// [`paired`] does this for two; this does it for any number, and the rotation is what makes it work. A
/// machine drifts — thermal, a neighbour, the scheduler — over the tens of milliseconds it takes to measure
/// three workloads one after another, and a comparison between them then reports the drift. Measured on the
/// calibration workloads: timing 1×, 2× and 4× as three consecutive samples gave doubling ratios between
/// 1.73 and 1.98 across five back-to-back runs, with the *first* size drifting 16% while the others held.
/// Interleaved, the same measurement is stable, because each round pays whatever the machine is doing at
/// that moment to all three sizes at once.
///
/// The rotation offsets by the round index, so no workload is systematically first, last, or adjacent to
/// the same neighbour.
pub fn interleaved(
    labels: &[&str],
    rounds: usize,
    workloads: &mut [&mut dyn FnMut() -> Count],
) -> Vec<Sample> {
    let mut samples: Vec<Sample> = labels.iter().map(|label| Sample::new(*label)).collect();
    let width = workloads.len();
    if width == 0 || samples.len() != width {
        return samples;
    }
    for index in 0..rounds {
        for step in 0..width {
            let which = (index + step) % width;
            let Some(workload) = workloads.get_mut(which) else {
                continue;
            };
            let start = Instant::now();
            let count = workload();
            let elapsed = start.elapsed();
            if let Some(sample) = samples.get_mut(which) {
                sample.push(Round {
                    elapsed: Nanos(elapsed.as_nanos().min(u128::from(u64::MAX)) as u64),
                    work: count,
                });
            }
        }
    }
    samples
}

/// The smallest non-zero interval this machine's clock can report, measured rather than assumed.
///
/// Every benchmark in this crate must do work that is large next to this figure; the calibration self-test
/// states the multiple and enforces it. A benchmark whose unit of work is near the clock's resolution is
/// measuring the clock.
#[must_use]
pub fn clock_resolution_nanos() -> Nanos {
    let mut smallest = u64::MAX;
    for _ in 0..1_000 {
        let start = Instant::now();
        let mut delta = start.elapsed().as_nanos() as u64;
        // Spin until the clock moves at all: the first non-zero reading is the resolution.
        while delta == 0 {
            delta = start.elapsed().as_nanos() as u64;
        }
        smallest = smallest.min(delta);
    }
    Nanos(smallest)
}

/// Does this machine's clock ever appear to run backwards across `samples` readings?
///
/// Checked rather than assumed: every duration in this crate is computed by subtraction, and
/// [`crate::units::Nanos`] saturates rather than wrapping precisely because the answer might one day be
/// yes on some machine.
#[must_use]
pub fn clock_is_monotonic(samples: usize) -> bool {
    let start = Instant::now();
    let mut previous = start.elapsed();
    for _ in 0..samples {
        let now = start.elapsed();
        if now < previous {
            return false;
        }
        previous = now;
    }
    true
}
