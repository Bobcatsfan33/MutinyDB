//! `MemBackend`: the in-memory implementation (`ARCHITECTURE.md` §5.5).
//!
//! A `BTreeMap`, which gives the ordered scans the trait promises for free and gives them from the
//! same total order on values that `docs/SEMANTICS.md` defines and tests (S-7). Nothing here is
//! clever, and when `RocksBackend` arrives in C4 this is what it will be tested against.

use std::collections::BTreeMap;

use schweep_zset::Value;

use crate::backend::{Key, StateBackend, WriteBatch};
use crate::codec::{decode_entries, encode_entries};
use crate::error::{Result, StateError};

#[derive(Debug, Clone, Default)]
pub struct MemBackend {
    entries: BTreeMap<Key, i64>,
}

impl MemBackend {
    #[must_use]
    pub fn new() -> MemBackend {
        MemBackend::default()
    }
}

impl StateBackend for MemBackend {
    /// Atomic by construction: the batch is validated into a staging map first, so an overflow
    /// part-way through leaves the store untouched rather than half-updated.
    fn write(&mut self, batch: &WriteBatch) -> Result<()> {
        let mut staged: BTreeMap<Key, i64> = BTreeMap::new();
        for (key, weight) in batch.entries() {
            let base = match staged.get(key) {
                Some(pending) => *pending,
                None => self.entries.get(key).copied().unwrap_or(0),
            };
            let sum = base
                .checked_add(*weight)
                .ok_or(StateError::WeightOverflow {
                    while_doing: "adding a weight to operator state",
                })?;
            staged.insert(key.clone(), sum);
        }

        for (key, sum) in staged {
            if sum == 0 {
                self.entries.remove(&key);
            } else {
                self.entries.insert(key, sum);
            }
        }
        Ok(())
    }

    fn visit_prefix(
        &self,
        prefix: &[Value],
        visitor: &mut dyn FnMut(&Key, i64) -> bool,
    ) -> Result<()> {
        // A prefix is one contiguous B-tree range. Starting at the prefix avoids C2's full-map filter;
        // the first non-matching key ends the range, and no intermediate `Vec` is allocated (D-25).
        for (key, weight) in self.entries.range(prefix.to_vec()..) {
            if key.len() < prefix.len() || !key.starts_with(prefix) {
                break;
            }
            if !visitor(key, *weight) {
                break;
            }
        }
        Ok(())
    }

    fn get(&self, key: &[Value]) -> Result<Option<i64>> {
        Ok(self.entries.get(key).copied())
    }

    fn len(&self) -> usize {
        self.entries.len()
    }

    fn iter_all(&self) -> Result<Vec<(Key, i64)>> {
        Ok(self
            .entries
            .iter()
            .map(|(key, weight)| (key.clone(), *weight))
            .collect())
    }

    fn snapshot(&self) -> Result<Vec<u8>> {
        Ok(encode_entries(&self.iter_all()?))
    }

    fn restore(&mut self, bytes: &[u8]) -> Result<()> {
        let entries = decode_entries(bytes)?;
        self.entries = entries.into_iter().collect();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]

    use super::*;
    use schweep_zset::Value;

    fn key(values: &[i64]) -> Key {
        values.iter().map(|v| Value::Int(*v)).collect()
    }

    #[test]
    fn weights_accumulate_and_a_zero_weight_entry_disappears() {
        let mut backend = MemBackend::new();
        let mut batch = WriteBatch::new();
        batch.add(key(&[1, 10]), 2);
        batch.add(key(&[1, 20]), 1);
        backend.write(&batch).unwrap();
        assert_eq!(backend.len(), 2);
        assert_eq!(backend.get(&key(&[1, 10])).unwrap(), Some(2));

        let mut batch = WriteBatch::new();
        batch.add(key(&[1, 10]), -2);
        backend.write(&batch).unwrap();
        assert_eq!(backend.get(&key(&[1, 10])).unwrap(), None);
        assert_eq!(backend.len(), 1, "no tombstone is left behind");
    }

    #[test]
    fn additions_within_one_batch_are_combined() {
        let mut backend = MemBackend::new();
        let mut batch = WriteBatch::new();
        batch.add(key(&[1]), 3);
        batch.add(key(&[1]), -1);
        backend.write(&batch).unwrap();
        assert_eq!(backend.get(&key(&[1])).unwrap(), Some(2));
    }

    #[test]
    fn a_batch_that_nets_to_zero_within_itself_leaves_nothing() {
        let mut backend = MemBackend::new();
        let mut batch = WriteBatch::new();
        batch.add(key(&[7]), 1);
        batch.add(key(&[7]), -1);
        backend.write(&batch).unwrap();
        assert!(backend.is_empty());
    }

    #[test]
    fn a_prefix_scan_returns_only_matching_keys_in_key_order() {
        let mut backend = MemBackend::new();
        let mut batch = WriteBatch::new();
        for k in [[2, 1], [1, 30], [1, 10], [1, 20]] {
            batch.add(key(&k), 1);
        }
        backend.write(&batch).unwrap();

        let found = backend.scan_prefix(&[Value::Int(1)]).unwrap();
        assert_eq!(
            found,
            vec![(key(&[1, 10]), 1), (key(&[1, 20]), 1), (key(&[1, 30]), 1),],
            "a prefix scan is ordered and excludes other prefixes"
        );
    }

    #[test]
    fn an_empty_prefix_scans_everything() {
        let mut backend = MemBackend::new();
        let mut batch = WriteBatch::new();
        batch.add(key(&[1]), 1);
        batch.add(key(&[2]), 1);
        backend.write(&batch).unwrap();
        assert_eq!(backend.scan_prefix(&[]).unwrap().len(), 2);
    }

    #[test]
    fn a_prefix_visitor_stops_without_reading_the_rest() {
        let mut backend = MemBackend::new();
        let mut batch = WriteBatch::new();
        for suffix in 0..1_000 {
            batch.add(key(&[1, suffix]), 1);
        }
        backend.write(&batch).unwrap();

        let mut visited = 0;
        backend
            .visit_prefix(&[Value::Int(1)], &mut |_key, _weight| {
                visited += 1;
                false
            })
            .unwrap();
        assert_eq!(visited, 1, "the visitor's stop signal is an actual bound");
    }

    #[test]
    fn a_null_key_is_scannable_and_sorts_first() {
        let mut backend = MemBackend::new();
        let mut batch = WriteBatch::new();
        batch.add(vec![Value::Null, Value::Int(1)], 1);
        batch.add(vec![Value::Int(0), Value::Int(1)], 1);
        backend.write(&batch).unwrap();

        let all = backend.iter_all().unwrap();
        assert_eq!(
            all.first().map(|(k, _)| k.first()),
            Some(Some(&Value::Null)),
            "nulls sort first (S-7), and the backend inherits that order"
        );
        // A null prefix scans only null-keyed rows — which is why the join must decline to probe
        // with a null key rather than relying on the scan to miss (S-26).
        assert_eq!(backend.scan_prefix(&[Value::Null]).unwrap().len(), 1);
    }

    #[test]
    fn an_overflowing_batch_leaves_the_store_untouched() {
        let mut backend = MemBackend::new();
        let mut batch = WriteBatch::new();
        batch.add(key(&[1]), i64::MAX);
        backend.write(&batch).unwrap();

        let mut batch = WriteBatch::new();
        batch.add(key(&[2]), 5);
        batch.add(key(&[1]), 1);
        assert_eq!(
            backend.write(&batch).unwrap_err(),
            StateError::WeightOverflow {
                while_doing: "adding a weight to operator state"
            }
        );
        assert_eq!(
            backend.get(&key(&[2])).unwrap(),
            None,
            "the earlier addition in the failed batch must not have landed"
        );
        assert_eq!(backend.get(&key(&[1])).unwrap(), Some(i64::MAX));
    }

    #[test]
    fn a_snapshot_restores_to_an_identical_backend() {
        let mut backend = MemBackend::new();
        let mut batch = WriteBatch::new();
        batch.add(key(&[1, 10]), 2);
        batch.add(vec![Value::Null, Value::Str("x".into())], -3);
        backend.write(&batch).unwrap();

        let bytes = backend.snapshot().unwrap();
        let mut restored = MemBackend::new();
        // Put something in it first: restore must REPLACE, not merge.
        let mut noise = WriteBatch::new();
        noise.add(key(&[99]), 1);
        restored.write(&noise).unwrap();

        restored.restore(&bytes).unwrap();
        assert_eq!(restored.iter_all().unwrap(), backend.iter_all().unwrap());
        assert_eq!(restored.get(&key(&[99])).unwrap(), None, "restore replaces");
    }

    #[test]
    fn an_empty_batch_is_a_no_op() {
        let mut backend = MemBackend::new();
        backend.write(&WriteBatch::new()).unwrap();
        assert!(backend.is_empty());
    }
}
