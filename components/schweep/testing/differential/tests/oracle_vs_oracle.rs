//! **The C0 exit gate.**
//!
//! `ARCHITECTURE.md` §6 C0: *"harness runs oracle-vs-oracle over 1,000 randomized scenarios."*
//!
//! With the oracle on both sides this does not test the oracle — it tests the harness, which is
//! the point of doing it before there is an engine. What it establishes is that 1,000 seeds all
//! generate, bind, seal, and compare cleanly, and that the scenarios they produce actually
//! contain the shapes §7 requires. A green suite that never generated a retraction would be a
//! green suite that proves nothing, so coverage is asserted here rather than assumed.
//!
//! The harness's ability to *fail* is proven separately, in `detects_divergence.rs`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use schweep_differential::scenario::Operation;
use schweep_differential::{sweep, Family, OracleEngine, Scenario};

/// The gate: 1,000 randomized scenarios, oracle against oracle, compared at every sealed epoch.
#[test]
fn oracle_vs_oracle_over_one_thousand_randomized_scenarios() {
    let report = match sweep::<OracleEngine, OracleEngine>(0..1000) {
        Ok(report) => report,
        Err(divergence) => panic!("{divergence}"),
    };

    // Printed so the CI log carries the actual numbers rather than only a green tick, and so
    // that any claim made about this gate elsewhere can be checked against it.
    println!(
        "C0 differential gate: {} scenarios, {} epochs, {} answer comparisons, 0 divergences\n\
         families: {:?}\noperations: {:?}\n\
         scenarios with an empty epoch: {} · empty-input scenarios: {}",
        report.scenarios,
        report.epochs,
        report.comparisons,
        report.families,
        report.operations,
        report.scenarios_with_an_empty_epoch,
        report.empty_input_scenarios,
    );

    assert_eq!(report.scenarios, 1000);

    // Every scenario contributes at least the epoch-0 comparison, and almost all contribute more.
    assert_eq!(
        report.comparisons,
        report.epochs + report.scenarios,
        "every scenario is compared once per sealed epoch, plus once before any epoch"
    );
    assert!(
        report.epochs >= 3000,
        "1,000 scenarios should carry thousands of epochs, got {}",
        report.epochs
    );

    // §7: the generator must always produce these. "Must always" is only a claim if it is
    // checked, so it is checked.
    for required in [
        Operation::InsertNew,
        Operation::InsertDuplicate,
        Operation::RetractPartial,
        Operation::RetractAll,
        Operation::UpdateInPlace,
        Operation::ChurnSameEpoch,
    ] {
        assert!(
            report.operations.contains(&required),
            "the generator never produced {required:?} across 1,000 seeds; \
             §7 requires it and the bar does not move"
        );
    }

    assert!(
        report.scenarios_with_an_empty_epoch > 0,
        "§7 requires empty epochs"
    );
    assert!(
        report.empty_input_scenarios > 0,
        "§7 requires empty inputs (a scenario where nothing is ever inserted)"
    );

    // All four query families, so the gate covers rungs 1, 2, and 3 rather than one of them a
    // thousand times.
    for family in [
        Family::FilterProject,
        Family::Join,
        Family::Aggregate,
        Family::JoinAggregate,
    ] {
        assert!(
            report.families.contains(&family),
            "no scenario in 1,000 used the {} family",
            family.name()
        );
    }
}

/// Retractions must appear from **epoch one**, not eventually (§6 C0 pitfalls).
///
/// The pitfall §6 names is postponing retractions until the engine can cope. This asserts they
/// are present in the very first epoch of some scenarios, which is the earliest they can be and
/// the thing that would quietly stop being true if anyone ever "simplified" the generator.
#[test]
fn retractions_appear_in_the_first_epoch_of_some_scenarios() {
    let mut scenarios_with_a_first_epoch_retraction = 0;
    let mut scenarios_with_any_retraction = 0;

    for seed in 0..1000 {
        let scenario = Scenario::generate(seed).unwrap();
        let mut any = false;
        for (index, epoch) in scenario.epochs.iter().enumerate() {
            let negative = epoch
                .tables()
                .values()
                .flatten()
                .any(|(_, weight)| *weight < 0);
            if negative {
                any = true;
                if index == 0 {
                    scenarios_with_a_first_epoch_retraction += 1;
                }
            }
        }
        if any {
            scenarios_with_any_retraction += 1;
        }
    }

    assert!(
        scenarios_with_a_first_epoch_retraction > 0,
        "no scenario carried a retraction in epoch 1; the bar includes negative weights from \
         day one (§6 C0)"
    );
    assert!(
        scenarios_with_any_retraction > 300,
        "only {scenarios_with_any_retraction} of 1,000 scenarios contained any retraction; \
         retractions should be ordinary, not rare"
    );
}

/// Weight multiplicities above 1 must be common, not incidental.
///
/// A generator that only ever emits weight ±1 would leave the whole multiset half of the Z-set
/// model untested — joins multiplying weights (S-26), `COUNT(*)` summing them (S-30) — while
/// looking perfectly healthy.
#[test]
fn weights_above_one_are_common() {
    let mut scenarios_with_a_big_weight = 0;
    for seed in 0..500 {
        let scenario = Scenario::generate(seed).unwrap();
        let big = scenario
            .epochs
            .iter()
            .flat_map(|e| e.tables().values())
            .flatten()
            .any(|(_, weight)| weight.abs() > 1);
        if big {
            scenarios_with_a_big_weight += 1;
        }
    }
    assert!(
        scenarios_with_a_big_weight > 150,
        "only {scenarios_with_a_big_weight} of 500 scenarios used a weight above 1"
    );
}

/// Enough scenarios must produce a **non-empty answer** for the comparison to mean anything.
///
/// Two empty answers agree no matter how broken either side is. A suite whose scenarios all
/// returned nothing would be green, fast, and worthless — and nothing else in this file would
/// notice, because every other assertion here is about the *inputs*. This measures the outputs.
///
/// The floor is 40%. Measured at the time of writing: 211 of 400 seeds (53%) produce a non-empty
/// answer at some epoch. Scenarios that legitimately answer nothing — the empty-input case, a
/// filter that admits no row — are a real case worth comparing too, so the target is a healthy
/// majority rather than all of them.
#[test]
fn a_healthy_share_of_scenarios_produce_a_non_empty_answer() {
    let mut productive = 0;
    let total = 400;
    for seed in 0..total {
        let scenario = Scenario::generate(seed).unwrap();
        let report = schweep_differential::compare::<OracleEngine, OracleEngine>(&scenario)
            .unwrap_or_else(|d| panic!("{d}"));
        if report
            .answers
            .iter()
            .any(|a| a.lines().count() > 1 && !a.starts_with("ERROR"))
        {
            productive += 1;
        }
    }
    assert!(
        productive * 100 >= total * 40,
        "only {productive} of {total} scenarios ever produced a non-empty answer; \
         the harness is mostly comparing nothing to nothing"
    );
}

/// An empty epoch must not move the answer (§7, I-3).
///
/// Checked directly rather than inferred from the sweep passing: the sweep would also pass if
/// empty epochs changed the answer in the same wrong way on both sides.
#[test]
fn an_empty_epoch_never_changes_the_answer() {
    let mut checked = 0;
    for seed in 0..400 {
        let scenario = Scenario::generate(seed).unwrap();
        if !scenario.has_empty_epoch() {
            continue;
        }
        let report = match schweep_differential::compare::<OracleEngine, OracleEngine>(&scenario) {
            Ok(report) => report,
            Err(divergence) => panic!("{divergence}"),
        };
        for (index, epoch) in scenario.epochs.iter().enumerate() {
            if !epoch.is_empty() {
                continue;
            }
            // answers[i] is the answer after i epochs, so an empty epoch i+1 must leave
            // answers[i+1] equal to answers[i].
            let before = report.answers.get(index);
            let after = report.answers.get(index + 1);
            assert_eq!(
                before,
                after,
                "seed {}: empty epoch {} changed the answer",
                seed,
                index + 1
            );
            checked += 1;
        }
    }
    assert!(
        checked > 50,
        "expected to find many empty epochs, found {checked}"
    );
}
