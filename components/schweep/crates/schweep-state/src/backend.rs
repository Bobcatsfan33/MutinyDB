//! The `StateBackend` trait (`ARCHITECTURE.md` §5.5, D-15).

use std::fmt;

use schweep_zset::Value;

use crate::error::Result;

/// A key: a tuple of values, ordered by the total order on values (S-7).
///
/// Operators lay keys out so that a prefix means something — the join stores
/// `[join key values…, row values…]`, which makes "every row with this join key" a prefix scan.
pub type Key = Vec<Value>;

/// One atomic unit of change to a backend.
///
/// Batching is not an optimisation here, it is the contract: an operator's state must move from
/// one consistent condition to another with no observable point in between, or a checkpoint taken
/// mid-update (C4) would record a state no run ever had. C1's linear operators needed none of
/// this; the join is the first operator with state to keep consistent.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WriteBatch {
    adds: Vec<(Key, i64)>,
}

impl WriteBatch {
    #[must_use]
    pub fn new() -> WriteBatch {
        WriteBatch::default()
    }

    /// Add `weight` to the entry at `key`, creating it if absent.
    ///
    /// There is no `put` and no `delete`, and that is deliberate: every change to operator state in
    /// this engine is the addition of a weight, and a row leaves when its weight reaches zero. An
    /// interface with `delete` would invite an operator to decide that a retraction is a deletion,
    /// which is the special case I-5 forbids.
    pub fn add(&mut self, key: Key, weight: i64) {
        self.adds.push((key, weight));
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.adds.is_empty()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.adds.len()
    }

    #[must_use]
    pub fn entries(&self) -> &[(Key, i64)] {
        &self.adds
    }
}

/// An ordered key-value store for operator state.
pub trait StateBackend: fmt::Debug + Send {
    /// Apply a batch atomically: either every addition lands or none does.
    ///
    /// Entries whose weight reaches zero are removed, so the store holds no tombstones — the same
    /// rule the canonical form of a Z-set follows (S-8), and the reason a churning workload does
    /// not leak one entry per row it has ever seen.
    fn write(&mut self, batch: &WriteBatch) -> Result<()>;

    /// Visit every `(key, weight)` whose key starts with `prefix`, in key order, without collecting.
    ///
    /// Order is part of the contract, not an accident of the implementation: two backends that
    /// scanned in different orders would make an operator's output depend on which one it was
    /// given, and I-2 forbids that.
    ///
    /// Returning `false` stops the scan. That makes MIN and MAX O(1) entries after the B-tree seek,
    /// while a fold such as SUM stays O(group) CPU and O(1) memory (D-25).
    fn visit_prefix(
        &self,
        prefix: &[Value],
        visitor: &mut dyn FnMut(&Key, i64) -> bool,
    ) -> Result<()>;

    /// Collect a prefix for compatibility, diagnostics, and tests.
    ///
    /// Operators must use [`StateBackend::visit_prefix`]. This helper intentionally retains the C4
    /// API shape without making its O(prefix-size) allocation part of a query step.
    fn scan_prefix(&self, prefix: &[Value]) -> Result<Vec<(Key, i64)>> {
        let mut entries = Vec::new();
        self.visit_prefix(prefix, &mut |key, weight| {
            entries.push((key.clone(), weight));
            true
        })?;
        Ok(entries)
    }

    /// The weight at `key`, or `None` if absent.
    fn get(&self, key: &[Value]) -> Result<Option<i64>>;

    /// How many entries are held. This is what an operator reports as its state size for the I-9
    /// accounting, and it is a count of entries rather than bytes (D-15).
    fn len(&self) -> usize;

    #[must_use]
    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Every entry, in key order. For fingerprints and tests; not a hot path.
    fn iter_all(&self) -> Result<Vec<(Key, i64)>>;

    /// Serialise the whole backend, for a checkpoint (`docs/DURABILITY.md` C1).
    ///
    /// D-15 recorded that named snapshots were deliberately absent until C4 designed the checkpoint
    /// protocol. This is that addition, and it is the last one: §5.5 freezes this trait at C4's exit.
    ///
    /// A snapshot is bytes rather than a backend-defined handle because a checkpoint has to be a
    /// *file* — publishable by rename, checksummable, and readable by a process that has not opened
    /// the backend yet (C3, C4 of the checkpoint sequence).
    fn snapshot(&self) -> Result<Vec<u8>>;

    /// Replace the backend's contents with a snapshot.
    ///
    /// Replace, not merge: recovery loads a checkpoint into a backend whose contents are unknown, and
    /// merging would leave whatever was there. `restore` of a snapshot must give a backend that
    /// compares equal to the one the snapshot was taken from — which is what makes I-7's
    /// byte-identical claim checkable.
    fn restore(&mut self, bytes: &[u8]) -> Result<()>;
}
