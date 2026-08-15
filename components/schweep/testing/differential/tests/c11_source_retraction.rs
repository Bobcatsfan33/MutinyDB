//! C11 exit gate: source retraction equals an oracle replay with that source absent.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;

use schweep_log::SyncPolicy;
use schweep_oracle::Oracle;
use schweep_plan::Catalog;
use schweep_server::{Engine, Policy};
use schweep_zset::{DataType, EpochDeltas, Field, Row, Schema, Value};

const QUERIES: &[&str] = &[
    "SELECT t0.id AS id, t0.n AS n FROM t0 WHERE t0.n > 2",
    "SELECT t0.id AS id, t0.n AS n, t1.m AS m FROM t0 JOIN t1 ON t0.k = t1.k",
    "SELECT t0.k AS k, SUM(t0.n) AS s FROM t0 GROUP BY t0.k",
    "SELECT t0.k AS k, SUM(t1.m) AS s FROM t0 JOIN t1 ON t0.k = t1.k GROUP BY t0.k",
];

type AttributedTable = (String, Vec<(Row, i64)>, Vec<(Row, i64)>);

fn catalog() -> Catalog {
    BTreeMap::from([
        (
            "t0".to_owned(),
            Schema::new_table(vec![
                Field::not_null("id", DataType::Int64),
                Field::not_null("k", DataType::Int64),
                Field::not_null("n", DataType::Int64),
            ])
            .unwrap(),
        ),
        (
            "t1".to_owned(),
            Schema::new_table(vec![
                Field::not_null("id", DataType::Int64),
                Field::not_null("k", DataType::Int64),
                Field::not_null("m", DataType::Int64),
            ])
            .unwrap(),
        ),
    ])
}

fn row(id: i64, key: i64, value: i64) -> Row {
    Row::new(vec![Value::Int(id), Value::Int(key), Value::Int(value)])
}

fn batches(seed: u64) -> [AttributedTable; 2] {
    let s = seed as i64;
    [
        (
            "t0".to_owned(),
            vec![
                (row(10_000 + s * 10, s % 3, 3 + s % 7), 1 + (s % 2)),
                (row(10_001 + s * 10, (s + 1) % 3, 1 + s % 5), 1),
            ],
            vec![
                (row(20_000 + s * 10, s % 3, 5 + s % 4), 1),
                (row(20_001 + s * 10, (s + 2) % 3, 2 + s % 6), 1 + (s % 3)),
            ],
        ),
        (
            "t1".to_owned(),
            vec![
                (row(30_000 + s * 10, s % 3, 7 + s % 9), 1),
                (row(30_001 + s * 10, (s + 1) % 3, 2 + s % 3), 1 + (s % 2)),
            ],
            vec![
                (row(40_000 + s * 10, s % 3, 11 + s % 5), 1),
                (row(40_001 + s * 10, (s + 2) % 3, 4 + s % 7), 1),
            ],
        ),
    ]
}

fn run(seed: u64, compact_before_retract: bool) {
    let dir = std::env::temp_dir().join(format!(
        "schweep-c11-{seed}-{compact_before_retract}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    let catalog = catalog();
    let mut engine = Engine::open(
        &dir,
        catalog.clone(),
        Policy::default(),
        SyncPolicy::Deferred,
        0,
    )
    .unwrap();
    let handles: Vec<_> = QUERIES
        .iter()
        .map(|sql| {
            engine
                .register(sql, schweep_memo::Admission::bounded())
                .unwrap()
        })
        .collect();

    let generated = batches(seed);
    let mut expected = EpochDeltas::new();
    for (index, (table, poisoned, trusted)) in generated.iter().enumerate() {
        engine
            .ingest(
                "poisoned",
                table,
                &format!("p-{seed}-{index}"),
                poisoned.clone(),
            )
            .unwrap();
        engine
            .ingest(
                "trusted",
                table,
                &format!("t-{seed}-{index}"),
                trusted.clone(),
            )
            .unwrap();
        expected.extend(table.clone(), trusted.clone());
    }
    engine.seal().unwrap();
    if compact_before_retract {
        engine.compact().unwrap();
    }
    let receipt = engine.retract_source("poisoned", None, None).unwrap();
    assert_eq!(receipt.sealed_epoch, Some(2));
    assert_eq!(receipt.tables, 2);

    let mut oracle = Oracle::new(catalog.clone()).unwrap();
    oracle.seal_epoch(expected).unwrap();
    oracle.seal_epoch(EpochDeltas::new()).unwrap();
    for (sql, handle) in QUERIES.iter().zip(&handles) {
        let query = schweep_sql::bind_sql(sql, &catalog).unwrap().query;
        let expected = oracle
            .canonical_answer_at(&query, oracle.sealed_epoch())
            .unwrap()
            .render();
        let (_, actual) = engine.read(*handle).unwrap();
        assert_eq!(actual, expected, "seed {seed}, query {sql}");
    }

    // The generated negative epoch survives full bootstrap after compaction and restart.
    drop(engine);
    let reopened = Engine::open(&dir, catalog, Policy::default(), SyncPolicy::Deferred, 0).unwrap();
    for (sql, handle) in QUERIES.iter().zip(&handles) {
        let query = schweep_sql::bind_sql(sql, reopened.catalog())
            .unwrap()
            .query;
        let expected = oracle
            .canonical_answer_at(&query, oracle.sealed_epoch())
            .unwrap()
            .render();
        assert_eq!(
            reopened.read(*handle).unwrap().1,
            expected,
            "restart: {sql}"
        );
    }
    assert_eq!(
        reopened
            .explain_maintenance()
            .lines()
            .filter(|line| line.contains("query "))
            .count(),
        QUERIES.len(),
        "all shared memo registrations remain live"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn retract_source_matches_world_without_source_over_seeded_join_and_aggregate_suite() {
    let seeds = std::env::var("SCHWEEP_C11_SEEDS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(128_u64);
    for seed in 0..seeds {
        run(seed, seed % 8 == 0);
    }
}

#[test]
fn predicate_retraction_matches_where_and_does_not_advance_on_retry() {
    let dir = std::env::temp_dir().join(format!("schweep-c11-predicate-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut engine =
        Engine::open(&dir, catalog(), Policy::default(), SyncPolicy::Deferred, 0).unwrap();
    engine
        .ingest("s", "t0", "one", vec![(row(1, 1, 3), 1), (row(2, 1, 9), 2)])
        .unwrap();
    engine.seal().unwrap();
    let first = engine
        .retract_source("s", Some("t0"), Some("t0.n > 5"))
        .unwrap();
    assert_eq!(first.rows, 1);
    assert_eq!(first.multiplicity, 2);
    assert_eq!(first.sealed_epoch, Some(2));
    let retry = engine
        .retract_source("s", Some("t0"), Some("t0.n > 5"))
        .unwrap();
    assert_eq!(retry.sealed_epoch, None);
    assert_eq!(engine.epoch(), 2);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn retry_seals_negative_batches_that_were_acked_before_a_crash() {
    let dir = std::env::temp_dir().join(format!("schweep-c11-pending-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut engine =
        Engine::open(&dir, catalog(), Policy::default(), SyncPolicy::Deferred, 0).unwrap();
    let contributed = row(1, 1, 9);
    engine
        .ingest("s", "t0", "original", vec![(contributed.clone(), 1)])
        .unwrap();
    engine.seal().unwrap();

    // State left by a crash after the generated batch was acknowledged and before transaction seal.
    engine
        .ingest(
            "s",
            "t0",
            "retract:s:2:t0:simulated",
            vec![(contributed, -1)],
        )
        .unwrap();
    assert_eq!(engine.epoch(), 1);
    let receipt = engine.retract_source("s", None, None).unwrap();
    assert_eq!(receipt.sealed_epoch, Some(2));
    assert_eq!(receipt.tables, 1);
    assert_eq!(receipt.rows, 1);
    assert_eq!(receipt.multiplicity, 1);
    assert_eq!(engine.epoch(), 2);
    assert_eq!(
        engine.retract_source("s", None, None).unwrap().sealed_epoch,
        None
    );
    let _ = std::fs::remove_dir_all(&dir);
}
