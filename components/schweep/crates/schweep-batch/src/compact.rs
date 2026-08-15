//! Compaction: publish-then-swap, never in-place (`docs/DURABILITY.md` §4, §6 C7).
//!
//! This module is the P-sequence, in order, with each step labelled. It is written as one function on
//! purpose: an ordering split across three files is an ordering nobody can check, and the ordering is
//! the entire correctness argument.
//!
//! ```text
//!   P1 anchor ─ P2 write ─ P3 fsync ─ P4 manifest ─ P5 publish ─┐
//!                                                              │  the snapshot exists
//!   P6 write the retained suffix ────────────────────────────────┤  the old log is STILL authoritative
//!   P7 swap the pointer ─────────────────────────────────────────┤  ← the one commit point
//!   P8 delete the old segment ─ P9 delete old snapshots ─────────┘
//! ```
//!
//! **The invariant, at every instant: either the whole log is authoritative, or a published snapshot
//! plus the retained suffix is. Never neither.** Seven of the eight kill points leave the first; the
//! eighth leaves the second, and by then both halves of the pair are complete and published.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use schweep_log::{FaultInjector, Log, Seam, SyncPolicy};
use schweep_zset::{Schema, ZSetBatch};

use crate::error::{BatchError, Result};
use crate::snapshot::{self, Manifest};

/// What a compaction did, for reporting and for the gates.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Compacted {
    /// The epoch the snapshot covers — everything at or below it is now in Parquet, not in the log.
    pub anchor: u64,
    pub snapshot: PathBuf,
    /// Rows written per table, after consolidation.
    pub rows: BTreeMap<String, usize>,
    /// Tokens carried across the swap in the dedup ledger. **The number that keeps I-4 true.**
    pub tokens: usize,
}

/// The snapshot directory for an epoch.
#[must_use]
pub fn snapshot_dir(root: &Path, epoch: u64) -> PathBuf {
    root.join(format!("snap-{epoch:010}"))
}

/// Run one compaction.
///
/// `anchor` must be an epoch the caller has a published checkpoint for — see P1 in
/// `docs/DURABILITY.md` §4 for why compacting past it would delete records recovery still needs. The
/// caller passes it in rather than having this function read `CURRENT`, because the checkpoint
/// directory belongs to whoever owns the circuit, and this function owns neither.
pub fn compact(
    log: &mut Log,
    anchor: u64,
    integrals: &BTreeMap<String, ZSetBatch>,
    faults: &mut FaultInjector,
    sync: SyncPolicy,
) -> Result<Compacted> {
    let root = log.directory().to_path_buf();

    // ---- P1 · anchor ---------------------------------------------------------------------------
    if anchor == 0 {
        return Err(BatchError::NoCheckpointToAnchorTo);
    }
    if anchor <= log.retained_from() {
        return Err(BatchError::Log(schweep_log::LogError::NothingToCompact {
            anchor,
            retained_from: log.retained_from(),
        }));
    }
    if faults.reached(Seam::CompactBeforeSnapshot) {
        return Err(BatchError::Log(schweep_log::LogError::InjectedFault(
            Seam::CompactBeforeSnapshot.name(),
        )));
    }

    let published = snapshot_dir(&root, anchor);
    let partial = root.join(format!("snap-{anchor:010}.partial"));
    // A leftover `.partial` from an earlier crashed attempt is not a state to reason about; it is
    // removed before writing, exactly as R5's cleanup would.
    let _ = fs::remove_dir_all(&partial);
    fs::create_dir_all(&partial)?;

    // ---- P2 · write every table's integral, plus the dedup ledger -------------------------------
    let mut rows = BTreeMap::new();
    let mut checksums = BTreeMap::new();
    for (table, integral) in integrals {
        let path = snapshot::table_path(&partial, table);
        let written = snapshot::write_table(&path, integral)?;
        let bytes = fs::read(&path)?;
        rows.insert(table.clone(), written);
        checksums.insert(table.clone(), (written, schweep_log::record::crc32(&bytes)));
    }
    let ledger = log.dedup_ledger();
    let tokens = schweep_log::dedup::decode(&ledger)?.len();
    fs::write(partial.join(schweep_log::log::DEDUP_LEDGER), &ledger)?;

    if faults.reached(Seam::CompactAfterWriteBeforeFsync) {
        // A `.partial` with possibly-torn Parquet. Ignored and deleted; the whole log is authoritative.
        return Err(BatchError::Log(schweep_log::LogError::InjectedFault(
            Seam::CompactAfterWriteBeforeFsync.name(),
        )));
    }

    // ---- P3 · fsync the files, then the directory ------------------------------------------------
    if sync == SyncPolicy::Full {
        for entry in fs::read_dir(&partial)? {
            let entry = entry?;
            fs::File::open(entry.path())?.sync_all()?;
        }
        fs::File::open(&partial)?.sync_all()?;
    }

    if faults.reached(Seam::CompactAfterFsyncBeforeManifest) {
        // Good files, no manifest. No manifest, no snapshot.
        return Err(BatchError::Log(schweep_log::LogError::InjectedFault(
            Seam::CompactAfterFsyncBeforeManifest.name(),
        )));
    }

    // ---- P4 · manifest --------------------------------------------------------------------------
    let manifest = Manifest {
        epoch: anchor,
        tables: checksums,
        dedup_crc: schweep_log::record::crc32(&ledger),
    };
    {
        let path = partial.join(snapshot::MANIFEST);
        fs::write(&path, manifest.encode())?;
        if sync == SyncPolicy::Full {
            fs::File::open(&path)?.sync_all()?;
        }
    }

    if faults.reached(Seam::CompactAfterManifestBeforePublish) {
        // A complete `.partial` that was never renamed. Still ignored: publication is the commit point.
        return Err(BatchError::Log(schweep_log::LogError::InjectedFault(
            Seam::CompactAfterManifestBeforePublish.name(),
        )));
    }

    // ---- P5 · publish the snapshot ---------------------------------------------------------------
    let _ = fs::remove_dir_all(&published);
    fs::rename(&partial, &published)?;
    if sync == SyncPolicy::Full {
        fs::File::open(&root)?.sync_all()?;
    }

    if faults.reached(Seam::CompactAfterPublishBeforeSegment) {
        // A published snapshot that nothing points at. `LOG` is what makes a snapshot live, so the
        // whole log is still authoritative.
        return Err(BatchError::Log(schweep_log::LogError::InjectedFault(
            Seam::CompactAfterPublishBeforeSegment.name(),
        )));
    }

    // ---- P6, P7, P8 · the log's side: retained suffix, the swap, then the trim -------------------
    log.compact(anchor, &published, faults)?;

    if faults.reached(Seam::CompactAfterTrimBeforeCleanup) {
        // Stale snapshot directories left behind. Never selected, because `LOG` names the live one.
        return Err(BatchError::Log(schweep_log::LogError::InjectedFault(
            Seam::CompactAfterTrimBeforeCleanup.name(),
        )));
    }

    // ---- P9 · delete superseded snapshots --------------------------------------------------------
    cleanup(&root, anchor)?;

    Ok(Compacted {
        anchor,
        snapshot: published,
        rows,
        tokens,
    })
}

/// Delete `.partial` snapshots and every published snapshot older than the live one (P9, R5).
///
/// Idempotent, because recovery calls it too: deleting an already-deleted directory is a no-op, which
/// is what lets a crash during cleanup be a non-event.
pub fn cleanup(root: &Path, live: u64) -> Result<()> {
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_string();
        if name.ends_with(".partial") {
            if entry.path().is_dir() {
                let _ = fs::remove_dir_all(entry.path());
            } else {
                let _ = fs::remove_file(entry.path());
            }
            continue;
        }
        if let Some(digits) = name.strip_prefix("snap-") {
            if let Ok(epoch) = digits.parse::<u64>() {
                if epoch < live {
                    let _ = fs::remove_dir_all(entry.path());
                }
            }
        }
    }
    Ok(())
}

/// Load the live snapshot's integrals, if the log has one (B1).
pub fn live_integrals(
    log: &Log,
    catalog: &BTreeMap<String, Schema>,
) -> Result<BTreeMap<String, ZSetBatch>> {
    match log.snapshot() {
        None => Ok(BTreeMap::new()),
        Some(dir) => snapshot::load(dir, catalog),
    }
}
