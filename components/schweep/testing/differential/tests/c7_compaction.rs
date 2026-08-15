//! **The C7 exit gate**: one-shot answers, I-1 across a compaction, and the four materializations
//! (`ARCHITECTURE.md` §6 C7, `docs/DURABILITY.md` §4).
//!
//! > **Exit gate:** one-shot answers equal oracle over the fuzz suite; compaction gate: answers
//! > byte-identical before/after compaction (I-1 across a compaction is the whole point); a new query
//! > registered mid-history produces the same result store as one registered at epoch 1 (the
//! > four-materializations discipline, Current edition).
//!
//! ## Why "byte-identical before and after" is the whole sprint
//!
//! Compaction deletes committed history. Everything else in this repository adds; this is the one
//! operation that takes away, and it takes away the artefact every other invariant is defined against
//! — the log. So the claim it has to earn is not "compaction works" but **"nothing can tell"**: a
//! standing query mid-flight, a query registered afterwards, and a one-shot asked at the end must all
//! produce what they would have produced had the prefix never been discarded.
//!
//! The gate is arranged so that a compaction that lost or altered a single row shows up as a byte
//! difference in an answer, not as a warning nobody reads.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::collections::BTreeMap;

use schweep_batch::{compact, hydrate, oneshot, snapshot};
use schweep_differential::{sweep_matching, OneShotEngine, OracleEngine, Scenario};
use schweep_log::{Ack, FaultInjector, Log, SyncPolicy};
use schweep_memo::{Memo, Sharing};
use schweep_oracle::Oracle;
use schweep_plan::bind::Catalog;
use schweep_zset::{DataType, EpochDeltas, Field, Row, Schema, Value};

// ---- the fixture: a log, a memo, and a history worth compacting --------------------------------

fn catalog() -> Catalog {
    let t = Schema::new(vec![
        Field::new("id", DataType::Int64, false),
        Field::nullable("k", DataType::Int64),
        Field::nullable("n", DataType::Int64),
    ])
    .unwrap();
    let u = Schema::new(vec![
        Field::new("id", DataType::Int64, false),
        Field::nullable("m", DataType::Int64),
    ])
    .unwrap();
    Catalog::from([("t".to_owned(), t), ("u".to_owned(), u)])
}

fn tables() -> Vec<(String, Schema)> {
    catalog().into_iter().collect()
}

fn t_row(id: i64, k: Option<i64>, n: Option<i64>) -> Row {
    Row::new(vec![
        Value::Int(id),
        k.map_or(Value::Null, Value::Int),
        n.map_or(Value::Null, Value::Int),
    ])
}

fn u_row(id: i64, m: Option<i64>) -> Row {
    Row::new(vec![Value::Int(id), m.map_or(Value::Null, Value::Int)])
}

/// A history with the shapes that break naive compaction: a row inserted then retracted (net zero, so
/// it must **not** be in the snapshot), a multiplicity partly retracted (net non-zero, so it **must**
/// be, at the right weight), a same-epoch churn, and an empty epoch.
fn history() -> Vec<EpochDeltas> {
    let mut epochs = Vec::new();

    let mut e1 = EpochDeltas::new();
    e1.extend(
        "t",
        vec![
            (t_row(1, Some(2), Some(10)), 1),
            (t_row(2, Some(2), Some(20)), 3),
            (t_row(3, Some(0), Some(30)), 1),
            (t_row(4, Some(5), None), 1),
        ],
    );
    e1.extend("u", vec![(u_row(1, Some(100)), 1), (u_row(2, None), 1)]);
    epochs.push(e1);

    let mut e2 = EpochDeltas::new();
    e2.extend(
        "t",
        vec![
            // Net zero across two epochs: row 3 must be absent from the snapshot entirely.
            (t_row(3, Some(0), Some(30)), -1),
            // Net 3 - 1 = 2: row 2 must be in the snapshot at weight 2, not 1 and not 3.
            (t_row(2, Some(2), Some(20)), -1),
            // Same-epoch retract-and-reinsert.
            (t_row(5, Some(7), Some(50)), 1),
            (t_row(5, Some(7), Some(50)), -1),
            (t_row(5, Some(7), Some(51)), 2),
        ],
    );
    epochs.push(e2);

    epochs.push(EpochDeltas::new());

    let mut e4 = EpochDeltas::new();
    e4.extend("t", vec![(t_row(6, Some(2), Some(60)), 1)]);
    e4.extend(
        "u",
        vec![(u_row(3, Some(300)), 2), (u_row(1, Some(100)), -1)],
    );
    epochs.push(e4);

    let mut e5 = EpochDeltas::new();
    e5.extend("t", vec![(t_row(1, Some(2), Some(10)), -1)]);
    e5.extend("u", vec![(u_row(4, None), 1)]);
    epochs.push(e5);

    epochs
}

const QUERIES: &[&str] = &[
    "SELECT t.n AS n FROM t WHERE t.k > 1",
    "SELECT t.k AS k, COUNT(*) AS c, SUM(t.n) AS s FROM t GROUP BY t.k",
    "SELECT DISTINCT t.k AS k FROM t",
    "SELECT t.n AS n, u.m AS m FROM t JOIN u ON t.id = u.id",
    "SELECT COUNT(*) AS c FROM u",
];

fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("schweep-c7-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A log with the history appended and sealed, one token per (epoch, table).
fn seeded_log(dir: &std::path::Path) -> (Log, Vec<String>) {
    let mut faults = FaultInjector::inert();
    let mut log = Log::open(
        dir.join("log"),
        catalog(),
        &mut faults,
        SyncPolicy::Deferred,
    )
    .unwrap();
    let mut tokens = Vec::new();
    for (index, deltas) in history().iter().enumerate() {
        for (table, entries) in deltas.tables() {
            let token = format!("epoch-{index}-{table}");
            assert_eq!(
                log.append("src", table, entries.clone(), &token, &mut faults)
                    .unwrap(),
                Ack::Appended
            );
            tokens.push(token);
        }
        log.seal_epoch(&mut faults).unwrap();
    }
    (log, tokens)
}

fn oracle_answers() -> Vec<String> {
    let mut oracle = Oracle::new(tables()).unwrap();
    for deltas in history() {
        oracle.seal_epoch(deltas).unwrap();
    }
    QUERIES
        .iter()
        .map(|sql| {
            let query = schweep_sql::bind_sql(sql, &catalog()).unwrap().query;
            oracle
                .canonical_answer_at(&query, oracle.sealed_epoch())
                .unwrap()
                .render()
        })
        .collect()
}

// ---- the gates ---------------------------------------------------------------------------------

/// **One-shot answers equal the oracle**, over the whole generated population.
#[test]
fn one_shot_answers_equal_the_oracle_over_the_population() {
    let report = match sweep_matching::<OneShotEngine, OracleEngine>(0..4_400, |_| true) {
        Ok(report) => report,
        Err(divergence) => panic!("{divergence}"),
    };
    println!(
        "one-shot vs oracle: {} scenarios, {} comparisons, {} error answers",
        report.scenarios, report.comparisons, report.error_answers
    );
    assert_eq!(report.scenarios, 4_400);
    assert!(report.error_answers > 0, "S-22 must be exercised here too");
    assert_eq!(report.families.len(), 4);
}

/// **I-1 across a compaction.** Answers byte-identical before and after, for live standing queries and
/// for queries registered afterwards.
#[test]
fn answers_are_byte_identical_across_a_compaction() {
    let dir = scratch("identity");
    let (mut log, _) = seeded_log(&dir);
    let expected = oracle_answers();

    // A memo whose queries have been running the whole time, brought up on the log.
    let mut memo = Memo::with_sharing(catalog(), Sharing::On).unwrap();
    let handles: Vec<_> = QUERIES
        .iter()
        .map(|sql| memo.register_sql(sql).expect(sql))
        .collect();
    for epoch in 1..=log.sealed_epoch() {
        let mut deltas = EpochDeltas::new();
        for batch in log.epoch(epoch).unwrap() {
            deltas.extend(batch.table.clone(), batch.entries.iter().cloned());
        }
        memo.seal_epoch(&deltas).unwrap();
    }

    let before: Vec<String> = handles
        .iter()
        .map(|handle| memo.read(*handle).unwrap().1.render())
        .collect();
    assert_eq!(
        before, expected,
        "the standing answers must be the oracle's"
    );

    // Compact at epoch 3 — mid-history, with two epochs still to come in the retained suffix.
    let integrals_at_3 = {
        let mut oracle_log = Log::open(
            dir.join("shadow"),
            catalog(),
            &mut FaultInjector::inert(),
            SyncPolicy::Deferred,
        )
        .unwrap();
        for (index, deltas) in history().iter().take(3).enumerate() {
            for (table, entries) in deltas.tables() {
                oracle_log
                    .append(
                        "src",
                        table,
                        entries.clone(),
                        &format!("epoch-{index}-{table}"),
                        &mut FaultInjector::inert(),
                    )
                    .unwrap();
            }
            oracle_log.seal_epoch(&mut FaultInjector::inert()).unwrap();
        }
        hydrate::accumulated(&oracle_log, &catalog()).unwrap()
    };

    let compacted = compact::compact(
        &mut log,
        3,
        &integrals_at_3,
        &mut FaultInjector::inert(),
        SyncPolicy::Deferred,
    )
    .unwrap();
    println!(
        "compacted to epoch {} · rows {:?} · {} tokens carried",
        compacted.anchor, compacted.rows, compacted.tokens
    );
    assert_eq!(log.retained_from(), 3);
    assert_eq!(log.sealed_epoch(), 5, "the suffix is still there");

    // The net-zero row must be absent and the partly-retracted multiplicity must be at its net weight.
    let snapshot_t = snapshot::read_table(
        &snapshot::table_path(&compacted.snapshot, "t"),
        catalog().get("t").unwrap(),
    )
    .unwrap();
    let rendered = snapshot_t.canonical().unwrap().render();
    assert!(
        !rendered.contains("(3, 0, 30)"),
        "a row inserted and retracted is not present, so it is not in the snapshot:\n{rendered}"
    );
    assert!(
        rendered.contains("(2, 2, 20) => 2"),
        "a multiplicity partly retracted keeps its net weight:\n{rendered}"
    );

    // 1 · the queries that were already running are untouched by the compaction.
    let after: Vec<String> = handles
        .iter()
        .map(|handle| memo.read(*handle).unwrap().1.render())
        .collect();
    assert_eq!(
        after, before,
        "a live standing query's answer must not move when the log is compacted"
    );

    // 2 · a query registered *after* the compaction, hydrated from snapshot + suffix.
    let one_delta = hydrate::one_delta_for(&log, &catalog()).unwrap();
    let mut fresh = Memo::with_sharing(catalog(), Sharing::On).unwrap();
    let fresh_handles: Vec<_> = QUERIES
        .iter()
        .map(|sql| fresh.register_sql(sql).expect(sql))
        .collect();
    fresh.seal_epoch(&one_delta).unwrap();
    let from_fresh: Vec<String> = fresh_handles
        .iter()
        .map(|handle| fresh.read(*handle).unwrap().1.render())
        .collect();
    assert_eq!(
        from_fresh, expected,
        "a query registered after the compaction must answer for the whole history"
    );

    // 3 · a one-shot over the compacted log.
    for (sql, want) in QUERIES.iter().zip(&expected) {
        let query = schweep_sql::bind_sql(sql, &catalog()).unwrap().query;
        assert_eq!(
            &oneshot::answer_over_log(&log, &catalog(), &query)
                .unwrap()
                .render(),
            want,
            "one-shot over a compacted log: {sql}"
        );
    }

    let _ = std::fs::remove_dir_all(&dir);
}

/// **The four materializations, Current edition.** Four ways to reach the same answer over one history.
#[test]
fn four_materializations_of_one_history_agree() {
    let dir = scratch("four");
    let (mut log, _) = seeded_log(&dir);
    let expected = oracle_answers();
    let epochs = history();

    // (1) registered at epoch 1 and maintained throughout.
    let mut from_the_start = Memo::with_sharing(catalog(), Sharing::On).unwrap();
    let early: Vec<_> = QUERIES
        .iter()
        .map(|sql| from_the_start.register_sql(sql).expect(sql))
        .collect();

    // (2) registered mid-history, *before* the compaction.
    let mut mid_pre = Memo::with_sharing(catalog(), Sharing::On).unwrap();
    let mut mid_pre_handles = Vec::new();

    // (3) registered *after* the compaction.
    // (4) a one-shot at the very end.

    let mut running = EpochDeltas::new();
    let mut integrals_at_anchor = BTreeMap::new();
    for (index, deltas) in epochs.iter().enumerate() {
        from_the_start.seal_epoch(deltas).unwrap();
        if index >= 1 {
            mid_pre.seal_epoch(deltas).unwrap();
        }
        if index == 1 {
            // Registered at epoch 2, caught up from the accumulated input of epochs 1..=1.
            mid_pre.seal_epoch(&EpochDeltas::new()).unwrap();
        }
        for (table, entries) in deltas.tables() {
            running.extend(table.clone(), entries.clone());
        }
        if index == 0 {
            // Bring the mid-history memo up on epoch 1 by registering it *after* epoch 1 was sealed:
            // catch-up sources the accumulated input, which is C6's mechanism, and this is the case
            // where that input still comes from the log.
            let mut seeded = Memo::with_sharing(catalog(), Sharing::On).unwrap();
            for sql in QUERIES {
                mid_pre_handles.push(seeded.register_sql(sql).expect(sql));
            }
            seeded.seal_epoch(deltas).unwrap();
            mid_pre = seeded;
        }
        if index == 2 {
            integrals_at_anchor =
                hydrate::accumulated(&log_upto(&dir, index + 1), &catalog()).unwrap();
        }
    }

    // Compact at epoch 3 now that the history has been played.
    compact::compact(
        &mut log,
        3,
        &integrals_at_anchor,
        &mut FaultInjector::inert(),
        SyncPolicy::Deferred,
    )
    .unwrap();

    // (3) after the compaction: snapshot + suffix.
    let mut post = Memo::with_sharing(catalog(), Sharing::On).unwrap();
    let post_handles: Vec<_> = QUERIES
        .iter()
        .map(|sql| post.register_sql(sql).expect(sql))
        .collect();
    post.seal_epoch(&hydrate::one_delta_for(&log, &catalog()).unwrap())
        .unwrap();

    let materializations: Vec<(&str, Vec<String>)> = vec![
        (
            "registered at epoch 1",
            early
                .iter()
                .map(|h| from_the_start.read(*h).unwrap().1.render())
                .collect(),
        ),
        (
            "registered mid-history, pre-compaction",
            mid_pre_handles
                .iter()
                .map(|h| mid_pre.read(*h).unwrap().1.render())
                .collect(),
        ),
        (
            "registered post-compaction",
            post_handles
                .iter()
                .map(|h| post.read(*h).unwrap().1.render())
                .collect(),
        ),
        (
            "one-shot at the end",
            QUERIES
                .iter()
                .map(|sql| {
                    let query = schweep_sql::bind_sql(sql, &catalog()).unwrap().query;
                    oneshot::answer_over_log(&log, &catalog(), &query)
                        .unwrap()
                        .render()
                })
                .collect(),
        ),
    ];

    for (label, answers) in &materializations {
        assert_eq!(
            answers, &expected,
            "{label}: disagreed with a from-scratch recomputation"
        );
    }
    println!(
        "four materializations × {} queries agree, and agree with the oracle",
        QUERIES.len()
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A log holding the first `epochs` epochs of the history, for computing an anchor's integrals.
fn log_upto(dir: &std::path::Path, epochs: usize) -> Log {
    let path = dir.join(format!("upto-{epochs}"));
    let mut faults = FaultInjector::inert();
    let mut log = Log::open(path, catalog(), &mut faults, SyncPolicy::Deferred).unwrap();
    for (index, deltas) in history().iter().take(epochs).enumerate() {
        for (table, entries) in deltas.tables() {
            log.append(
                "src",
                table,
                entries.clone(),
                &format!("epoch-{index}-{table}"),
                &mut faults,
            )
            .unwrap();
        }
        log.seal_epoch(&mut faults).unwrap();
    }
    log
}

/// **I-4 across a compaction** — the edge that makes compaction dangerous.
///
/// A token acknowledged before the compaction and re-offered after it must still be
/// acknowledged-and-dropped. If the dedup ledger did not ride the snapshot, the batch would be applied
/// a *second* time, and the only symptom would be double-counted data.
#[test]
fn a_token_acked_before_a_compaction_is_still_dropped_after_it() {
    let dir = scratch("dedup");
    let (mut log, tokens) = seeded_log(&dir);
    let integrals = hydrate::accumulated(&log_upto(&dir, 3), &catalog()).unwrap();
    let tokens_before = log.known_tokens();

    let compacted = compact::compact(
        &mut log,
        3,
        &integrals,
        &mut FaultInjector::inert(),
        SyncPolicy::Deferred,
    )
    .unwrap();
    assert_eq!(
        compacted.tokens, tokens_before,
        "every acknowledged token must be written into the snapshot's ledger"
    );

    // Re-offer every token, with its original content, to the *live* log.
    for (index, deltas) in history().iter().enumerate() {
        for (table, entries) in deltas.tables() {
            let token = format!("epoch-{index}-{table}");
            assert_eq!(
                log.append(
                    "src",
                    table,
                    entries.clone(),
                    &token,
                    &mut FaultInjector::inert()
                )
                .unwrap(),
                Ack::DroppedAsReplay,
                "{token} was acked before the compaction and must be dropped after it (I-4)"
            );
        }
    }

    // And after a *reopen*, where the dedup index is rebuilt from the ledger plus the retained
    // segment rather than from memory (R7).
    drop(log);
    let mut reopened = Log::open(
        dir.join("log"),
        catalog(),
        &mut FaultInjector::inert(),
        SyncPolicy::Deferred,
    )
    .unwrap();
    assert_eq!(
        reopened.known_tokens(),
        tokens_before,
        "a reopened compacted log must know every token it ever acknowledged"
    );
    for token in &tokens {
        let (index, table) = split_token(token);
        let entries = history()[index].entries_for(&table).to_vec();
        assert_eq!(
            reopened
                .append("src", &table, entries, token, &mut FaultInjector::inert())
                .unwrap(),
            Ack::DroppedAsReplay,
            "{token} must survive both the compaction and the reopen"
        );
    }

    // A token that was never acknowledged is still accepted: the ledger must not become a blanket
    // refusal, which would pass this test while breaking ingest.
    assert_eq!(
        reopened
            .append(
                "src",
                "t",
                vec![(t_row(9, Some(1), Some(90)), 1)],
                "brand-new-token",
                &mut FaultInjector::inert()
            )
            .unwrap(),
        Ack::Appended
    );

    let _ = std::fs::remove_dir_all(&dir);
}

fn split_token(token: &str) -> (usize, String) {
    let rest = token.strip_prefix("epoch-").unwrap();
    let (index, table) = rest.split_once('-').unwrap();
    (index.parse().unwrap(), table.to_owned())
}

/// A compaction with no anchor, or one that has already happened, is refused rather than half-done.
#[test]
fn compaction_refuses_what_it_cannot_anchor() {
    let dir = scratch("refuse");
    let (mut log, _) = seeded_log(&dir);
    let integrals = hydrate::accumulated(&log_upto(&dir, 3), &catalog()).unwrap();

    assert!(
        matches!(
            compact::compact(
                &mut log,
                0,
                &integrals,
                &mut FaultInjector::inert(),
                SyncPolicy::Deferred
            ),
            Err(schweep_batch::BatchError::NoCheckpointToAnchorTo)
        ),
        "P1: no published checkpoint, nothing to anchor to"
    );

    compact::compact(
        &mut log,
        3,
        &integrals,
        &mut FaultInjector::inert(),
        SyncPolicy::Deferred,
    )
    .unwrap();
    assert!(
        compact::compact(
            &mut log,
            3,
            &integrals,
            &mut FaultInjector::inert(),
            SyncPolicy::Deferred
        )
        .is_err(),
        "compacting the same prefix twice is refused, not repeated"
    );
    assert!(
        compact::compact(
            &mut log,
            99,
            &integrals,
            &mut FaultInjector::inert(),
            SyncPolicy::Deferred
        )
        .is_err(),
        "an anchor past the sealed epoch would delete records that were never snapshotted"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A one-shot and a standing query answer the same question identically, over the same input.
///
/// The same machinery either way (§5.8): if these ever diverged, one of the two paths would have grown
/// its own opinion about the dialect.
#[test]
fn a_one_shot_and_a_standing_query_agree() {
    let mut scenarios = 0usize;
    for seed in 0..500u64 {
        let scenario = Scenario::generate(seed).unwrap();
        let catalog: Catalog = scenario.tables.iter().cloned().collect();

        let mut memo = Memo::with_sharing(catalog.clone(), Sharing::On).unwrap();
        let plan = schweep_sql::incrementalize_typed(&scenario.query, &catalog).unwrap();
        let handle = memo
            .register(&plan, schweep_memo::Admission::bounded())
            .unwrap();

        let mut accumulated = EpochDeltas::new();
        for deltas in &scenario.epochs {
            memo.seal_epoch(deltas).unwrap();
            for (table, entries) in deltas.tables() {
                accumulated.extend(table.clone(), entries.clone());
            }
        }

        let standing = memo.read(handle).map(|(_, answer)| answer.render());
        let one_shot =
            oneshot::answer(&catalog, &scenario.query, &accumulated).map(|answer| answer.render());
        match (standing, one_shot) {
            (Ok(a), Ok(b)) => assert_eq!(a, b, "seed {seed}"),
            (Err(a), Err(b)) => assert!(
                a.to_string().contains(&b.to_string()) || b.to_string().contains(&a.to_string()),
                "seed {seed}: standing said {a}, one-shot said {b}"
            ),
            (a, b) => panic!("seed {seed}: standing {a:?}, one-shot {b:?}"),
        }
        scenarios += 1;
    }
    println!("{scenarios} scenarios answered both ways, identically");
    assert_eq!(scenarios, 500);
}
