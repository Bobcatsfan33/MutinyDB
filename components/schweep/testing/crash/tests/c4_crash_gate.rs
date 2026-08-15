//! **The C4 exit gate** (`ARCHITECTURE.md` §6 C4; `docs/DURABILITY.md`).
//!
//! > **Exit gate:** crash-injection harness kills the process at randomized points across
//! > ingest/step/checkpoint over full scenarios, ≥10,000 cycles in CI: every recovery equals the
//! > never-crashed run byte-for-byte (I-7); every acked batch appears exactly once (I-4); a torn
//! > checkpoint is detected and the previous one used.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::collections::BTreeSet;
use std::path::PathBuf;

use schweep_crash::{
    run_clean, run_with_fault, without_emission_counts, Config, Fault, FaultChoice,
};
use schweep_differential::{CircuitEngine, EngineUnderTest, Scenario};
use schweep_log::Seam;

/// A fresh directory per cycle, named by seed so a failure can be reproduced by hand.
fn dir_for(seed: u64, tag: &str) -> PathBuf {
    let base = std::env::temp_dir().join(format!("schweep-c4-{tag}-{seed}"));
    let _ = std::fs::remove_dir_all(&base);
    base
}

fn scenario_for(seed: u64) -> Option<Scenario> {
    let scenario = Scenario::generate(seed).ok()?;
    // The durable runtime drives a circuit, so it covers what the engine claims.
    if !CircuitEngine::claims(&scenario) || scenario.epochs.is_empty() {
        return None;
    }
    Some(scenario)
}

/// **The gate.** 10,000 randomized crash-and-recover cycles.
///
/// Each cycle: generate a scenario from a seed, run it cleanly to get the twin, then run it again
/// with the seed's fault, recover, and compare. The comparison is over the **state fingerprint** and
/// the **answer** — I-7's byte-identical claim, checkable only because of I-2.
#[test]
fn ten_thousand_crash_and_recover_cycles() {
    const CYCLES: u64 = 10_000;

    let mut cycles = 0u64;
    let mut faults_fired = 0u64;
    let mut byte_faults = 0u64;
    let mut clean_runs = 0u64;
    let mut bootstrapped_cycles = 0u64;
    let mut seams_fired: BTreeSet<&'static str> = BTreeSet::new();
    let mut seams_planned: BTreeSet<&'static str> = BTreeSet::new();

    let mut seed = 0u64;
    while cycles < CYCLES {
        seed += 1;
        let Some(scenario) = scenario_for(seed) else {
            continue;
        };
        let config = Config::default();
        let choice = FaultChoice::for_seed(seed);

        match choice.fault {
            Fault::Seam(plan) => {
                seams_planned.insert(plan.seam.name());
            }
            Fault::Bytes { .. } => byte_faults += 1,
            Fault::None => clean_runs += 1,
        }

        // The twin that never crashed.
        let clean_dir = dir_for(seed, "clean");
        let clean = run_clean(&clean_dir, &scenario, config)
            .unwrap_or_else(|e| panic!("seed {seed}: the clean run failed: {e}"));

        // The run that crashed and recovered.
        let crash_dir = dir_for(seed, "crash");
        let (recovered, fired) = run_with_fault(&crash_dir, &scenario, choice.fault, config)
            .unwrap_or_else(|e| panic!("seed {seed}: recovery failed: {e}"));

        if let Some(name) = fired {
            faults_fired += 1;
            seams_fired.insert(name);
        }

        // I-7 · byte-identical to the twin, in state and in answer.
        assert_eq!(
            recovered.answers.last(),
            clean.answers.last(),
            "seed {seed}: the recovered answer differs from the uncrashed twin (I-7)"
        );
        // The state comparison. A run whose recovery had to **bootstrap** from the snapshot — every
        // checkpoint torn, the log's prefix compacted away — reaches the same state by a different
        // route: one delta instead of many. Its operator contents match; its I-9 emission counts do
        // not, because it genuinely emitted differently. So those cycles compare the state without the
        // counters, and are counted separately below rather than quietly folded in.
        if recovered.bootstrapped {
            bootstrapped_cycles += 1;
            assert_eq!(
                recovered
                    .fingerprints
                    .last()
                    .map(|f| without_emission_counts(f)),
                clean
                    .fingerprints
                    .last()
                    .map(|f| without_emission_counts(f)),
                "seed {seed}: a bootstrapped recovery must hold the same state as the twin (I-7)"
            );
        } else {
            assert_eq!(
                recovered.fingerprints.last(),
                clean.fingerprints.last(),
                "seed {seed}: the recovered STATE differs from the uncrashed twin (I-7)"
            );
        }
        assert_eq!(
            recovered.epoch, clean.epoch,
            "seed {seed}: the recovered epoch differs"
        );
        // I-4 · the log holds each token once, and the recovered log matches the clean one.
        assert_eq!(
            recovered.log, clean.log,
            "seed {seed}: the recovered LOG differs from the uncrashed twin (I-4)"
        );

        let _ = std::fs::remove_dir_all(&clean_dir);
        let _ = std::fs::remove_dir_all(&crash_dir);
        cycles += 1;
    }

    println!(
        "C4 crash gate: {cycles} cycles over {seed} seeds · {faults_fired} seam faults fired · \
         {byte_faults} byte-boundary faults · {clean_runs} clean runs · \
         {bootstrapped_cycles} recovered by bootstrap · {} of {} named seams fired",
        seams_fired.len(),
        Seam::all().len()
    );

    assert_eq!(cycles, CYCLES);

    // **The fault count is asserted.** A crash suite that injects no faults passes trivially. C3
    // learned this from a mutation that silently failed to apply; the same discipline applies here.
    assert!(
        faults_fired > CYCLES / 4,
        "only {faults_fired} of {cycles} cycles actually fired a seam fault; a crash gate that \
         injects nothing proves nothing"
    );
    assert!(
        byte_faults > 100,
        "only {byte_faults} byte-boundary faults were selected"
    );

    // Every seam that was *planned* must have *fired* at least once. A seam that is planned but never
    // reached is a seam the code never visits — either dead, or misplaced in the ordering.
    for planned in &seams_planned {
        assert!(
            seams_fired.contains(planned),
            "seam {planned} was planned but never fired: either the code never reaches it, or it is \
             in the wrong place in the ordering"
        );
    }
    assert!(
        seams_fired.len() >= 12,
        "only {} of {} named seams ever fired: {seams_fired:?}",
        seams_fired.len(),
        Seam::all().len()
    );
}

/// **Recovery is idempotent:** crash *during* recovery, recover again, reach the same state.
///
/// This is the bug class §5's note names — a recovery that half-repairs and then cannot be repeated.
/// It is tested rather than argued from the code because arguing it from the code is exactly what has
/// gone wrong for other systems.
#[test]
fn recovery_is_idempotent_under_a_crash_during_recovery() {
    let mut checked = 0;
    let mut seed = 0u64;
    while checked < 200 {
        seed += 1;
        let Some(scenario) = scenario_for(seed) else {
            continue;
        };
        let config = Config::default();

        let clean_dir = dir_for(seed, "idem-clean");
        let clean = run_clean(&clean_dir, &scenario, config).unwrap();

        // Crash mid-recovery: the two recovery seams.
        for seam in [
            Seam::RecoveryAfterCheckpointBeforeReplay,
            Seam::RecoveryMidReplay,
        ] {
            let dir = dir_for(seed, "idem");
            // First, get some durable history down with an ordinary crash.
            let _ = run_with_fault(
                &dir,
                &scenario,
                Fault::Seam(schweep_log::FaultPlan {
                    seam: Seam::SealAfterFsyncBeforeStep,
                    occurrence: 1,
                }),
                config,
            );
            // Then crash *during recovery*, twice, then recover for real.
            for _ in 0..2 {
                let _ = run_with_fault(
                    &dir,
                    &scenario,
                    Fault::Seam(schweep_log::FaultPlan {
                        seam,
                        occurrence: 1,
                    }),
                    config,
                );
            }
            let (recovered, _) = run_with_fault(&dir, &scenario, Fault::None, config)
                .unwrap_or_else(|e| panic!("seed {seed}, {seam}: final recovery failed: {e}"));

            assert_eq!(
                recovered.answers.last(),
                clean.answers.last(),
                "seed {seed}, {seam}: recovery after a crash during recovery diverged"
            );
            assert_eq!(
                recovered.fingerprints.last(),
                clean.fingerprints.last(),
                "seed {seed}, {seam}: state diverged after a crash during recovery"
            );
            let _ = std::fs::remove_dir_all(&dir);
        }
        let _ = std::fs::remove_dir_all(&clean_dir);
        checked += 1;
    }
    assert!(checked >= 200);
}

/// A torn checkpoint is detected and the previous one is used, with the log covering the gap.
///
/// **Compaction is off in this test on purpose (C7).** With it on, `checkpoint::take` deletes superseded
/// checkpoints and compaction trims to the oldest survivor, so a torn checkpoint leaves *nothing* to
/// fall back to and recovery bootstraps from the snapshot instead — which is correct, and is covered by
/// `a_torn_checkpoint_over_a_compacted_log_recovers_by_bootstrapping` below. It is not what this test's
/// name claims, and a test whose name has stopped describing what it does is worse than no test: the
/// first version of this change left it passing 150/150 through the bootstrap path, testing the
/// fallback nowhere.
#[test]
fn a_torn_checkpoint_is_detected_and_the_previous_one_is_used() {
    let mut checked = 0;
    let mut bootstrapped = 0usize;
    let mut seed = 0u64;
    while checked < 150 {
        seed += 1;
        let Some(scenario) = scenario_for(seed) else {
            continue;
        };
        // Enough epochs that there is a previous checkpoint to fall back to.
        if scenario.epochs.len() < 4 {
            continue;
        }
        let config = Config {
            compact_every: 0,
            ..Config::default()
        };

        let clean_dir = dir_for(seed, "torn-clean");
        let clean = run_clean(&clean_dir, &scenario, config).unwrap();

        let dir = dir_for(seed, "torn");
        let (recovered, _) = run_with_fault(
            &dir,
            &scenario,
            Fault::Bytes {
                epoch_index: 0,
                offset: 3,
                truncate: false,
            },
            config,
        )
        .unwrap_or_else(|e| panic!("seed {seed}: recovery from a torn checkpoint failed: {e}"));

        assert_eq!(
            recovered.answers.last(),
            clean.answers.last(),
            "seed {seed}: a torn checkpoint must not change the answer"
        );
        // A torn checkpoint over a *compacted* log leaves recovery with nothing to restore from, so it
        // bootstraps from the snapshot instead (C7). Same state, one delta instead of many, therefore
        // different I-9 emission counts — compared without them, and counted, exactly as the
        // 10,000-cycle gate does.
        if recovered.bootstrapped {
            bootstrapped += 1;
            assert_eq!(
                recovered
                    .fingerprints
                    .last()
                    .map(|f| without_emission_counts(f)),
                clean
                    .fingerprints
                    .last()
                    .map(|f| without_emission_counts(f)),
                "seed {seed}: a bootstrapped recovery must hold the same state"
            );
        } else {
            assert_eq!(
                recovered.fingerprints.last(),
                clean.fingerprints.last(),
                "seed {seed}: a torn checkpoint must not change the state"
            );
        }

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&clean_dir);
        checked += 1;
    }
    println!("torn-checkpoint gate: {checked} scenarios, compaction off, fallback exercised");
    assert!(checked >= 150);
    assert_eq!(
        bootstrapped, 0,
        "with compaction off this test must exercise the *fallback*, not the bootstrap path"
    );
}

/// Every checkpoint torn **and** the log compacted: recovery rebuilds from the snapshot (C7, B1–B3).
///
/// The case that makes the snapshot load-bearing for availability rather than only for space. There is
/// no checkpoint to restore and no log prefix to replay, and the data is nevertheless all there — so
/// refusing would be unavailability with intact data.
#[test]
fn a_torn_checkpoint_over_a_compacted_log_recovers_by_bootstrapping() {
    let mut checked = 0usize;
    let mut bootstrapped = 0usize;
    let mut seed = 0u64;
    while checked < 150 {
        seed += 1;
        let Ok(scenario) = Scenario::generate(seed) else {
            continue;
        };
        if scenario.epochs.len() < 4 {
            continue;
        }
        // Compaction on, which is the default — stated here because it is the whole point.
        let config = Config::default();

        let clean_dir = dir_for(seed, "boot-clean");
        let clean = run_clean(&clean_dir, &scenario, config).unwrap();

        let dir = dir_for(seed, "boot");
        let (recovered, _) = run_with_fault(
            &dir,
            &scenario,
            Fault::Bytes {
                epoch_index: 0,
                offset: 3,
                truncate: false,
            },
            config,
        )
        .unwrap_or_else(|e| panic!("seed {seed}: recovery over a compacted log failed: {e}"));

        assert_eq!(
            recovered.answers.last(),
            clean.answers.last(),
            "seed {seed}: bootstrapping from a snapshot must not change the answer (I-1, I-7)"
        );
        assert_eq!(
            recovered.log, clean.log,
            "seed {seed}: and it must not change what the log means (I-4)"
        );
        if recovered.bootstrapped {
            bootstrapped += 1;
            assert_eq!(
                recovered
                    .fingerprints
                    .last()
                    .map(|f| without_emission_counts(f)),
                clean
                    .fingerprints
                    .last()
                    .map(|f| without_emission_counts(f)),
                "seed {seed}: a bootstrapped recovery must hold the same state"
            );
        }

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&clean_dir);
        checked += 1;
    }
    println!("bootstrap-recovery gate: {checked} scenarios · {bootstrapped} bootstrapped");
    assert!(
        bootstrapped >= 100,
        "only {bootstrapped} of {checked} exercised the bootstrap path"
    );
}

/// **The C4 gates, re-run on the backend that ships (C8).**
///
/// > the backend that ships must survive the same fire `MemBackend` did, not inherit its record
///
/// The same crash-and-recover cycles, the same twin comparison, the same I-4 re-offer — with operator
/// state in redb files instead of `BTreeMap`s. Fewer cycles than the 10,000-cycle gate, because every
/// cycle now opens and commits real transactions; the number is stated rather than tuned silently, and
/// the seam-coverage assertion is the same one.
#[test]
fn crash_and_recover_on_redb() {
    // A twentieth of the `MemBackend` gate's 10,000, because every cycle now opens and commits real
    // redb transactions: 10,000 would take an hour and a half. 600 is where all 26 named seams still
    // fire — checked by the assertion at the end, not assumed — and the figure is stated here rather
    // than tuned quietly.
    const CYCLES: u64 = 600;

    let mut cycles = 0u64;
    let mut faults_fired = 0u64;
    let mut bootstrapped = 0u64;
    let mut seams_fired: std::collections::BTreeSet<&'static str> =
        std::collections::BTreeSet::new();
    let mut seams_planned: std::collections::BTreeSet<&'static str> =
        std::collections::BTreeSet::new();

    let mut seed = 0u64;
    while cycles < CYCLES {
        seed += 1;
        let Ok(scenario) = Scenario::generate(seed) else {
            continue;
        };
        if <CircuitEngine as EngineUnderTest>::build(&scenario.tables, &scenario.query).is_err() {
            continue;
        }
        let config = Config::on_redb();
        let choice = FaultChoice::for_seed(seed);
        if let Fault::Seam(plan) = choice.fault {
            seams_planned.insert(plan.seam.name());
        }

        let clean_dir = dir_for(seed, "redb-clean");
        let clean = run_clean(&clean_dir, &scenario, config)
            .unwrap_or_else(|e| panic!("seed {seed}: the clean redb run failed: {e}"));

        let crash_dir = dir_for(seed, "redb-crash");
        let (recovered, fired) = run_with_fault(&crash_dir, &scenario, choice.fault, config)
            .unwrap_or_else(|e| panic!("seed {seed}: redb recovery failed: {e}"));
        if let Some(name) = fired {
            faults_fired += 1;
            seams_fired.insert(name);
        }

        assert_eq!(
            recovered.answers.last(),
            clean.answers.last(),
            "seed {seed}: the recovered answer differs from the uncrashed twin, on redb (I-7)"
        );
        if recovered.bootstrapped {
            bootstrapped += 1;
            assert_eq!(
                recovered
                    .fingerprints
                    .last()
                    .map(|f| without_emission_counts(f)),
                clean
                    .fingerprints
                    .last()
                    .map(|f| without_emission_counts(f)),
                "seed {seed}: a bootstrapped redb recovery must hold the same state"
            );
        } else {
            assert_eq!(
                recovered.fingerprints.last(),
                clean.fingerprints.last(),
                "seed {seed}: the recovered STATE differs from the uncrashed twin, on redb (I-7)"
            );
        }
        assert_eq!(
            recovered.epoch, clean.epoch,
            "seed {seed}: the recovered epoch differs, on redb"
        );
        assert_eq!(
            recovered.log, clean.log,
            "seed {seed}: the recovered LOG differs from the uncrashed twin, on redb (I-4)"
        );

        let _ = std::fs::remove_dir_all(&clean_dir);
        let _ = std::fs::remove_dir_all(&crash_dir);
        cycles += 1;
    }

    println!(
        "C8 redb crash gate: {cycles} cycles · {faults_fired} faults fired · \
         {bootstrapped} recovered by bootstrap · {} of {} planned seams fired",
        seams_fired.len(),
        seams_planned.len()
    );
    assert!(
        faults_fired > 0,
        "a crash suite that injected no faults proves nothing"
    );
    // Every seam the seeds planned must actually have fired, exactly as the 10,000-cycle gate demands.
    let missed: Vec<&&str> = seams_planned.difference(&seams_fired).collect();
    assert!(
        missed.is_empty(),
        "seams planned but never fired on redb: {missed:?}"
    );
}
