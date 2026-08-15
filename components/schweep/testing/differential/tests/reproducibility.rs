//! **The C0 exit gate, second half:** *"a seeded scenario is reproducible byte-for-byte from its
//! seed"* (`ARCHITECTURE.md` §6 C0).
//!
//! This is I-2 at the level of the test suite. If a seed did not re-create its run exactly, the
//! seed printed beside a failure would be decoration, every bug report would be a guess, and the
//! zero-flake policy would have nothing to stand on.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use schweep_differential::{compare, OracleEngine, Scenario};

/// The same seed produces a byte-identical scenario: same schemas, same query, same deltas.
#[test]
fn a_seed_reproduces_its_scenario_byte_for_byte() {
    for seed in [0_u64, 1, 7, 42, 999, 123_456, u64::MAX] {
        let first = Scenario::generate(seed).unwrap();
        let second = Scenario::generate(seed).unwrap();
        assert_eq!(
            first.render(),
            second.render(),
            "seed {seed} did not reproduce its scenario"
        );
    }
}

/// The same seed produces a byte-identical *run*: the same answer at every sealed epoch.
#[test]
fn a_seed_reproduces_its_run_byte_for_byte() {
    for seed in 0..200_u64 {
        let scenario = Scenario::generate(seed).unwrap();
        let first =
            compare::<OracleEngine, OracleEngine>(&scenario).unwrap_or_else(|d| panic!("{d}"));
        let second =
            compare::<OracleEngine, OracleEngine>(&scenario).unwrap_or_else(|d| panic!("{d}"));
        assert_eq!(first, second, "seed {seed} did not reproduce its run");

        // And regenerating the scenario from the seed, rather than reusing the object, must give
        // the same run too — otherwise reproduction would depend on holding the original.
        let regenerated = Scenario::generate(seed).unwrap();
        let third =
            compare::<OracleEngine, OracleEngine>(&regenerated).unwrap_or_else(|d| panic!("{d}"));
        assert_eq!(
            first, third,
            "seed {seed} did not reproduce its run from a regenerated scenario"
        );
    }
}

/// Different seeds produce different scenarios.
///
/// The reproducibility tests above would all pass if `generate` ignored its seed and returned
/// one fixed scenario. This is the test that rules that out.
#[test]
fn different_seeds_produce_different_scenarios() {
    let renders: Vec<String> = (0..100_u64)
        .map(|seed| Scenario::generate(seed).unwrap().render())
        .collect();
    let mut unique = renders.clone();
    unique.sort_unstable();
    unique.dedup();
    assert!(
        unique.len() > 90,
        "only {} of 100 seeds produced distinct scenarios",
        unique.len()
    );
}

/// The answer sequence depends on the data, not on the order rows arrive within an epoch.
///
/// An epoch is a *set* of changes that become visible together (S-6, I-3). Two runs that deliver
/// the same epoch's entries in different orders must produce the same answers — if they do not,
/// something downstream is order-sensitive, which I-2 forbids.
#[test]
fn shuffling_the_entries_within_an_epoch_does_not_change_any_answer() {
    use schweep_zset::EpochDeltas;

    for seed in 0..200_u64 {
        let scenario = Scenario::generate(seed).unwrap();

        // Reverse the entry order within every epoch. The net effect per row is unchanged, so
        // no retraction becomes malformed.
        let mut shuffled = scenario.clone();
        shuffled.epochs = scenario
            .epochs
            .iter()
            .map(|epoch| {
                let mut out = EpochDeltas::new();
                for (table, entries) in epoch.tables() {
                    for (row, weight) in entries.iter().rev() {
                        out.push(table, row.clone(), *weight);
                    }
                }
                out
            })
            .collect();

        let straight =
            compare::<OracleEngine, OracleEngine>(&scenario).unwrap_or_else(|d| panic!("{d}"));
        let reversed =
            compare::<OracleEngine, OracleEngine>(&shuffled).unwrap_or_else(|d| panic!("{d}"));
        assert_eq!(
            straight.answers, reversed.answers,
            "seed {seed}: reversing the entry order within each epoch changed an answer"
        );
    }
}
