//! The measurement behind the scenario generator's tuned constants (I-10).
//!
//! The generator has a dozen numbers in it — how wide the join-key domain is, how often an epoch
//! is empty, how insertions and retractions are weighted — and every one of them changes what the
//! test suite covers. I-10 says a constant may not steer behaviour without a receipt in the
//! ledger. A constant that steers *what gets tested* deserves that at least as much as one that
//! steers the engine: the engine's constants can make it slow, but these can make a green suite
//! meaningless.
//!
//! So this module measures what the generator actually produces, the numbers go into
//! `testing/evidence/c0-generator-coverage.json`, and `tests/evidence.rs` recomputes them and
//! fails if the committed artifact has drifted. The receipt cannot go stale without CI saying so.
//!
//! The measurement is a pure function of the seed range (I-2), so it is machine-independent:
//! no timing, no threads, nothing to reproduce but the seeds.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use schweep_oracle::Oracle;
use schweep_plan::Query;

use crate::harness::compare;
use crate::oracle_engine::OracleEngine;
use crate::scenario::{Family, Operation, Scenario};

/// The seed count the committed artifact is measured over.
///
/// 1,000 is the same bar the C0 gate runs at, so the receipt describes the population the gate
/// actually sweeps rather than a convenient sample of it.
pub const ARTIFACT_SEEDS: u64 = 1000;

/// What a sweep of seeds actually produced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Coverage {
    pub seeds: u64,
    pub scenarios: usize,
    pub epochs: usize,
    pub comparisons: usize,
    /// Scenarios producing a non-empty answer at some epoch — the ones where comparing two
    /// implementations can distinguish them at all.
    pub non_empty_answer: usize,
    pub empty_input_scenarios: usize,
    pub scenarios_with_an_empty_epoch: usize,
    pub scenarios_with_a_retraction: usize,
    pub scenarios_with_a_first_epoch_retraction: usize,
    pub scenarios_with_a_weight_above_one: usize,
    /// Per family: (scenarios, of which produced a non-empty answer).
    pub by_family: BTreeMap<&'static str, (usize, usize)>,
    /// Join-family only: scenarios where both tables held rows at the final epoch, and of those,
    /// how many had a non-empty *bare* join — the source with no filter, group, or projection.
    ///
    /// This pair is the receipt for `KEY_DOMAIN`. It is the measurement that showed joins were
    /// starving, and the one that would show it again if the key domain ever widened.
    pub join_both_tables_populated: usize,
    pub join_bare_join_non_empty: usize,
    /// Comparisons at which **both** sides reported the same live error (S-22).
    ///
    /// Before C3 this was necessarily zero: the generator produced nothing that could raise, and the
    /// gates asserted as much, because the two implementations disagreed about what an error meant.
    /// D-16 settled that, so raising expressions are now part of the population and this number is
    /// the receipt for how much of it they are.
    pub comparisons_that_raised: usize,
    pub scenarios_that_raised: usize,
    pub operations: Vec<Operation>,
}

/// Measure the generator over `seeds` scenarios, starting from seed 0.
pub fn measure(seeds: u64) -> Result<Coverage, String> {
    let mut c = Coverage {
        seeds,
        scenarios: 0,
        epochs: 0,
        comparisons: 0,
        non_empty_answer: 0,
        empty_input_scenarios: 0,
        scenarios_with_an_empty_epoch: 0,
        scenarios_with_a_retraction: 0,
        scenarios_with_a_first_epoch_retraction: 0,
        scenarios_with_a_weight_above_one: 0,
        by_family: BTreeMap::new(),
        join_both_tables_populated: 0,
        join_bare_join_non_empty: 0,
        comparisons_that_raised: 0,
        scenarios_that_raised: 0,
        operations: Vec::new(),
    };

    for seed in 0..seeds {
        let scenario = Scenario::generate(seed)?;
        let report = compare::<OracleEngine, OracleEngine>(&scenario).map_err(|d| d.to_string())?;

        c.scenarios += 1;
        c.epochs += report.epochs;
        c.comparisons += report.comparisons;

        let raised = report
            .answers
            .iter()
            .filter(|a| a.starts_with("ERROR"))
            .count();
        c.comparisons_that_raised += raised;
        if raised > 0 {
            c.scenarios_that_raised += 1;
        }

        let productive = report
            .answers
            .iter()
            .any(|a| a.lines().count() > 1 && !a.starts_with("ERROR"));
        if productive {
            c.non_empty_answer += 1;
        }
        if scenario.is_empty_input() {
            c.empty_input_scenarios += 1;
        }
        if scenario.has_empty_epoch() {
            c.scenarios_with_an_empty_epoch += 1;
        }

        let mut any_retraction = false;
        let mut first_epoch_retraction = false;
        let mut big_weight = false;
        for (index, epoch) in scenario.epochs.iter().enumerate() {
            for (_, weight) in epoch.tables().values().flatten() {
                if *weight < 0 {
                    any_retraction = true;
                    if index == 0 {
                        first_epoch_retraction = true;
                    }
                }
                if weight.abs() > 1 {
                    big_weight = true;
                }
            }
        }
        if any_retraction {
            c.scenarios_with_a_retraction += 1;
        }
        if first_epoch_retraction {
            c.scenarios_with_a_first_epoch_retraction += 1;
        }
        if big_weight {
            c.scenarios_with_a_weight_above_one += 1;
        }

        let slot = c.by_family.entry(scenario.family.name()).or_insert((0, 0));
        slot.0 += 1;
        if productive {
            slot.1 += 1;
        }

        for op in scenario.operations() {
            if !c.operations.contains(&op) {
                c.operations.push(op);
            }
        }

        if matches!(scenario.family, Family::Join | Family::JoinAggregate)
            && !scenario.is_empty_input()
        {
            measure_join(&scenario, &mut c)?;
        }
    }

    c.operations.sort_unstable();
    Ok(c)
}

/// Replay a join scenario into a bare oracle and ask two questions the tuning turned on:
/// did both tables end up with rows, and did the join itself match anything?
fn measure_join(scenario: &Scenario, c: &mut Coverage) -> Result<(), String> {
    let mut oracle = Oracle::new(scenario.tables.clone()).map_err(|e| e.to_string())?;
    for epoch in &scenario.epochs {
        oracle
            .seal_epoch(epoch.clone())
            .map_err(|e| e.to_string())?;
    }
    let at = oracle.sealed_epoch();

    let mut populated = true;
    for (name, _) in &scenario.tables {
        let rows = oracle.contents_at(name, at).map_err(|e| e.to_string())?;
        if rows.is_empty() {
            populated = false;
        }
    }
    if populated {
        c.join_both_tables_populated += 1;
    }

    // The source alone: no filter, no group, no projection. This isolates the join from every
    // other reason an answer could be empty.
    let bare = Query::from(scenario.query.source.clone());
    let answer = oracle
        .answer_at(&bare, at)
        .map_err(|e| e.to_string())?
        .canonical()
        .map_err(|e| e.to_string())?;
    if !answer.is_empty() {
        c.join_bare_join_non_empty += 1;
    }
    Ok(())
}

impl Coverage {
    /// Deterministic JSON. Hand-rolled rather than pulling in a serialiser: the shape is fixed,
    /// the key order must be stable for the artifact to diff cleanly, and one fewer dependency in
    /// a crate that gates every correctness claim is worth ten lines.
    #[must_use]
    pub fn to_json(&self) -> String {
        let mut out = String::new();
        out.push_str("{\n");
        out.push_str(
            "  \"$comment\": \"Receipt for the scenario generator's tuned constants (I-10). \
             Regenerate with: cargo run -p schweep-differential --bin generator-coverage \
             > testing/evidence/c0-generator-coverage.json. A deterministic function of the \
             seed range: no timing, no threads, machine-independent.\",\n",
        );
        let _ = writeln!(out, "  \"seeds\": {},", self.seeds);
        let _ = writeln!(out, "  \"scenarios\": {},", self.scenarios);
        let _ = writeln!(out, "  \"epochs\": {},", self.epochs);
        let _ = writeln!(out, "  \"comparisons\": {},", self.comparisons);
        let _ = writeln!(out, "  \"non_empty_answer\": {},", self.non_empty_answer);
        let _ = writeln!(
            out,
            "  \"empty_input_scenarios\": {},",
            self.empty_input_scenarios
        );
        let _ = writeln!(
            out,
            "  \"scenarios_with_an_empty_epoch\": {},",
            self.scenarios_with_an_empty_epoch
        );
        let _ = writeln!(
            out,
            "  \"scenarios_with_a_retraction\": {},",
            self.scenarios_with_a_retraction
        );
        let _ = writeln!(
            out,
            "  \"scenarios_with_a_first_epoch_retraction\": {},",
            self.scenarios_with_a_first_epoch_retraction
        );
        let _ = writeln!(
            out,
            "  \"scenarios_with_a_weight_above_one\": {},",
            self.scenarios_with_a_weight_above_one
        );
        let _ = writeln!(
            out,
            "  \"join_both_tables_populated\": {},",
            self.join_both_tables_populated
        );
        let _ = writeln!(
            out,
            "  \"join_bare_join_non_empty\": {},",
            self.join_bare_join_non_empty
        );
        let _ = writeln!(
            out,
            "  \"comparisons_that_raised\": {},",
            self.comparisons_that_raised
        );
        let _ = writeln!(
            out,
            "  \"scenarios_that_raised\": {},",
            self.scenarios_that_raised
        );

        out.push_str("  \"by_family\": {\n");
        let last = self.by_family.len().saturating_sub(1);
        for (i, (family, (total, productive))) in self.by_family.iter().enumerate() {
            let comma = if i == last { "" } else { "," };
            let _ = writeln!(
                out,
                "    \"{family}\": {{ \"scenarios\": {total}, \"non_empty_answer\": {productive} }}{comma}"
            );
        }
        out.push_str("  },\n");

        out.push_str("  \"operations\": [");
        for (i, op) in self.operations.iter().enumerate() {
            if i > 0 {
                out.push_str(", ");
            }
            let _ = write!(out, "\"{op:?}\"");
        }
        out.push_str("]\n}\n");
        out
    }
}
