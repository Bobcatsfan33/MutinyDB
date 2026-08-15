//! Exactly-once admission (I-4; `docs/DURABILITY.md` §1, steps A2–A4).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::collections::BTreeMap;

use schweep_log::{Ack, FaultInjector, Log, LogError, SyncPolicy};
use schweep_zset::{DataType, Field, Row, Schema, Value};

fn catalog() -> BTreeMap<String, Schema> {
    let mut c = BTreeMap::new();
    c.insert(
        "t".to_owned(),
        Schema::new_table(vec![Field::nullable("v", DataType::Int64)]).unwrap(),
    );
    c
}

fn row(v: i64) -> Row {
    Row::new(vec![Value::Int(v)])
}

fn dir(tag: &str) -> std::path::PathBuf {
    let p = std::env::temp_dir().join(format!("schweep-log-{tag}"));
    let _ = std::fs::remove_dir_all(&p);
    p
}

fn open(path: &std::path::Path) -> Log {
    let mut faults = FaultInjector::inert();
    Log::open(path, catalog(), &mut faults, SyncPolicy::Full).unwrap()
}

/// **A replayed token is acknowledged and dropped** (A3, I-4).
#[test]
fn a_replayed_token_is_acknowledged_and_dropped() {
    let path = dir("replay");
    let mut log = open(&path);
    let mut faults = FaultInjector::inert();

    let entries = vec![(row(1), 2)];
    assert_eq!(
        log.append("src", "t", entries.clone(), "tok", &mut faults)
            .unwrap(),
        Ack::Appended
    );
    // Offered again, byte-identical: dropped, and the caller still gets an ack.
    for _ in 0..3 {
        assert_eq!(
            log.append("src", "t", entries.clone(), "tok", &mut faults)
                .unwrap(),
            Ack::DroppedAsReplay
        );
    }
    log.seal_epoch(&mut faults).unwrap();
    assert_eq!(
        log.epoch(1).unwrap().len(),
        1,
        "applied in exactly one epoch"
    );
    assert_eq!(log.known_tokens(), 1);
}

/// **The same token with different content is refused loudly** — never silently rewritten (A4, I-4).
#[test]
fn the_same_token_with_different_content_is_refused_loudly() {
    let path = dir("reuse");
    let mut log = open(&path);
    let mut faults = FaultInjector::inert();

    log.append("src", "t", vec![(row(1), 1)], "tok", &mut faults)
        .unwrap();
    let err = log
        .append("src", "t", vec![(row(2), 1)], "tok", &mut faults)
        .unwrap_err();
    assert!(
        matches!(err, LogError::TokenReused { .. }),
        "expected TokenReused, got {err}"
    );

    // And the first batch is untouched: refusing must not have rewritten anything.
    log.seal_epoch(&mut faults).unwrap();
    let batches = log.epoch(1).unwrap();
    assert_eq!(batches.len(), 1);
    assert_eq!(batches[0].entries, vec![(row(1), 1)]);
}

/// Dedup survives a reopen, because the index is rebuilt from the log and never from memory (R6).
#[test]
fn dedup_survives_a_reopen() {
    let path = dir("reopen");
    {
        let mut log = open(&path);
        let mut faults = FaultInjector::inert();
        log.append("src", "t", vec![(row(1), 1)], "tok", &mut faults)
            .unwrap();
        log.seal_epoch(&mut faults).unwrap();
    }
    let mut log = open(&path);
    let mut faults = FaultInjector::inert();
    assert_eq!(log.sealed_epoch(), 1, "the sealed epoch survived");
    assert_eq!(
        log.append("src", "t", vec![(row(1), 1)], "tok", &mut faults)
            .unwrap(),
        Ack::DroppedAsReplay,
        "a token known only to the log on disk must still be recognised (A3, R6)"
    );
    // …and a different batch under that token is still refused after the reopen.
    assert!(log
        .append("src", "t", vec![(row(9), 1)], "tok", &mut faults)
        .is_err());
}

/// A malformed batch is refused and nothing is written (A1).
#[test]
fn a_malformed_batch_is_refused_and_writes_nothing() {
    let path = dir("malformed");
    let mut log = open(&path);
    let mut faults = FaultInjector::inert();

    let wide = Row::new(vec![Value::Int(1), Value::Int(2)]);
    assert!(log
        .append("src", "t", vec![(wide, 1)], "tok", &mut faults)
        .is_err());
    assert_eq!(log.known_tokens(), 0, "a refused batch leaves no token");

    // The token is still free afterwards, because nothing was recorded under it.
    assert_eq!(
        log.append("src", "t", vec![(row(1), 1)], "tok", &mut faults)
            .unwrap(),
        Ack::Appended
    );
}

/// A torn tail is discarded, and everything before it survives (R5).
#[test]
fn a_torn_tail_is_discarded_and_the_prefix_survives() {
    let path = dir("torn");
    {
        let mut log = open(&path);
        let mut faults = FaultInjector::inert();
        log.append("src", "t", vec![(row(1), 1)], "a", &mut faults)
            .unwrap();
        log.seal_epoch(&mut faults).unwrap();
        log.append("src", "t", vec![(row(2), 1)], "b", &mut faults)
            .unwrap();
        log.seal_epoch(&mut faults).unwrap();
    }
    // Chop the last few bytes: the final seal record is now torn.
    let segment = path.join("segment-00000001.log");
    let bytes = std::fs::read(&segment).unwrap();
    std::fs::write(&segment, &bytes[..bytes.len() - 3]).unwrap();

    let log = open(&path);
    assert_eq!(
        log.sealed_epoch(),
        1,
        "the torn seal record is discarded, so only epoch 1 is sealed"
    );
    assert_eq!(log.epoch(1).unwrap().len(), 1);
    assert_eq!(
        log.pending_batches().len(),
        1,
        "the batch after the surviving seal is durable but not yet visible"
    );
}

/// The source_id travels with every batch (§5.4, the MutinyDB seam).
#[test]
fn the_source_id_survives_a_reopen() {
    let path = dir("source");
    {
        let mut log = open(&path);
        let mut faults = FaultInjector::inert();
        log.append("ingest-7", "t", vec![(row(1), 1)], "tok", &mut faults)
            .unwrap();
        log.seal_epoch(&mut faults).unwrap();
    }
    let log = open(&path);
    assert_eq!(log.epoch(1).unwrap()[0].source_id, "ingest-7");
}
