//! The **SQL door** wearing the [`EngineUnderTest`] costume (§6 C5).
//!
//! Identical to [`crate::circuit_engine::CircuitEngine`] in every respect except one: the query
//! reaches the incrementalizer as *text*. The scenario's typed query is rendered to SQL
//! ([`crate::sql_render`]), parsed, bound, incrementalized, and instantiated — so a run of the
//! differential harness with this engine on one side tests the parser, the binder, and the name
//! derivation rules against the oracle, over generated data that includes retractions.
//!
//! A query the renderer declines has no SQL form, and this engine says so rather than substituting
//! something close to it: [`SqlEngine::build`] returns the decline as an error, and the gate counts it.

use schweep_circuit::Circuit;
use schweep_plan::bind::Catalog;
use schweep_plan::plan::Query;
use schweep_sql::{compile, instantiate, CircuitPlan};
use schweep_zset::{Canonical, EpochDeltas, Schema};

use crate::engine::EngineUnderTest;
use crate::sql_render::{sql_form, NoSqlForm};

/// An [`EngineUnderTest`] that gets its query from SQL text.
#[derive(Debug)]
pub struct SqlEngine {
    circuit: Circuit,
    sql: String,
}

impl SqlEngine {
    /// The SQL text this engine was built from — printed in failure reports, where it is the single
    /// most useful thing to see.
    #[must_use]
    pub fn sql(&self) -> &str {
        &self.sql
    }

    #[must_use]
    pub fn circuit(&self) -> &Circuit {
        &self.circuit
    }

    /// The circuit plan the SQL door produces, with the text it came from — the other half of I-6.
    ///
    /// Returns the [`NoSqlForm`] reason unchanged when the query cannot be written in this dialect,
    /// so callers can count reasons instead of counting failures.
    pub fn plan(
        tables: &[(String, Schema)],
        query: &Query,
    ) -> Result<Result<(String, CircuitPlan), String>, NoSqlForm> {
        let sql = sql_form(query)?;
        let catalog: Catalog = tables.iter().cloned().collect();
        Ok(compile(&sql, &catalog)
            .map(|plan| (sql.clone(), plan))
            .map_err(|e| format!("{sql}\n  did not compile: {e}")))
    }
}

impl EngineUnderTest for SqlEngine {
    fn name() -> &'static str {
        "sql"
    }

    fn build(tables: &[(String, Schema)], query: &Query) -> Result<Self, String> {
        let (sql, plan) = match SqlEngine::plan(tables, query) {
            Err(reason) => return Err(format!("no SQL form: {}", reason.label())),
            Ok(result) => result?,
        };
        let circuit = instantiate(&plan).map_err(|e| format!("{sql}\n  did not build: {e}"))?;
        Ok(SqlEngine { circuit, sql })
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
