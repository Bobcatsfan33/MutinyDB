//! C13's scheduled order-of-magnitude differential population.

#![allow(clippy::panic)]

use schweep_differential::{sweep_matching, CircuitEngine, OracleEngine};

const SEEDS: u64 = 44_000;

#[test]
#[ignore = "C13 scheduled gate: ten times the ordinary 4,400-seed population"]
fn forty_four_thousand_seed_engine_oracle_sweep() {
    let report = sweep_matching::<CircuitEngine, OracleEngine>(0..SEEDS, CircuitEngine::claims)
        .unwrap_or_else(|divergence| panic!("{divergence}"));
    println!(
        "C13 extended differential: {} claimed scenarios over {} seeds, {} epochs, {} comparisons, \
         {} matching error answers, zero divergences",
        report.scenarios,
        report.considered,
        report.epochs,
        report.comparisons,
        report.error_answers
    );
    let expected = usize::try_from(SEEDS).unwrap_or(0);
    assert_eq!(report.considered, expected);
    assert_eq!(report.scenarios, expected);
    assert_eq!(report.comparisons, report.epochs + report.scenarios);
    assert!(
        report.error_answers > 100,
        "extended sweep barely exercised errors"
    );
}
