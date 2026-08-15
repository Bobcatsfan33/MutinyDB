//! Result stores: the maintained integral of a query's output stream (`ARCHITECTURE.md` §1, §5.7).
//!
//! > **Integral / integrate** — summing all deltas of a stream from epoch 1 to N, yielding the
//! > full state as of N. `integrate(deltas) = current contents`. The **result store** for a
//! > standing query is the maintained integral of its output stream.
//!
//! This is the one piece of state a C1 circuit has. The operators keep nothing; the answer is
//! kept here, and it is kept by adding each epoch's output delta into it — never by recomputing.
//! Reading the answer is then a lookup, which is the whole point of the architecture: O(change)
//! to maintain, O(1) to read.

use std::collections::BTreeMap;

use schweep_zset::{Canonical, Row, Schema, ZSetBatch};

use crate::error::{CircuitError, Result};

/// The maintained integral of one output stream.
///
/// Held in a `BTreeMap` rather than a hash map so that iteration order is a function of the data
/// alone (I-2). An answer that depended on hash order would differ between two runs of the same
/// log, which is the failure I-2 exists to prevent.
#[derive(Debug, Clone)]
pub struct ResultStore {
    schema: Schema,
    integral: BTreeMap<Row, i64>,
}

impl ResultStore {
    #[must_use]
    pub fn new(schema: Schema) -> ResultStore {
        ResultStore {
            schema,
            integral: BTreeMap::new(),
        }
    }

    #[must_use]
    pub fn schema(&self) -> &Schema {
        &self.schema
    }

    /// The number of distinct rows held. This is the store's contribution to the engine's memory
    /// cost, and what a state fingerprint reports.
    #[must_use]
    pub fn len(&self) -> usize {
        self.integral.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.integral.is_empty()
    }

    /// Add one epoch's output delta into the integral.
    ///
    /// A row whose weight reaches zero is removed rather than kept at zero: the canonical form of
    /// a Z-set has no zero-weight entries (S-8), and keeping them would leak memory in exactly the
    /// slow way I-9 is about — a standing query over churning data would accumulate a tombstone
    /// per row it ever saw.
    ///
    /// The sign of a weight is never inspected (I-5): a retraction is added, like everything else.
    pub fn absorb(&mut self, delta: &ZSetBatch) -> Result<()> {
        if delta.schema() != &self.schema {
            return Err(CircuitError::ZSet(
                schweep_zset::ZSetError::SchemaMismatch {
                    left: self.schema.to_string(),
                    right: delta.schema().to_string(),
                },
            ));
        }
        for (row, weight) in delta.entries()? {
            match self.integral.entry(row) {
                std::collections::btree_map::Entry::Vacant(slot) => {
                    if weight != 0 {
                        slot.insert(weight);
                    }
                }
                std::collections::btree_map::Entry::Occupied(mut slot) => {
                    let sum =
                        slot.get()
                            .checked_add(weight)
                            .ok_or(CircuitError::WeightOverflow {
                                while_doing: "absorbing a delta into a result store",
                            })?;
                    if sum == 0 {
                        slot.remove();
                    } else {
                        slot.insert(sum);
                    }
                }
            }
        }
        Ok(())
    }

    /// The current contents, as a Z-set batch.
    pub fn contents(&self) -> Result<ZSetBatch> {
        let entries: Vec<(Row, i64)> = self
            .integral
            .iter()
            .map(|(row, weight)| (row.clone(), *weight))
            .collect();
        Ok(ZSetBatch::from_entries(self.schema.clone(), entries)?)
    }

    /// The current contents in canonical form (S-8) — the answer, as the harness compares it.
    pub fn canonical(&self) -> Result<Canonical> {
        Ok(self.contents()?.canonical()?)
    }

    /// Serialise the integral for a checkpoint (`docs/DURABILITY.md` C1).
    pub fn snapshot(&self) -> Result<Vec<u8>> {
        let entries: Vec<(Vec<schweep_zset::Value>, i64)> = self
            .integral
            .iter()
            .map(|(row, weight)| (row.values().to_vec(), *weight))
            .collect();
        Ok(schweep_state::encode_entries(&entries))
    }

    /// Replace the integral with a snapshot.
    pub fn restore(&mut self, bytes: &[u8]) -> Result<()> {
        let entries = schweep_state::decode_entries(bytes)
            .map_err(|e| CircuitError::Snapshot(e.to_string()))?;
        self.integral = entries
            .into_iter()
            .map(|(values, weight)| (Row::new(values), weight))
            .collect();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]

    use super::*;
    use schweep_zset::{DataType, Field, Value};

    fn schema() -> Schema {
        Schema::new(vec![Field::nullable("v", DataType::Int64)]).unwrap()
    }

    fn batch(entries: Vec<(i64, i64)>) -> ZSetBatch {
        ZSetBatch::from_entries(
            schema(),
            entries
                .into_iter()
                .map(|(v, w)| (Row::new(vec![Value::Int(v)]), w))
                .collect(),
        )
        .unwrap()
    }

    #[test]
    fn absorbing_deltas_maintains_the_integral() {
        let mut store = ResultStore::new(schema());
        store.absorb(&batch(vec![(1, 2), (2, 1)])).unwrap();
        store.absorb(&batch(vec![(1, 1), (3, 5)])).unwrap();
        assert_eq!(
            store.canonical().unwrap().render(),
            "(v: Int64)\n(1) => 3\n(2) => 1\n(3) => 5\n"
        );
    }

    #[test]
    fn a_row_retracted_to_zero_leaves_no_tombstone() {
        let mut store = ResultStore::new(schema());
        store.absorb(&batch(vec![(1, 1), (2, 1)])).unwrap();
        store.absorb(&batch(vec![(1, -1)])).unwrap();
        assert_eq!(
            store.canonical().unwrap().render(),
            "(v: Int64)\n(2) => 1\n"
        );
        assert_eq!(
            store.len(),
            1,
            "a drained row is removed, not kept at weight zero"
        );
    }

    #[test]
    fn a_row_can_pass_through_zero_and_come_back() {
        let mut store = ResultStore::new(schema());
        store.absorb(&batch(vec![(1, 1)])).unwrap();
        store.absorb(&batch(vec![(1, -1)])).unwrap();
        store.absorb(&batch(vec![(1, 4)])).unwrap();
        assert_eq!(
            store.canonical().unwrap().render(),
            "(v: Int64)\n(1) => 4\n"
        );
    }

    #[test]
    fn absorbing_an_empty_delta_changes_nothing() {
        let mut store = ResultStore::new(schema());
        store.absorb(&batch(vec![(7, 3)])).unwrap();
        let before = store.canonical().unwrap();
        store.absorb(&batch(vec![])).unwrap();
        assert_eq!(store.canonical().unwrap(), before);
    }

    #[test]
    fn the_order_deltas_arrive_in_does_not_change_the_integral() {
        let mut a = ResultStore::new(schema());
        a.absorb(&batch(vec![(1, 1)])).unwrap();
        a.absorb(&batch(vec![(2, 2)])).unwrap();
        let mut b = ResultStore::new(schema());
        b.absorb(&batch(vec![(2, 2)])).unwrap();
        b.absorb(&batch(vec![(1, 1)])).unwrap();
        assert_eq!(a.canonical().unwrap(), b.canonical().unwrap());
    }

    #[test]
    fn overflow_is_reported_rather_than_wrapped() {
        let mut store = ResultStore::new(schema());
        store.absorb(&batch(vec![(1, i64::MAX)])).unwrap();
        assert_eq!(
            store.absorb(&batch(vec![(1, 1)])).unwrap_err(),
            CircuitError::WeightOverflow {
                while_doing: "absorbing a delta into a result store"
            }
        );
    }

    #[test]
    fn a_delta_with_the_wrong_schema_is_refused() {
        let mut store = ResultStore::new(schema());
        let other = ZSetBatch::from_entries(
            Schema::new(vec![Field::nullable("w", DataType::Int64)]).unwrap(),
            vec![(Row::new(vec![Value::Int(1)]), 1)],
        )
        .unwrap();
        assert!(store.absorb(&other).is_err());
    }
}
