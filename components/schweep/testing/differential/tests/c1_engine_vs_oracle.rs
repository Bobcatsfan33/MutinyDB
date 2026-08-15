//! **The C1 exit gate** (`ARCHITECTURE.md` §6 C1).
//!
//! > **Exit gate:** differential harness green, engine-vs-oracle, over randomized filter/project
//! > scenarios including retractions; I-2 gate: two runs of the same scenario produce
//! > byte-identical state and answers.
//!
//! This is the first time I-1 has two implementations to compare. One maintains its answer from
//! deltas and never looks at the whole input; the other replays the entire log and recomputes
//! from scratch. Everything C0 built exists so that this file can be short.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use schweep_differential::{
    compare, sweep_matching, CircuitEngine, Family, OracleEngine, Scenario,
};
use schweep_plan::plan::{GroupBy, Named, Query, Source};
use schweep_plan::{AggFunc, Expr};
use schweep_zset::{DataType, Field, Schema};

/// How many seeds to sweep to reach ~1,000 filter/project scenarios.
///
/// The generator picks one family in four, so a quarter of seeds qualify. Sweeping a fixed seed
/// range and counting what lands is honest about the sample; asking for "the first 1,000
/// qualifying seeds" would be the same set of scenarios described less clearly.
const SEEDS: u64 = 4400;

/// C1's gate is about **rung 1**, and it stays that way now that the engine also does joins.
///
/// `CircuitEngine::claims` widened in C2 to include `Family::Join`. If this gate had kept using it,
/// C1's numbers would have silently become C1-and-C2's numbers, and neither sprint's gate would
/// mean what its section of `docs/PROGRESS.md` says.
fn is_rung_one(scenario: &Scenario) -> bool {
    scenario.family == Family::FilterProject
}

/// The answer, or the live error, as one comparable string.
///
/// From C3 a query may legitimately have no answer (S-22), so a test that unwrapped the answer would
/// be asserting the absence of errors rather than the property it is about. Rendering both means the
/// comparison covers error text too, which is what I-1 requires.
fn rendered(engine: &CircuitEngine) -> String {
    use schweep_differential::EngineUnderTest;
    match engine.answer() {
        Ok(answer) => answer.render(),
        Err(message) => format!("ERROR: {message}"),
    }
}

/// The gate: engine against oracle, every sealed epoch, over randomized rung-1 scenarios.
#[test]
fn engine_vs_oracle_over_a_thousand_filter_project_scenarios() {
    let report = match sweep_matching::<CircuitEngine, OracleEngine>(0..SEEDS, is_rung_one) {
        Ok(report) => report,
        Err(divergence) => panic!("{divergence}"),
    };

    println!(
        "C1 differential gate: {} filter/project scenarios of {} seeds considered \
         ({} skipped as out of rung 1), {} epochs, {} answer comparisons, 0 divergences",
        report.scenarios, report.considered, report.skipped, report.epochs, report.comparisons,
    );

    assert!(
        report.scenarios >= 1000,
        "the gate needs at least 1,000 filter/project scenarios, got {}",
        report.scenarios
    );
    assert_eq!(
        report.comparisons,
        report.epochs + report.scenarios,
        "every scenario is compared once per sealed epoch, plus once before any epoch"
    );

    // D-16 closed Q-2, so raising expressions are part of the population now. The fence that used
    // to stand here — `error_answers == 0` — asserted that none occurred, because the two
    // implementations disagreed about what an error meant. The claim is stronger now: the sweep
    // above passing means both sides agreed at every comparison, error text included (`compare`
    // treats two different messages as a divergence), and this asserts the population actually
    // contains some so that agreement is not vacuous.
    assert!(
        report.error_answers > 0,
        "no comparison raised, so agreement about errors was never tested (S-22, D-16)"
    );
}

/// Retractions must be present in the scenarios the gate actually ran — not merely in the
/// generator's repertoire.
///
/// §6 C1 says "including retractions", and the way that quietly stops being true is that the
/// family filter selects a corner of the generator where they do not occur. So it is measured on
/// the filtered population, not the whole one.
#[test]
fn the_gate_population_is_full_of_retractions() {
    let mut scenarios = 0;
    let mut with_retraction = 0;
    let mut with_first_epoch_retraction = 0;
    let mut with_weight_above_one = 0;

    for seed in 0..SEEDS {
        let scenario = Scenario::generate(seed).unwrap();
        if !is_rung_one(&scenario) {
            continue;
        }
        scenarios += 1;

        let mut any = false;
        let mut first = false;
        let mut big = false;
        for (index, epoch) in scenario.epochs.iter().enumerate() {
            for (_, weight) in epoch.tables().values().flatten() {
                if *weight < 0 {
                    any = true;
                    if index == 0 {
                        first = true;
                    }
                }
                if weight.abs() > 1 {
                    big = true;
                }
            }
        }
        with_retraction += usize::from(any);
        with_first_epoch_retraction += usize::from(first);
        with_weight_above_one += usize::from(big);
    }

    println!(
        "C1 gate population: {scenarios} scenarios, {with_retraction} with a retraction, \
         {with_first_epoch_retraction} with one in epoch 1, \
         {with_weight_above_one} with a weight above 1"
    );
    assert!(
        with_retraction * 10 >= scenarios * 7,
        "only {with_retraction} of {scenarios} gate scenarios contain a retraction"
    );
    assert!(
        with_first_epoch_retraction > 0,
        "no gate scenario retracts in epoch one; the bar includes negative weights from day one"
    );
    assert!(
        with_weight_above_one * 10 >= scenarios * 5,
        "only {with_weight_above_one} of {scenarios} gate scenarios use a weight above 1"
    );
}

/// **The I-2 gate:** two runs of one scenario produce byte-identical state *and* answers.
///
/// Answers alone would not be enough. Two runs can agree on every answer while accumulating
/// different internal state, and that difference becomes a wrong answer at some later epoch — or,
/// from C4, a recovery that does not match its uncrashed twin (I-7). So the comparison is over
/// the full state fingerprint: the epoch, every operator's declared and actual state, and the
/// whole result store.
#[test]
fn i2_two_runs_of_a_scenario_produce_byte_identical_state_and_answers() {
    use schweep_differential::EngineUnderTest;

    let mut checked = 0;
    for seed in 0..SEEDS {
        let scenario = Scenario::generate(seed).unwrap();
        if !is_rung_one(&scenario) {
            continue;
        }
        checked += 1;
        if checked > 400 {
            break;
        }

        let run = |scenario: &Scenario| -> (Vec<String>, Vec<String>) {
            let mut engine =
                CircuitEngine::build(&scenario.tables, &scenario.query).expect("rung 1 builds");
            let mut states = vec![engine.state_fingerprint().unwrap()];
            let mut answers = vec![rendered(&engine)];
            for epoch in &scenario.epochs {
                engine.seal_epoch(epoch).unwrap();
                states.push(engine.state_fingerprint().unwrap());
                answers.push(rendered(&engine));
            }
            (states, answers)
        };

        let (states_a, answers_a) = run(&scenario);
        let (states_b, answers_b) = run(&scenario);
        assert_eq!(
            answers_a, answers_b,
            "seed {seed}: answers differed between runs"
        );
        assert_eq!(
            states_a, states_b,
            "seed {seed}: state differed between runs"
        );

        // And from a scenario regenerated from the seed, not merely the same object — otherwise
        // "reproducible" would only mean "repeatable while you still hold it".
        let (states_c, answers_c) = run(&Scenario::generate(seed).unwrap());
        assert_eq!(
            answers_a, answers_c,
            "seed {seed}: answers differed after regeneration"
        );
        assert_eq!(
            states_a, states_c,
            "seed {seed}: state differed after regeneration"
        );
    }
    assert!(
        checked > 100,
        "expected many rung-1 scenarios, found {checked}"
    );
}

/// §6 C1's pitfall, as an assertion: **linear operators hold no state.**
///
/// > resist adding any state to linear operators; if a linear operator seems to need state, the
/// > design is wrong.
///
/// The circuit checks each operator's declaration against its actual size after every step, so a
/// violation already fails the run. This checks the other half: that the declarations really do
/// say `stateless`, and that a whole scenario of churn leaves them that way. An operator that
/// declared `unbounded` and then kept everything would satisfy the runtime check and fail here.
#[test]
fn linear_operators_declare_and_hold_no_state() {
    use schweep_differential::EngineUnderTest;

    let mut checked = 0;
    for seed in 0..SEEDS {
        let scenario = Scenario::generate(seed).unwrap();
        if !is_rung_one(&scenario) || scenario.is_empty_input() {
            continue;
        }
        checked += 1;
        if checked > 200 {
            break;
        }

        let mut engine = CircuitEngine::build(&scenario.tables, &scenario.query).unwrap();
        for epoch in &scenario.epochs {
            engine.seal_epoch(epoch).unwrap();
        }
        let fingerprint = engine.state_fingerprint().unwrap();
        let linear = fingerprint
            .lines()
            .filter(|l| l.contains(" filter ") || l.contains(" project "));
        let mut seen = 0;
        for line in linear {
            assert!(
                line.contains("state_bound=stateless") && line.contains("state_size=0"),
                "seed {seed}: a linear operator is holding state:\n{line}"
            );
            seen += 1;
        }
        let _ = seen;
    }
    assert!(
        checked > 100,
        "expected many populated rung-1 scenarios, found {checked}"
    );
}

/// A one-shot query is the degenerate case of a standing one (§0).
///
/// > A one-shot query is just the degenerate case: a circuit fed one big delta (the whole
/// > dataset) and then torn down — same machinery, one code path.
///
/// Feeding a scenario's whole history as a single epoch must give the same answer as feeding it
/// epoch by epoch. This is linearity itself, checked end to end: if it ever fails, the circuit is
/// carrying something across epochs that it should not be.
#[test]
fn feeding_the_whole_history_as_one_epoch_gives_the_same_answer() {
    use schweep_differential::EngineUnderTest;
    use schweep_zset::EpochDeltas;

    let mut checked = 0;
    for seed in 0..SEEDS {
        let scenario = Scenario::generate(seed).unwrap();
        if !is_rung_one(&scenario) {
            continue;
        }
        checked += 1;
        if checked > 300 {
            break;
        }

        let mut incremental = CircuitEngine::build(&scenario.tables, &scenario.query).unwrap();
        for epoch in &scenario.epochs {
            incremental.seal_epoch(epoch).unwrap();
        }

        let mut everything = EpochDeltas::new();
        for epoch in &scenario.epochs {
            for (table, entries) in epoch.tables() {
                everything.extend(table.clone(), entries.iter().cloned());
            }
        }
        let mut one_shot = CircuitEngine::build(&scenario.tables, &scenario.query).unwrap();
        one_shot.seal_epoch(&everything).unwrap();

        assert_eq!(
            rendered(&incremental),
            rendered(&one_shot),
            "seed {seed}: the incremental answer and the one-shot answer differ"
        );
    }
    assert!(
        checked > 100,
        "expected many rung-1 scenarios, found {checked}"
    );
}

/// The engine now implements the whole surface `docs/SEMANTICS.md` defines, so there is nothing left
/// for the adapter to refuse — and this test records that rather than being deleted.
///
/// It has moved twice. In C1 it asserted that a join and a `GROUP BY` were both refused by name; in
/// C2 the join moved to the "builds" side; in C3 the `GROUP BY` did too. What remains is the claim
/// that matters: everything in the dialect builds, and everything outside it is turned away by the
/// **binder**, which names the construct (S-12) — not by a hand-written check in an adapter.
#[test]
fn the_engine_builds_the_whole_dialect_and_the_binder_refuses_the_rest() {
    use schweep_differential::EngineUnderTest;

    let ints = |names: &[&str]| {
        Schema::new_table(
            names
                .iter()
                .map(|n| Field::nullable(*n, DataType::Int64))
                .collect(),
        )
        .unwrap()
    };
    let tables = vec![
        ("l".to_owned(), ints(&["id", "x"])),
        ("r".to_owned(), ints(&["id", "y"])),
    ];

    let join = Query::from(Source::join(
        Source::scan("l", "l"),
        Source::scan("r", "r"),
        vec![("l.id".to_owned(), "r.id".to_owned())],
    ));
    // Rung 2 arrived in C2: a join now builds rather than being refused. This assertion moved with
    // the boundary instead of being deleted, so the file still records where the boundary is.
    assert!(
        CircuitEngine::build(&tables, &join).is_ok(),
        "a rung-2 join must build now that C2 has landed"
    );

    // Rung 3 arrived in C3: a GROUP BY now builds too.
    let grouped = Query::from(Source::scan("l", "l")).group_by(GroupBy {
        keys: vec![Named::new("k", Expr::column("l.id"))],
        aggregates: vec![Named::new("n", AggFunc::CountStar)],
        having: None,
    });
    assert!(
        CircuitEngine::build(&tables, &grouped).is_ok(),
        "a rung-3 GROUP BY must build now that C3 has landed"
    );

    // A GROUP BY with no keys is the grand total, and D-20 made it legal: one group, always present.
    let grand_total = Query::from(Source::scan("l", "l")).group_by(GroupBy {
        keys: vec![],
        aggregates: vec![Named::new("n", AggFunc::CountStar)],
        having: None,
    });
    if let Err(e) = CircuitEngine::build(&tables, &grand_total) {
        panic!("a grand total must build now that D-20 has settled S-33: {e}");
    }

    // What is still refused is refused by the binder, by name: a GROUP BY that computes nothing.
    let nothing = Query::from(Source::scan("l", "l")).group_by(GroupBy {
        keys: vec![],
        aggregates: vec![],
        having: None,
    });
    let error = CircuitEngine::build(&tables, &nothing)
        .expect_err("a GROUP BY with neither keys nor aggregates must be refused");
    assert!(
        error.contains("computes nothing"),
        "the refusal must name what it refused: {error}"
    );

    // And a rung-1 query over the same tables does build, so the refusals above are about the
    // constructs and not about the fixture.
    let ok = Query::from(Source::scan("l", "l"))
        .filter(Expr::is_not_null(Expr::column("l.id")))
        .project(vec![Named::new("out", Expr::column("l.x"))]);
    assert!(CircuitEngine::build(&tables, &ok).is_ok());
}

/// The oracle and the circuit are genuinely different implementations, so the harness comparing
/// them can fail.
///
/// C0 proved the harness catches a saboteur when both sides were the oracle. This proves the same
/// thing for the pairing that matters from now on: swap the circuit's answer for a mutilated one
/// and the harness must still notice.
#[test]
fn the_gate_would_catch_a_wrong_circuit() {
    use schweep_differential::SaboteurEngine;

    let mut caught = 0;
    let mut examined = 0;
    for seed in 0..SEEDS {
        let scenario = Scenario::generate(seed).unwrap();
        if !is_rung_one(&scenario) {
            continue;
        }
        let truthful =
            compare::<CircuitEngine, OracleEngine>(&scenario).unwrap_or_else(|d| panic!("{d}"));
        if !truthful
            .answers
            .iter()
            .any(|a| a.lines().count() > 1 && !a.starts_with("ERROR"))
        {
            continue;
        }
        if examined >= 150 {
            break;
        }
        examined += 1;
        // The saboteur is the oracle minus one answer row. Against the real circuit it must
        // register as a divergence at some epoch.
        assert!(
            compare::<CircuitEngine, SaboteurEngine>(&scenario).is_err(),
            "seed {seed}: the harness accepted a wrong answer against the circuit"
        );
        caught += 1;
    }
    assert!(
        examined > 100,
        "expected many productive scenarios, found {examined}"
    );
    assert_eq!(caught, examined);
}
