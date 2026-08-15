//! **THE CALIBRATION GATE** — the instruments, before any benchmark reports a number (§6 C10).
//!
//! C9 shipped three instruments that were wrong and passed anyway. This file is the sprint's answer, and
//! its standing is that of a gate rather than a helper: if it is red, every number the suite produces is
//! withdrawn, because there is no way to tell which of them the fault touched.
//!
//! What it checks, and what each check would catch:
//!
//! | Check | Catches |
//! | --- | --- |
//! | the clock is monotonic | a timer that runs backwards, which subtraction turns into a huge positive |
//! | the clock's resolution is measured | a benchmark whose unit of work is a reading of the clock |
//! | timing is **linear in work** | a deleted workload, a harness timing the wrong region, a broken timer |
//! | counting is **exact** | an off-by-one in an accounting instrument, invisible to every ratio |
//! | harness overhead is small | a per-round cost attributed to the workload |
//! | pairing is order-insensitive | a comparison that measures which side ran first |
//! | units do not silently convert | C9's fraction-as-percentage bug, in the type system |
//!
//! Everything here is about *ratios and counts*, never about absolute durations, because an absolute
//! duration is a property of the machine and a band tight enough to check one would fail on another. See
//! `calibrate.rs` for that argument in full.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use schweep_bench::calibrate::{counted_work, is_measurable, spin, Band};
use schweep_bench::timer::{clock_is_monotonic, clock_resolution_nanos, paired, sample};
use schweep_bench::units::{Count, Nanos, Ratio};

/// Work sizes for the linearity checks.
///
/// **Smaller in debug, and that is not a shortcut.** Every check here is about a *ratio* or a *count*, and
/// neither needs a large absolute workload — what a workload must be is far above the clock's resolution,
/// which `the_clocks_resolution_is_measured_and_the_workloads_clear_it` enforces at a thousandfold rather
/// than leaving to judgement. Measured: this file took **three minutes** in CI's debug build at the release
/// size, on every push. A three-minute gate on every push is a gate somebody eventually routes around,
/// which is a worse instrument than a smaller one that runs.
const UNIT_WORK: u64 = if cfg!(debug_assertions) {
    50_000
} else {
    2_000_000
};

/// Rounds per sample. Odd, so the median is a measured value rather than the mean of two.
const ROUNDS: usize = 9;

/// Time `spin(units)` and return the median. Used where only one size is measured; the linearity check
/// measures its three sizes **interleaved**, for the reason `timer::interleaved` documents.
fn median_spin_nanos(units: u64) -> Nanos {
    sample(format!("spin({units})"), ROUNDS, || {
        (Count(units), spin(units))
    })
    .median()
    .expect("a sample of nine rounds has a median")
}

#[test]
fn the_clock_does_not_run_backwards() {
    assert!(
        clock_is_monotonic(200_000),
        "Instant went backwards on this machine. Every duration in this crate is a subtraction, so this \
         would surface as an enormous positive reading rather than as an error — which is why Nanos \
         saturates, and why this is checked rather than assumed."
    );
}

#[test]
fn the_clocks_resolution_is_measured_and_the_workloads_clear_it() {
    let resolution = clock_resolution_nanos();
    println!("clock resolution: {} ({resolution})", resolution.describe());
    assert!(
        !resolution.is_zero(),
        "the clock reports a zero resolution, which means the probe measured nothing"
    );
    assert!(
        resolution.0 < 10_000,
        "the clock's resolution is {resolution}, coarser than 10 µs. Every band in this suite assumes a \
         finer clock than that; on a machine this coarse the numbers would be quantisation."
    );

    // And the smallest workload the suite uses must be far above it.
    let smallest = median_spin_nanos(UNIT_WORK);
    println!("smallest calibration workload: {}", smallest.describe());
    assert!(
        is_measurable(smallest, resolution),
        "the smallest workload takes {smallest} against a clock resolution of {resolution}; a benchmark \
         must do at least {}× the resolution or it is measuring the clock",
        schweep_bench::MIN_WORK_OVER_CLOCK_RESOLUTION
    );
}

/// **The linearity check.** Twice the work, twice the time — on any machine.
///
/// This is the check that would have caught the whole class of C9's instrument bugs, because it is a
/// statement the instrument makes about itself rather than about the thing it measures. Note what it
/// catches that nothing else does: an optimiser that deleted the workload reports a ratio near 1.0, and a
/// harness timing the wrong region reports a ratio near 1.0, and both look like a fast machine otherwise.
#[test]
fn twice_the_work_takes_about_twice_the_time() {
    // **Interleaved, not one sample after another.** Measured while writing this file: three consecutive
    // samples put the doubling ratio anywhere between 1.73 and 1.98 across five back-to-back runs, because
    // the machine drifts over the ~100 ms between the first size and the last and the first size wears all
    // of it. Interleaving pays the drift to every size at once.
    let mut one_work = || (Count(UNIT_WORK), spin(UNIT_WORK)).0;
    let mut two_work = || (Count(UNIT_WORK * 2), spin(UNIT_WORK * 2)).0;
    let mut four_work = || (Count(UNIT_WORK * 4), spin(UNIT_WORK * 4)).0;
    let samples = schweep_bench::interleaved(
        &["1x", "2x", "4x"],
        ROUNDS,
        &mut [&mut one_work, &mut two_work, &mut four_work],
    );
    let one = samples[0].median().expect("a sample has a median");
    let two = samples[1].median().expect("a sample has a median");
    let four = samples[2].median().expect("a sample has a median");

    let doubling = two.ratio_to(one).expect("a non-zero baseline");
    let quadrupling = four.ratio_to(one).expect("a non-zero baseline");
    println!(
        "linearity: 1× {} · 2× {} ({doubling}) · 4× {} ({quadrupling})",
        one.describe(),
        two.describe(),
        four.describe()
    );

    let double_band = Band::around(
        2.0,
        0.2,
        "a doubling of a dependent-arithmetic chain is linear to within scheduling noise; ±20% catches an \
         instrument that is wrong by a factor without firing on a busy runner",
    );
    let quadruple_band = Band::around(
        4.0,
        0.2,
        "the same argument at four times the work, which also catches a super-linear harness cost that a \
         doubling could hide",
    );
    assert!(
        double_band.admits(doubling),
        "{}",
        double_band.complain(doubling, "doubling the work")
    );
    assert!(
        quadruple_band.admits(quadrupling),
        "{}",
        quadruple_band.complain(quadrupling, "quadrupling the work")
    );
}

/// **The counting check.** A workload that performs exactly N operations is counted as exactly N.
///
/// Exactly, not approximately: an off-by-one is invisible in every ratio and fatal in a per-operation cost,
/// which is the number the swarm benchmark exists to report.
#[test]
fn a_workload_of_known_count_is_counted_exactly() {
    for asked in [0u64, 1, 2, 1_000, 999_983] {
        let (performed, _) = counted_work(asked);
        assert_eq!(
            performed,
            Count(asked),
            "a workload asked for {asked} operations reported {performed}. The count a benchmark \
             publishes comes from the workload, not from the caller's belief about the workload, and this \
             is the check that keeps those two the same number."
        );
    }
}

/// The harness's own cost must be small next to the work it times, or it is part of every number.
#[test]
fn the_harness_costs_little_next_to_the_work_it_times() {
    let empty = sample("empty", 101, || (Count(0), ())).median().unwrap();
    let real = median_spin_nanos(UNIT_WORK);
    println!(
        "harness overhead per round: {} against a workload of {}",
        empty.describe(),
        real.describe()
    );
    let share = empty.ratio_to(real).unwrap_or(Ratio(0.0));
    assert!(
        share.get() < 0.001,
        "the harness costs {empty} per round against a {real} workload ({share} of it). Anything above a \
         thousandth is a systematic addition to every measurement rather than noise around it."
    );
}

/// **Pairing must not measure which side ran first.**
///
/// The DuckDB comparison lives or dies on this: two implementations timed one after the other on a machine
/// that drifts will report the drift as a difference between them. The check runs the *same* workload as
/// both sides of a paired comparison — so the true answer is 1.0 — and requires the measured ratio to say
/// so.
#[test]
fn pairing_the_same_workload_against_itself_reports_no_difference() {
    let (left, right) = paired(
        "identical-a",
        "identical-b",
        12,
        || (Count(UNIT_WORK), spin(UNIT_WORK)),
        || (Count(UNIT_WORK), spin(UNIT_WORK)),
    );
    let ratio = left
        .median()
        .unwrap()
        .ratio_to(right.median().unwrap())
        .unwrap();
    println!(
        "{}\n{}\npaired ratio: {ratio}",
        left.render(),
        right.render()
    );

    let band = Band::around(
        1.0,
        0.15,
        "the two sides run identical work, so the true ratio is exactly 1.0; ±15% is the machine's own \
         round-to-round variation, and a reading outside it means the pairing is measuring order",
    );
    assert!(
        band.admits(ratio),
        "{}",
        band.complain(ratio, "the same workload paired against itself")
    );
}

/// A sample reports its **range**, and the slowest round is available as the number to quote.
///
/// Worst-honest form is a property of the instrument here, not a habit of whoever writes the report: if
/// `slowest()` were missing, every report would quote a median and the range would become a footnote.
#[test]
fn a_sample_reports_its_range_and_its_slowest_round() {
    let taken = sample("ranged", 7, || (Count(UNIT_WORK), spin(UNIT_WORK)));
    let (fastest, median, slowest) = (
        taken.fastest().unwrap(),
        taken.median().unwrap(),
        taken.slowest().unwrap(),
    );
    assert!(fastest <= median && median <= slowest, "{}", taken.render());
    assert!(
        taken.render().contains("range"),
        "a rendered sample must show its range next to its median: {}",
        taken.render()
    );
    assert!(taken.spread().unwrap().get() >= 1.0);
}

/// An empty sample reports nothing rather than zero, because zero is a number somebody will quote.
#[test]
fn an_empty_sample_has_no_median_rather_than_a_median_of_zero() {
    let empty = schweep_bench::Sample::new("nothing");
    assert!(empty.median().is_none());
    assert!(empty.slowest().is_none());
    assert!(empty.render().contains("no rounds"));
}
