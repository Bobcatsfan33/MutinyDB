//! One-shot execution wearing the [`EngineUnderTest`] costume (§6 C7).
//!
//! A one-shot query keeps nothing: this adapter accumulates the scenario's input as it goes and, when
//! asked for an answer, builds an ephemeral circuit, feeds it the whole accumulated input as one delta,
//! reads it, and drops it. Sweeping the generated population through it is the C7 gate condition
//! *"one-shot answers equal oracle over the fuzz suite"*.
//!
//! It is also the honest measure of what one-shot costs: every answer is a full recomputation, which is
//! exactly what the standing path exists to avoid. The adapter makes that cost visible rather than
//! hiding it — [`OneShotEngine::recomputations`] counts them.

use std::collections::BTreeMap;

use schweep_batch::oneshot;
use schweep_plan::bind::Catalog;
use schweep_plan::plan::Query;
use schweep_zset::{Canonical, EpochDeltas, Schema, ZSetBatch};

use crate::engine::EngineUnderTest;

/// An [`EngineUnderTest`] that answers by recomputing, once, through an ephemeral circuit.
#[derive(Debug)]
pub struct OneShotEngine {
    catalog: Catalog,
    query: Query,
    /// The accumulated input, consolidated as it grows — the same integral a snapshot holds.
    integrals: BTreeMap<String, ZSetBatch>,
    recomputations: std::cell::Cell<usize>,
}

impl OneShotEngine {
    /// How many ephemeral circuits this engine has built and thrown away.
    #[must_use]
    pub fn recomputations(&self) -> usize {
        self.recomputations.get()
    }

    #[must_use]
    pub fn integrals(&self) -> &BTreeMap<String, ZSetBatch> {
        &self.integrals
    }
}

impl EngineUnderTest for OneShotEngine {
    fn name() -> &'static str {
        "one-shot"
    }

    fn build(tables: &[(String, Schema)], query: &Query) -> Result<Self, String> {
        let catalog: Catalog = tables.iter().cloned().collect();
        // Bind once here so a query outside the dialect is refused at build time, exactly as the
        // standing path refuses it — otherwise a one-shot could answer something the engine would not.
        schweep_plan::bind(query, &catalog).map_err(|e| e.to_string())?;
        Ok(OneShotEngine {
            catalog,
            query: query.clone(),
            integrals: BTreeMap::new(),
            recomputations: std::cell::Cell::new(0),
        })
    }

    fn seal_epoch(&mut self, deltas: &EpochDeltas) -> Result<(), String> {
        for (table, entries) in deltas.tables() {
            let schema = self
                .catalog
                .get(table)
                .ok_or_else(|| format!("no table named {table:?}"))?;
            let delta = ZSetBatch::from_entries(schema.clone(), entries.clone())
                .map_err(|e| e.to_string())?;
            let merged = match self.integrals.get(table) {
                Some(held) => held.add(&delta).map_err(|e| e.to_string())?,
                None => delta,
            };
            self.integrals.insert(
                table.clone(),
                merged.consolidate().map_err(|e| e.to_string())?,
            );
        }
        Ok(())
    }

    fn answer(&self) -> Result<Canonical, String> {
        self.recomputations.set(self.recomputations.get() + 1);
        oneshot::answer_over_integrals(&self.catalog, &self.query, &self.integrals)
            .map_err(|e| e.to_string())
    }

    /// A one-shot holds no derived state between questions — the circuit is gone — so its whole
    /// condition is the input it has accumulated and the answer that input produces. Saying that is
    /// more honest than inventing a state dump.
    fn state_fingerprint(&self) -> Result<String, String> {
        let mut out =
            String::from("one-shot · no derived state (an ephemeral circuit per answer)\n");
        for (table, integral) in &self.integrals {
            out.push_str(&format!("input {table}\n"));
            out.push_str(&integral.canonical().map_err(|e| e.to_string())?.render());
        }
        out.push_str(&self.answer()?.render());
        Ok(out)
    }
}
