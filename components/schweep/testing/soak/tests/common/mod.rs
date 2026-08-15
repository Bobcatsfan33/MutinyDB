//! The C9 late-registration fixture: a segment on disk, and a memo to catch up from it.
//!
//! Two test binaries need this — `c9_memo_ceiling` measures memory under a cgroup and
//! `c9_late_registration` checks the answers against the oracle — and they are separate binaries because
//! resident memory is a property of the *process*. Two copies of a fixture would be two places for the
//! discipline below to drift out of, so it lives here once.
//!
//! **It lives under `tests/` rather than in the crate's library**, because it panics on failure and the
//! workspace forbids that in library code (`CLAUDE.md` rule 1: panicking is acceptable only in tests and
//! test harnesses). It briefly lived in `src/` and clippy was right to refuse it.
//!
//! **The discipline that matters is the `sync_data` per epoch**, and it is here because CI's OOM killer put
//! it here. See [`write_segment`].

#![allow(
    dead_code,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::io::{BufWriter, Write};

use schweep_log::{Epochs, Record};
use schweep_memo::{Memo, Sharing};
use schweep_plan::bind::Catalog;
use schweep_state::RedbFactory;
use schweep_zset::{DataType, EpochDeltas, Field, Row, Schema, Value};

/// Rows per table per epoch, and the padding width — together these set how fast the log grows.
pub const ROWS_PER_EPOCH: i64 = 750;
pub const PADDING: usize = 480;

/// Only ids below this reach the answer, so the *answer* stays small however large the input grows.
pub const ANSWER_KEYS: i64 = 100;

#[must_use]
pub fn catalog() -> Catalog {
    let table = || {
        Schema::new(vec![
            Field::new("id", DataType::Int64, false),
            Field::new("pad", DataType::Utf8, false),
        ])
        .unwrap()
    };
    Catalog::from([("a".to_owned(), table()), ("b".to_owned(), table())])
}

#[must_use]
pub fn row(id: i64) -> Row {
    Row::new(vec![
        Value::Int(id),
        Value::Str(format!("{:width$}", id, width = PADDING)),
    ])
}

/// The same shape C8 settled on: a join with near-unique keys behind a selective filter. Large state,
/// large input, small answer.
#[must_use]
pub fn sql() -> String {
    format!("SELECT a.id AS id FROM a JOIN b ON a.id = b.id WHERE a.id < {ANSWER_KEYS}")
}

#[must_use]
pub fn directory_bytes(dir: &std::path::Path) -> u64 {
    let mut total = 0u64;
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    for entry in entries.flatten() {
        let Ok(meta) = entry.metadata() else { continue };
        if meta.is_dir() {
            total += directory_bytes(&entry.path());
        } else {
            total += meta.len();
        }
    }
    total
}

/// Write a segment holding at least `target` bytes **and** at least `min_epochs` epochs. Returns the
/// number of epochs sealed and the number of ids inserted.
///
/// **Written frame by frame rather than through [`schweep_log::Log`], and that is the point of the whole
/// file.** A `Log` keeps every sealed batch resident, so filling a gigabyte of history through it costs a
/// gigabyte of memory — the fixture would OOM under the very ceiling the gate applies, and the measurement
/// would be of the fixture rather than of the catch-up. These are the log's own frames, written by the
/// log's own encoder, so the reader below verifies exactly what the log would have written.
///
/// Two size conditions rather than one because the two phases want different things: the ceiling phase
/// wants bytes, the correctness phase wants a *history* — enough epochs that a late registration is
/// genuinely catching up over many of them rather than over one big one.
pub fn write_segment(
    path: &std::path::Path,
    target: u64,
    min_epochs: u64,
    retract: bool,
) -> (u64, i64) {
    let file = std::fs::File::create(path).unwrap();
    let mut out = BufWriter::with_capacity(64 * 1024, file);
    let mut epochs = 0u64;
    let mut ids = 0i64;
    let mut written = 0u64;
    loop {
        for table in ["a", "b"] {
            let mut entries = Vec::with_capacity(ROWS_PER_EPOCH as usize);
            for index in 0..ROWS_PER_EPOCH {
                entries.push((row(ids + index), 1i64));
            }
            // Retractions from day one, in the input the catch-up will stream (I-5). A catch-up that only
            // ever saw insertions would not test the path a real history takes.
            if retract && ids > ANSWER_KEYS + ROWS_PER_EPOCH {
                for index in 0..(ROWS_PER_EPOCH / 10) {
                    entries.push((row(ids - ROWS_PER_EPOCH + ANSWER_KEYS + index), -1));
                }
            }
            let frame = schweep_log::record::frame(
                &Record::Append {
                    source_id: "filler".to_owned(),
                    dedup_token: format!("{table}{epochs}"),
                    table: table.to_owned(),
                    entries,
                }
                .encode(),
            );
            written += frame.len() as u64;
            out.write_all(&frame).unwrap();
        }
        epochs += 1;
        let seal = schweep_log::record::frame(&Record::SealEpoch { epoch: epochs }.encode());
        written += seal.len() as u64;
        out.write_all(&seal).unwrap();

        // **Flush and sync every epoch, and this is not belt-and-braces.** A memory cgroup charges the
        // PAGE CACHE to the cgroup, and dirty pages cannot be reclaimed until they have been written back
        // — so a fixture that writes 384 MB without syncing dirties pages faster than writeback cleans
        // them, and the cgroup's OOM killer fires. It did: the ceiling gate passed one CI run and was
        // killed with exit code 137 on the next, having got no further than printing its own header. The
        // bug was in the fixture, not in the engine, and it is exactly why a flake is a bug and not a
        // re-run.
        //
        // One sync per epoch bounds the dirty set at one epoch's frames — under a megabyte at this shape.
        // It costs a few hundred fsyncs, which is fewer than the log itself performs for the same history.
        out.flush().unwrap();
        out.get_ref().sync_data().unwrap();

        ids += ROWS_PER_EPOCH;
        if written >= target && epochs >= min_epochs {
            out.into_inner().unwrap().sync_all().unwrap();
            return (epochs, ids);
        }
        assert!(
            epochs < 20_000,
            "the segment is not growing toward {target} bytes; it stalled at {written}"
        );
    }
}

/// Every epoch in the segment, as the deltas a catch-up consumes — one epoch resident at a time.
pub fn stream(segment: &std::path::Path) -> impl Iterator<Item = EpochDeltas> {
    Epochs::open(segment).unwrap().map(|sealed| {
        let sealed = sealed.unwrap();
        let mut deltas = EpochDeltas::new();
        for batch in sealed.batches {
            deltas.extend(batch.table, batch.entries);
        }
        deltas
    })
}

pub fn memo(dir: &std::path::Path) -> Memo {
    Memo::without_input_cache(catalog(), Sharing::On, Box::new(RedbFactory::new(dir))).unwrap()
}
