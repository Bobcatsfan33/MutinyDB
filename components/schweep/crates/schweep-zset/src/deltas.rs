//! The input deltas of one epoch (`docs/SEMANTICS.md` S-4, S-6).
//!
//! One bundle of `(row, weight)` entries per table, forming the change that becomes visible when
//! an epoch is sealed. This lives in `schweep-zset` because it *is* the delta representation —
//! §1's "a Z-set representing what changed between two epochs" — and every crate depends on this
//! one. Its eventual home for the *write path* is `schweep-log` (§5.4), which arrives in C4; until
//! then one shared type beats a private copy per crate (D-14).
//!
//! ## Entries are not consolidated on the way in
//!
//! A delta may legitimately contain `(r, +1)` and `(r, -1)` for the same row — the same-epoch
//! retract-and-reinsert that §7 requires the scenario generator to produce, and the shape a real
//! `UPDATE` takes in a Z-set. Merging those at the door would hide the shape from every
//! implementation downstream, and hiding it is exactly how an engine that mishandles it passes.
//!
//! Nothing here inspects the sign of a weight (I-5).

use std::collections::BTreeMap;

use crate::row::Row;

/// Per-table input deltas for one epoch.
///
/// Tables are held in a `BTreeMap` so iteration order is a function of the data alone (I-2).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EpochDeltas {
    tables: BTreeMap<String, Vec<(Row, i64)>>,
}

impl EpochDeltas {
    #[must_use]
    pub fn new() -> EpochDeltas {
        EpochDeltas::default()
    }

    /// Append one entry. A negative weight is a retraction and needs no special call (I-5).
    pub fn push(&mut self, table: impl Into<String>, row: Row, weight: i64) {
        self.tables
            .entry(table.into())
            .or_default()
            .push((row, weight));
    }

    pub fn extend(
        &mut self,
        table: impl Into<String>,
        entries: impl IntoIterator<Item = (Row, i64)>,
    ) {
        self.tables.entry(table.into()).or_default().extend(entries);
    }

    #[must_use]
    pub fn tables(&self) -> &BTreeMap<String, Vec<(Row, i64)>> {
        &self.tables
    }

    /// The entries for one table, or an empty slice if this epoch does not touch it.
    #[must_use]
    pub fn entries_for(&self, table: &str) -> &[(Row, i64)] {
        self.tables.get(table).map_or(&[], Vec::as_slice)
    }

    /// True if this epoch carries no changes at all. Empty epochs are legal and the generator
    /// produces them deliberately (§7): the answer must not move.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.tables.values().all(Vec::is_empty)
    }

    /// The number of entries across all tables, retractions included.
    #[must_use]
    pub fn entry_count(&self) -> usize {
        self.tables.values().map(Vec::len).sum()
    }

    /// A deterministic rendering, for scenario fingerprints and failure reports.
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = String::new();
        for (table, entries) in &self.tables {
            for (row, weight) in entries {
                out.push_str(&format!("  {table}: {row} => {weight}\n"));
            }
        }
        if out.is_empty() {
            out.push_str("  (empty epoch)\n");
        }
        out
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]

    use super::*;
    use crate::value::Value;

    fn row(v: i64) -> Row {
        Row::new(vec![Value::Int(v)])
    }

    #[test]
    fn entries_are_kept_in_arrival_order_and_not_merged() {
        let mut d = EpochDeltas::new();
        d.push("t", row(1), 1);
        d.push("t", row(1), -1);
        // The retract-and-reinsert shape survives the door.
        assert_eq!(
            d.entries_for("t"),
            &[(row(1), 1), (row(1), -1)],
            "a delta that nets to zero must still carry both entries"
        );
        assert_eq!(d.entry_count(), 2);
        assert!(
            !d.is_empty(),
            "carrying entries is not the same as being empty"
        );
    }

    #[test]
    fn an_untouched_table_yields_an_empty_slice_not_an_error() {
        let d = EpochDeltas::new();
        assert!(d.entries_for("absent").is_empty());
        assert!(d.is_empty());
        assert_eq!(d.render(), "  (empty epoch)\n");
    }

    #[test]
    fn tables_iterate_in_name_order_whatever_order_they_arrived_in() {
        let mut a = EpochDeltas::new();
        a.push("z", row(1), 1);
        a.push("a", row(2), 1);
        let mut b = EpochDeltas::new();
        b.push("a", row(2), 1);
        b.push("z", row(1), 1);
        assert_eq!(a, b);
        assert_eq!(a.render(), b.render());
        assert_eq!(
            a.tables().keys().collect::<Vec<_>>(),
            vec!["a", "z"],
            "iteration order is a function of the data (I-2)"
        );
    }
}
