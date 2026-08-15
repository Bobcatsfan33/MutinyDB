//! The live-error set (`docs/SEMANTICS.md` S-22, S-22b, S-22c; D-16).
//!
//! An evaluation error is a property of the *contents*, not of the change: the query has no answer
//! while data that raises is present, and the answer returns when that data is retracted. The
//! oracle recomputes from scratch, so "the live errors" are simply the errors encountered while
//! recomputing over the current contents — collected rather than propagated, because the least
//! message can only be reported once all of them are known (S-22c).
//!
//! Keyed by message in a `BTreeMap`, so "least" is the first entry and the choice does not depend
//! on the order rows were visited (I-2).

use std::collections::BTreeMap;

use schweep_plan::PlanError;

use crate::error::{OracleError, Result};

#[derive(Debug, Clone, Default)]
pub struct LiveErrors {
    by_message: BTreeMap<String, PlanError>,
}

impl LiveErrors {
    #[must_use]
    pub fn new() -> LiveErrors {
        LiveErrors::default()
    }

    /// Record an error as live, or propagate it if it is not a fact about the data.
    ///
    /// A refusal or a type mismatch is a statement about the *query*, and a query that does not
    /// bind never runs — so those are bugs if they reach here and are returned rather than
    /// collected. Only the S-22 evaluation errors become live.
    pub fn record(&mut self, error: PlanError) -> Result<()> {
        if !error.is_evaluation_error() {
            return Err(OracleError::Plan(error));
        }
        self.by_message.insert(error.to_string(), error);
        Ok(())
    }

    /// The least live error by message (S-22c), or `None` if the query has an answer.
    #[must_use]
    pub fn least(&self) -> Option<PlanError> {
        self.by_message.values().next().cloned()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.by_message.is_empty()
    }

    /// How many distinct live errors there are. More than one is legal (S-22c).
    #[must_use]
    pub fn len(&self) -> usize {
        self.by_message.len()
    }
}
