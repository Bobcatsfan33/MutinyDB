//! The streaming segment reader agrees with the log it read, and stops where the log stops (C9).
//!
//! Two readers over one file is exactly the arrangement that drifts, so the test is equivalence: for every
//! epoch, `Epochs` must yield what `Log::epoch` holds — batches, tokens, sources, entries and order. And at
//! a torn tail both must draw the same line, because R6's rule ("stop at the first record that fails CRC or
//! is short") is what makes a crash mid-write a non-event.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::collections::BTreeMap;

use schweep_log::{Epochs, FaultInjector, Log, SyncPolicy};
use schweep_zset::{DataType, Field, Row, Schema, Value};

fn catalog() -> BTreeMap<String, Schema> {
    BTreeMap::from([(
        "t".to_owned(),
        Schema::new_table(vec![
            Field::not_null("k", DataType::Int64),
            Field::not_null("n", DataType::Int64),
        ])
        .unwrap(),
    )])
}

fn row(k: i64, n: i64) -> Row {
    Row::new(vec![Value::Int(k), Value::Int(n)])
}

fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("schweep-stream-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Build a log with several epochs, retractions included, and return it with its directory.
fn filled(dir: &std::path::Path, epochs: u64) -> Log {
    let mut faults = FaultInjector::inert();
    let mut log = Log::open(dir, catalog(), &mut faults, SyncPolicy::Deferred).unwrap();
    for epoch in 1..=epochs {
        let insert = vec![
            (row(epoch as i64, 1), 1i64),
            (row(epoch as i64, 2), 1i64),
            // A retraction, from the first epoch that has something to retract (I-5).
            (row(epoch as i64 - 1, 1), -1i64),
        ];
        log.append("a", "t", insert, &format!("a{epoch}"), &mut faults)
            .unwrap();
        // A second source in the same epoch, so an epoch is more than one batch.
        log.append(
            "b",
            "t",
            vec![(row(100 + epoch as i64, 7), 1)],
            &format!("b{epoch}"),
            &mut faults,
        )
        .unwrap();
        log.seal_epoch(&mut faults).unwrap();
    }
    log
}

#[test]
fn a_streamed_segment_yields_exactly_what_the_log_holds() {
    let dir = scratch("equivalence");
    let log = filled(&dir, 12);

    let streamed: Vec<schweep_log::SealedEpoch> = Epochs::open(log.segment_path())
        .unwrap()
        .map(Result::unwrap)
        .collect();

    assert_eq!(
        streamed.len() as u64,
        log.sealed_epoch(),
        "the stream must yield one item per sealed epoch"
    );
    for sealed in &streamed {
        assert_eq!(
            sealed.batches,
            log.epoch(sealed.epoch).unwrap(),
            "epoch {} differs between the stream and the log",
            sealed.epoch
        );
    }
    assert!(
        streamed.iter().map(|s| s.epoch).eq(1..=12),
        "epochs must arrive in order, numbered as the log numbers them"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// An append with no seal after it is **not** an epoch — the same rule the live path follows.
#[test]
fn an_unsealed_tail_is_not_yielded_as_an_epoch() {
    let dir = scratch("unsealed");
    let mut faults = FaultInjector::inert();
    let mut log = filled(&dir, 3);
    log.append("a", "t", vec![(row(9, 9), 1)], "never-sealed", &mut faults)
        .unwrap();

    let streamed: Vec<schweep_log::SealedEpoch> = Epochs::open(log.segment_path())
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(
        streamed.len(),
        3,
        "the pending append must not appear as a fourth epoch"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A torn tail stops the stream where the log stops, at every truncation point.
#[test]
fn a_torn_tail_stops_the_stream_where_the_log_stops() {
    let dir = scratch("torn");
    let log = filled(&dir, 6);
    let segment = log.segment_path().to_path_buf();
    let whole = std::fs::read(&segment).unwrap();
    let full: Vec<u64> = Epochs::open(&segment)
        .unwrap()
        .map(|sealed| sealed.unwrap().epoch)
        .collect();
    assert_eq!(full, (1..=6).collect::<Vec<u64>>());
    drop(log);

    // Every truncation, byte by byte. A reader that trusted a frame's own length header would read past
    // the end of the file on many of these; the point is that none of them panics and none of them
    // invents an epoch.
    for cut in 0..whole.len() {
        let torn = dir.join(format!("torn-{cut}"));
        std::fs::write(&torn, &whole[..cut]).unwrap();
        let epochs: Vec<u64> = Epochs::open(&torn)
            .unwrap()
            .map(|sealed| sealed.unwrap().epoch)
            .collect();
        assert!(
            epochs.iter().copied().eq(1..=epochs.len() as u64),
            "truncating to {cut} bytes yielded {epochs:?}, which is not a prefix of the epochs"
        );
        assert!(
            epochs.len() <= full.len(),
            "truncating to {cut} bytes yielded more epochs than the whole file has"
        );
        let _ = std::fs::remove_file(&torn);
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// A flipped byte inside a record is a torn tail as far as the reader is concerned (R6).
#[test]
fn a_flipped_byte_ends_the_stream_rather_than_being_read() {
    let dir = scratch("flipped");
    let log = filled(&dir, 6);
    let segment = log.segment_path().to_path_buf();
    let whole = std::fs::read(&segment).unwrap();
    drop(log);

    // Flip a byte in the middle of the file: everything from that record onward is discarded, and the
    // epochs before it are still readable.
    let mut flipped = whole.clone();
    let at = whole.len() / 2;
    flipped[at] ^= 0xFF;
    let target = dir.join("flipped-segment");
    std::fs::write(&target, &flipped).unwrap();

    let epochs: Vec<u64> = Epochs::open(&target)
        .unwrap()
        .map(|sealed| sealed.unwrap().epoch)
        .collect();
    assert!(
        epochs.len() < 6,
        "a flipped byte must end the stream: got {epochs:?}"
    );
    assert!(
        epochs.iter().copied().eq(1..=epochs.len() as u64),
        "and what it read before the flip must still be a prefix: {epochs:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
