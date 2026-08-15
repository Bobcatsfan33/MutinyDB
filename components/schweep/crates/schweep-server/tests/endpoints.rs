//! What every endpoint does, over a real socket (D-23).
//!
//! The network differential gate proves the *answers* are right. This file proves the contract around them
//! — the status a refusal carries, what a handle means after a restart, what `/txn` guarantees and what it
//! does not, and that a source at its bound is refused rather than buffered. Those are statements about
//! the wire, and a document is not evidence for them.
//!
//! Every server here binds `127.0.0.1:0` and is reached through [`Client`]; nothing sleeps and nothing
//! guesses a port.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use common::{config, row, Harness, SUM};
use schweep_server::admission::Policy;
use schweep_server::{ErrorKind, ServerConfig};
use schweep_zset::Row;

/// The happy path, end to end: register, ingest, seal, read, subscribe, deregister.
#[test]
fn the_endpoints_round_trip() {
    let h = Harness::fresh("round-trip");
    let handle: u64 = h
        .client
        .register(SUM)
        .unwrap()
        .body()
        .unwrap()
        .trim()
        .parse()
        .unwrap();

    assert_eq!(
        h.client
            .ingest("a", "t", "b1", &[(row(1, 10), 1), (row(1, 5), 1)])
            .unwrap()
            .body()
            .unwrap()
            .trim(),
        "appended"
    );
    assert_eq!(h.client.seal().unwrap().body().unwrap().trim(), "1");
    assert_eq!(
        h.client.answer(handle).unwrap().unwrap(),
        "(k: Int64, s: Int64)\n(1, 15) => 1\n",
        "the answer is the canonical render: schema line first, then rows (S-8). The client strips\n         only the `epoch N` line the endpoint prefixes."
    );
    assert_eq!(h.client.epoch_of(handle).unwrap(), Some(1));

    // A retraction goes through the same door as an insert (I-5).
    h.client.ingest("a", "t", "b2", &[(row(1, 5), -1)]).unwrap();
    h.client.seal().unwrap();
    assert_eq!(
        h.client.answer(handle).unwrap().unwrap(),
        "(k: Int64, s: Int64)\n(1, 10) => 1\n"
    );

    // Subscribing from 0 replays both sealed epochs and hands back the token to use next.
    let subscribed = h.client.subscribe(handle, 0).unwrap();
    let body = subscribed.body().unwrap();
    assert!(body.starts_with("token 2\nepochs 2\n"), "{body}");
    assert!(body.contains("+ (1, 15) => 1"), "{body}");
    assert!(body.contains("- (1, 15) => 1"), "{body}");

    h.client.deregister(handle).unwrap();
    assert_eq!(
        h.client.read(handle).unwrap().kind(),
        Some(ErrorKind::NotFound),
        "a deregistered handle is gone, not empty"
    );
}

/// **The error taxonomy, over the wire.** Every kind, reached by a real request (D-23).
#[test]
fn every_error_kind_is_reachable_and_only_one_is_retryable() {
    let h = Harness::fresh("taxonomy");

    // Refused: outside the dialect.
    let refused = h.client.register("SELECT * FROM t").unwrap();
    assert_eq!(refused.kind(), Some(ErrorKind::Refused), "{refused:?}");

    // NotFound: an unknown handle, and an unknown path.
    assert_eq!(
        h.client.read(9_999).unwrap().kind(),
        Some(ErrorKind::NotFound)
    );
    assert_eq!(
        h.client.request("GET", "/nonesuch", &[]).unwrap().kind(),
        Some(ErrorKind::NotFound)
    );

    // Rejected: the same dedup token with different content — the I-4 conflict.
    h.client
        .ingest("a", "t", "same", &[(row(1, 1), 1)])
        .unwrap();
    let conflict = h
        .client
        .ingest("a", "t", "same", &[(row(2, 2), 1)])
        .unwrap();
    assert_eq!(
        conflict.kind(),
        Some(ErrorKind::Rejected),
        "a token reused with different content must conflict, not overwrite: {conflict:?}"
    );

    // Overloaded: a source at its bound. The only retryable kind.
    let tight = ServerConfig {
        policy: Policy {
            queue_bound: 2,
            ..Policy::default()
        },
        ..config()
    };
    let dir = std::env::temp_dir().join(format!("schweep-c9-overload-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let tight = Harness::start(dir, tight);
    for batch in 0..2 {
        assert!(tight
            .client
            .ingest("a", "t", &format!("b{batch}"), &[(row(1, 1), 1)])
            .unwrap()
            .is_ok());
    }
    let over = tight
        .client
        .ingest("a", "t", "b2", &[(row(1, 1), 1)])
        .unwrap();
    assert_eq!(over.kind(), Some(ErrorKind::Overloaded), "{over:?}");
    assert!(
        over.kind().unwrap().retryable(),
        "Overloaded is the kind a client may retry"
    );
    for kind in [
        ErrorKind::Refused,
        ErrorKind::NotFound,
        ErrorKind::Rejected,
        ErrorKind::Internal,
    ] {
        assert!(!kind.retryable(), "{kind:?} must not invite a retry");
    }
}

/// **Backpressure, not buffering.** A source at its bound is refused; sealing frees it; and a noisy source
/// does not consume another's allowance.
#[test]
fn a_full_queue_refuses_and_a_seal_frees_it() {
    let dir = std::env::temp_dir().join(format!("schweep-c9-backpressure-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let h = Harness::start(
        dir,
        ServerConfig {
            policy: Policy {
                queue_bound: 3,
                ..Policy::default()
            },
            ..config()
        },
    );

    for batch in 0..3 {
        assert!(h
            .client
            .ingest("fast", "t", &format!("f{batch}"), &[(row(1, 1), 1)])
            .unwrap()
            .is_ok());
    }
    assert_eq!(
        h.client
            .ingest("fast", "t", "f3", &[(row(1, 1), 1)])
            .unwrap()
            .kind(),
        Some(ErrorKind::Overloaded),
        "the fourth batch must be refused, never queued: an unbounded queue is a memory leak"
    );

    // A different source is unaffected — the whole point of a per-source bound.
    assert!(
        h.client
            .ingest("slow", "t", "s0", &[(row(2, 2), 1)])
            .unwrap()
            .is_ok(),
        "one source at its bound must not starve another"
    );

    // Sealing is what empties a queue.
    h.client.seal().unwrap();
    assert!(h
        .client
        .ingest("fast", "t", "f4", &[(row(1, 1), 1)])
        .unwrap()
        .is_ok());

    // And the refusal is *reported*: a server shedding load silently looks healthy.
    let health = h.client.health().unwrap().body().unwrap().to_owned();
    assert!(
        health.contains("refused"),
        "health must report refusals: {health}"
    );
}

/// **The bound that bounds memory.** A source is limited in bytes, not only in batches — and a batch that
/// can never fit is `Refused`, not invited to retry.
#[test]
fn a_source_is_bounded_in_bytes_and_an_oversized_batch_is_refused_not_retried() {
    let dir = std::env::temp_dir().join(format!("schweep-c9-bytes-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let h = Harness::start(
        dir,
        ServerConfig {
            policy: Policy {
                // Generous in batches, tight in bytes: only the byte bound can fire here, which is the
                // point — a count bound admits batches of any size.
                queue_bound: 1_000,
                queue_bytes: 4_000,
            },
            ..config()
        },
    );

    // ~34 bytes per row on the wire, so a hundred rows is comfortably inside 4,000 and a few of them are
    // not. The exact figures are in testing/evidence/c9-bounds.json.
    let batch: Vec<(Row, i64)> = (0..40).map(|k| (row(k, 1), 1)).collect();
    let mut admitted = 0u32;
    let mut overloaded = None;
    for attempt in 0..8 {
        let response = h
            .client
            .ingest("wide", "t", &format!("w{attempt}"), &batch)
            .unwrap();
        match response.kind() {
            None => admitted += 1,
            Some(kind) => {
                overloaded = Some(kind);
                break;
            }
        }
    }
    assert!(
        admitted > 0 && admitted < 8,
        "the byte bound must fire before the count bound of 1,000: {admitted} batches were admitted"
    );
    assert_eq!(
        overloaded,
        Some(ErrorKind::Overloaded),
        "a source over its byte bound must be told to back off — a seal will free the bytes"
    );

    // And a single batch bigger than the whole budget: retrying it forever would never work, so the kind
    // must say that.
    let huge: Vec<(Row, i64)> = (0..500).map(|k| (row(k, 1), 1)).collect();
    let refused = h.client.ingest("wide", "t", "huge", &huge).unwrap();
    assert_eq!(
        refused.kind(),
        Some(ErrorKind::Refused),
        "an unfittable batch must not carry the retryable kind: {refused:?}"
    );
    let (_, message) = refused.body().unwrap_err();
    assert!(
        message.contains("split it"),
        "the refusal must tell the client what to do instead: {message}"
    );

    // A seal frees the bytes, and the source works again.
    h.client.seal().unwrap();
    assert!(h
        .client
        .ingest("wide", "t", "after-seal", &batch)
        .unwrap()
        .is_ok());
}

/// **D-22.** A registration is server-owned and durable: the handle, its SQL, its admission and its answer
/// all survive a restart.
#[test]
fn a_registration_and_its_answer_survive_a_restart() {
    let h = Harness::fresh("durable-registry");
    let handle: u64 = h
        .client
        .register(SUM)
        .unwrap()
        .body()
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let unbounded: u64 = h
        .client
        .register_unbounded(
            "SELECT t.n AS n, COUNT(*) AS c FROM t GROUP BY t.n",
            "n is a user-supplied key space",
        )
        .unwrap()
        .body()
        .unwrap()
        .trim()
        .parse()
        .unwrap();

    h.client
        .ingest("a", "t", "b1", &[(row(1, 10), 1), (row(2, 7), 1)])
        .unwrap();
    h.client.seal().unwrap();
    let before = h.client.answer(handle).unwrap().unwrap();
    let epoch_before = h.client.epoch_of(handle).unwrap();

    let h = h.restart();

    assert_eq!(
        h.client.answer(handle).unwrap().unwrap(),
        before,
        "the same handle must answer the same thing after a restart (D-22)"
    );
    assert_eq!(
        h.client.epoch_of(handle).unwrap(),
        epoch_before,
        "and at the same epoch: recovery is replay, not re-baseline (I-7)"
    );

    // The next handle is not reused, and the unbounded admission came back with it (I-9).
    let next: u64 = h
        .client
        .register("SELECT t.n AS n FROM t WHERE t.k > 0")
        .unwrap()
        .body()
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(
        next > unbounded,
        "a handle must never be reissued: {next} is not past {unbounded}"
    );
    let health = h.client.health().unwrap().body().unwrap().to_owned();
    assert_eq!(
        health.matches("live").count(),
        3,
        "all three registrations must be live after the restart: {health}"
    );

    // Ingest continues into the epoch after the one recovery ended at.
    h.client.ingest("a", "t", "b2", &[(row(1, 5), 1)]).unwrap();
    h.client.seal().unwrap();
    assert_eq!(
        h.client.answer(handle).unwrap().unwrap(),
        "(k: Int64, s: Int64)\n(1, 15) => 1\n(2, 7) => 1\n"
    );
}

/// **D-22's quarantine.** A persisted registration that no longer binds is held, not dropped, and it says
/// why — so a server does not come back healthy while answering nothing.
#[test]
fn a_registration_that_no_longer_binds_is_quarantined_and_not_dropped() {
    let h = Harness::fresh("quarantine");
    let handle: u64 = h
        .client
        .register(SUM)
        .unwrap()
        .body()
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let dir = h.dir.clone();
    drop(h);

    // Rewrite the persisted SQL to something that cannot bind — what a dialect change would do to an old
    // registration. The registry is rewritten through its own encoder, so this is a legal file.
    let mut registry = schweep_server::Registry::load(&dir).unwrap();
    let entry = registry.entries.get_mut(&handle).unwrap();
    entry.sql = "SELECT t.missing AS m FROM t".to_owned();
    registry.store(&dir).unwrap();

    let h = Harness::start(dir, config());
    let failure = h.client.read(handle).unwrap();
    assert_eq!(
        failure.kind(),
        Some(ErrorKind::Rejected),
        "a quarantined handle reports a conflict, not a not-found: {failure:?}"
    );
    let (_, message) = failure.body().unwrap_err();
    assert!(
        message.contains("missing"),
        "the quarantine must name what broke: {message}"
    );
    let health = h.client.health().unwrap().body().unwrap().to_owned();
    assert!(
        health.contains("QUARANTINED"),
        "health must show the quarantine rather than a clean sheet: {health}"
    );

    // And a client can clear it, which is the only way out (D-22).
    h.client.deregister(handle).unwrap();
    assert_eq!(
        h.client.read(handle).unwrap().kind(),
        Some(ErrorKind::NotFound)
    );
}

/// **MD-2 ask 3.** `/txn` appends N batches and seals them into one epoch — and the honest limit is that a
/// refusal partway leaves the earlier appends pending, not rolled back.
#[test]
fn a_transaction_seals_its_batches_into_one_epoch_and_says_what_it_does_not_guarantee() {
    let h = Harness::fresh("txn");
    let handle: u64 = h
        .client
        .register(SUM)
        .unwrap()
        .body()
        .unwrap()
        .trim()
        .parse()
        .unwrap();

    let batches = vec![
        ("t".to_owned(), "x1".to_owned(), vec![(row(1, 1), 1)]),
        ("t".to_owned(), "x2".to_owned(), vec![(row(2, 2), 1)]),
        ("t".to_owned(), "x3".to_owned(), vec![(row(3, 3), 1)]),
    ];
    let sealed = h.client.transaction("a", &batches).unwrap();
    assert_eq!(sealed.body().unwrap().trim(), "1", "one epoch, not three");
    assert_eq!(
        h.client.answer(handle).unwrap().unwrap(),
        "(k: Int64, s: Int64)\n(1, 1) => 1\n(2, 2) => 1\n(3, 3) => 1\n",
        "all three batches must be visible in the same epoch (I-3)"
    );

    // The stated limit: a conflict partway through does not seal, and does not roll back either. The batch
    // that landed is pending, and the retry's dedup tokens are what make that safe (I-4).
    let conflicting = vec![
        ("t".to_owned(), "y1".to_owned(), vec![(row(4, 4), 1)]),
        // `x1` again, with different content: a conflict.
        ("t".to_owned(), "x1".to_owned(), vec![(row(9, 9), 1)]),
    ];
    let refused = h.client.transaction("a", &conflicting).unwrap();
    assert_eq!(refused.kind(), Some(ErrorKind::Rejected), "{refused:?}");
    assert_eq!(
        h.client.epoch_of(handle).unwrap(),
        Some(1),
        "a refused transaction seals nothing"
    );
    let health = h.client.health().unwrap().body().unwrap().to_owned();
    assert!(
        health.contains("pending_appends 1"),
        "the append that landed is pending, not rolled back — the limit D-23 records: {health}"
    );

    // The retry with the same tokens is what completes it, and I-4 drops the one that already landed.
    let retry = vec![
        ("t".to_owned(), "y1".to_owned(), vec![(row(4, 4), 1)]),
        ("t".to_owned(), "y2".to_owned(), vec![(row(5, 5), 1)]),
    ];
    assert_eq!(
        h.client
            .transaction("a", &retry)
            .unwrap()
            .body()
            .unwrap()
            .trim(),
        "2"
    );
    assert_eq!(
        h.client.answer(handle).unwrap().unwrap(),
        "(k: Int64, s: Int64)\n(1, 1) => 1\n(2, 2) => 1\n(3, 3) => 1\n(4, 4) => 1\n(5, 5) => 1\n",
        "the retry must not double-count the batch that had already landed (I-4)"
    );
}

/// The N×append+seal path is not deprecated by `/txn`: both produce the same epoch, byte for byte.
#[test]
fn the_transaction_and_the_append_seal_path_produce_the_same_epoch() {
    let one = Harness::fresh("txn-equivalence-a");
    let many = Harness::fresh("txn-equivalence-b");
    let handle_of = |h: &Harness| -> u64 {
        h.client
            .register(SUM)
            .unwrap()
            .body()
            .unwrap()
            .trim()
            .parse()
            .unwrap()
    };
    let (a, b) = (handle_of(&one), handle_of(&many));

    let batches = vec![
        ("t".to_owned(), "p1".to_owned(), vec![(row(1, 1), 1)]),
        ("t".to_owned(), "p2".to_owned(), vec![(row(1, 4), 1)]),
        ("t".to_owned(), "p3".to_owned(), vec![(row(2, 2), -1)]),
    ];
    one.client.transaction("a", &batches).unwrap();
    for (table, token, entries) in &batches {
        many.client.ingest("a", table, token, entries).unwrap();
    }
    many.client.seal().unwrap();

    assert_eq!(
        one.client.answer(a).unwrap().unwrap(),
        many.client.answer(b).unwrap().unwrap(),
        "/txn is a convenience over the primitive, not a different semantics"
    );
    assert_eq!(
        one.client.epoch_of(a).unwrap(),
        many.client.epoch_of(b).unwrap()
    );
    assert_eq!(
        one.client.counters().unwrap().body().unwrap(),
        many.client.counters().unwrap().body().unwrap(),
        "and it does the same work: identical counters through both paths (I-6)"
    );
}

/// Graceful shutdown reports what it drained, and the next start recovers from it.
#[test]
fn shutdown_reports_its_drain_and_recovery_continues_from_it() {
    let h = Harness::fresh("shutdown");
    let handle: u64 = h
        .client
        .register(SUM)
        .unwrap()
        .body()
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    h.client.ingest("a", "t", "b1", &[(row(1, 3), 1)]).unwrap();
    h.client.seal().unwrap();
    // An append with no seal: durable and unsealed, which is what shutdown must report rather than hide.
    h.client.ingest("a", "t", "b2", &[(row(1, 4), 1)]).unwrap();

    let report = h.client.shutdown().unwrap().body().unwrap().to_owned();
    assert!(report.contains("epoch 1"), "{report}");
    assert!(
        report.contains("pending_appends 1"),
        "shutdown must say what it left unsealed: {report}"
    );
    assert!(report.contains("registrations 1"), "{report}");

    let h = h.restart();
    // The unsealed append is still there, and the next seal applies it exactly once (I-4, I-7).
    assert_eq!(
        h.client.answer(handle).unwrap().unwrap(),
        "(k: Int64, s: Int64)\n(1, 3) => 1\n"
    );
    h.client.seal().unwrap();
    assert_eq!(
        h.client.answer(handle).unwrap().unwrap(),
        "(k: Int64, s: Int64)\n(1, 7) => 1\n",
        "the append that survived shutdown is applied once, in the next epoch"
    );
}

/// `/oneshot` answers without a registration, over the same accumulated input (C7).
#[test]
fn a_one_shot_query_needs_no_registration() {
    let h = Harness::fresh("oneshot");
    h.client
        .ingest("a", "t", "b1", &[(row(1, 10), 1), (row(2, 5), 1)])
        .unwrap();
    h.client.seal().unwrap();

    let body = h.client.oneshot(SUM).unwrap().body().unwrap().to_owned();
    assert!(body.contains("(1, 10) => 1"), "{body}");
    assert!(body.contains("(2, 5) => 1"), "{body}");
    assert!(
        h.client
            .health()
            .unwrap()
            .body()
            .unwrap()
            .contains("registrations 0"),
        "a one-shot leaves nothing standing behind it"
    );

    // And it agrees with a registration over the same data — the C7 identity, over the wire.
    let handle: u64 = h
        .client
        .register(SUM)
        .unwrap()
        .body()
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let standing = h.client.read(handle).unwrap().body().unwrap().to_owned();
    let standing = standing.split_once('\n').unwrap().1.to_owned();
    assert_eq!(
        standing, body,
        "one-shot and incremental must not disagree (I-1)"
    );
}

/// **I-2 through the network door.** Two servers fed the same requests hold identical state.
///
/// §6 C9 asks that the server add no nondeterminism of its own, and the I-6 counter gate proves the
/// network door does the same *work* as the typed one. This proves the smaller, blunter thing: the same
/// sequence of requests, twice, on two directories, produces the same fingerprint — counters, state sizes,
/// wiring and all — and the same answer. If the server ever consulted a clock, hashed by address, or
/// depended on which port it got, this is what would fail.
#[test]
fn two_servers_fed_the_same_requests_are_byte_identical() {
    let one = Harness::fresh("determinism-a");
    let two = Harness::fresh("determinism-b");

    for h in [&one, &two] {
        let handle: u64 = h
            .client
            .register(SUM)
            .unwrap()
            .body()
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert_eq!(handle, 0, "handles start at 0 on a fresh directory");
        // A second query that shares the group-by with the first, so the memo's sharing decisions are in
        // the fingerprint too.
        h.client
            .register("SELECT t.k AS k, SUM(t.n) AS s FROM t GROUP BY t.k")
            .unwrap();
        for epoch in 1..=6i64 {
            h.client
                .ingest(
                    "a",
                    "t",
                    &format!("b{epoch}"),
                    &[(row(epoch % 3, epoch), 1)],
                )
                .unwrap();
            if epoch % 2 == 0 {
                h.client
                    .ingest("b", "t", &format!("c{epoch}"), &[(row(0, epoch), -1)])
                    .unwrap();
                h.client.seal().unwrap();
            }
        }
    }

    assert_eq!(
        one.client.fingerprint().unwrap().body().unwrap(),
        two.client.fingerprint().unwrap().body().unwrap(),
        "two servers given the same requests must hold the same state (I-2)"
    );
    assert_eq!(
        one.client.counters().unwrap().body().unwrap(),
        two.client.counters().unwrap().body().unwrap(),
        "and must have done the same work getting there"
    );
    assert_eq!(
        one.client.answer(0).unwrap().unwrap(),
        two.client.answer(0).unwrap().unwrap()
    );
    assert_eq!(
        one.client.plan(0).unwrap().body().unwrap(),
        two.client.plan(0).unwrap().body().unwrap()
    );
}

#[test]
fn explain_maintenance_reports_shared_work_without_double_counting_the_dataflow() {
    let h = Harness::fresh("explain-maintenance");
    h.client
        .register("SELECT t.n AS n FROM t WHERE t.k > 1")
        .unwrap();
    h.client
        .register("SELECT DISTINCT t.n AS n FROM t WHERE t.k > 1")
        .unwrap();
    h.client
        .ingest("source", "t", "batch", &[(row(2, 10), 1)])
        .unwrap();
    h.client.seal().unwrap();

    let report = h
        .client
        .explain_maintenance()
        .unwrap()
        .body()
        .unwrap()
        .to_owned();
    assert!(report.starts_with("EXPLAIN MAINTENANCE\nepoch 1\n"));
    assert!(report.contains("query 0"));
    assert!(report.contains("query 1"));
    assert!(report.contains("shared with 1 other quer(y|ies)"));
    assert!(report.contains("dataflow:"));
    assert!(report.contains("counted once each"));
    assert!(report.contains("testing/evidence/c10-benchmarks.json"));
}
