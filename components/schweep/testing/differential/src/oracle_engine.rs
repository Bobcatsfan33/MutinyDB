//! The oracle, wearing the [`EngineUnderTest`] costume.
//!
//! In C0 this sits on **both** sides of every comparison. That sounds circular, and it would be
//! if the claim were "the oracle is correct" — but that is not the claim. What oracle-vs-oracle
//! proves is that *the harness itself works*: that scenarios generate, that epochs seal in order,
//! that answers are read at the right epoch, that comparison detects a difference when there is
//! one and reports it usefully, and that a seed reproduces a run byte for byte.
//!
//! A harness that cannot be trusted would make every later sprint's green tick meaningless, and
//! the cheapest time to find out is before there is an engine to blame. From C1, one side becomes
//! `schweep-circuit` and the comparison starts testing the engine instead (I-1).

use schweep_oracle::Oracle;
use schweep_plan::Query;
use schweep_zset::{Canonical, EpochDeltas, Schema};

use crate::engine::EngineUnderTest;

/// An [`EngineUnderTest`] backed by the naive reference engine.
#[derive(Debug)]
pub struct OracleEngine {
    oracle: Oracle,
    query: Query,
}

impl EngineUnderTest for OracleEngine {
    fn name() -> &'static str {
        "oracle"
    }

    fn build(tables: &[(String, Schema)], query: &Query) -> Result<Self, String> {
        let oracle = Oracle::new(tables.to_vec()).map_err(|e| e.to_string())?;
        Ok(OracleEngine {
            oracle,
            query: query.clone(),
        })
    }

    fn seal_epoch(&mut self, deltas: &EpochDeltas) -> Result<(), String> {
        self.oracle
            .seal_epoch(deltas.clone())
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    fn answer(&self) -> Result<Canonical, String> {
        self.oracle
            .canonical_answer_at(&self.query, self.oracle.sealed_epoch())
            .map_err(|e| e.to_string())
    }

    /// The oracle holds no derived state — it replays the log on every question (§5.1) — so its
    /// whole condition is its epoch and its answer. Saying exactly that is more honest than
    /// inventing a state dump it does not have.
    fn state_fingerprint(&self) -> Result<String, String> {
        Ok(format!(
            "oracle @ epoch {}\nno derived state (recomputes from the log prefix)\n{}",
            self.oracle.sealed_epoch(),
            self.answer()?.render()
        ))
    }
}

/// A deliberately wrong engine, used to prove the harness can *fail*.
///
/// A comparison harness that has never rejected anything is not evidence — it might be comparing
/// nothing at all. This one is the oracle with a single, well-defined lie: it drops the last
/// entry of every answer. The harness must catch it, on some seed, and say where.
///
/// It exists only under `cfg(test)`-style use by the harness's own gate; nothing else may use it.
#[derive(Debug)]
pub struct SaboteurEngine {
    inner: OracleEngine,
}

impl EngineUnderTest for SaboteurEngine {
    fn name() -> &'static str {
        "saboteur (deliberately wrong)"
    }

    fn build(tables: &[(String, Schema)], query: &Query) -> Result<Self, String> {
        Ok(SaboteurEngine {
            inner: OracleEngine::build(tables, query)?,
        })
    }

    fn seal_epoch(&mut self, deltas: &EpochDeltas) -> Result<(), String> {
        self.inner.seal_epoch(deltas)
    }

    fn answer(&self) -> Result<Canonical, String> {
        let truth = self.inner.answer()?;
        let mut entries = truth.entries().to_vec();
        entries.pop();
        schweep_zset::ZSetBatch::from_entries(truth.schema().clone(), entries)
            .and_then(|b| b.canonical())
            .map_err(|e| e.to_string())
    }

    fn state_fingerprint(&self) -> Result<String, String> {
        Ok(format!("saboteur\n{}", self.answer()?.render()))
    }
}
