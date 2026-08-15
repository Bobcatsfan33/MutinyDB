//! **A slow consumer is refused, not buffered** (§6 C9) — the backpressure mutation's instrument.
//!
//! A source that keeps appending while nothing seals is the shape that kills a server: every batch is
//! legitimate, the client is behaving, and an unbounded queue turns "we accepted your data" into an OOM
//! kill. So this pushes far more than the bounds allow, from several sources at once, and asserts two
//! things — that refusals arrive, and that **resident memory stays inside a budget**. The budget comes
//! first, because a server could refuse and still buffer and only memory would notice.
//!
//! **Its own test binary, like every other RSS measurement in this crate.** Resident memory is a property
//! of the *process*, so a sibling test in the same binary inflates it — which is not hypothetical: the C9
//! memo-ceiling gate first failed in the full-workspace run for exactly that reason, at 123.9 MB against a
//! 96 MiB budget, having peaked at 54.6 MB alone. This file's budget is 32 MiB and its honest measurement
//! is 19 MB, so it has less headroom than most and needs the isolation more.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::sync::atomic::Ordering;

use schweep_log::SyncPolicy;
use schweep_server::{Client, Policy, Server, ServerConfig};
use schweep_soak::Curve;
use schweep_zset::{DataType, Field, Row, Schema, Value};

fn catalog() -> schweep_plan::bind::Catalog {
    schweep_plan::bind::Catalog::from([(
        "t".to_owned(),
        Schema::new_table(vec![
            Field::not_null("k", DataType::Int64),
            Field::not_null("n", DataType::Int64),
        ])
        .unwrap(),
    )])
}

/// **A slow consumer is refused, not buffered.** The instrument for the backpressure mutation.
///
/// A source that keeps appending while nothing seals is the shape that kills a server: every batch is
/// legitimate, the client is behaving, and an unbounded queue turns "we accepted your data" into an OOM
/// kill. So the test pushes far more than the bounds allow, from several sources at once, and asserts two
/// things — that refusals arrive, and that **resident memory stays inside a budget**. The second is the one
/// that matters: a server could refuse and still buffer, and only the budget would notice.
#[test]
fn a_slow_consumer_is_refused_rather_than_buffered() {
    let dir = std::env::temp_dir().join(format!("schweep-c9-slow-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);

    let mut server = Server::bind(
        &dir,
        catalog(),
        ServerConfig {
            policy: Policy::default(),
            sync: SyncPolicy::Deferred,
            // No checkpoints: nothing seals in this test, so a checkpoint would never fire anyway, and
            // saying so is better than leaving a reader to work it out.
            checkpoint_every: 0,
        },
    )
    .unwrap();
    let address = server.address().unwrap();
    let running = server.running_flag();
    let thread = std::thread::spawn(move || {
        let _ = server.serve();
    });
    let client = Client::new(address);

    // 600-row batches, about 20 KB each on the wire. **At this width the count bound is what fires** —
    // measured: 64 batches and 5,235,416 bytes queued per source, inside the 8 MiB byte bound — and the
    // byte bound's own teeth are shown by
    // `crates/schweep-server/tests/endpoints.rs::a_source_is_bounded_in_bytes_and_an_oversized_batch_is_refused_not_retried`.
    // Four sources, because per-source bounds must hold per source: four times the allowance is the most
    // the server may ever be holding, and this asserts it does not exceed that.
    const SOURCES: usize = 4;
    const ATTEMPTS: usize = 200;
    let wide: Vec<(Row, i64)> = (0..600)
        .map(|index| (Row::new(vec![Value::Int(index), Value::Int(index)]), 1i64))
        .collect();

    let mut curve = Curve::default();
    let mut admitted = 0usize;
    let mut refused = 0usize;
    curve.sample();
    for attempt in 0..ATTEMPTS {
        for source in 0..SOURCES {
            let response = client
                .ingest(
                    &format!("slow{source}"),
                    "t",
                    &format!("s{source}-{attempt}"),
                    &wide,
                )
                .unwrap();
            match response.kind() {
                None => admitted += 1,
                Some(kind) => {
                    assert!(
                        kind.retryable() || kind == schweep_server::ErrorKind::Refused,
                        "a refused append must either invite a retry or say it never will: {kind:?}"
                    );
                    refused += 1;
                }
            }
        }
        curve.sample();
    }
    let health = client.health().unwrap().body().unwrap().to_owned();
    let _ = client.shutdown();
    running.store(false, Ordering::SeqCst);
    let _ = thread.join();

    println!(
        "C9 slow-consumer gate: {admitted} admitted · {refused} refused out of {} attempts",
        SOURCES * ATTEMPTS
    );
    println!("RSS curve: {}", curve.render());
    println!("{health}");

    // **The budget comes first**, because it is the instrument the sprint asked for: a server can refuse
    // and still buffer, and only resident memory notices that. Asserting the refusal count first would
    // shadow it — which it did, until this order was fixed.
    //
    // Four sources at 64 batches of 20 KB is about 5 MB of legitimate held data; the rest
    // is the process. Without backpressure this workload offers 4 x 200 x 20 KB = about 16 MB of queued
    // batches, and their in-memory form is several times their wire size, so a server that buffered
    // instead of refusing climbs well past this budget — which is what the mutation demonstrates.
    // **A tuned constant, in the ledger** (`SLOW_CONSUMER_BUDGET_BYTES`, `testing/evidence/c9-soak.json`).
    // Measured: 19,070,976 bytes with backpressure in force, and 45,793,280 with it removed. 32 MiB sits
    // between them — comfortably above the honest run, comfortably below the buffering one — because a
    // budget set far above both would be a number that never fails, and the first version of this line was
    // exactly that: at 160 MiB the mutation was caught only by the refusal count, and the budget, which is
    // the instrument the sprint asked for, watched it happen.
    const SLOW_CONSUMER_BUDGET_BYTES: u64 = 32 * 1024 * 1024;
    assert!(
        curve.peak() <= SLOW_CONSUMER_BUDGET_BYTES,
        "peak resident memory {} exceeded {SLOW_CONSUMER_BUDGET_BYTES} bytes while nothing was sealed \
         — the queues are buffering, not refusing.\ncurve: {}",
        curve.peak(),
        curve.render()
    );

    assert!(
        refused > 0,
        "nothing was refused: the server accepted {admitted} unsealed batches, which is unbounded \
         buffering with a schedule"
    );
    assert!(
        admitted < SOURCES * ATTEMPTS,
        "every attempt was admitted; there is no backpressure at all"
    );

    // And the server says what it did: a server shedding load silently looks healthy.
    assert!(
        health.contains("refused"),
        "the refusals must be visible in /health: {health}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
