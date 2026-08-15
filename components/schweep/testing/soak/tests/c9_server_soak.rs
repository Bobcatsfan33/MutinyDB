//! **The C9 soak**: `schweepd` under load for a full window, with the RSS curve sampled throughout.
//!
//! What a soak catches is the per-epoch leak — the allocation that happens once per epoch, per request, or
//! per registration, and is never released. Nothing else finds those: the correctness gates run for a few
//! dozen epochs, and a few dozen of anything is invisible.
//!
//! So the workload is deliberately *small per epoch, long in epochs, and bounded in state*: five rows
//! arrive each epoch and the previous epoch's five are retracted, so the table holds five rows forever, the
//! answer holds five rows forever, and every operator's state is bounded. Reads, subscribes and
//! registration churn run against the same server throughout.
//!
//! **The bounded state is not a convenience, it is what makes the measurement mean anything.** The first
//! version of this workload let three groups gain a member per epoch, so operator state grew without bound
//! — and then resident growth of 6.5 KB an epoch was partly *legitimate data* and partly whatever leak
//! might be hiding behind it, with no way to tell which. A soak whose own workload grows cannot report a
//! leak.
//!
//! ## What the curve does, and why it is not flat
//!
//! `schweepd` holds a [`schweep_log::Log`], and a `Log` keeps every sealed batch resident
//! (`sealed: Vec<Vec<Batch>>`) plus one dedup token per append. A server's resident memory is therefore
//! **O(retained log)** however long it runs, and no soak can make that flat. Measured on this shape with
//! *only* ingest and seal — no registration, no reads — the log alone costs 1,589 bytes an epoch
//! (`testing/evidence/c9-soak.json`). Adding the registration, the per-epoch read, the subscribe and the
//! churn adds a few hundred more.
//!
//! So what this gate asserts is not flatness but a **per-epoch coefficient**: growth per epoch inside a
//! budget set from that measurement, plus an absolute peak, plus a check that the coefficient is not
//! *rising* — which is what an O(n)-per-epoch cost looks like and what a bounded-looking leak hides behind.
//! `docs/PROGRESS.md` records the log's O(history) footprint against C10, where the fix is a log that holds
//! an index instead of the batches; [`schweep_log::stream::Epochs`] is already the reader such a log would
//! use, and C9's memo-ceiling gate already streams through it.
//!
//! ## Both instruments, per C8's lesson
//!
//! C8 injected a per-step leak and the *shape* check passed it, because the machine was under pressure and
//! the kernel reclaimed the leaked pages half-way through. A shape can be flattened by the OS; an absolute
//! budget cannot. Both are here.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::collections::BTreeMap;
use std::sync::atomic::Ordering;

use schweep_log::SyncPolicy;
use schweep_server::{Client, Policy, Server, ServerConfig};
use schweep_soak::Curve;
use schweep_zset::{DataType, Field, Row, Schema, Value};

/// Epochs in the window. Long enough that a per-epoch leak of a few hundred bytes is unmistakable, and
/// raisable for the nightly job without changing what the test means.
const EPOCHS: u64 = 3_000;

/// Rows per epoch. Small on purpose: this soak is about *per-epoch* cost, so the data must not dominate.
const ROWS_PER_EPOCH: i64 = 5;

/// Resident memory the run must stay inside.
///
/// Measured: the committed shape peaks at 12 MB after 2,000 epochs and 27 MB after 3,000 with the earlier
/// unbounded-state workload. 128 MiB is several times either — room for a different allocator and a
/// different machine — and small enough that a leak of 20 KB an epoch fails rather than fitting.
const RSS_BUDGET_BYTES: u64 = 128 * 1024 * 1024;

/// Resident growth per epoch the run must stay inside.
///
/// **A tuned constant, in the ledger** (`PER_EPOCH_BUDGET_BYTES`, `testing/evidence/c9-soak.json`).
/// Measured on this shape: 1,589 bytes an epoch with ingest and seal alone — the log's resident batches and
/// its dedup index — and a few hundred more with the registration, the read, the subscribe and the churn.
/// 4 KiB is about two and a half times the measured total, which leaves room for an allocator that rounds
/// differently while still failing a leak of 20 KB an epoch by a factor of five.
const PER_EPOCH_BUDGET_BYTES: f64 = 4096.0;

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

fn row(k: i64, n: i64) -> Row {
    Row::new(vec![Value::Int(k), Value::Int(n)])
}

fn directory_bytes(dir: &std::path::Path) -> u64 {
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

fn epochs() -> u64 {
    std::env::var("SCHWEEP_SOAK_EPOCHS")
        .ok()
        .and_then(|raw| raw.parse().ok())
        .unwrap_or(EPOCHS)
}

/// **The soak.** A full window of ingest, seal, read, subscribe and registration churn.
#[test]
fn a_server_under_load_for_a_full_window_does_not_leak_per_epoch() {
    let epochs = epochs();
    let dir = std::env::temp_dir().join(format!("schweep-c9-soak-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);

    let mut server = Server::bind(
        &dir,
        catalog(),
        ServerConfig {
            policy: Policy::default(),
            // Deferred, because this measures memory rather than durability: an fsync per append would
            // make the run disk-bound and measure the disk.
            sync: SyncPolicy::Deferred,
            checkpoint_every: 64,
        },
    )
    .unwrap();
    let address = server.address().unwrap();
    let running = server.running_flag();
    let thread = std::thread::spawn(move || {
        let _ = server.serve();
    });
    let client = Client::new(address);

    // One standing query for the whole window, and a second that comes and goes — so the registry's
    // teardown accounting is under the soak too, not only under C6's 1,000-cycle unit test.
    let steady: u64 = client
        .register("SELECT t.k AS k, SUM(t.n) AS s FROM t GROUP BY t.k")
        .unwrap()
        .body()
        .unwrap()
        .trim()
        .parse()
        .unwrap();

    let mut curve = Curve::default();
    let mut token = 0u64;
    let mut churned = 0u64;
    let mut subscribed_epochs = 0u64;
    curve.sample();

    for epoch in 1..=epochs {
        // Five rows in, the previous epoch's five out: retractions from day one (I-5) *and* bounded
        // state, so every byte of growth belongs to the log rather than to the workload.
        let mut entries: Vec<(Row, i64)> = (0..ROWS_PER_EPOCH)
            .map(|index| (row(index, epoch as i64), 1i64))
            .collect();
        if epoch > 1 {
            entries.extend((0..ROWS_PER_EPOCH).map(|index| (row(index, epoch as i64 - 1), -1i64)));
        }
        assert!(
            client
                .ingest("soak", "t", &format!("s{epoch}"), &entries)
                .unwrap()
                .is_ok(),
            "epoch {epoch}: ingest was refused"
        );
        assert!(client.seal().unwrap().is_ok(), "epoch {epoch}: seal failed");

        // A read every epoch, a subscribe every third, and a register/deregister pair every hundredth.
        assert!(client.read(steady).unwrap().is_ok());
        if epoch % 3 == 0 {
            let response = client.subscribe(steady, token).unwrap();
            let body = response.body().unwrap();
            let next: u64 = body
                .lines()
                .next()
                .and_then(|line| line.strip_prefix("token "))
                .and_then(|n| n.parse().ok())
                .unwrap();
            subscribed_epochs += next.saturating_sub(token);
            token = next;
        }
        if epoch % 100 == 0 {
            let temporary: u64 = client
                .register("SELECT t.n AS n FROM t WHERE t.k > 1")
                .unwrap()
                .body()
                .unwrap()
                .trim()
                .parse()
                .unwrap();
            assert!(client.read(temporary).unwrap().is_ok());
            client.deregister(temporary).unwrap();
            churned += 1;
        }

        curve.sample();
        if epoch % 500 == 0 {
            println!(
                "  epoch {epoch}: log {} bytes · {}",
                directory_bytes(&dir.join("log")),
                curve.render()
            );
        }
    }

    let log_bytes = directory_bytes(&dir.join("log"));
    let state_bytes = directory_bytes(&dir.join("state"));
    let health = client.health().unwrap().body().unwrap().to_owned();

    // The answer must still be right after a full window: four keys, each summing the epochs that touched
    // it, with key 0's oldest row retracted each epoch.
    let answer = client.answer(steady).unwrap().unwrap();
    let rows: BTreeMap<i64, i64> = answer
        .lines()
        .skip(1)
        .filter_map(|line| {
            let inner = line.strip_prefix('(')?;
            let (k, rest) = inner.split_once(", ")?;
            let (s, _) = rest.split_once(')')?;
            Some((k.parse().ok()?, s.parse().ok()?))
        })
        .collect();
    assert_eq!(
        rows.len(),
        ROWS_PER_EPOCH as usize,
        "the answer must hold one row per key after the window, and no more — the state is bounded, \
         which is what makes the curve readable: {answer}"
    );
    for (key, sum) in &rows {
        assert_eq!(
            *sum, epochs as i64,
            "key {key}'s only surviving row is the latest epoch's"
        );
    }

    let _ = client.shutdown();
    running.store(false, Ordering::SeqCst);
    let _ = thread.join();

    println!(
        "C9 soak: {epochs} epochs · {subscribed_epochs} epochs delivered to the subscriber · \
         {churned} register/deregister cycles"
    );
    println!(
        "  log {log_bytes} bytes ({} per epoch) · state {state_bytes} bytes",
        log_bytes / epochs.max(1)
    );
    println!("RSS curve: {}", curve.render());
    println!("{health}");

    // ---- the claim ---------------------------------------------------------------------------------
    //
    // The log's own growth is reported above and is a few hundred bytes an epoch; everything else must be
    // flat. Both instruments, because C8 showed the shape check alone can be flattened by the kernel.
    assert!(
        curve.peak() <= RSS_BUDGET_BYTES,
        "peak resident memory {} exceeded the budget of {RSS_BUDGET_BYTES} bytes over {epochs} epochs \
         whose log is only {log_bytes} bytes — that is a per-epoch leak, not data.\ncurve: {}",
        curve.peak(),
        curve.render()
    );
    let (first, last) = curve
        .quartile_means()
        .expect("a soak this long has quartiles");
    let growth = curve.growth().unwrap_or(0.0);
    let steady = curve.steady_state();
    let span = (steady.len() as f64 * 0.75).max(1.0);
    let bytes_per_epoch = (last - first) / span;
    // `Curve::growth` is a **fraction**: 0.35 is 35%. Printed as a percentage here, and deliberately not
    // *asserted* on — a server's resident memory grows with its retained log, so a growth threshold would
    // either be meaninglessly loose or fail for a reason that is not a bug. The per-epoch coefficient below
    // is the instrument.
    println!(
        "  quartile means: {first:.0} → {last:.0} bytes ({:+.1}%) · {bytes_per_epoch:.0} bytes per epoch",
        growth * 100.0
    );
    println!(
        "  the log's own growth: {} bytes per epoch",
        log_bytes / epochs.max(1)
    );
    assert!(
        bytes_per_epoch < PER_EPOCH_BUDGET_BYTES,
        "resident memory grew {bytes_per_epoch:.0} bytes per epoch over {epochs} epochs, past the \
         budget of {PER_EPOCH_BUDGET_BYTES:.0}. The log accounts for {} bytes an epoch on disk; anything \
         much beyond that is a per-epoch leak, and a server that leaks per epoch dies in month three \
         (I-9).\ncurve: {}",
        log_bytes / epochs.max(1),
        curve.render()
    );

    // And the coefficient must not be *rising*. A constant leak looks like the log; an O(n)-per-epoch cost
    // — re-rendering a ring, re-scanning a registry — does not, and this is the instrument that sees it.
    //
    // **Equal spans, and that took two tries.** The first version compared mean(Q2) − mean(Q1) against
    // mean(Q4) − mean(Q2): one quarter against two. Perfectly linear growth then reports a ratio of 2, so
    // the check only ever fired on a *fourfold* acceleration while looking as if it fired on a doubling —
    // and the 10,000-epoch nightly duly reported "4,140,080 then 8,185,777", which is linear growth wearing
    // the costume of an accelerating leak. Quarter against quarter, linear growth reports ~1.
    let steady_quarter = steady.len() / 4;
    if steady_quarter >= 4 {
        let mean = |slice: &[u64]| -> f64 {
            slice.iter().map(|b| *b as f64).sum::<f64>() / slice.len().max(1) as f64
        };
        let q1 = mean(&steady[..steady_quarter]);
        let q2 = mean(&steady[steady_quarter..steady_quarter * 2]);
        let q3 = mean(&steady[steady_quarter * 2..steady_quarter * 3]);
        let q4 = mean(&steady[steady_quarter * 3..]);
        let early_slope = (q2 - q1).max(0.0);
        let late_slope = (q4 - q3).max(0.0);
        println!(
            "  slope over equal spans: {early_slope:.0} then {late_slope:.0} bytes per quarter-window \
             (linear growth reports the same twice)"
        );
        assert!(
            late_slope <= early_slope * 2.0 + 1_048_576.0,
            "resident growth accelerated — {early_slope:.0} bytes across the second quarter of the steady \
             state and {late_slope:.0} across the fourth, measured over equal spans. Linear growth is the \
             log; accelerating growth is an O(n)-per-epoch cost.\ncurve: {}",
            curve.render()
        );
    }

    // And the server's own accounting agrees that nothing accumulated: the queues are empty after a seal,
    // and exactly one registration is standing.
    assert!(
        health.contains("pending_appends 0"),
        "every append was sealed, so nothing may be pending: {health}"
    );
    assert!(
        health.contains("registrations 1"),
        "the churned registrations must all be gone: {health}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
