//! Bootstrap: building a circuit that was never there (`docs/DURABILITY.md` §5 B1–B3, §6 C7).
//!
//! A standing query registered now, or a one-shot query, must answer for the **whole** history. Before
//! a compaction that means replaying the log; after one it means the snapshot plus the retained
//! suffix. Nothing downstream can tell which, and that indistinguishability is the point of the
//! sprint:
//!
//! ```text
//!   before compaction:   log epochs 1..n              ─┐
//!   after  compaction:   snapshot @ E + epochs E+1..n ─┴─► one accumulated input ─► the same answer
//! ```
//!
//! **Why one delta rather than a replay of each epoch.** Every operator's state is a function of the
//! accumulated input rather than of how it was divided into epochs, and every answer is the integral of
//! its sink's output deltas — so `Δ₁ + … + Δₙ` applied at once integrates to what the pieces integrate
//! to. That is I-2 restated, and it is the same argument C6's mid-history attach rests on. Bootstrap
//! and attach are the *same* mechanism with different sources.

use std::collections::BTreeMap;

use schweep_log::Log;
use schweep_zset::{EpochDeltas, Row, Schema, ZSetBatch};

use crate::compact;
use crate::error::Result;

/// The accumulated input of every table, from wherever it currently lives.
///
/// This is the function that makes a compaction invisible. It reads the snapshot if there is one, adds
/// the retained log's epochs, and returns one integral per table — the same value, whether or not a
/// compaction has happened.
pub fn accumulated(
    log: &Log,
    catalog: &BTreeMap<String, Schema>,
) -> Result<BTreeMap<String, ZSetBatch>> {
    // B1 · the snapshot, verified against its manifest; B3 · every epoch the log still holds.
    accumulated_upto(log, catalog, log.sealed_epoch())
}

/// The accumulated input as of `upto`, rather than as of the log's latest epoch.
///
/// What compaction's P2 writes: the snapshot covers exactly the anchor epoch, not everything the log
/// happens to hold. Passing the latest epoch instead would put records into the snapshot that the
/// retained suffix also holds, and they would then be applied twice by every bootstrap.
pub fn accumulated_upto(
    log: &Log,
    catalog: &BTreeMap<String, Schema>,
    upto: u64,
) -> Result<BTreeMap<String, ZSetBatch>> {
    let mut integrals = compact::live_integrals(log, catalog)?;
    for epoch in (log.retained_from() + 1)..=upto.min(log.sealed_epoch()) {
        for batch in log.epoch(epoch)? {
            let schema = catalog.get(&batch.table).ok_or_else(|| {
                crate::error::BatchError::UnknownTable {
                    table: batch.table.clone(),
                }
            })?;
            let delta = ZSetBatch::from_entries(schema.clone(), batch.entries.clone())?;
            let merged = match integrals.get(&batch.table) {
                Some(held) => held.add(&delta)?,
                None => delta,
            };
            integrals.insert(batch.table.clone(), merged.consolidate()?);
        }
    }
    Ok(integrals)
}

/// The accumulated input as one delta, ready to be fed to a fresh circuit (B2).
pub fn as_one_delta(integrals: &BTreeMap<String, ZSetBatch>) -> Result<EpochDeltas> {
    let mut deltas = EpochDeltas::new();
    for (table, integral) in integrals {
        let entries: Vec<(Row, i64)> = integral.canonical()?.entries().to_vec();
        deltas.extend(table.clone(), entries);
    }
    Ok(deltas)
}

/// Everything a fresh circuit needs to be brought to the log's current epoch, in one call.
pub fn one_delta_for(log: &Log, catalog: &BTreeMap<String, Schema>) -> Result<EpochDeltas> {
    as_one_delta(&accumulated(log, catalog)?)
}
