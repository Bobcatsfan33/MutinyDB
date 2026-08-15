//! **The C10 residency gate**: a log's resident memory does not track its history.
//!
//! C9 ended with this named as its largest known limit, measured rather than suspected:
//!
//! > `schweepd`'s resident memory is O(retained log), and no soak can make it flat. `Log` keeps every
//! > sealed batch resident (`sealed: Vec<Vec<Batch>>`) plus one dedup token per append … **Scheduled: C10**
//! > — `docs/PROGRESS.md`, C9
//!
//! C10 pages the batches: the log now holds a **byte range per epoch** and reads the records back from the
//! segment when somebody asks. This file is what turns that from a claim into a number.
//!
//! ## The measurement, and why it is a difference rather than a value
//!
//! Resident memory has a floor that has nothing to do with the log — the allocator, the runtime, this test
//! binary. Quoting a value would be quoting that floor. So the gate opens a log over a **small** history
//! and over a **ten times larger** one, and asks what the extra history cost:
//!
//! ```text
//!   cost of history = RSS(10× history) − RSS(1× history)
//! ```
//!
//! Under the old design that difference was the extra history itself, near enough. The claim now is that it
//! is a small fraction of it, and the assertion is written as a fraction so it means the same thing at any
//! size.
//!
//! Its own test binary, per the rule C9 learned the hard way: resident memory is a property of the process,
//! so a sibling test in the same binary would measure into this one.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::collections::BTreeMap;
use std::io::{BufWriter, Write};

use schweep_log::{Epochs, FaultInjector, Log, Record, SyncPolicy};
use schweep_soak::rss::rss_bytes;
use schweep_zset::{DataType, Field, Row, Schema, Value};

/// Rows per epoch per table, and how wide a row is. Together they set how fast the segment grows.
const ROWS_PER_EPOCH: i64 = 400;
const PADDING: usize = 480;

/// The small history, in bytes. The large one is ten times this.
const SMALL_HISTORY_BYTES: u64 = 24 * 1024 * 1024;

/// The most of the extra history the log may hold resident.
///
/// **A tuned constant, in the ledger** (`HISTORY_RESIDENCY_SHARE`, `testing/evidence/c10-residency.json`).
/// Five percent. Under the pre-C10 design this figure was approximately 1.0 — the log held the history —
/// so any threshold below a half would have failed it; five percent is chosen instead to leave room for the
/// span index (16 bytes an epoch), the dedup index (one entry per append, measured and reported below), and
/// the allocator's own behaviour, while still failing by a wide margin if the batches ever come back.
const HISTORY_RESIDENCY_SHARE: f64 = 0.05;

fn catalog() -> BTreeMap<String, Schema> {
    let table = || {
        Schema::new_table(vec![
            Field::not_null("id", DataType::Int64),
            Field::not_null("pad", DataType::Utf8),
        ])
        .unwrap()
    };
    BTreeMap::from([("a".to_owned(), table()), ("b".to_owned(), table())])
}

fn row(id: i64) -> Row {
    Row::new(vec![Value::Int(id), Value::Str(format!("{id:PADDING$}"))])
}

/// Write a segment of at least `target` bytes, frame by frame, syncing every epoch.
///
/// The per-epoch sync is C9's page-cache lesson carried forward: under a memory cgroup the page cache is
/// charged to the cgroup and dirty pages are not reclaimable until writeback has cleaned them, so a
/// fixture that writes hundreds of megabytes without syncing is killed by the OOM killer rather than
/// measured.
fn write_segment(path: &std::path::Path, target: u64, rows_per_epoch: i64) -> (u64, u64) {
    let file = std::fs::File::create(path).unwrap();
    let mut out = BufWriter::with_capacity(64 * 1024, file);
    let (mut epochs, mut ids, mut written) = (0u64, 0i64, 0u64);
    while written < target {
        for table in ["a", "b"] {
            let entries: Vec<(Row, i64)> = (0..rows_per_epoch)
                .map(|index| (row(ids + index), 1i64))
                .collect();
            let framed = schweep_log::record::frame(
                &Record::Append {
                    source_id: "filler".to_owned(),
                    dedup_token: format!("{table}{epochs}"),
                    table: table.to_owned(),
                    entries,
                }
                .encode(),
            );
            out.write_all(&framed).unwrap();
            written += framed.len() as u64;
        }
        epochs += 1;
        let seal = schweep_log::record::frame(&Record::SealEpoch { epoch: epochs }.encode());
        out.write_all(&seal).unwrap();
        written += seal.len() as u64;
        ids += rows_per_epoch;
        out.flush().unwrap();
        out.get_ref().sync_data().unwrap();
    }
    out.into_inner().unwrap().sync_all().unwrap();
    (epochs, written)
}

/// Open a log over a freshly written segment of `target` bytes, read every epoch once, and report what it
/// cost in resident memory.
fn open_and_read(name: &str, target: u64) -> (u64, u64, u64, usize) {
    let dir = std::env::temp_dir().join(format!(
        "schweep-c10-residency-{name}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    // The name `Log::open` looks for when no pointer names another (`DEFAULT_SEGMENT`).
    let segment = dir.join("segment-00000001.log");
    let (epochs, written) = write_segment(&segment, target, ROWS_PER_EPOCH);

    let before = rss_bytes().expect("this platform reports RSS");
    let mut faults = FaultInjector::inert();
    let log = Log::open(&dir, catalog(), &mut faults, SyncPolicy::Deferred).unwrap();
    assert_eq!(log.sealed_epoch(), epochs, "every epoch must be indexed");

    // Read every epoch back, one at a time — the access pattern a replay or a catch-up has. Holding one
    // epoch at a time is the whole claim, so the test does exactly that and drops each before the next.
    let mut rows_seen = 0u64;
    for epoch in 1..=epochs {
        let batches = log.epoch(epoch).unwrap();
        rows_seen += batches.iter().map(|b| b.entries.len() as u64).sum::<u64>();
    }
    let after = rss_bytes().expect("this platform reports RSS");
    let dedup = log.dedup_len();
    println!(
        "  {name}: {epochs} epochs · {written} bytes of history · {rows_seen} rows read · \
         RSS {before} → {after} · dedup index {dedup} tokens · span index {} bytes",
        log.index_bytes()
    );
    let _ = std::fs::remove_dir_all(&dir);
    (written, after, epochs, dedup)
}

/// **The gate.** Ten times the history must not cost ten times the memory.
#[test]
fn a_logs_resident_memory_does_not_track_its_history() {
    let (small_bytes, small_rss, small_epochs, small_dedup) =
        open_and_read("small", SMALL_HISTORY_BYTES);
    let (large_bytes, large_rss, large_epochs, large_dedup) =
        open_and_read("large", SMALL_HISTORY_BYTES * 10);

    let extra_history = large_bytes.saturating_sub(small_bytes);
    let extra_rss = large_rss.saturating_sub(small_rss);
    let share = extra_rss as f64 / extra_history.max(1) as f64;
    println!(
        "C10 residency gate: {} extra bytes of history cost {} extra bytes of RSS ({:.2}% of it)",
        extra_history,
        extra_rss,
        share * 100.0
    );
    println!(
        "  epochs {small_epochs} → {large_epochs} · dedup tokens {small_dedup} → {large_dedup}"
    );

    assert!(
        share < HISTORY_RESIDENCY_SHARE,
        "ten times the history cost {extra_rss} bytes of resident memory against {extra_history} bytes \
         of history — {:.1}% of it, past the {:.0}% this gate allows. Before C10 this figure was \
         approximately 100%, because the log held every sealed batch; a number near it again means the \
         batches are resident once more.",
        share * 100.0,
        HISTORY_RESIDENCY_SHARE * 100.0
    );

    // What *does* still grow with history, stated as a number rather than left to be discovered: one dedup
    // token per acknowledged append (I-4 — a token forgotten is a batch applied twice), and sixteen bytes
    // of span index per epoch.
    assert_eq!(
        large_dedup as u64,
        large_epochs * 2,
        "the dedup index holds one token per append, and this workload appends twice per epoch"
    );
}

/// The two readers of one format must agree, at every epoch.
///
/// `Log::epoch` now seeks to a span and decodes it; `stream::Epochs` walks the file from the start. Two
/// readers is the arrangement that drifts, and the span index is a *third* thing that can be wrong — an
/// off-by-one in a span boundary would show up as a missing or duplicated record rather than as an error.
#[test]
fn the_span_index_and_a_full_scan_agree_epoch_for_epoch() {
    let dir = std::env::temp_dir().join(format!("schweep-c10-agree-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    // The name `Log::open` looks for when no pointer names another (`DEFAULT_SEGMENT`).
    let segment = dir.join("segment-00000001.log");
    // Many *epochs*, not many bytes: this is a correctness check on epoch boundaries, and a boundary bug
    // shows up per epoch. Two rows an epoch gives hundreds of spans in a small file.
    let (epochs, _) = write_segment(&segment, 512 * 1024, 2);

    let mut faults = FaultInjector::inert();
    let log = Log::open(&dir, catalog(), &mut faults, SyncPolicy::Deferred).unwrap();
    let streamed: Vec<schweep_log::SealedEpoch> = Epochs::open(log.segment_path())
        .unwrap()
        .map(Result::unwrap)
        .collect();

    assert_eq!(streamed.len() as u64, epochs);
    for sealed in &streamed {
        assert_eq!(
            log.epoch(sealed.epoch).unwrap(),
            sealed.batches,
            "epoch {} differs between the span index and a full scan",
            sealed.epoch
        );
    }
    println!("  span index agrees with a full scan over {epochs} epochs");
    let _ = std::fs::remove_dir_all(&dir);
}
