//! **The C9 memo-ceiling gate**: a query registered *late*, over more input than the process may hold.
//!
//! C8 shipped its ceiling gate with a named hole, and this is the file that closes it:
//!
//! > Not that a **`Memo`** runs under a ceiling. A memo keeps the accumulated input in memory for C7's
//! > mid-history catch-up, so its footprint tracks the *data*, not the state. This gate drives a circuit
//! > directly for that reason, and `docs/PROGRESS.md` names the limitation.
//! > — `testing/soak/tests/c8_ceiling.rs`
//!
//! Two things changed in C9. A memo can now be built **without an input cache**
//! ([`Memo::without_input_cache`]), so it holds no copy of the data; and catch-up can be fed as a
//! **stream of per-epoch deltas** ([`Memo::register_from_chunks`]) rather than one accumulated delta, so a
//! registration is O(largest epoch) resident instead of O(history). Together they make the claim this gate
//! measures: *a late registration completes over an accumulated input larger than the memory ceiling, with
//! flat resident memory, and answers what the oracle answers.*
//!
//! ## What this file measures, and what its sibling does
//!
//! Here: **memory**. A log whose accumulated input is a multiple of the ceiling, one late registration over
//! it, RSS sampled **once per chunk the memo pulls** — so the curve is a picture of the catch-up itself,
//! rather than of something running beside it. The answer is checked arithmetically, for the reason C8
//! gives: at this size an oracle would not fit under the ceiling either.
//!
//! **Correctness lives in `c9_late_registration.rs`**, a separate test binary, where an oracle recomputes
//! over a history small enough to recompute. The split is not tidiness: resident memory is a property of
//! the *process*, so a sibling test in this binary inflates the measurement. It did — the two started in
//! one file, and this gate failed the full-workspace run at 123.9 MB against a 96 MiB budget having peaked
//! at 54.6 MB alone. **One RSS-measuring test per binary.**
//!
//! ## How the ceiling is established
//!
//! Read back from the cgroup, exactly as C8 does, and `CURRENT_CEILING_REQUIRED=1` makes its absence a
//! failure. Without a ceiling this file prints that it is a smoke test and holds itself to a fixed RSS
//! budget instead — which is the instrument that actually catches a leak, per C8's lesson that a *shape*
//! check can be flattened by the kernel reclaiming what leaked.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use schweep_memo::Admission;
mod common;

use common::{catalog, directory_bytes, memo, sql, stream, write_segment, ANSWER_KEYS};
use schweep_soak::{ceiling, Ceiling, Curve};

/// How much bigger than the ceiling the *accumulated input* must be.
///
/// Three, not C8's ten, and the difference is deliberate: C8's multiplier is about **state**, which redb
/// spills, and ten times the ceiling is a statement about the spill working. This gate's claim is about the
/// **input** a catch-up streams past, where the honest statement is "more than the process may hold" — three
/// times establishes that, and thirty times would only establish that the test can write a bigger file. The
/// multiplier is in the ledger with the rest of C9's constants.
const INPUT_MULTIPLIER: u64 = 3;

/// The accumulated input the smoke run builds when no ceiling is in force.
const SMOKE_INPUT_BYTES: u64 = 256 * 1024 * 1024;

/// The resident budget the smoke run holds itself to. C8 measured this shape's clean peak at 38 MB and an
/// injected leak's at 214 MB; 96 MiB sits between them, closer to the clean one.
const SMOKE_RSS_BUDGET_BYTES: u64 = 96 * 1024 * 1024;

/// **The gate.** A late registration over an accumulated input larger than the ceiling.
///
/// Named for its budget rather than for "flat memory", which is what it was called first. The catch-up's
/// resident memory is *not* flat — it carries a small per-pass residue, measured and characterised below —
/// and a test whose name claims a property its assertions decline to check is a claim in the worst possible
/// place: the one a reader believes without reading.
#[test]
fn a_late_registration_catches_up_over_more_input_than_the_ceiling_inside_its_budget() {
    let ceiling = ceiling();
    let required = std::env::var("CURRENT_CEILING_REQUIRED").is_ok_and(|value| value == "1");
    let (target_input_bytes, ceiling_bytes) = match (&ceiling, required) {
        (Ceiling::Cgroup { bytes, .. }, _) => (bytes * INPUT_MULTIPLIER, Some(*bytes)),
        (Ceiling::Unlimited, true) => panic!(
            "CURRENT_CEILING_REQUIRED=1 but no cgroup memory ceiling is in force. The CI job must apply \
             one — this test reads it back rather than assuming it, because a ceiling gate on a machine \
             with free memory proves nothing (§6 C8's pitfall, and C9 inherits it)."
        ),
        (Ceiling::Unlimited, false) => {
            println!(
                "NOT A GATE: {} — running a reduced shape as a smoke test. The C9 memo-ceiling gate is \
                 the CI job that applies a cgroup limit and sets CURRENT_CEILING_REQUIRED=1.",
                ceiling.describe()
            );
            (SMOKE_INPUT_BYTES, None)
        }
    };

    println!(
        "C9 memo-ceiling gate: ceiling {} · target accumulated input {target_input_bytes} bytes \
         ({INPUT_MULTIPLIER}x)",
        ceiling.describe()
    );

    let root = std::env::temp_dir().join(format!("schweep-c9-ceiling-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let segment = root.join("segment");

    // The history. Written with no retractions in this phase, so the arithmetic below is exact and the
    // answer's expected contents need no bookkeeping; retraction through a catch-up is phase 1's job,
    // where an oracle checks it.
    let (epochs, ids) = write_segment(&segment, target_input_bytes, 120, false);
    let input_bytes = std::fs::metadata(&segment).unwrap().len();
    println!("  wrote {epochs} epochs · {ids} ids · {input_bytes} bytes of accumulated input");
    assert!(
        input_bytes >= target_input_bytes,
        "the log holds {input_bytes} bytes, short of the {target_input_bytes} the gate claims"
    );

    let plan = schweep_sql::compile(&sql(), &catalog()).unwrap();
    let mut memo = memo(&root.join("state"));

    // **Two curves, one per phase, and that is not fussiness.** Replaying into an empty memo holds almost
    // nothing (there is nothing registered to hold state); the catch-up builds a join's integrals and
    // steps to a plateau in its first chunk. One curve spanning both reads that step as 220% "growth" and
    // as a climb in every quarter — which is how this gate first failed, on a shape that was doing exactly
    // what it should. A shape check is a statement about *one* regime, so each regime gets its own series.
    let mut replay_curve = Curve::default();
    replay_curve.sample();
    for deltas in stream(&segment) {
        memo.seal_epoch(&deltas).unwrap();
        replay_curve.sample();
    }
    println!(
        "  replayed {epochs} epochs into an empty memo · peak {} bytes · {}",
        replay_curve.peak(),
        replay_curve.render()
    );

    // **The catch-up.** One sample per chunk the memo pulls, so the curve is the catch-up's own shape.
    let mut curve = Curve::default();
    let mut pulled = 0u64;
    let handle = memo
        .register_from_chunks(
            &plan,
            Admission::bounded(),
            stream(&segment).inspect(|_| {
                pulled += 1;
                curve.sample();
            }),
        )
        .unwrap();
    curve.sample();

    assert_eq!(
        pulled, epochs,
        "the memo must pull every epoch: a catch-up that stopped early would answer confidently and \
         wrongly"
    );

    let state_bytes = directory_bytes(&root.join("state"));
    println!("RSS curve: {}", curve.render());
    println!(
        "  samples (MiB): {}",
        curve
            .samples
            .iter()
            .map(|b| format!("{}", b / (1024 * 1024)))
            .collect::<Vec<_>>()
            .join(" ")
    );
    println!("  operator state after the catch-up: {state_bytes} bytes");

    // ---- the answer ------------------------------------------------------------------------------
    //
    // Arithmetic, not an oracle: at this size a from-scratch oracle would not fit under the ceiling
    // either, which is C8's reasoning and it has not changed. Phase 1 is where an oracle checks the same
    // path over a history small enough to recompute.
    let answer = memo.read(handle).unwrap().1;
    let expected = ANSWER_KEYS.min(ids) as usize;
    assert_eq!(
        answer.len(),
        expected,
        "the join answer must hold one row per id below {ANSWER_KEYS}"
    );
    for (row, weight) in answer.entries() {
        assert_eq!(*weight, 1, "each id joins once: {row} => {weight}");
    }

    // ---- the claim: memory stayed inside its budget ------------------------------------------------
    let budget = ceiling_bytes.unwrap_or(SMOKE_RSS_BUDGET_BYTES);
    assert!(
        curve.peak() <= budget,
        "peak resident memory {} exceeded the budget of {budget} bytes while catching up over \
         {input_bytes} bytes of accumulated input — the catch-up is holding the history, which is the \
         whole failure this gate exists to catch.\ncurve: {}",
        curve.peak(),
        curve.render()
    );
    assert!(
        curve.peak() < input_bytes,
        "resident memory {} reached the size of the input {input_bytes}; the catch-up is not streaming",
        curve.peak()
    );

    // The replay phase is held to the same budget, separately: it is the cheaper regime and a regression
    // there would otherwise hide inside the catch-up's larger plateau.
    assert!(
        replay_curve.peak() <= budget,
        "peak resident memory {} during the replay exceeded the budget of {budget} bytes",
        replay_curve.peak()
    );

    // ---- the shape of the catch-up, and what it honestly is ----------------------------------------
    //
    // **The catch-up's resident memory climbs, monotonically, and that is measured rather than excused.**
    // Three probes separate the cause: 349 chunks carrying five rows each — negligible state — still climb
    // in every quarter (8.1 → 8.5 MiB, ~1.2 KB a chunk); 349 chunks of 750 padded rows climb further
    // (34.3 → 38.5 MiB, ~12 KB a chunk); and **60 chunks carrying the same total data do not climb at all**
    // (126.5 → 123.5 MiB). So the cost is **per pass**, not per byte of state, and it is a few percent of
    // whatever a chunk carries — the signature of retained allocator arenas or of redb's per-commit
    // bookkeeping, and *not* established to be either. `docs/PROGRESS.md` files it for C10.
    //
    // What follows from that: `climbs_every_quarter` is the wrong instrument here and asserting it would be
    // asserting something false. The instrument is a **coefficient** — growth as a fraction of the input
    // streamed — which is the same shape of claim the C9 soak makes for the same reason.
    //
    // Also note `Curve::growth` is a **fraction**, not a percentage: 0.25 is 25%. The first version of this
    // assertion read `growth < 25.0` and would have allowed 2,500%.
    let (first, last) = curve
        .quartile_means()
        .expect("a 349-chunk catch-up has quartiles");
    let growth = curve.growth().unwrap_or(0.0);
    let grown_bytes = (last - first).max(0.0);
    let share_of_input = grown_bytes / input_bytes as f64;
    println!(
        "  catch-up quartile means: {first:.0} → {last:.0} bytes ({:+.1}%) · {grown_bytes:.0} bytes \
         grown, {:.2}% of the input streamed",
        growth * 100.0,
        share_of_input * 100.0
    );
    assert!(
        share_of_input < 0.125,
        "resident memory grew {grown_bytes:.0} bytes across the catch-up, {:.2}% of the \
         {input_bytes} bytes it streamed. Measured at the committed shape: 3.1%. A catch-up whose \
         residue is a large fraction of its input is holding the history in all but name.\ncurve: {}",
        share_of_input * 100.0,
        curve.render()
    );

    let _ = std::fs::remove_dir_all(&root);
}
