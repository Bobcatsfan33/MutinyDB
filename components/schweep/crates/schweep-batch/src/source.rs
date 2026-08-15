//! Reconstruct source ownership from snapshot provenance plus the retained log (C11).
//!
//! This module deliberately returns Z-sets. A source can insert and retract the same row over time;
//! ownership is its *net contribution*, not a bag of historical events. Consolidation makes retrying a
//! completed source retraction a no-op because the negative transaction is attributed to the same source.

use std::collections::BTreeMap;

use schweep_log::Log;
use schweep_zset::{Schema, ZSetBatch};

use crate::error::{BatchError, Result};

/// Source id -> table -> consolidated net contribution.
pub type SourceIntegrals = BTreeMap<String, BTreeMap<String, ZSetBatch>>;

/// Reconstruct every source's contribution through `anchor`, excluding unsealed appends.
pub fn source_integrals_upto(
    log: &Log,
    catalog: &BTreeMap<String, Schema>,
    anchor: u64,
) -> Result<SourceIntegrals> {
    if anchor < log.retained_from() {
        return Err(BatchError::Log(schweep_log::LogError::EpochCompacted {
            requested: anchor,
            retained_from: log.retained_from(),
        }));
    }
    if anchor > log.sealed_epoch() {
        return Err(BatchError::Log(schweep_log::LogError::EpochOutOfRange {
            requested: anchor,
            sealed: log.sealed_epoch(),
        }));
    }

    let mut out = match log.snapshot() {
        None => SourceIntegrals::new(),
        Some(dir) => match crate::snapshot::read_provenance(dir, catalog)? {
            Some(provenance) => provenance,
            None if log.retained_from() == 0 => SourceIntegrals::new(),
            None => {
                return Err(BatchError::ProvenanceUnavailable {
                    epoch: log.retained_from(),
                })
            }
        },
    };

    for epoch in (log.retained_from() + 1)..=anchor {
        for batch in log.epoch(epoch)? {
            extend(
                &mut out,
                catalog,
                &batch.source_id,
                &batch.table,
                batch.entries,
            )?;
        }
    }
    consolidate(out)
}

/// Reconstruct one source at the latest visible epoch, plus its acknowledged unsealed appends.
///
/// Including pending appends makes a recall requested between append and seal cancel those inputs in the
/// very epoch that would otherwise make them visible.
pub fn source_integral(
    log: &Log,
    catalog: &BTreeMap<String, Schema>,
    source_id: &str,
) -> Result<BTreeMap<String, ZSetBatch>> {
    let mut all = source_integrals_upto(log, catalog, log.sealed_epoch())?;
    let mut source = all.remove(source_id).unwrap_or_default();
    for batch in log
        .pending_batches()
        .iter()
        .filter(|batch| batch.source_id == source_id)
    {
        extend_one(&mut source, catalog, &batch.table, batch.entries.clone())?;
    }
    consolidate_one(source)
}

fn extend(
    out: &mut SourceIntegrals,
    catalog: &BTreeMap<String, Schema>,
    source_id: &str,
    table: &str,
    entries: Vec<(schweep_zset::Row, i64)>,
) -> Result<()> {
    let source = out.entry(source_id.to_owned()).or_default();
    extend_one(source, catalog, table, entries)
}

fn extend_one(
    source: &mut BTreeMap<String, ZSetBatch>,
    catalog: &BTreeMap<String, Schema>,
    table: &str,
    entries: Vec<(schweep_zset::Row, i64)>,
) -> Result<()> {
    let schema = catalog
        .get(table)
        .cloned()
        .ok_or_else(|| BatchError::UnknownTable {
            table: table.to_owned(),
        })?;
    let delta = ZSetBatch::from_entries(schema.clone(), entries)?;
    let combined = match source.remove(table) {
        Some(current) => current.add(&delta)?,
        None => delta,
    };
    source.insert(table.to_owned(), combined);
    Ok(())
}

fn consolidate(mut all: SourceIntegrals) -> Result<SourceIntegrals> {
    for tables in all.values_mut() {
        *tables = consolidate_one(std::mem::take(tables))?;
    }
    all.retain(|_, tables| !tables.is_empty());
    Ok(all)
}

fn consolidate_one(tables: BTreeMap<String, ZSetBatch>) -> Result<BTreeMap<String, ZSetBatch>> {
    let mut out = BTreeMap::new();
    for (table, batch) in tables {
        let batch = batch.consolidate()?;
        if !batch.is_empty() {
            out.insert(table, batch);
        }
    }
    Ok(out)
}
