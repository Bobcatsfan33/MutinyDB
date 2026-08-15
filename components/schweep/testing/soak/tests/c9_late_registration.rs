//! **A late registration answers what the oracle answers** (§6 C9).
//!
//! Half of C9's memo-ceiling claim is correctness and half is memory, and they need different sizes: an
//! oracle over the whole history is cheap at thirty epochs and impossible at three hundred megabytes. This
//! file is the correctness half — a log of a few dozen epochs, an oracle recomputing over the same history,
//! and three answers compared: the late registration, a registration present from epoch 0, and the oracle.
//!
//! **It is a separate test binary from `c9_memo_ceiling.rs` on purpose.** That file measures the resident
//! memory of its own process, and resident memory is a property of the *process* — so a sibling test in the
//! same binary inflates it whether it runs concurrently or before. It did: the two started life in one file
//! and the ceiling gate failed in the full-workspace run at 123.9 MB against a 96 MiB budget, having peaked
//! at 54.6 MB when run alone. One RSS-measuring test per binary, and this is the other half of that split.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use schweep_memo::Admission;
mod common;

use common::{catalog, memo, sql, stream, write_segment};

/// **Phase 1 — correctness.** A late registration answers what the oracle answers, and what a
/// registration that was there all along answers.
#[test]
fn a_late_registration_answers_what_the_oracle_answers() {
    let root = std::env::temp_dir().join(format!("schweep-c9-late-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);

    // Small: a few dozen epochs, so a from-scratch oracle over the whole history is cheap.
    std::fs::create_dir_all(&root).unwrap();
    let segment = root.join("segment");
    let (epochs, _) = write_segment(&segment, 4 * 1024 * 1024, 30, true);
    assert!(
        epochs > 8,
        "the history must be long enough to be a history"
    );

    let plan = schweep_sql::compile(&sql(), &catalog()).unwrap();
    let bound = schweep_sql::bind_sql(&sql(), &catalog()).unwrap();

    // The oracle: full recomputation over the same history, epoch by epoch (I-1).
    let mut oracle = schweep_oracle::Oracle::new(catalog()).unwrap();
    // A registration present from epoch 0, stepped like the live path.
    let mut early_memo = memo(&root.join("early-state"));
    let early = early_memo.register(&plan, Admission::bounded());
    let early = match early {
        Ok(handle) => handle,
        // A memo without an input cache refuses `register`, by design: it has no history to catch up from.
        // At epoch 0 there is nothing to catch up, so the streaming door is the one to use.
        Err(_) => early_memo
            .register_from_chunks(&plan, Admission::bounded(), std::iter::empty())
            .unwrap(),
    };
    for deltas in stream(&segment) {
        oracle.seal_epoch(deltas.clone()).unwrap();
        early_memo.seal_epoch(&deltas).unwrap();
    }

    // The late registration: same history, streamed in as chunks, after the fact.
    let mut late_memo = memo(&root.join("late-state"));
    for deltas in stream(&segment) {
        late_memo.seal_epoch(&deltas).unwrap();
    }
    let late = late_memo
        .register_from_chunks(&plan, Admission::bounded(), stream(&segment))
        .unwrap();

    let oracle_answer = oracle.answer(&bound.query).unwrap().canonical().unwrap();
    let early_answer = early_memo.read(early).unwrap().1;
    let late_answer = late_memo.read(late).unwrap().1;

    assert_eq!(
        late_answer.render(),
        oracle_answer.render(),
        "a query registered at epoch {epochs} must answer as though it had been there since epoch 0 (I-1)"
    );
    assert_eq!(
        late_answer.render(),
        early_answer.render(),
        "and it must agree with the registration that was there all along"
    );
    assert_eq!(
        late_memo.epoch(),
        early_memo.epoch(),
        "catching up must not move the epoch: nothing was sealed by registering"
    );
    println!(
        "C9 late-registration correctness: {epochs} epochs · {} answer rows · oracle, early and late \
         registrations agree",
        late_answer.len()
    );

    let _ = std::fs::remove_dir_all(&root);
}
