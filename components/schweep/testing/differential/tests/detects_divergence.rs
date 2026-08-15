//! Proof that the harness can **fail**.
//!
//! A comparison suite that has never rejected anything is not evidence. It might be comparing
//! nothing at all: reading the wrong epoch, comparing an answer to itself, swallowing an error.
//! Oracle-vs-oracle passing 1,000 scenarios is only meaningful alongside this file, which puts a
//! known-wrong implementation on one side and requires the harness to catch it, name the epoch,
//! and print enough to reproduce the run.
//!
//! From C1 this is what stands between "the engine is correct" and "the harness never looks".

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use schweep_differential::{compare, DivergenceKind, OracleEngine, SaboteurEngine, Scenario};

/// The saboteur — the oracle with one entry dropped from every answer — must be caught.
#[test]
fn the_harness_catches_a_deliberately_wrong_implementation() {
    let mut caught = 0;
    let mut examined = 0;

    for seed in 0..300_u64 {
        let scenario = Scenario::generate(seed).unwrap();
        // A scenario whose answer is always empty has nothing for the saboteur to drop, so it
        // cannot expose the lie. Those are not counted against the harness.
        let truthful =
            compare::<OracleEngine, OracleEngine>(&scenario).unwrap_or_else(|d| panic!("{d}"));
        let has_a_nonempty_answer = truthful
            .answers
            .iter()
            .any(|a| a.lines().count() > 1 && !a.starts_with("ERROR"));
        if !has_a_nonempty_answer {
            continue;
        }
        examined += 1;

        match compare::<OracleEngine, SaboteurEngine>(&scenario) {
            Ok(_) => panic!(
                "seed {seed}: the harness accepted an implementation that drops an answer row"
            ),
            Err(divergence) => {
                caught += 1;
                assert_eq!(divergence.seed, seed);
                assert_eq!(divergence.kind, DivergenceKind::Answer);
                assert_eq!(divergence.left_name, "oracle");
                assert_eq!(divergence.right_name, "saboteur (deliberately wrong)");
                assert_ne!(divergence.left, divergence.right);
            }
        }
    }

    println!("saboteur gate: {caught} of {examined} sabotaged runs caught");
    assert!(
        examined > 100,
        "only {examined} of 300 scenarios had a non-empty answer to sabotage"
    );
    assert_eq!(
        caught,
        examined,
        "the harness missed {} wrong implementations",
        examined - caught
    );
}

/// A divergence report must be actionable on its own.
///
/// Someone reading CI output should be able to re-create the failure without asking anyone
/// anything: the seed, the epoch, both answers, and the whole scenario.
#[test]
fn a_divergence_report_contains_everything_needed_to_reproduce_it() {
    let scenario = (0..200_u64)
        .filter_map(|seed| Scenario::generate(seed).ok())
        .find(|s| compare::<OracleEngine, SaboteurEngine>(s).is_err() && !s.is_empty_input())
        .expect("some seed must expose the saboteur");

    let divergence = compare::<OracleEngine, SaboteurEngine>(&scenario)
        .expect_err("the saboteur must be caught");
    let rendered = divergence.to_string();

    assert!(
        rendered.contains(&format!("seed {}", scenario.seed)),
        "the report must name the seed:\n{rendered}"
    );
    assert!(
        rendered.contains("at epoch"),
        "the report must name the epoch:\n{rendered}"
    );
    assert!(
        rendered.contains("--- oracle ---"),
        "the report must show both sides by name:\n{rendered}"
    );
    assert!(
        rendered.contains("--- scenario (reproduce with this seed) ---"),
        "the report must include the scenario:\n{rendered}"
    );
    assert!(
        rendered.contains(&format!("family={}", scenario.family.name())),
        "the scenario dump must describe the query shape:\n{rendered}"
    );

    // And the seed in the report really does re-create the scenario.
    let regenerated = Scenario::generate(divergence.seed).unwrap();
    assert_eq!(regenerated.render(), scenario.render());
}

/// The comparison must be per-epoch, not just at the end.
///
/// An engine that is wrong in the middle and right at the end is wrong (I-3). This checks the
/// harness reports the *first* epoch at which the answers parted, not the last.
#[test]
fn divergence_is_reported_at_the_first_epoch_where_answers_part() {
    for seed in 0..200_u64 {
        let scenario = Scenario::generate(seed).unwrap();
        let Ok(truthful) = compare::<OracleEngine, OracleEngine>(&scenario) else {
            continue;
        };
        let Err(divergence) = compare::<OracleEngine, SaboteurEngine>(&scenario) else {
            continue;
        };

        // Every answer strictly before the reported epoch must have been empty — that is the
        // only way the saboteur's dropped row could have gone unnoticed.
        for index in 0..divergence.epoch {
            let answer = truthful
                .answers
                .get(index)
                .expect("the truthful run covers every compared epoch");
            assert!(
                answer.lines().count() <= 1 || answer.starts_with("ERROR"),
                "seed {seed}: divergence reported at epoch {} but the answer at epoch {index} \
                 was already non-empty, so it should have been caught earlier:\n{answer}",
                divergence.epoch
            );
        }
    }
}
