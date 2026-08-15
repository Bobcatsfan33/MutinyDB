//! **The C5 exit gate**: the SQL door, and I-6 — the two doors compile to one plan.
//!
//! §6 C5 asks for three things, and this file is all three:
//!
//! 1. *"randomized queries generated over randomized schemas (a small query fuzzer: hundreds of
//!    shapes, thousands of runs) — green engine-vs-oracle"*: the whole generated population, rendered
//!    to SQL, parsed, bound, incrementalized, and compared against the oracle at every sealed epoch.
//! 2. *"I-6 gate: typed-API and SQL doors produce identical plans (structural hash equality) and
//!    identical counters on the gate suite"*: both, on every scenario that has a SQL form.
//! 3. *"every refusal names its construct"*: asserted over a table of out-of-dialect SQL in
//!    `schweep-sql/tests/dialect.rs`, and re-asserted here for the refusals the fuzzer can reach.
//!
//! ## The population, and what is honest about it
//!
//! The SQL door is driven by rendering the *existing* typed population back to SQL
//! ([`schweep_differential::sql_form`]). Some typed queries have no SQL form in this dialect — a
//! projection over a GROUP BY is the big one, because SQL takes a group key's output name from the
//! select list. Those are **counted by reason and printed**, and the counts are asserted, so the day
//! coverage shrinks the gate says so instead of staying green.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use schweep_differential::{
    sql_form, sweep_matching, CircuitEngine, EngineUnderTest, Family, NoSqlForm, OracleEngine,
    Scenario, SqlEngine,
};

/// The same figure C3's gate sweeps, for the same reason: enough shapes that every family, every
/// operation kind, and both empty-input and empty-epoch cases occur many times over.
const SEEDS: u64 = 4400;

/// Does this scenario have a SQL form? The predicate the sweep filters on.
fn has_sql_form(scenario: &Scenario) -> bool {
    sql_form(&scenario.query).is_ok()
}

/// **The gate.** Every renderable scenario, through the SQL door, against the oracle.
#[test]
fn the_sql_door_agrees_with_the_oracle_over_the_whole_renderable_population() {
    let report = match sweep_matching::<SqlEngine, OracleEngine>(0..SEEDS, has_sql_form) {
        Ok(report) => report,
        Err(divergence) => panic!("{divergence}"),
    };

    println!(
        "C5 SQL-door gate: {} scenarios, {} epochs, {} comparisons ({} skipped as unrenderable)",
        report.scenarios, report.epochs, report.comparisons, report.skipped
    );
    println!("  families: {:?}", report.families);
    println!("  operations: {:?}", report.operations);
    println!(
        "  empty-input scenarios: {}, scenarios with an empty epoch: {}, error answers: {}",
        report.empty_input_scenarios, report.scenarios_with_an_empty_epoch, report.error_answers
    );

    // A green sweep over nothing is not a gate. Every claim below is about the *population*, so that
    // "the SQL door passed" cannot be true of a suite that quietly stopped covering anything.
    assert!(
        report.scenarios > 1_500,
        "only {} scenarios had a SQL form out of {SEEDS}; the gate is measuring too little",
        report.scenarios
    );
    assert_eq!(
        report.families,
        vec![
            Family::FilterProject,
            Family::Join,
            Family::Aggregate,
            Family::JoinAggregate
        ],
        "the SQL door must be exercised over every dialect rung, not only the easy ones"
    );
    assert!(
        report
            .operations
            .iter()
            .any(|op| format!("{op:?}").contains("Retract")),
        "the population must contain retractions (§7); operations were {:?}",
        report.operations
    );
    assert!(
        report.error_answers > 0,
        "no evaluation error was ever raised through the SQL door, so S-22 is untested here"
    );
    assert!(
        report.empty_input_scenarios > 0 && report.scenarios_with_an_empty_epoch > 0,
        "the empty cases must occur: {} empty-input, {} with an empty epoch",
        report.empty_input_scenarios,
        report.scenarios_with_an_empty_epoch
    );
}

/// Coverage, stated rather than assumed: every seed is either renderable or refused for a **named**
/// reason, and the census is printed.
#[test]
fn every_scenario_either_has_a_sql_form_or_a_named_reason() {
    let mut renderable = 0usize;
    let mut declined: Vec<(NoSqlForm, usize)> = Vec::new();

    for seed in 0..SEEDS {
        let scenario = match Scenario::generate(seed) {
            Ok(scenario) => scenario,
            Err(e) => panic!("seed {seed} failed to generate: {e}"),
        };
        match sql_form(&scenario.query) {
            Ok(_) => renderable += 1,
            Err(reason) => match declined.iter_mut().find(|(r, _)| *r == reason) {
                Some((_, count)) => *count += 1,
                None => declined.push((reason, 1)),
            },
        }
    }
    declined.sort_unstable();

    println!("SQL-form census over {SEEDS} seeds: {renderable} renderable");
    for (reason, count) in &declined {
        println!("  {count:>5} declined — {}", reason.label());
    }

    let total: usize = renderable + declined.iter().map(|(_, c)| c).sum::<usize>();
    assert_eq!(
        total, SEEDS as usize,
        "every seed must be accounted for, either as renderable or by a named reason"
    );

    // The two reasons the generator actually produces. Asserted so that a change which made one of
    // them unreachable — by narrowing the generator, say — is noticed here rather than celebrated as
    // improved coverage.
    for expected in [
        NoSqlForm::WholeInputSchema,
        NoSqlForm::ProjectionOverGroupBy,
    ] {
        assert!(
            declined.iter().any(|(r, c)| *r == expected && *c > 0),
            "{} never occurred, so the census is not measuring what it claims",
            expected.label()
        );
    }
}

/// **I-6, first half:** the typed door and the SQL door produce structurally identical plans.
///
/// The comparison is on the s-expression *form* first and the hash second. That order is deliberate:
/// the form makes a failure readable — two trees, side by side — and the hash then proves the thing
/// the memo will actually rely on (§5.7).
#[test]
fn i6_the_two_doors_compile_to_structurally_identical_plans() {
    let mut compared = 0usize;

    for seed in 0..SEEDS {
        let scenario = match Scenario::generate(seed) {
            Ok(scenario) => scenario,
            Err(e) => panic!("seed {seed} failed to generate: {e}"),
        };
        let sql_side = match SqlEngine::plan(&scenario.tables, &scenario.query) {
            Err(_) => continue,
            Ok(Ok(pair)) => pair,
            Ok(Err(e)) => panic!("seed {seed}: the SQL door failed to compile\n{e}"),
        };
        let (sql, sql_plan) = sql_side;
        let typed_plan = match CircuitEngine::plan(&scenario.tables, &scenario.query) {
            Ok(plan) => plan,
            Err(e) => panic!("seed {seed}: the typed door failed to compile: {e}"),
        };

        assert_eq!(
            typed_plan.structural_form(),
            sql_plan.structural_form(),
            "seed {seed}: the two doors compiled different plans (I-6)\n  SQL was: {sql}"
        );
        assert_eq!(
            typed_plan.structural_hash(),
            sql_plan.structural_hash(),
            "seed {seed}: identical forms must hash identically"
        );
        assert_eq!(
            typed_plan.output_schema, sql_plan.output_schema,
            "seed {seed}: the answer's schema is part of the answer (S-8)"
        );
        compared += 1;
    }

    println!("I-6 plan equality: {compared} scenarios compiled through both doors");
    assert!(
        compared > 1_500,
        "only {compared} scenarios reached both doors; I-6 is measuring too little"
    );
}

/// **I-6, second half:** identical counters.
///
/// Two circuits that should be the same are compared not only by their answers but by how much work
/// each node did (§7: "counters catch divergence before answers diverge"). A sharing bug or a
/// mis-incrementalized operator can produce the right answer by the wrong route for a long time.
#[test]
fn i6_the_two_doors_execute_identical_counters() {
    let mut compared = 0usize;
    let mut counter_totals = 0usize;

    // A shorter sweep than the plan comparison: this one steps two circuits through every epoch, and
    // the property it checks is per-node rather than per-population. Stated here rather than left to
    // be inferred from the range.
    for seed in 0..SEEDS / 4 {
        let scenario = match Scenario::generate(seed) {
            Ok(scenario) => scenario,
            Err(e) => panic!("seed {seed} failed to generate: {e}"),
        };
        if !has_sql_form(&scenario) {
            continue;
        }

        let mut typed = match CircuitEngine::build(&scenario.tables, &scenario.query) {
            Ok(engine) => engine,
            Err(e) => panic!("seed {seed}: the typed door failed to build: {e}"),
        };
        let mut sql = match SqlEngine::build(&scenario.tables, &scenario.query) {
            Ok(engine) => engine,
            Err(e) => panic!("seed {seed}: the SQL door failed to build: {e}"),
        };

        assert_eq!(
            typed.circuit().counters(),
            sql.circuit().counters(),
            "seed {seed}: the two doors start with different counters"
        );

        for (index, deltas) in scenario.epochs.iter().enumerate() {
            let typed_sealed = typed.seal_epoch(deltas);
            let sql_sealed = sql.seal_epoch(deltas);
            assert_eq!(
                typed_sealed.is_ok(),
                sql_sealed.is_ok(),
                "seed {seed}, epoch {}: one door sealed and the other did not",
                index + 1
            );
            assert_eq!(
                typed.circuit().counters(),
                sql.circuit().counters(),
                "seed {seed}, epoch {}: the doors did different amounts of work\n  SQL was: {}",
                index + 1,
                sql.sql()
            );
        }

        counter_totals += typed.circuit().counters().iter().sum::<usize>();
        compared += 1;
    }

    println!(
        "I-6 counter equality: {compared} scenarios stepped through both doors, \
         {counter_totals} entries emitted in total"
    );
    assert!(
        compared > 300,
        "only {compared} scenarios were counter-compared; the gate is measuring too little"
    );
    assert!(
        counter_totals > 0,
        "the counters were all zero, so nothing was actually executed"
    );
}

/// The SQL door is not a second dialect: what it refuses, it refuses **by name** (S-12, S-35).
///
/// The exhaustive table lives in `schweep-sql/tests/dialect.rs`. This is the subset the gate cares
/// about — the refusals a person writing SQL against a *generated* schema would actually hit.
#[test]
fn the_refusals_the_fuzzer_can_reach_name_their_constructs() {
    let scenario = match Scenario::generate(7) {
        Ok(scenario) => scenario,
        Err(e) => panic!("seed 7 failed to generate: {e}"),
    };
    let catalog: schweep_plan::bind::Catalog = scenario.tables.iter().cloned().collect();
    let table = match scenario.tables.first() {
        Some((name, _)) => name.clone(),
        None => panic!("the generator produced no tables"),
    };

    for (sql, construct) in [
        (format!("SELECT * FROM \"{table}\" AS \"a\""), "SELECT *"),
        (
            format!("SELECT \"a\".\"c0\" + 1 FROM \"{table}\" AS \"a\""),
            "AS",
        ),
        (
            format!("SELECT COUNT(*) AS \"n\" FROM \"{table}\" AS \"a\" WHERE COUNT(*) > 1"),
            "WHERE",
        ),
        (
            format!("SELECT NULL AS \"n\" FROM \"{table}\" AS \"a\""),
            "untyped NULL",
        ),
        (
            format!("SELECT \"a\".\"c0\" AS \"k\" FROM \"{table}\" AS \"a\" ORDER BY \"k\""),
            "ORDER BY",
        ),
    ] {
        match schweep_sql::compile(&sql, &catalog) {
            Ok(_) => panic!("{sql} was accepted"),
            Err(error) => {
                let message = error.to_string();
                assert!(
                    message.contains(construct),
                    "{sql}\n  was refused as {message:?}, which does not name {construct:?}"
                );
            }
        }
    }
}
