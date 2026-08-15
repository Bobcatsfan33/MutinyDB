//! **The C9 exit gate, part one**: the harness over the network, and I-6 through a third door.
//!
//! §6 C9 asks for two things this file is:
//!
//! 1. *"the differential harness runs OVER THE NETWORK: the same scenario families through the network
//!    door, green vs the oracle"* — the whole renderable population, every epoch delivered by
//!    `POST /ingest` + `POST /seal` to a real `schweepd` on loopback, every answer read back through
//!    `GET /read` and compared to the oracle byte for byte (I-1).
//! 2. *"same-door extends: network, SQL and typed doors produce identical plans and identical counters"* —
//!    C5 proved two doors agree; the network is the third, and the comparison is the same one: structural
//!    hash equality of the plan, and equality of the execution counters after every sealed epoch (I-6).
//!
//! ## What "over the network" costs, stated
//!
//! Each scenario starts its own server with its own log and redb state store, which is roughly 80 ms of
//! setup — so this gate is about an order of magnitude slower per scenario than the in-process doors, and
//! that is the price of testing the thing that ships. It is not paid twice: the population is the same one
//! C5 sweeps, so a divergence here that C5 does not see is a bug *in the server*, which is precisely the
//! signal C9 needs.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use schweep_differential::{
    sql_form, sweep_matching, EngineUnderTest, Family, NetworkEngine, OracleEngine, Scenario,
};
use schweep_memo::{Admission, Memo, Sharing};
use schweep_plan::bind::Catalog;
use schweep_server::Client;
use schweep_sql::{compile, incrementalize_typed};

/// The population C5 sweeps through the SQL door, so the network door's claim is over the same set.
const SEEDS: u64 = 4400;

/// A smaller sample for the counter comparison, which needs three engines per scenario rather than two.
/// Small on purpose and *counted*, so nobody reads it as the whole population.
const DOOR_SAMPLE: u64 = 120;

fn has_sql_form(scenario: &Scenario) -> bool {
    sql_form(&scenario.query).is_ok()
}

/// **The gate.** Every renderable scenario, over a socket, against the oracle.
#[test]
fn the_network_door_agrees_with_the_oracle_over_the_whole_renderable_population() {
    let report = match sweep_matching::<NetworkEngine, OracleEngine>(0..SEEDS, has_sql_form) {
        Ok(report) => report,
        Err(divergence) => panic!("{divergence}"),
    };

    println!(
        "C9 network-door gate: {} scenarios, {} epochs, {} comparisons ({} skipped as unrenderable)",
        report.scenarios, report.epochs, report.comparisons, report.skipped
    );
    println!("  families: {:?}", report.families);
    println!("  operations: {:?}", report.operations);
    println!(
        "  empty-input scenarios: {}, scenarios with an empty epoch: {}, error answers: {}",
        report.empty_input_scenarios, report.scenarios_with_an_empty_epoch, report.error_answers
    );

    // The same population assertions C5 makes, for the same reason: a green sweep over nothing is not a
    // gate, and the day the network door quietly stops covering a family, this says so.
    assert!(
        report.scenarios > 1_500,
        "only {} scenarios reached the network door out of {SEEDS}; the gate is measuring too little",
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
        "every dialect rung must cross the wire, not only the easy ones"
    );
    assert!(
        report
            .operations
            .iter()
            .any(|op| format!("{op:?}").contains("Retract")),
        "the population must contain retractions (I-5, §7); operations were {:?}",
        report.operations
    );
    assert!(
        report.empty_input_scenarios > 0 && report.scenarios_with_an_empty_epoch > 0,
        "an empty input and an empty epoch must both cross the wire: {report:?}"
    );
    assert!(
        report.error_answers > 0,
        "D-16 error answers must cross the wire as errors, not as empty answers: {report:?}"
    );
}

/// **I-6, three doors.** Typed, SQL text, and the network compile to one plan.
#[test]
fn the_three_doors_produce_one_plan() {
    let mut compared = 0usize;
    let mut skipped = 0usize;
    for seed in 0..DOOR_SAMPLE {
        let scenario = Scenario::generate(seed).unwrap();
        let Ok(sql) = sql_form(&scenario.query) else {
            skipped += 1;
            continue;
        };
        let catalog: Catalog = scenario.tables.iter().cloned().collect();

        let typed = incrementalize_typed(&scenario.query, &catalog).unwrap();
        let text = compile(&sql, &catalog).unwrap();
        assert_eq!(
            typed.structural_hash(),
            text.structural_hash(),
            "seed {seed}: the typed and SQL doors disagreed before the network was involved\n{sql}"
        );

        // The network's plan comes back as `hash {:016x}` and then the structural form, which is the same
        // rendering the in-process doors produce — so this compares the plan, not a summary of it.
        let door = NetworkEngine::build(&scenario.tables, &scenario.query).unwrap();
        let reported = door
            .client()
            .plan(door.handle())
            .unwrap()
            .body()
            .unwrap()
            .to_owned();
        let expected = format!(
            "hash {:016x}\n{}",
            typed.structural_hash(),
            typed.structural_form()
        );
        assert_eq!(
            reported, expected,
            "seed {seed}: the network door's plan is not the plan the other two doors produce\n{sql}"
        );
        compared += 1;
    }
    println!("C9 I-6 plan gate: {compared} scenarios compared across three doors, {skipped} unrenderable");
    assert!(
        compared > 40,
        "only {compared} scenarios had a SQL form; the three-door comparison is measuring too little"
    );
}

/// **I-6, the other half.** The same work, counter for counter, in-process and over the wire.
///
/// Answer equality alone would let the server take a different execution path to the same answer — a
/// different amount of work, a different sharing decision, an operator stepped twice. The counters are what
/// make "the same door" a statement about execution rather than about output.
#[test]
fn the_network_door_does_the_same_work_as_the_typed_door() {
    let mut compared = 0usize;
    let mut epochs_compared = 0usize;
    for seed in 0..DOOR_SAMPLE {
        let scenario = Scenario::generate(seed).unwrap();
        let Ok(sql) = sql_form(&scenario.query) else {
            continue;
        };
        let catalog: Catalog = scenario.tables.iter().cloned().collect();

        // The typed door, in a memo, because that is what the server runs: one query registered into a
        // shared dataflow. Comparing against a private circuit would compare two different shapes.
        let plan = incrementalize_typed(&scenario.query, &catalog).unwrap();
        let mut memo = Memo::with_sharing(catalog, Sharing::On).unwrap();
        memo.register(&plan, Admission::bounded()).unwrap();

        let mut door = NetworkEngine::build(&scenario.tables, &scenario.query).unwrap();
        assert_eq!(
            counters_of(&memo),
            over_the_wire(door.client()),
            "seed {seed}: the two doors differed at epoch 0, before any input\n{sql}"
        );

        for (index, input) in scenario.epochs.iter().enumerate() {
            memo.seal_epoch(input).unwrap();
            door.seal_epoch(input).unwrap();
            assert_eq!(
                counters_of(&memo),
                over_the_wire(door.client()),
                "seed {seed}: the doors did different work at epoch {}\n{sql}",
                index + 1
            );
            epochs_compared += 1;
        }
        compared += 1;
    }
    println!(
        "C9 I-6 counter gate: {compared} scenarios, {epochs_compared} epochs compared counter for counter"
    );
    assert!(
        compared > 40 && epochs_compared > 200,
        "the counter comparison is measuring too little: {compared} scenarios, {epochs_compared} epochs"
    );
}

/// The counter rendering `schweepd` serves, produced locally from a memo — the same format, so the
/// comparison is of numbers rather than of two spellings.
fn counters_of(memo: &Memo) -> String {
    let counters = memo.dataflow().counters();
    let steps = memo.dataflow().step_counters();
    let mut out = format!("operator steps {}\n", memo.dataflow().operator_steps());
    for (index, emitted) in counters.iter().enumerate() {
        out.push_str(&format!(
            "node {index} emitted {emitted} stepped {}\n",
            steps.get(index).copied().unwrap_or(0)
        ));
    }
    out
}

fn over_the_wire(client: &Client) -> String {
    client.counters().unwrap().body().unwrap().to_owned()
}
