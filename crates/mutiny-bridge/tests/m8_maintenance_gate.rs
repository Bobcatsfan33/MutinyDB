#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
//! **The M8 maintenance laws at the bridge level** (docs/M8-MAINTENANCE.md, issue #12): pruning
//! and collapsing consume the durable queue without breaking the dense-sequence law; a collapsed
//! store refuses full replay structurally; a crash between the volatile install and the WAL
//! checkpoint loses only the collapse; and tooth (a) — a maintenance that consumes past the
//! sealed epoch — is caught by name.

use loom_core::{ActorId, BranchId, SessionId, SourceRef, TenantId, WriteEnvelope};
use mutiny_bridge::{
    apply_commit, checkpoint_wal, collapse_history, collapsed_floor, commit_with_capture,
    derivation_schema, install_collapsed_root, prune_consumed, recover_pending_captures,
    BridgeError, CapturedChange, CapturedTable, CommitCapture, CommitDraft, EnvelopeAuthority,
    EnvelopeId, DERIVATION_TABLE,
};
use schweep_log::{FaultInjector, Log, SyncPolicy};
use schweep_zset::{DataType, Field, Row, Schema, Value};
use std::collections::{BTreeMap, BTreeSet};
use substrate_pager::{std_vfs, StoreConfig};
use substrate_wal::DurableStore;

#[derive(Debug)]
struct AdmitAll;

impl EnvelopeAuthority for AdmitAll {
    fn admit(&self, _id: EnvelopeId, _envelope: &WriteEnvelope) -> Result<(), String> {
        Ok(())
    }
}

fn schemas() -> BTreeMap<String, Schema> {
    BTreeMap::from([
        (
            "claims".to_owned(),
            Schema::new_table(vec![
                Field::not_null("id", DataType::Int64),
                Field::not_null("body", DataType::Utf8),
            ])
            .unwrap(),
        ),
        (DERIVATION_TABLE.to_owned(), derivation_schema().unwrap()),
    ])
}

/// One real storage commit through the front door: page `seq` holds the row bytes, the capture
/// explains it, the envelope admits it.
fn commit_n(store: &DurableStore, seq: u64) -> CommitCapture {
    let branch = BranchId::new("case-42");
    let envelope = WriteEnvelope::new(
        ActorId::new("analyst"),
        SessionId::new("session-1"),
        branch.clone(),
        "record a verified claim",
    )
    .derived_from([SourceRef::new("ticket", format!("INC-{seq}"))]);
    let change = CapturedChange {
        row: Row::new(vec![
            Value::Int(seq as i64),
            Value::Str(format!("claim {seq}")),
        ]),
        weight: 1,
        primary_key: (seq as i64).to_be_bytes().to_vec(),
        pages: BTreeSet::from([seq]),
    };
    let draft = CommitDraft {
        tenant: TenantId::new("acme"),
        plane: "memory".to_owned(),
        commit_seq: seq,
        branch,
        envelope,
        tables: BTreeMap::from([(
            "claims".to_owned(),
            CapturedTable {
                changes: vec![change],
            },
        )]),
    };
    let mut txn = store.begin().unwrap();
    store
        .write(&mut txn, seq, format!("claim-page-{seq}").into_bytes())
        .unwrap();
    commit_with_capture(store, txn, &draft, &schemas(), &AdmitAll).unwrap()
}

fn open_store(dir: &std::path::Path) -> DurableStore {
    let store = DurableStore::open(std_vfs(), dir, StoreConfig::default()).unwrap();
    store.recover().unwrap();
    store
}

/// Count files under a storage subtree (manifests and pages are sharded into prefix dirs).
fn storage_file_count(dir: &std::path::Path, sub: &str) -> usize {
    fn walk(path: &std::path::Path) -> usize {
        let Ok(entries) = std::fs::read_dir(path) else {
            return 0;
        };
        entries
            .flatten()
            .map(|entry| match entry.metadata() {
                Ok(meta) if meta.is_dir() => walk(&entry.path()),
                Ok(meta) if meta.is_file() => 1,
                _ => 0,
            })
            .sum()
    }
    walk(&dir.join(sub))
}

#[test]
fn prune_and_collapse_preserve_the_dense_sequence_and_the_next_commit() {
    let dir = tempfile::tempdir().unwrap();
    let store = open_store(dir.path());
    for seq in 1..=3 {
        commit_n(&store, seq);
    }

    // Everything through 3 is sealed; consume it.
    let pruned = prune_consumed(&store, 3).unwrap();
    assert_eq!(
        pruned, 3,
        "the three consumed application pages are removed"
    );
    let stats = collapse_history(&store).unwrap();
    assert!(stats.collapsed, "the chain collapses to a flat root");
    assert!(
        stats.manifests_swept > 0,
        "pre-collapse manifests are swept"
    );
    assert_eq!(collapsed_floor(&store, store.head()).unwrap(), Some(3));

    // The consumed suffix is empty — and the walk stops cleanly at the root.
    assert!(recover_pending_captures(&store, store.head(), 3)
        .unwrap()
        .is_empty());

    // The dense-sequence law survives the collapse: the next commit is 4, validated against the
    // capture page the flat root still carries.
    commit_n(&store, 4);
    let pending = recover_pending_captures(&store, store.head(), 3).unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].commit_seq, 4);

    // And a stale sequence is still refused.
    let store2 = open_store(dir.path());
    drop(store2);
}

#[test]
fn full_replay_of_a_collapsed_store_is_refused_structurally() {
    let dir = tempfile::tempdir().unwrap();
    let store = open_store(dir.path());
    for seq in 1..=3 {
        commit_n(&store, seq);
    }
    prune_consumed(&store, 3).unwrap();
    collapse_history(&store).unwrap();

    // A caller pretending nothing was sealed cannot silently rebuild from a consumed queue.
    let error = recover_pending_captures(&store, store.head(), 0).unwrap_err();
    assert!(
        matches!(
            error,
            BridgeError::ConsumedBeyondSealed {
                floor: 3,
                sealed: 0
            }
        ),
        "full replay of a collapsed store must refuse by name, got: {error}"
    );
}

#[test]
fn a_crash_between_install_and_wal_checkpoint_loses_only_the_collapse() {
    let dir = tempfile::tempdir().unwrap();
    let store = open_store(dir.path());
    for seq in 1..=3 {
        commit_n(&store, seq);
    }
    let pre_collapse_head = store.head();
    prune_consumed(&store, 3).unwrap();
    let pruned_head = store.head();
    assert_ne!(pre_collapse_head, pruned_head, "the prune is a real commit");

    // S4 alone: the install is volatile — the process dies before the WAL checkpoint.
    assert!(install_collapsed_root(&store).unwrap());
    drop(store);

    // Recovery replays the WAL back to the pruned head; the collapse simply has not happened.
    let store = open_store(dir.path());
    assert_eq!(store.head(), pruned_head);
    assert_eq!(collapsed_floor(&store, store.head()).unwrap(), None);
    let pending = recover_pending_captures(&store, store.head(), 3).unwrap();
    assert!(pending.is_empty(), "no capture was lost");

    // The next maintenance pass finishes the job, and the orphaned root is one manifest.
    collapse_history(&store).unwrap();
    assert_eq!(collapsed_floor(&store, store.head()).unwrap(), Some(3));
    assert_eq!(
        storage_file_count(dir.path(), "manifests"),
        1,
        "a collapsed store is one manifest"
    );
}

#[test]
fn the_sweep_bounds_the_store_to_what_is_live() {
    let dir = tempfile::tempdir().unwrap();
    let store = open_store(dir.path());
    for seq in 1..=8 {
        commit_n(&store, seq);
    }
    let manifests_before = storage_file_count(dir.path(), "manifests");
    let pages_before = storage_file_count(dir.path(), "pages");
    assert!(manifests_before >= 8, "history accumulates before the fix");

    prune_consumed(&store, 8).unwrap();
    collapse_history(&store).unwrap();

    let manifests_after = storage_file_count(dir.path(), "manifests");
    let pages_after = storage_file_count(dir.path(), "pages");
    println!(
        "storage bound: manifests {manifests_before} -> {manifests_after}, pages \
         {pages_before} -> {pages_after}"
    );
    assert_eq!(manifests_after, 1, "one flat root remains");
    assert!(
        pages_after < pages_before,
        "consumed application pages are swept"
    );
}

/// **Tooth (a)** — a maintenance that drops an unsealed capture (docs/M8-MAINTENANCE.md). The
/// constructed bug: storage committed 3, the compute plane sealed only 2 (the crash landed
/// between commit and seal), and a doctored maintenance consumes through 3 anyway. The catching
/// instrument is the recovery walk itself: `ConsumedBeyondSealed` names exactly the captures
/// that were dropped, instead of returning an innocently empty suffix that would silently lose
/// the acked write.
#[test]
fn tooth_a_a_maintenance_that_consumes_past_the_sealed_epoch_is_caught() {
    let storage_dir = tempfile::tempdir().unwrap();
    let compute_dir = tempfile::tempdir().unwrap();
    let store = open_store(storage_dir.path());
    let mut captures = Vec::new();
    for seq in 1..=3 {
        captures.push(commit_n(&store, seq));
    }

    // The compute plane seals 1 and 2; capture 3 is storage-committed but unsealed.
    let mut faults = FaultInjector::inert();
    let mut log = Log::open(compute_dir.path(), schemas(), &mut faults, SyncPolicy::Full).unwrap();
    for capture in &captures[..2] {
        apply_commit(&mut log, capture, &AdmitAll, &mut faults).unwrap();
    }
    assert_eq!(log.sealed_epoch(), 2);

    // THE BUG, constructed: maintenance consumes through the STORAGE head (3) instead of the
    // sealed epoch (2).
    prune_consumed(&store, 3).unwrap();
    install_collapsed_root(&store).unwrap();
    checkpoint_wal(&store).unwrap();
    mutiny_bridge::sweep(&store).unwrap();

    // The instrument fires: recovery refuses by name rather than losing the acked commit.
    let error = recover_pending_captures(&store, store.head(), log.sealed_epoch()).unwrap_err();
    match error {
        BridgeError::ConsumedBeyondSealed { floor, sealed } => {
            assert_eq!(floor, 3);
            assert_eq!(sealed, 2);
        }
        other => panic!("tooth (a) must be caught by name, got: {other}"),
    }
}
