//! The memo wearing the [`EngineUnderTest`] costume — one query, registered into a shared dataflow.
//!
//! A memo holding a single standing query shares nothing, so this adapter does not test sharing. What
//! it tests is everything sharing is built on: that a registration wires a plan correctly into a live
//! dataflow, that catch-up and priming behave, that a sink's answer and its live errors are the ones
//! the query would have had in a private circuit. Running the whole generated population through it
//! means the memo's *plumbing* is under I-1, not only under the C6 gate's hand-written battery.
//!
//! Sharing itself is checked by `testing/differential/tests/c6_memo.rs`, which registers overlapping
//! queries and compares the same battery with sharing on and off (I-8).

use schweep_memo::{Admission, Handle, Memo, Sharing};
use schweep_plan::bind::Catalog;
use schweep_plan::plan::Query;
use schweep_sql::incrementalize_typed;
use schweep_zset::{Canonical, EpochDeltas, Schema};

use crate::engine::EngineUnderTest;

/// An [`EngineUnderTest`] that answers through a memo registration.
#[derive(Debug)]
pub struct MemoEngine {
    memo: Memo,
    handle: Handle,
}

impl MemoEngine {
    /// Build a memo with one query registered, at a chosen sharing setting.
    pub fn with_sharing(
        tables: &[(String, Schema)],
        query: &Query,
        sharing: Sharing,
    ) -> Result<MemoEngine, String> {
        let catalog: Catalog = tables.iter().cloned().collect();
        let plan = incrementalize_typed(query, &catalog).map_err(|e| e.to_string())?;
        let mut memo = Memo::with_sharing(catalog, sharing).map_err(|e| e.to_string())?;
        let handle = memo
            .register(&plan, Admission::bounded())
            .map_err(|e| e.to_string())?;
        Ok(MemoEngine { memo, handle })
    }

    #[must_use]
    pub fn memo(&self) -> &Memo {
        &self.memo
    }
}

impl EngineUnderTest for MemoEngine {
    fn name() -> &'static str {
        "memo"
    }

    fn build(tables: &[(String, Schema)], query: &Query) -> Result<Self, String> {
        MemoEngine::with_sharing(tables, query, Sharing::On)
    }

    fn seal_epoch(&mut self, deltas: &EpochDeltas) -> Result<(), String> {
        self.memo.seal_epoch(deltas).map_err(|e| e.to_string())?;
        Ok(())
    }

    fn answer(&self) -> Result<Canonical, String> {
        self.memo
            .read(self.handle)
            .map(|(_, answer)| answer)
            .map_err(|e| e.to_string())
    }

    fn state_fingerprint(&self) -> Result<String, String> {
        self.memo
            .dataflow()
            .state_fingerprint()
            .map_err(|e| e.to_string())
    }
}
