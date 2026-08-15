//! **Exactly-once per epoch**, and what a resume token means (D-23).
//!
//! The token is the epoch number and the server holds no cursor, so the whole guarantee reduces to one
//! rule: `GET /subscribe?from=T` returns every sealed epoch **strictly after** T, and the token it hands
//! back is the last epoch it delivered. Two failures follow from getting that boundary wrong, and both are
//! silent:
//!
//! - `>=` instead of `>` redelivers the epoch the client just consumed — a **duplicate**, which a
//!   subscriber applying deltas turns into a doubled row;
//! - a token computed as "from + how many I sent" **skips** an epoch the moment a poll returns nothing.
//!
//! So the assertion is not "the deltas looked right". It is that across a whole consumption history the
//! multiset of delivered epochs is exactly `1..=N`, each once — which is the property the resume-token
//! mutation must break, and does.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use common::{row, Harness, SUM};
use schweep_server::{ErrorKind, SUBSCRIPTION_RING};

/// Parse a `/subscribe` body: `token N`, `epochs K`, then K blocks of `epoch M` and the delta's lines.
fn parse(body: &str) -> (u64, Vec<(u64, String)>) {
    let mut lines = body.lines();
    let token: u64 = lines
        .next()
        .and_then(|line| line.strip_prefix("token "))
        .and_then(|n| n.parse().ok())
        .expect("the first line is the next token");
    let count: usize = lines
        .next()
        .and_then(|line| line.strip_prefix("epochs "))
        .and_then(|n| n.parse().ok())
        .expect("the second line is how many epochs follow");

    let mut deltas: Vec<(u64, String)> = Vec::new();
    for line in lines {
        match line.strip_prefix("epoch ") {
            Some(number) => deltas.push((number.parse().unwrap(), String::new())),
            None => {
                if let Some((_, body)) = deltas.last_mut().map(|d| (d.0, &mut d.1)) {
                    body.push_str(line);
                    body.push('\n');
                }
            }
        }
    }
    assert_eq!(
        count,
        deltas.len(),
        "the count must match the blocks: {body}"
    );
    (token, deltas)
}

/// Register, then run `epochs` epochs, polling on the schedule `poll` decides. Returns every
/// `(epoch, delta)` delivered, in delivery order, including any redelivery.
fn consume(
    h: &Harness,
    handle: u64,
    epochs: u64,
    poll: impl Fn(u64) -> bool,
) -> Vec<(u64, String)> {
    let mut token = 0u64;
    let mut delivered = Vec::new();
    for epoch in 1..=epochs {
        h.client
            .ingest("a", "t", &format!("b{epoch}"), &[(row(1, epoch as i64), 1)])
            .unwrap();
        h.client.seal().unwrap();
        if poll(epoch) {
            let response = h.client.subscribe(handle, token).unwrap();
            let (next, deltas) = parse(response.body().unwrap());
            delivered.extend(deltas);
            token = next;
        }
    }
    // Drain whatever the schedule left behind: a subscriber that stops polling has not lost anything.
    let response = h.client.subscribe(handle, token).unwrap();
    let (_, deltas) = parse(response.body().unwrap());
    delivered.extend(deltas);
    delivered
}

fn register(h: &Harness) -> u64 {
    h.client
        .register(SUM)
        .unwrap()
        .body()
        .unwrap()
        .trim()
        .parse()
        .unwrap()
}

/// **The gate.** Every epoch is delivered exactly once, on every polling schedule.
#[test]
fn every_sealed_epoch_is_delivered_exactly_once_on_every_polling_schedule() {
    // Four schedules, chosen to hit the boundary from both sides: poll after every epoch (so a poll
    // always has exactly one epoch), poll rarely (so a poll has several), poll on a pattern that
    // includes consecutive polls with nothing in between (the "token computed from the count" bug), and
    // poll only at the very end (so one poll carries the whole history).
    type Schedule = (&'static str, fn(u64) -> bool);
    let schedules: [Schedule; 4] = [
        ("every epoch", |_| true),
        ("every third", |epoch| epoch % 3 == 0),
        ("twice in a row, then a gap", |epoch| epoch % 5 <= 1),
        ("never until the end", |_| false),
    ];

    for (name, poll) in schedules {
        let h = Harness::fresh(&format!(
            "subscribe-{}",
            name.replace(' ', "-").replace(',', "")
        ));
        let handle = register(&h);
        let delivered = consume(&h, handle, 12, poll);

        let epochs: Vec<u64> = delivered.iter().map(|(epoch, _)| *epoch).collect();
        assert_eq!(
            epochs,
            (1..=12).collect::<Vec<u64>>(),
            "schedule {name:?}: every sealed epoch exactly once, in order — no duplicate, no skip"
        );
        // A delta is never empty: "nothing changed" says so, because an empty body is indistinguishable
        // from a transport that dropped one.
        assert!(
            delivered.iter().all(|(_, delta)| !delta.is_empty()),
            "schedule {name:?}: an epoch was delivered with an empty delta"
        );
    }
}

/// Redelivery is what makes exactly-once *per epoch* safe: the same token returns the same bytes.
#[test]
fn resubscribing_from_an_old_token_redelivers_the_same_bytes() {
    let h = Harness::fresh("subscribe-redelivery");
    let handle = register(&h);
    for epoch in 1..=5u64 {
        h.client
            .ingest("a", "t", &format!("b{epoch}"), &[(row(1, epoch as i64), 1)])
            .unwrap();
        h.client.seal().unwrap();
    }

    let first = h.client.subscribe(handle, 0).unwrap();
    let (token, once) = parse(first.body().unwrap());
    let again = h.client.subscribe(handle, 0).unwrap();
    let (token_again, twice) = parse(again.body().unwrap());

    assert_eq!(
        token, 5,
        "the token is the last epoch delivered, not a count"
    );
    assert_eq!(token_again, token);
    assert_eq!(
        once, twice,
        "a client that crashed before recording its token must get the same epochs, byte for byte"
    );

    // And from the token, nothing: a caught-up subscriber is not a subscriber in trouble.
    let caught_up = h.client.subscribe(handle, token).unwrap();
    let (still, none) = parse(caught_up.body().unwrap());
    assert!(none.is_empty(), "{none:?}");
    assert_eq!(
        still, token,
        "polling while caught up must not move the token"
    );
}

/// A token *ahead* of the server is refused rather than served an empty success.
#[test]
fn a_token_from_the_future_is_not_silently_accepted() {
    let h = Harness::fresh("subscribe-future");
    let handle = register(&h);
    h.client.ingest("a", "t", "b1", &[(row(1, 1), 1)]).unwrap();
    h.client.seal().unwrap();

    // Epoch 9 does not exist yet. There is nothing after it, so the honest answer is "no epochs, and
    // your token is unchanged" — the same shape as being caught up. What must *not* happen is the token
    // moving backwards to 1, which would redeliver an epoch the client had supposedly consumed.
    let response = h.client.subscribe(handle, 9).unwrap();
    let (token, deltas) = parse(response.body().unwrap());
    assert!(deltas.is_empty(), "{deltas:?}");
    assert_eq!(
        token, 9,
        "a token ahead of the log must come back unchanged, never rewound"
    );
}

/// **The ring is bounded, and a token behind it is refused.** A gap is a refusal, never a re-baseline
/// dressed as a delta (D-23) — and the bound is what stops a subscriber that stopped consuming from being
/// a memory leak with a schedule.
#[test]
fn a_token_behind_the_ring_is_refused_and_names_the_oldest_epoch_it_has() {
    let h = Harness::fresh("subscribe-ring");
    let handle = register(&h);

    // One past the ring, so exactly one epoch has fallen out of it.
    let epochs = SUBSCRIPTION_RING as u64 + 1;
    for epoch in 1..=epochs {
        h.client
            .ingest("a", "t", &format!("b{epoch}"), &[(row(1, epoch as i64), 1)])
            .unwrap();
        h.client.seal().unwrap();
    }

    let refused = h.client.subscribe(handle, 0).unwrap();
    assert_eq!(
        refused.kind(),
        Some(ErrorKind::Rejected),
        "a token behind the ring must be refused: {refused:?}"
    );
    let (_, message) = refused.body().unwrap_err();
    assert!(
        message.contains("behind the oldest retained epoch 1"),
        "the refusal must name the oldest epoch the server still has: {message}"
    );

    // From the oldest epoch it does have, the whole ring is served — and it is exactly the ring's size.
    let ok = h.client.subscribe(handle, 1).unwrap();
    let (token, deltas) = parse(ok.body().unwrap());
    assert_eq!(deltas.len(), SUBSCRIPTION_RING);
    assert_eq!(token, epochs);
    assert_eq!(
        deltas.first().map(|(epoch, _)| *epoch),
        Some(2),
        "the ring holds the newest {SUBSCRIPTION_RING} epochs, oldest first"
    );

    // The read path is unaffected: the *answer* is durable even when the deltas are not.
    let answer = h.client.answer(handle).unwrap().unwrap();
    let total: i64 = (1..=epochs as i64).sum();
    assert!(
        answer.contains(&format!("(1, {total}) => 1")),
        "the answer must be complete even for a subscriber that fell behind: {answer}"
    );
}

/// A subscription on a quarantined or unknown handle fails by kind, not by silence.
#[test]
fn subscribing_to_a_handle_that_is_not_there_fails_by_kind() {
    let h = Harness::fresh("subscribe-unknown");
    assert_eq!(
        h.client.subscribe(7, 0).unwrap().kind(),
        Some(ErrorKind::NotFound)
    );
    let handle = register(&h);
    h.client.deregister(handle).unwrap();
    assert_eq!(
        h.client.subscribe(handle, 0).unwrap().kind(),
        Some(ErrorKind::NotFound),
        "a deregistered handle's subscription is gone, not empty"
    );
}
