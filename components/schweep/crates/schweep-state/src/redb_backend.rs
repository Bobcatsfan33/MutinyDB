//! `RedbBackend`: the durable implementation (D-19, amending D-5; `ARCHITECTURE.md` §5.5).
//!
//! **It implements the C4 surface plus D-25's additive bounded-scan amendment.** C10 proved that the
//! original `scan_prefix -> Vec` contract made one operation require O(group) memory even when state
//! spilled correctly. `visit_prefix` fixes that without changing any existing method's meaning; the
//! reasoning and the renewed freeze are recorded under D-25.
//!
//! ## The shape
//!
//! One redb `Database` per backend instance, one table inside it, keys encoded by
//! [`crate::codec::encode_key`] — the order-preserving codec C4 built for exactly this, whose byte
//! order *is* the S-7 order on values. That is what makes redb's B-tree ranges give the trait's
//! promised scan order for free rather than by sorting afterwards:
//!
//! ```text
//!   scan_prefix([Int(3)])  →  range( encode_key([Int(3)]) ..= encode_key([Int(3)]) + 0xFF… )
//!                             ↑ a prefix of the encoded key is a prefix of the byte string
//! ```
//!
//! ## The one friction the frozen trait caused, stated plainly
//!
//! `StateBackend::len` returns `usize`, not `Result<usize>`. redb cannot count a table without a read
//! transaction, and a transaction can fail. So this backend **maintains the count itself**, updated
//! inside the same write transaction that changes the entries — which is not a workaround so much as
//! the thing the trait's signature was always asking for: a count you can read without asking the disk.
//!
//! It is also the only friction. Every other method mapped without argument, and the two that most
//! plausibly wouldn't — `scan_prefix`'s ordering guarantee and `write`'s atomicity — mapped *better*
//! than to `MemBackend`, because redb gives both natively.
//!
//! ## What the freeze does cost, and it is worth knowing
//!
//! `snapshot()` returns `Vec<u8>`, so a checkpoint of this backend **materialises all of its entries in
//! memory**. For state larger than RAM — the whole point of C8 — that means the state can spill but a
//! checkpoint of it cannot. That is a consequence of the frozen signature, not of redb, and it is named
//! in `docs/PROGRESS.md` rather than worked around here: changing it means unfreezing the trait, which
//! is a decision above this file's pay grade.

use std::path::{Path, PathBuf};

use redb::{Database, ReadableDatabase, ReadableTable, ReadableTableMetadata, TableDefinition};
use schweep_zset::Value;

use crate::backend::{Key, StateBackend, WriteBatch};
use crate::codec::{decode_entries, decode_key, encode_entries, encode_key};
use crate::error::{Result, StateError};

/// The one table every backend instance holds. Named, because redb requires a name, and there is
/// exactly one so the name carries no information.
const ENTRIES: TableDefinition<&[u8], i64> = TableDefinition::new("entries");

/// redb's in-memory page cache, in bytes.
///
/// **A tuned constant, and therefore in the ledger with its receipt**
/// (`testing/evidence/registry.json`, `redb_cache_bytes`). It steers behaviour: too small and every
/// probe reaches the disk, too large and the cache is the memory ceiling C8's gate is about. The
/// number below is the one the benchmark artifact measured, not the one that felt right.
pub const CACHE_BYTES: usize = 1024 * 1024;

/// Operator state in a redb file.
#[derive(Debug)]
pub struct RedbBackend {
    db: Database,
    path: PathBuf,
    /// Live entries, maintained inside the write transaction that changes them.
    ///
    /// See the module docs: `StateBackend::len` is infallible, and counting a redb table is not.
    entries: usize,
}

impl RedbBackend {
    /// Create or open a backend at `path`.
    pub fn open(path: impl AsRef<Path>) -> Result<RedbBackend> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| StateError::Backend(e.to_string()))?;
        }
        let db = Database::builder()
            .set_cache_size(CACHE_BYTES)
            .create(&path)
            .map_err(|e| StateError::Backend(e.to_string()))?;

        // The table has to exist before the first read, and creating it is a write.
        {
            let txn = db
                .begin_write()
                .map_err(|e| StateError::Backend(e.to_string()))?;
            txn.open_table(ENTRIES)
                .map_err(|e| StateError::Backend(e.to_string()))?;
            txn.commit()
                .map_err(|e| StateError::Backend(e.to_string()))?;
        }

        let mut backend = RedbBackend {
            db,
            path,
            entries: 0,
        };
        backend.entries = backend.count()?;
        Ok(backend)
    }

    /// The file this backend's state lives in — for the reconciliation gate, which compares the
    /// entries a query *reports* against the bytes the backend actually occupies.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The size on disk, in bytes.
    ///
    /// Deliberately **not** on the trait: the trait is frozen, and it accounts in *entries* (D-15).
    /// Bytes are a property of one implementation, and `EXPLAIN STATE` reconciles against them from
    /// outside rather than by widening a frozen interface.
    pub fn bytes_on_disk(&self) -> Result<u64> {
        Ok(std::fs::metadata(&self.path)
            .map_err(|e| StateError::Backend(e.to_string()))?
            .len())
    }

    /// Count the table the expensive way. Called once at open, and by tests that want the truth
    /// rather than the cached number.
    pub fn count(&self) -> Result<usize> {
        let txn = self
            .db
            .begin_read()
            .map_err(|e| StateError::Backend(e.to_string()))?;
        let table = txn
            .open_table(ENTRIES)
            .map_err(|e| StateError::Backend(e.to_string()))?;
        let len = table
            .len()
            .map_err(|e| StateError::Backend(e.to_string()))?;
        Ok(len as usize)
    }

    /// The upper bound of the byte range whose keys start with `prefix`.
    ///
    /// `None` when the prefix is all `0xFF` bytes — there is no greater key, so the scan runs to the
    /// end of the table.
    fn prefix_end(prefix: &[u8]) -> Option<Vec<u8>> {
        let mut end = prefix.to_vec();
        while let Some(last) = end.pop() {
            if last != 0xFF {
                end.push(last + 1);
                return Some(end);
            }
        }
        None
    }
}

impl StateBackend for RedbBackend {
    /// One redb write transaction: atomic by the store's own guarantee rather than by staging.
    ///
    /// `MemBackend` stages into a second map so an overflow part-way through leaves the store
    /// untouched. Here the transaction does that job, and the overflow check still comes first — a
    /// weight that would overflow is refused before anything is written, so the failure is the same
    /// failure on both backends. That sameness is what the backend-invariance gate compares.
    fn write(&mut self, batch: &WriteBatch) -> Result<()> {
        if batch.is_empty() {
            return Ok(());
        }
        let txn = self
            .db
            .begin_write()
            .map_err(|e| StateError::Backend(e.to_string()))?;
        let mut live = self.entries;
        {
            let mut table = txn
                .open_table(ENTRIES)
                .map_err(|e| StateError::Backend(e.to_string()))?;
            for (key, weight) in batch.entries() {
                let encoded = encode_key(key);
                let base = table
                    .get(encoded.as_slice())
                    .map_err(|e| StateError::Backend(e.to_string()))?
                    .map_or(0, |value| value.value());
                let sum = base
                    .checked_add(*weight)
                    .ok_or(StateError::WeightOverflow {
                        while_doing: "adding a weight to operator state",
                    })?;
                if sum == 0 {
                    // No tombstones, exactly as the trait requires: a row leaves when its weight
                    // reaches zero, so a churning workload does not leak an entry per row it saw.
                    if table
                        .remove(encoded.as_slice())
                        .map_err(|e| StateError::Backend(e.to_string()))?
                        .is_some()
                    {
                        live -= 1;
                    }
                } else {
                    if table
                        .insert(encoded.as_slice(), sum)
                        .map_err(|e| StateError::Backend(e.to_string()))?
                        .is_none()
                    {
                        live += 1;
                    }
                }
            }
        }
        txn.commit()
            .map_err(|e| StateError::Backend(e.to_string()))?;
        // Updated only after the commit: a failed transaction must not move the count, or `len()`
        // would report state the store does not hold.
        self.entries = live;
        Ok(())
    }

    fn visit_prefix(
        &self,
        prefix: &[Value],
        visitor: &mut dyn FnMut(&Key, i64) -> bool,
    ) -> Result<()> {
        let start = encode_key(prefix);
        let txn = self
            .db
            .begin_read()
            .map_err(|e| StateError::Backend(e.to_string()))?;
        let table = txn
            .open_table(ENTRIES)
            .map_err(|e| StateError::Backend(e.to_string()))?;

        // The codec is order-preserving (D-15), so "keys starting with this prefix" is a *byte range*
        // and the B-tree walks it in the S-7 order the trait promises. No sort afterwards, and no
        // full scan: this is the access pattern D-19 chose a B-tree for.
        let range = match RedbBackend::prefix_end(&start) {
            Some(end) => table.range(start.as_slice()..end.as_slice()),
            None => table.range(start.as_slice()..),
        }
        .map_err(|e| StateError::Backend(e.to_string()))?;

        for entry in range {
            let (key, weight) = entry.map_err(|e| StateError::Backend(e.to_string()))?;
            let decoded = decode_key(key.value())?;
            if !visitor(&decoded, weight.value()) {
                break;
            }
        }
        Ok(())
    }

    fn get(&self, key: &[Value]) -> Result<Option<i64>> {
        let encoded = encode_key(key);
        let txn = self
            .db
            .begin_read()
            .map_err(|e| StateError::Backend(e.to_string()))?;
        let table = txn
            .open_table(ENTRIES)
            .map_err(|e| StateError::Backend(e.to_string()))?;
        Ok(table
            .get(encoded.as_slice())
            .map_err(|e| StateError::Backend(e.to_string()))?
            .map(|value| value.value()))
    }

    fn len(&self) -> usize {
        self.entries
    }

    fn iter_all(&self) -> Result<Vec<(Key, i64)>> {
        let txn = self
            .db
            .begin_read()
            .map_err(|e| StateError::Backend(e.to_string()))?;
        let table = txn
            .open_table(ENTRIES)
            .map_err(|e| StateError::Backend(e.to_string()))?;
        let mut out = Vec::new();
        for entry in table
            .iter()
            .map_err(|e| StateError::Backend(e.to_string()))?
        {
            let (key, weight) = entry.map_err(|e| StateError::Backend(e.to_string()))?;
            out.push((decode_key(key.value())?, weight.value()));
        }
        Ok(out)
    }

    /// The same bytes `MemBackend` produces for the same contents.
    ///
    /// Not "a redb file": the snapshot format is the trait's, so a checkpoint taken on one backend
    /// restores into the other. That is not a nice-to-have — it is what lets the backend-invariance
    /// gate compare a recovery across backends, and what would let a deployment change backends
    /// without reloading its data.
    fn snapshot(&self) -> Result<Vec<u8>> {
        Ok(encode_entries(&self.iter_all()?))
    }

    fn restore(&mut self, bytes: &[u8]) -> Result<()> {
        let entries = decode_entries(bytes)?;
        let txn = self
            .db
            .begin_write()
            .map_err(|e| StateError::Backend(e.to_string()))?;
        let mut live = 0usize;
        {
            // Replace, not merge (the trait's word): recovery restores into a backend whose contents
            // are unknown, and merging would leave whatever was there.
            txn.delete_table(ENTRIES)
                .map_err(|e| StateError::Backend(e.to_string()))?;
            let mut table = txn
                .open_table(ENTRIES)
                .map_err(|e| StateError::Backend(e.to_string()))?;
            for (key, weight) in &entries {
                table
                    .insert(encode_key(key).as_slice(), *weight)
                    .map_err(|e| StateError::Backend(e.to_string()))?;
                live += 1;
            }
        }
        txn.commit()
            .map_err(|e| StateError::Backend(e.to_string()))?;
        self.entries = live;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::MemBackend;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("schweep-redb-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("state.redb")
    }

    fn key(a: i64, b: &str) -> Key {
        vec![Value::Int(a), Value::Str(b.to_owned())]
    }

    #[test]
    fn a_prefix_scan_returns_the_prefix_in_order() {
        let path = scratch("scan");
        let mut backend = RedbBackend::open(&path).unwrap();
        let mut batch = WriteBatch::new();
        for (a, b) in [(2, "b"), (1, "z"), (1, "a"), (3, "c"), (1, "m")] {
            batch.add(key(a, b), 1);
        }
        backend.write(&batch).unwrap();

        let scanned = backend.scan_prefix(&[Value::Int(1)]).unwrap();
        assert_eq!(
            scanned,
            vec![(key(1, "a"), 1), (key(1, "m"), 1), (key(1, "z"), 1),],
            "the prefix, in the S-7 order on values, without a sort afterwards"
        );
        assert_eq!(backend.scan_prefix(&[Value::Int(9)]).unwrap(), vec![]);
        assert_eq!(backend.len(), 5);
    }

    #[test]
    fn a_prefix_visitor_stops_after_one_redb_entry() {
        let path = scratch("bounded-scan");
        let mut backend = RedbBackend::open(&path).unwrap();
        let mut batch = WriteBatch::new();
        for suffix in 0..1_000 {
            batch.add(vec![Value::Int(1), Value::Int(suffix)], 1);
        }
        backend.write(&batch).unwrap();

        let mut visited = 0;
        backend
            .visit_prefix(&[Value::Int(1)], &mut |_key, _weight| {
                visited += 1;
                false
            })
            .unwrap();
        assert_eq!(visited, 1, "the redb cursor must honour the bound");
    }

    /// The count is maintained, not guessed — including through the zero-weight removals.
    #[test]
    fn len_tracks_the_table_through_additions_and_removals() {
        let path = scratch("len");
        let mut backend = RedbBackend::open(&path).unwrap();

        let mut batch = WriteBatch::new();
        batch.add(key(1, "a"), 2);
        batch.add(key(2, "b"), 1);
        backend.write(&batch).unwrap();
        assert_eq!(backend.len(), 2);
        assert_eq!(backend.count().unwrap(), 2, "and it agrees with the table");

        // A retraction to zero removes the entry; a partial one does not.
        let mut batch = WriteBatch::new();
        batch.add(key(1, "a"), -1);
        batch.add(key(2, "b"), -1);
        backend.write(&batch).unwrap();
        assert_eq!(backend.len(), 1);
        assert_eq!(backend.count().unwrap(), 1);
        assert_eq!(backend.get(&key(1, "a")).unwrap(), Some(1));
        assert_eq!(
            backend.get(&key(2, "b")).unwrap(),
            None,
            "no tombstone is left behind"
        );

        // A row can pass through zero and come back, and the count follows it.
        let mut batch = WriteBatch::new();
        batch.add(key(2, "b"), 1);
        backend.write(&batch).unwrap();
        assert_eq!(backend.len(), 2);
        assert_eq!(backend.count().unwrap(), 2);
    }

    /// Two entries for one key inside a batch net out — the same arithmetic `MemBackend` does.
    #[test]
    fn a_batch_nets_within_itself() {
        let path = scratch("net");
        let mut backend = RedbBackend::open(&path).unwrap();
        let mut batch = WriteBatch::new();
        batch.add(key(1, "a"), 3);
        batch.add(key(1, "a"), -3);
        batch.add(key(2, "b"), 1);
        backend.write(&batch).unwrap();
        assert_eq!(backend.len(), 1);
        assert_eq!(backend.get(&key(1, "a")).unwrap(), None);
    }

    /// An overflowing weight is refused and nothing is written — the trait's atomicity, on the store's
    /// own transaction rather than on a staging map.
    #[test]
    fn an_overflowing_batch_leaves_the_store_untouched() {
        let path = scratch("overflow");
        let mut backend = RedbBackend::open(&path).unwrap();
        let mut batch = WriteBatch::new();
        batch.add(key(1, "a"), i64::MAX);
        backend.write(&batch).unwrap();

        let mut batch = WriteBatch::new();
        batch.add(key(2, "b"), 1);
        batch.add(key(1, "a"), 1);
        assert!(matches!(
            backend.write(&batch),
            Err(StateError::WeightOverflow { .. })
        ));
        assert_eq!(
            backend.len(),
            1,
            "the successful add in the batch is gone too"
        );
        assert_eq!(backend.get(&key(2, "b")).unwrap(), None);
        assert_eq!(backend.get(&key(1, "a")).unwrap(), Some(i64::MAX));
    }

    /// **The snapshot format is the trait's, not redb's.** A snapshot taken on one backend restores
    /// into the other, which is what lets a checkpoint outlive a change of backend.
    #[test]
    fn a_snapshot_crosses_between_backends() {
        let path = scratch("snap");
        let mut redb = RedbBackend::open(&path).unwrap();
        let mut batch = WriteBatch::new();
        for (a, b) in [(1, "a"), (2, "b"), (3, "c")] {
            batch.add(key(a, b), a);
        }
        redb.write(&batch).unwrap();

        let mut mem = MemBackend::new();
        mem.restore(&redb.snapshot().unwrap()).unwrap();
        assert_eq!(mem.iter_all().unwrap(), redb.iter_all().unwrap());
        assert_eq!(mem.len(), redb.len());

        // And back the other way, into a backend that already held something else.
        let other = scratch("snap-back");
        let mut fresh = RedbBackend::open(&other).unwrap();
        let mut noise = WriteBatch::new();
        noise.add(key(99, "z"), 5);
        fresh.write(&noise).unwrap();
        fresh.restore(&mem.snapshot().unwrap()).unwrap();
        assert_eq!(
            fresh.iter_all().unwrap(),
            redb.iter_all().unwrap(),
            "restore replaces; it does not merge"
        );
        assert_eq!(fresh.len(), 3);
    }

    /// State survives closing and reopening the file, and the cached count is rebuilt from the table.
    #[test]
    fn state_survives_a_reopen() {
        let path = scratch("reopen");
        {
            let mut backend = RedbBackend::open(&path).unwrap();
            let mut batch = WriteBatch::new();
            batch.add(key(1, "a"), 1);
            batch.add(key(2, "b"), 2);
            backend.write(&batch).unwrap();
        }
        let reopened = RedbBackend::open(&path).unwrap();
        assert_eq!(
            reopened.len(),
            2,
            "the count is recovered, not assumed zero"
        );
        assert_eq!(reopened.get(&key(2, "b")).unwrap(), Some(2));
    }

    /// A prefix of all-`0xFF` bytes has no upper bound, so the scan must run to the end of the table
    /// rather than returning nothing.
    #[test]
    fn a_prefix_at_the_top_of_the_key_space_still_scans() {
        assert_eq!(RedbBackend::prefix_end(&[0x01, 0xFF]), Some(vec![0x02]));
        assert_eq!(RedbBackend::prefix_end(&[0xFF, 0xFF]), None);
        assert_eq!(RedbBackend::prefix_end(&[]), None);
    }
}
