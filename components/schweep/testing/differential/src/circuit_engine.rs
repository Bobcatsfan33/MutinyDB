//! `schweep-circuit` wearing the [`EngineUnderTest`] costume — the **typed door** of I-6.
//!
//! This is the file C0 was built to make possible. With it, the differential harness compares an
//! *incremental* engine to a *recompute-from-scratch* one, at every sealed epoch, over the same
//! seeded scenarios.
//!
//! ## What it supports, and what it refuses by name
//!
//! Dialect rungs 1–3 plus `DISTINCT`: a scan or an INNER equi-join, an optional `WHERE`, an optional
//! `GROUP BY` with aggregates and `HAVING`, an optional projection, and an optional `DISTINCT`. That
//! is the whole surface `docs/SEMANTICS.md` defines, so from C3 there is nothing left for this
//! adapter to refuse — the refusals that remain live in the binder, which turns away anything outside
//! the dialect by name (S-12).
//!
//! ## Where the wiring went (C5)
//!
//! It used to be here: this file walked a `Query` and called `CircuitBuilder` directly. It no longer
//! does, because C5 gave that job to `schweep_sql::incremental` and two implementations of one
//! translation is exactly the thing I-6 exists to prevent. The typed door now calls
//! [`schweep_sql::incrementalize_typed`] and the SQL door calls `schweep_sql::compile`; both end in
//! `schweep_sql::instantiate`. There is one path from a query to a circuit, and this file is a caller
//! of it rather than a copy.

use schweep_circuit::Circuit;
use schweep_plan::bind::Catalog;
use schweep_plan::plan::Query;
use schweep_sql::{incrementalize_typed, instantiate, CircuitPlan};
use schweep_zset::{Canonical, EpochDeltas, Schema};

use crate::engine::EngineUnderTest;
use crate::scenario::{Family, Scenario};

/// An [`EngineUnderTest`] backed by a real circuit, reached through the typed API.
#[derive(Debug)]
pub struct CircuitEngine {
    circuit: Circuit,
}

impl CircuitEngine {
    /// True if this engine claims the scenario's query — the predicate the gates filter on.
    ///
    /// Stated as a property of the *scenario family* rather than by attempting a build and seeing
    /// what happens: a sweep that discovered its own coverage by catching errors would silently
    /// shrink the day a build started failing for an unrelated reason.
    #[must_use]
    pub fn claims(scenario: &Scenario) -> bool {
        matches!(
            scenario.family,
            Family::FilterProject | Family::Join | Family::Aggregate | Family::JoinAggregate
        )
    }

    /// True if the scenario groups — the predicate the C3 gate sweeps on.
    #[must_use]
    pub fn claims_aggregate(scenario: &Scenario) -> bool {
        matches!(scenario.family, Family::Aggregate | Family::JoinAggregate)
    }

    /// True if the scenario is a join — used by the C2 gate to sweep the rung-2 population.
    #[must_use]
    pub fn claims_join(scenario: &Scenario) -> bool {
        scenario.family == Family::Join
    }

    #[must_use]
    pub fn circuit(&self) -> &Circuit {
        &self.circuit
    }

    /// Take the circuit out, for a caller that needs to drive it directly.
    ///
    /// C4's durable runtime builds a circuit of the right shape through this adapter — reusing the
    /// binder and the wiring rather than duplicating them — and then owns it, because recovery has to
    /// restore state into it and step it from the log rather than from a scenario.
    #[must_use]
    pub fn into_circuit(self) -> Circuit {
        self.circuit
    }

    /// The circuit plan the typed door produces — one half of the I-6 comparison.
    pub fn plan(tables: &[(String, Schema)], query: &Query) -> Result<CircuitPlan, String> {
        let catalog: Catalog = tables.iter().cloned().collect();
        incrementalize_typed(query, &catalog).map_err(|e| e.to_string())
    }

    /// The same engine, with operator state from `factory` (C8).
    ///
    /// The backend-invariance gate runs the whole generated population through this twice — once on
    /// `MemBackend`, once on `RedbBackend` — and compares answers and logical state fingerprints. The
    /// costume is the same costume: nothing about the comparison knows which store is underneath.
    pub fn build_with(
        tables: &[(String, Schema)],
        query: &Query,
        factory: &mut dyn schweep_state::BackendFactory,
    ) -> Result<Self, String> {
        let plan = CircuitEngine::plan(tables, query)?;
        let circuit = schweep_sql::instantiate_with(&plan, factory).map_err(|e| e.to_string())?;
        Ok(CircuitEngine { circuit })
    }
}

impl EngineUnderTest for CircuitEngine {
    fn name() -> &'static str {
        "circuit"
    }

    fn build(tables: &[(String, Schema)], query: &Query) -> Result<Self, String> {
        let plan = CircuitEngine::plan(tables, query)?;
        let circuit = instantiate(&plan).map_err(|e| e.to_string())?;
        Ok(CircuitEngine { circuit })
    }

    fn seal_epoch(&mut self, deltas: &EpochDeltas) -> Result<(), String> {
        self.circuit.step(deltas).map_err(|e| e.to_string())?;
        Ok(())
    }

    fn answer(&self) -> Result<Canonical, String> {
        self.circuit.answer().map_err(|e| e.to_string())
    }

    fn state_fingerprint(&self) -> Result<String, String> {
        self.circuit.state_fingerprint().map_err(|e| e.to_string())
    }
}
