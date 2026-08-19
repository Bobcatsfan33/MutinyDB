//! Forked standing state (M5), on the path MD-5's spike verdict selected: **Option B**, the
//! deliberate fallback. A fork hydrates the child's standing operators from the parent's live
//! state — O(state), measured and published, never claimed O(1) — and the fork itself is an
//! ordinary commit through the M1 front door, so recovery rebuilds every branch's circuits by
//! replaying the commit history through the lineage this module models.
//!
//! This library is deliberately pure: lineage records, descendant computation, and merge
//! planning. The storage, engine, and trust wiring live in the composed host, and the spike
//! evidence behind the path choice lives in `tests/m5_spike.rs` + `evidence/`.

use loom_core::SourceRef;
use schweep_zset::{DataType, Field, Row, Schema, Value};
use std::collections::{BTreeMap, BTreeSet};

/// The fork-lineage relation: an ordinary table on the ordinary commit path. One row per fork or
/// rewind event; never retracted (a rewind is recorded, not erased — auditable, never destroyed).
pub const FORKS_TABLE: &str = "mutiny_forks";

/// The source system fork/rewind/merge bookkeeping rows cite in their envelopes. Distinct from
/// the taint core's reserved internal system so lineage rows can never be mistaken for a
/// derivation chain hop.
pub const LINEAGE_SOURCE_SYSTEM: &str = "loom";

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ForkKind {
    Fork,
    Rewind,
}

impl ForkKind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            ForkKind::Fork => "fork",
            ForkKind::Rewind => "rewind",
        }
    }

    #[must_use]
    pub fn parse(text: &str) -> Option<ForkKind> {
        match text {
            "fork" => Some(ForkKind::Fork),
            "rewind" => Some(ForkKind::Rewind),
            _ => None,
        }
    }
}

/// One durable lineage event, exactly as the relation stores it.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ForkEvent {
    pub child: String,
    pub parent: String,
    /// The storage commit sequence (= compute epoch) the event landed at.
    pub at_epoch: u64,
    pub kind: ForkKind,
}

#[derive(Debug, thiserror::Error)]
pub enum ForkError {
    #[error("the fork relation schema is invalid: {reason}")]
    Schema { reason: String },
    #[error("fork relation row is malformed: {reason}")]
    Row { reason: String },
    #[error("branch {child:?} already has a fork record (from {parent:?} at epoch {at_epoch})")]
    AlreadyForked {
        child: String,
        parent: String,
        at_epoch: u64,
    },
    #[error("branch {branch:?} has no fork record, so it cannot be merged or rewound by lineage")]
    UnknownBranch { branch: String },
    #[error("branch {branch:?} was rewound at epoch {at_epoch} and carries no standing state")]
    Rewound { branch: String, at_epoch: u64 },
}

/// The fixed lineage schema. Creation and every reader use this one function, for the same reason
/// the bridge fixes `derivation_schema` and the taint core fixes its ledger schema.
pub fn forks_schema() -> Result<Schema, ForkError> {
    Schema::new_table(vec![
        Field::not_null("child", DataType::Utf8),
        Field::not_null("parent", DataType::Utf8),
        Field::not_null("at_epoch", DataType::Int64),
        Field::not_null("kind", DataType::Utf8),
    ])
    .map_err(|error| ForkError::Schema {
        reason: error.to_string(),
    })
}

impl ForkEvent {
    #[must_use]
    pub fn to_row(&self) -> Row {
        Row::new(vec![
            Value::Str(self.child.clone()),
            Value::Str(self.parent.clone()),
            Value::Int(self.at_epoch as i64),
            Value::Str(self.kind.as_str().to_owned()),
        ])
    }

    pub fn from_row(row: &Row) -> Result<ForkEvent, ForkError> {
        let text = |index: usize| -> Result<String, ForkError> {
            match row.get(index) {
                Some(Value::Str(value)) => Ok(value.clone()),
                other => Err(ForkError::Row {
                    reason: format!("column {index} is {other:?}, expected a string"),
                }),
            }
        };
        let at_epoch = match row.get(2) {
            Some(Value::Int(value)) if *value >= 0 => *value as u64,
            other => {
                return Err(ForkError::Row {
                    reason: format!("column 2 is {other:?}, expected a non-negative integer"),
                })
            }
        };
        let kind_text = text(3)?;
        let kind = ForkKind::parse(&kind_text).ok_or_else(|| ForkError::Row {
            reason: format!("unknown lineage kind {kind_text:?}"),
        })?;
        Ok(ForkEvent {
            child: text(0)?,
            parent: text(1)?,
            at_epoch,
            kind,
        })
    }
}

/// The envelope source a fork/rewind record cites: lineage is derived from the parent branch.
#[must_use]
pub fn lineage_source(parent: &str) -> SourceRef {
    SourceRef::new(LINEAGE_SOURCE_SYSTEM, format!("branch/{parent}"))
}

/// The durable merge marker a merged row's envelope carries **in addition to** the row's own
/// original sources (Loom I-2's per-key rule: each merged record keeps its own parents). The
/// marker is the composed analog of Loom's merged-from memory: its presence in the derivation
/// relation is what makes a crash-resumed or repeated merge a no-op instead of a double-apply.
#[must_use]
pub fn merge_marker_source(child: &str) -> SourceRef {
    SourceRef::new(LINEAGE_SOURCE_SYSTEM, format!("merge/{child}"))
}

/// The lineage, folded from the relation's events.
#[derive(Clone, Debug, Default)]
pub struct Lineage {
    forks: BTreeMap<String, (String, u64)>,
    rewinds: BTreeMap<String, u64>,
}

impl Lineage {
    pub fn from_events(events: impl IntoIterator<Item = ForkEvent>) -> Result<Lineage, ForkError> {
        let mut lineage = Lineage::default();
        for event in events {
            match event.kind {
                ForkKind::Fork => {
                    if let Some((parent, at_epoch)) = lineage.forks.get(&event.child) {
                        return Err(ForkError::AlreadyForked {
                            child: event.child,
                            parent: parent.clone(),
                            at_epoch: *at_epoch,
                        });
                    }
                    lineage
                        .forks
                        .insert(event.child, (event.parent, event.at_epoch));
                }
                ForkKind::Rewind => {
                    lineage.rewinds.insert(event.child, event.at_epoch);
                }
            }
        }
        Ok(lineage)
    }

    /// The fork record for a branch, if it was created by a fork.
    #[must_use]
    pub fn fork_of(&self, branch: &str) -> Option<(&str, u64)> {
        self.forks
            .get(branch)
            .map(|(parent, at_epoch)| (parent.as_str(), *at_epoch))
    }

    #[must_use]
    pub fn rewound_at(&self, branch: &str) -> Option<u64> {
        self.rewinds.get(branch).copied()
    }

    /// Whether the branch currently carries standing state: not rewound. Session roots (branches
    /// with no fork record) are active by definition.
    #[must_use]
    pub fn is_active(&self, branch: &str) -> bool {
        !self.rewinds.contains_key(branch)
    }

    /// Every **active** transitive descendant of `branch`, in deterministic order. This is the
    /// set a taint heal cascades to: a descendant's standing state was hydrated from an ancestor
    /// and may hold the contaminated rows, and retract-by-key skips branches that never inherited
    /// or already diverged away from them.
    #[must_use]
    pub fn active_descendants(&self, branch: &str) -> Vec<String> {
        let mut found = BTreeSet::new();
        let mut frontier = vec![branch.to_owned()];
        while let Some(current) = frontier.pop() {
            for (child, (parent, _)) in &self.forks {
                if parent == &current && !found.contains(child) {
                    found.insert(child.clone());
                    frontier.push(child.clone());
                }
            }
        }
        found
            .into_iter()
            .filter(|child| self.is_active(child))
            .collect()
    }

    /// Plan a merge of `child` into its parent: which of the candidate rows (each with the epoch
    /// of the commit that introduced it) are post-fork divergence not yet marked as merged.
    ///
    /// This is where the Loom double-count class dies: rows at or before the fork epoch are
    /// shared history and are never re-applied, and rows whose key carries the durable merge
    /// marker were already merged by a previous (possibly crashed) attempt.
    pub fn merge_divergence<'a, T>(
        &self,
        child: &str,
        candidates: &'a [(u64, String, T)],
        already_merged: &BTreeSet<String>,
    ) -> Result<Vec<&'a (u64, String, T)>, ForkError> {
        let Some((_, fork_epoch)) = self.fork_of(child) else {
            return Err(ForkError::UnknownBranch {
                branch: child.to_owned(),
            });
        };
        if let Some(at_epoch) = self.rewound_at(child) {
            return Err(ForkError::Rewound {
                branch: child.to_owned(),
                at_epoch,
            });
        }
        Ok(candidates
            .iter()
            .filter(|(epoch, key, _)| *epoch > fork_epoch && !already_merged.contains(key))
            .collect())
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    fn event(child: &str, parent: &str, at: u64, kind: ForkKind) -> ForkEvent {
        ForkEvent {
            child: child.to_owned(),
            parent: parent.to_owned(),
            at_epoch: at,
            kind,
        }
    }

    #[test]
    fn descendants_cascade_transitively_and_skip_rewound_branches() {
        let lineage = Lineage::from_events([
            event("hyp-a", "sess-a", 4, ForkKind::Fork),
            event("hyp-b", "sess-a", 5, ForkKind::Fork),
            event("hyp-a2", "hyp-a", 7, ForkKind::Fork),
            event("hyp-b", "sess-a", 9, ForkKind::Rewind),
        ])
        .expect("lineage folds");
        assert_eq!(
            lineage.active_descendants("sess-a"),
            vec!["hyp-a", "hyp-a2"]
        );
        assert!(!lineage.is_active("hyp-b"));
        assert_eq!(lineage.fork_of("hyp-a2"), Some(("hyp-a", 7)));
    }

    #[test]
    fn a_second_fork_record_for_the_same_child_is_refused() {
        let result = Lineage::from_events([
            event("hyp-a", "sess-a", 4, ForkKind::Fork),
            event("hyp-a", "sess-b", 6, ForkKind::Fork),
        ]);
        assert!(matches!(result, Err(ForkError::AlreadyForked { .. })));
    }

    #[test]
    fn merge_divergence_excludes_shared_history_and_already_merged_rows() {
        let lineage = Lineage::from_events([event("hyp-a", "sess-a", 4, ForkKind::Fork)])
            .expect("lineage folds");
        let candidates = vec![
            (3, "inherited".to_owned(), ()),
            (5, "diverged-1".to_owned(), ()),
            (6, "diverged-2".to_owned(), ()),
        ];
        let already: BTreeSet<String> = ["diverged-1".to_owned()].into();
        let plan = lineage
            .merge_divergence("hyp-a", &candidates, &already)
            .expect("plan");
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].1, "diverged-2");

        // A rewound branch cannot be merged; its state is gone by decision, not by accident.
        let rewound = Lineage::from_events([
            event("hyp-a", "sess-a", 4, ForkKind::Fork),
            event("hyp-a", "sess-a", 8, ForkKind::Rewind),
        ])
        .expect("lineage folds");
        assert!(matches!(
            rewound.merge_divergence("hyp-a", &candidates, &BTreeSet::new()),
            Err(ForkError::Rewound { .. })
        ));
    }

    #[test]
    fn fork_events_round_trip_through_their_relation_rows() {
        let original = event("hyp-a", "sess-a", 4, ForkKind::Fork);
        let parsed = ForkEvent::from_row(&original.to_row()).expect("row parses");
        assert_eq!(parsed, original);
        assert!(forks_schema().is_ok());
    }
}
