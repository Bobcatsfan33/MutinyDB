//! The crash gate at `SyncPolicy::Full` — the nightly job (`docs/DURABILITY.md` §5).
//!
//! **What this adds, honestly: nothing observable.** An in-process crash cannot see the difference
//! between a synced and an unsynced write, because `write_all` has already put the bytes in the page
//! cache and the page cache survives a simulated crash. This job does not test power loss, and no
//! count it prints should be read as if it did.
//!
//! **What it is for: the `fsync` path cannot rot.** The equivalence gate runs `Deferred` because
//! `Full` costs minutes rather than seconds, which means every `sync_all` call in the log and the
//! checkpoint protocol would otherwise be exercised only by a handful of unit tests. A path that is
//! never run in bulk is a path that quietly stops compiling, stops being reached, or starts erroring
//! on a filesystem nobody tried. Running the real thing on a schedule keeps that honest.
//!
//! Marked `#[ignore]` so it never runs in the ordinary suite; the nightly workflow runs it explicitly.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::path::PathBuf;

use schweep_crash::{run_clean, run_with_fault, Config, FaultChoice};
use schweep_differential::{CircuitEngine, Scenario};

fn dir_for(seed: u64, tag: &str) -> PathBuf {
    let base = std::env::temp_dir().join(format!("schweep-c4-nightly-{tag}-{seed}"));
    let _ = std::fs::remove_dir_all(&base);
    base
}

/// The same comparison as the 10,000-cycle gate, with every write synced.
///
/// Fewer cycles, because each one performs real `fsync` calls: the number is chosen so the job takes
/// minutes rather than hours, and it is stated rather than tuned silently.
#[test]
#[ignore = "nightly: runs the crash gate with real fsync; scheduled, not part of the ordinary suite"]
fn crash_and_recover_with_real_fsync() {
    const CYCLES: u64 = 400;

    let mut cycles = 0u64;
    let mut faults_fired = 0u64;
    let mut bootstrapped = 0u64;
    let mut seed = 0u64;

    while cycles < CYCLES {
        seed += 1;
        let Some(scenario) = Scenario::generate(seed)
            .ok()
            .filter(|s| CircuitEngine::claims(s) && !s.epochs.is_empty())
        else {
            continue;
        };

        // The only difference from the ordinary gate.
        let config = Config::durable();
        let choice = FaultChoice::for_seed(seed);

        let clean_dir = dir_for(seed, "clean");
        let clean = run_clean(&clean_dir, &scenario, config)
            .unwrap_or_else(|e| panic!("seed {seed}: the clean run failed: {e}"));

        let crash_dir = dir_for(seed, "crash");
        let (recovered, fired) = run_with_fault(&crash_dir, &scenario, choice.fault, config)
            .unwrap_or_else(|e| panic!("seed {seed}: recovery failed: {e}"));
        if fired.is_some() {
            faults_fired += 1;
        }

        assert_eq!(
            recovered.answers.last(),
            clean.answers.last(),
            "seed {seed}: the recovered answer differs from the uncrashed twin (I-7)"
        );
        // The same bootstrap allowance the ordinary gate makes (C7): a recovery that had to rebuild
        // from the snapshot holds the same state by a different route, so its I-9 emission counts
        // differ. `Config::durable()` compacts like the ordinary gate, so this path is reached here too.
        let (recovered_state, clean_state) = if recovered.bootstrapped {
            bootstrapped += 1;
            (
                recovered
                    .fingerprints
                    .last()
                    .map(|f| schweep_crash::without_emission_counts(f)),
                clean
                    .fingerprints
                    .last()
                    .map(|f| schweep_crash::without_emission_counts(f)),
            )
        } else {
            (
                recovered.fingerprints.last().cloned(),
                clean.fingerprints.last().cloned(),
            )
        };
        assert_eq!(
            recovered_state, clean_state,
            "seed {seed}: the recovered state differs from the uncrashed twin (I-7)"
        );
        assert_eq!(
            recovered.log, clean.log,
            "seed {seed}: the recovered log differs from the uncrashed twin (I-4)"
        );

        let _ = std::fs::remove_dir_all(&clean_dir);
        let _ = std::fs::remove_dir_all(&crash_dir);
        cycles += 1;
    }

    println!(
        "C4 nightly (SyncPolicy::Full): {cycles} cycles over {seed} seeds · {faults_fired} faults \
         fired · {bootstrapped} recovered by bootstrap · every write fsynced. This observes nothing an in-process crash could not observe \
         at Deferred; it exists so the fsync path cannot rot."
    );
    assert_eq!(cycles, CYCLES);
    assert!(
        faults_fired > CYCLES / 4,
        "only {faults_fired} of {cycles} cycles fired a fault; a crash job that injects nothing \
         proves nothing"
    );

    // `Config::durable()` is the only thing that makes this job different from the ordinary gate, so
    // it is asserted rather than assumed.
    assert_eq!(
        Config::durable().sync,
        schweep_log::SyncPolicy::Full,
        "the nightly job must run with every write synced, or it is the ordinary gate again"
    );
}
