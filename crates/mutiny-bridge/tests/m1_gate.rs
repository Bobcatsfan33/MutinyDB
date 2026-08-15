#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use loom_core::{ActorId, BranchId, SessionId, SourceRef, TenantId, WriteEnvelope};
use mutiny_bridge::{
    apply_commit, ApplyDisposition, BridgeError, CapturedChange, CapturedTable, CommitCapture,
    EnvelopeAuthority, EnvelopeId, DERIVATION_TABLE,
};
use schweep_log::{FaultInjector, FaultPlan, Log, Seam, SyncPolicy};
use schweep_zset::{DataType, Field, Row, Schema, Value};
use std::collections::{BTreeMap, BTreeSet};
use substrate_pager::{Manifest, ManifestBody, ManifestId, PageId};

#[derive(Debug)]
struct ExactAuthority {
    expected: EnvelopeId,
}

impl EnvelopeAuthority for ExactAuthority {
    fn admit(&self, id: EnvelopeId, _envelope: &WriteEnvelope) -> Result<(), String> {
        if id == self.expected {
            Ok(())
        } else {
            Err("envelope is not in the trust-plane registry".to_owned())
        }
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
        (
            DERIVATION_TABLE.to_owned(),
            Schema::new_table(vec![
                Field::not_null("tenant", DataType::Utf8),
                Field::not_null("branch", DataType::Utf8),
                Field::not_null("table_name", DataType::Utf8),
                Field::not_null("row_key", DataType::Utf8),
                Field::not_null("source_system", DataType::Utf8),
                Field::not_null("source_record", DataType::Utf8),
                Field::not_null("envelope", DataType::Utf8),
            ])
            .unwrap(),
        ),
    ])
}

fn capture(seq: u64) -> (CommitCapture, ExactAuthority) {
    let branch = BranchId::new("case-42");
    let envelope = WriteEnvelope::new(
        ActorId::new("analyst"),
        SessionId::new("session-1"),
        branch.clone(),
        "record a verified claim",
    )
    .derived_from([
        SourceRef::new("ticket", "INC-42"),
        SourceRef::new("telemetry", "event-7"),
    ]);
    let authority = ExactAuthority {
        expected: EnvelopeId::of(&envelope),
    };
    let parent = Manifest::empty(4096);
    let parent_id = parent.id().unwrap();
    let manifest = Manifest {
        format_version: parent.format_version,
        body: ManifestBody::Overlay {
            base: parent_id,
            changes: BTreeMap::from([(7, Some(PageId::of(b"claim-page")))]),
        },
        parent: Some(parent_id),
        created_at_ms: 1234,
        schema_version: 3,
        page_size: 4096,
        depth: 1,
        page_count: 1,
    };
    let commit = manifest.id().unwrap();
    let change = CapturedChange {
        row: Row::new(vec![Value::Int(42), Value::Str("verified".to_owned())]),
        weight: 1,
        primary_key: 42i64.to_be_bytes().to_vec(),
        pages: BTreeSet::from([7]),
    };
    (
        CommitCapture {
            tenant: TenantId::new("acme"),
            plane: "memory".to_owned(),
            commit,
            commit_seq: seq,
            manifest,
            branch,
            envelope,
            physical_pages: BTreeSet::from([7]),
            tables: BTreeMap::from([(
                "claims".to_owned(),
                CapturedTable {
                    changes: vec![change],
                },
            )]),
        },
        authority,
    )
}

#[test]
fn one_storage_commit_is_one_epoch_and_replay_is_not_a_second_epoch() {
    let dir = tempfile::tempdir().unwrap();
    let mut faults = FaultInjector::inert();
    let mut log = Log::open(dir.path(), schemas(), &mut faults, SyncPolicy::Full).unwrap();
    let (capture, authority) = capture(1);

    let first = apply_commit(&mut log, &capture, &authority, &mut faults).unwrap();
    assert_eq!(first.epoch, 1);
    assert_eq!(first.disposition, ApplyDisposition::Applied);
    assert_eq!(first.appended_batches, 2);

    let replay = apply_commit(&mut log, &capture, &authority, &mut faults).unwrap();
    assert_eq!(replay.epoch, 1);
    assert_eq!(replay.disposition, ApplyDisposition::Replayed);
    assert_eq!(log.sealed_epoch(), 1);
    assert_eq!(log.epoch(1).unwrap().len(), 2);
}

#[test]
fn crash_at_every_append_or_seal_seam_recovers_to_the_clean_log() {
    let seams = [
        Seam::AckBeforeValidate,
        Seam::AckBeforeAppend,
        Seam::AckAfterAppendBeforeFsync,
        Seam::AckAfterFsyncBeforeIndex,
        Seam::AckAfterFsyncBeforeAck,
        Seam::SealBeforeRecord,
        Seam::SealAfterRecordBeforeFsync,
    ];

    let clean_dir = tempfile::tempdir().unwrap();
    let (capture, authority) = capture(1);
    let mut clean_faults = FaultInjector::inert();
    let mut clean = Log::open(
        clean_dir.path(),
        schemas(),
        &mut clean_faults,
        SyncPolicy::Full,
    )
    .unwrap();
    apply_commit(&mut clean, &capture, &authority, &mut clean_faults).unwrap();
    let clean_batches = clean.epoch(1).unwrap();

    for seam in seams {
        let crash_dir = tempfile::tempdir().unwrap();
        let mut planned = FaultInjector::planned(FaultPlan {
            seam,
            occurrence: 1,
        });
        let mut crashed = Log::open(
            crash_dir.path(),
            schemas(),
            &mut FaultInjector::inert(),
            SyncPolicy::Full,
        )
        .unwrap();
        assert!(apply_commit(&mut crashed, &capture, &authority, &mut planned).is_err());
        assert_eq!(planned.fired(), Some(seam));
        drop(crashed);

        let mut recovery_faults = FaultInjector::inert();
        let mut recovered = Log::open(
            crash_dir.path(),
            schemas(),
            &mut recovery_faults,
            SyncPolicy::Full,
        )
        .unwrap();
        apply_commit(&mut recovered, &capture, &authority, &mut recovery_faults).unwrap();
        assert_eq!(recovered.sealed_epoch(), 1, "seam {seam:?}");
        assert_eq!(recovered.epoch(1).unwrap(), clean_batches, "seam {seam:?}");
    }
}

#[test]
fn completeness_envelope_and_sequence_checks_fail_closed_before_an_epoch() {
    let dir = tempfile::tempdir().unwrap();
    let mut faults = FaultInjector::inert();
    let mut log = Log::open(dir.path(), schemas(), &mut faults, SyncPolicy::Full).unwrap();
    let (mut capture, authority) = capture(2);
    assert!(matches!(
        apply_commit(&mut log, &capture, &authority, &mut faults),
        Err(BridgeError::SequenceGap {
            expected: 1,
            found: 2
        })
    ));

    capture.commit_seq = 1;
    capture.physical_pages.insert(99);
    assert!(matches!(
        apply_commit(&mut log, &capture, &authority, &mut faults),
        Err(BridgeError::AuditMismatch { .. })
    ));
    assert_eq!(log.sealed_epoch(), 0);

    capture.physical_pages.remove(&99);
    capture.envelope.intent.clear();
    assert!(matches!(
        apply_commit(&mut log, &capture, &authority, &mut faults),
        Err(BridgeError::InvalidEnvelope)
    ));
    assert_eq!(log.sealed_epoch(), 0);
}

#[test]
fn altered_replay_is_rejected_loudly() {
    let dir = tempfile::tempdir().unwrap();
    let mut faults = FaultInjector::inert();
    let mut log = Log::open(dir.path(), schemas(), &mut faults, SyncPolicy::Full).unwrap();
    let (capture, authority) = capture(1);
    apply_commit(&mut log, &capture, &authority, &mut faults).unwrap();

    let mut altered = capture.clone();
    altered
        .tables
        .get_mut("claims")
        .expect("fixture table")
        .changes
        .get_mut(0)
        .expect("fixture row")
        .row = Row::new(vec![Value::Int(42), Value::Str("tampered".to_owned())]);
    assert!(matches!(
        apply_commit(&mut log, &altered, &authority, &mut faults),
        Err(BridgeError::ReplayMismatch { epoch: 1 })
    ));
    assert_eq!(log.sealed_epoch(), 1);
}

#[test]
fn manifest_content_address_is_not_trusted_from_the_caller() {
    let dir = tempfile::tempdir().unwrap();
    let mut faults = FaultInjector::inert();
    let mut log = Log::open(dir.path(), schemas(), &mut faults, SyncPolicy::Full).unwrap();
    let (mut capture, authority) = capture(1);
    capture.commit = ManifestId::from_bytes([9; 32]);
    assert!(matches!(
        apply_commit(&mut log, &capture, &authority, &mut faults),
        Err(BridgeError::ManifestIdMismatch)
    ));
}

#[test]
fn a_foreign_pending_writer_cannot_be_folded_into_the_commit_epoch() {
    let dir = tempfile::tempdir().unwrap();
    let mut faults = FaultInjector::inert();
    let mut log = Log::open(dir.path(), schemas(), &mut faults, SyncPolicy::Full).unwrap();
    log.append(
        "bypass/writer",
        "claims",
        vec![(
            Row::new(vec![Value::Int(9), Value::Str("bypass".to_owned())]),
            1,
        )],
        "foreign-token",
        &mut faults,
    )
    .unwrap();

    let (capture, authority) = capture(1);
    assert!(matches!(
        apply_commit(&mut log, &capture, &authority, &mut faults),
        Err(BridgeError::ForeignPendingBatch { .. })
    ));
    assert_eq!(log.sealed_epoch(), 0);
}

#[test]
fn randomized_commit_histories_equal_an_independent_direct_ingest_control() {
    const SEEDS: u64 = 64;
    const COMMITS_PER_SEED: u64 = 16;

    for seed in 1..=SEEDS {
        let bridge_dir = tempfile::tempdir().unwrap();
        let direct_dir = tempfile::tempdir().unwrap();
        let mut bridge_faults = FaultInjector::inert();
        let mut direct_faults = FaultInjector::inert();
        let mut bridge = Log::open(
            bridge_dir.path(),
            schemas(),
            &mut bridge_faults,
            SyncPolicy::Deferred,
        )
        .unwrap();
        let mut direct = Log::open(
            direct_dir.path(),
            schemas(),
            &mut direct_faults,
            SyncPolicy::Deferred,
        )
        .unwrap();
        let mut state = seed;
        let mut parent = Manifest::empty(4096).id().unwrap();

        for seq in 1..=COMMITS_PER_SEED {
            // A deliberately tiny, fully deterministic generator. Test failures reproduce from
            // the printed seed without a dependency on a library RNG's versioned algorithm.
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            let id = ((state >> 17) & 0x7fff) as i64;
            let page = (state % 31) + 1;
            let weight = if state & 1 == 0 { 1 } else { -1 };
            let branch = BranchId::new(format!("case-{}", state % 5));
            let source = SourceRef::new(
                format!("system-{}", (state >> 8) % 3),
                format!("record-{id}"),
            );
            let envelope = WriteEnvelope::new(
                ActorId::new(format!("actor-{}", state % 7)),
                SessionId::new(format!("session-{seed}-{seq}")),
                branch.clone(),
                "randomized M1 differential gate",
            )
            .derived_from([source.clone()]);
            let authority = ExactAuthority {
                expected: EnvelopeId::of(&envelope),
            };
            let manifest = Manifest {
                format_version: 2,
                body: ManifestBody::Overlay {
                    base: parent,
                    changes: BTreeMap::from([(page, Some(PageId::of(&state.to_le_bytes())))]),
                },
                parent: Some(parent),
                created_at_ms: seq,
                schema_version: 3,
                page_size: 4096,
                depth: (seq % 8) as u32,
                page_count: seq,
            };
            let commit = manifest.id().unwrap();
            let payload = Row::new(vec![Value::Int(id), Value::Str(format!("value-{state}"))]);
            let capture = CommitCapture {
                tenant: TenantId::new("differential"),
                plane: "memory".to_owned(),
                commit,
                commit_seq: seq,
                manifest,
                branch: branch.clone(),
                envelope,
                physical_pages: BTreeSet::from([page]),
                tables: BTreeMap::from([(
                    "claims".to_owned(),
                    CapturedTable {
                        changes: vec![CapturedChange {
                            row: payload.clone(),
                            weight,
                            primary_key: id.to_be_bytes().to_vec(),
                            pages: BTreeSet::from([page]),
                        }],
                    },
                )]),
            };

            apply_commit(&mut bridge, &capture, &authority, &mut bridge_faults)
                .unwrap_or_else(|error| panic!("seed {seed}, commit {seq}: {error}"));

            // Independent control: translate the contract directly, without calling bridge
            // preparation code, then compare every retained byte-level batch below.
            let commit_hex = commit.to_hex();
            direct
                .append(
                    "differential/memory/claims",
                    "claims",
                    vec![(payload, weight)],
                    &format!("{commit_hex}/claims"),
                    &mut direct_faults,
                )
                .unwrap();
            let derivation = Row::new(vec![
                Value::Str("differential".to_owned()),
                Value::Str(branch.as_str().to_owned()),
                Value::Str("claims".to_owned()),
                Value::Str(
                    id.to_be_bytes()
                        .iter()
                        .map(|b| format!("{b:02x}"))
                        .collect(),
                ),
                Value::Str(source.system),
                Value::Str(source.record_id),
                Value::Str(authority.expected.to_hex()),
            ]);
            direct
                .append(
                    "differential/trust/mutiny_derivation",
                    DERIVATION_TABLE,
                    vec![(derivation, weight)],
                    &format!("{commit_hex}/{DERIVATION_TABLE}"),
                    &mut direct_faults,
                )
                .unwrap();
            assert_eq!(direct.seal_epoch(&mut direct_faults).unwrap(), seq);
            parent = commit;
        }

        assert_eq!(bridge.sealed_epoch(), direct.sealed_epoch(), "seed {seed}");
        for epoch in 1..=COMMITS_PER_SEED {
            assert_eq!(
                bridge.epoch(epoch).unwrap(),
                direct.epoch(epoch).unwrap(),
                "seed {seed}, epoch {epoch}"
            );
        }
    }
}
