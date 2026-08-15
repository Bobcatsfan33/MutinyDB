#![allow(clippy::panic, clippy::unwrap_used)]

use mutiny_semantic::{
    route_exact, BridgeColumns, BridgeEmbeddingPlan, GenerationMigration, MigrationPhase,
    OneShotAnswer, OneShotSemanticSource, QueryRoute, ScalarColumns, ScalarPredicate,
    SemanticDelta, SemanticError, SemanticGroupPlan, SemanticGroups, SemanticHit, SemanticQuery,
    SemanticRecord, SemanticTopK,
};
use prism_types::vector::dot;
use prism_types::{Attributes, Embedder, Event, HashEmbedder, Query};
use schweep_zset::{Row, Value};
use std::collections::BTreeMap;
use std::fmt::Write as _;

fn columns(tenant: &str, event_time: i64, cost: f64, error: bool) -> ScalarColumns {
    ScalarColumns {
        tenant: tenant.to_owned(),
        event_time,
        cost,
        error,
    }
}

fn record(key: &str, space: &str, vector: Vec<f32>, columns: ScalarColumns) -> SemanticRecord {
    SemanticRecord::new(key, space, vector, columns).unwrap()
}

fn query(space: &str, k: usize, predicate: ScalarPredicate) -> SemanticQuery {
    SemanticQuery::new("standing", space, vec![1.0, 0.0, 0.0, 0.0], k, predicate).unwrap()
}

#[test]
fn hybrid_topk_moves_incrementally_and_retraction_restores_the_prior_answer() {
    let predicate = ScalarPredicate {
        tenant: Some("acme".to_owned()),
        min_cost: Some(10.0),
        error: Some(false),
        ..ScalarPredicate::default()
    };
    let mut state = SemanticTopK::new(query("model:v1", 2, predicate));
    let near = record(
        "near",
        "model:v1",
        vec![1.0, 0.01, 0.0, 0.0],
        columns("acme", 10, 20.0, false),
    );
    let next = record(
        "next",
        "model:v1",
        vec![0.9, 0.2, 0.0, 0.0],
        columns("acme", 11, 11.0, false),
    );
    let filtered = record(
        "filtered",
        "model:v1",
        vec![1.0, 0.0, 0.0, 0.0],
        columns("other", 12, 100.0, false),
    );
    let baseline = vec![
        SemanticDelta {
            record: next.clone(),
            weight: 1,
        },
        SemanticDelta {
            record: filtered,
            weight: 1,
        },
    ];
    state.apply_epoch(baseline).unwrap();
    assert_eq!(state.answer()[0].key, "next");

    let changed = state
        .apply_epoch([SemanticDelta {
            record: near.clone(),
            weight: 1,
        }])
        .unwrap();
    assert_eq!(state.answer()[0].key, "near");
    assert!(changed.iter().any(|delta| delta.weight == -1));
    assert!(changed.iter().any(|delta| delta.weight == 1));

    state
        .apply_epoch([SemanticDelta {
            record: near,
            weight: -1,
        }])
        .unwrap();
    assert_eq!(state.answer()[0].key, "next");
}

#[test]
fn generations_never_share_score_space_and_an_epoch_is_atomic() {
    let mut state = SemanticTopK::new(query("model:v1", 2, ScalarPredicate::default()));
    let valid = record(
        "v1",
        "model:v1",
        vec![1.0, 0.0, 0.0, 0.0],
        columns("acme", 1, 1.0, false),
    );
    let foreign = record(
        "v2",
        "model:v2",
        vec![1.0, 0.0, 0.0, 0.0],
        columns("acme", 1, 1.0, false),
    );
    assert!(matches!(
        state.apply_epoch([
            SemanticDelta {
                record: valid,
                weight: 1,
            },
            SemanticDelta {
                record: foreign,
                weight: 1,
            },
        ]),
        Err(SemanticError::SpaceMismatch { .. })
    ));
    assert!(state.answer().is_empty(), "the valid prefix must roll back");
}

#[test]
fn declared_state_ceiling_refuses_without_mutating() {
    let query = query("model:v1", 1, ScalarPredicate::default())
        .with_state_budget(32)
        .unwrap();
    let mut state = SemanticTopK::new(query);
    let row = record(
        "too-large",
        "model:v1",
        vec![1.0, 0.0, 0.0, 0.0],
        columns("acme", 1, 1.0, false),
    );
    assert!(matches!(
        state.apply_epoch([SemanticDelta {
            record: row,
            weight: 1,
        }]),
        Err(SemanticError::StateBudgetExceeded { .. })
    ));
    assert_eq!(state.state_bytes(), 0);
}

#[test]
fn randomized_epoch_answers_equal_an_independent_exact_one_shot_oracle() {
    for seed in 1..=64u64 {
        let predicate = ScalarPredicate {
            tenant: Some("acme".to_owned()),
            time_from: Some(10),
            min_cost: Some(5.0),
            ..ScalarPredicate::default()
        };
        let query = query("model:v1", 7, predicate.clone());
        let query_vector = query.vector.clone();
        let mut state = SemanticTopK::new(query);
        let mut oracle = BTreeMap::new();
        let mut rng = seed;

        for epoch in 1..=32 {
            rng = rng
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            let key = format!("{seed}-{epoch}");
            let vector = vec![
                ((rng >> 8) as u16 as f32) + 1.0,
                ((rng >> 24) as u16 as f32) + 1.0,
                ((rng >> 40) as u16 as f32) + 1.0,
                ((rng >> 56) as u8 as f32) + 1.0,
            ];
            let row = record(
                &key,
                "model:v1",
                vector,
                columns(
                    if rng & 1 == 0 { "acme" } else { "other" },
                    epoch,
                    (rng % 20) as f64,
                    rng & 4 != 0,
                ),
            );
            state
                .apply_epoch([SemanticDelta {
                    record: row.clone(),
                    weight: 1,
                }])
                .unwrap();
            oracle.insert(key, row);

            let mut expected = oracle
                .values()
                .filter(|row| predicate.admits(&row.columns))
                .map(|row| (dot(&query_vector, &row.vector), row.key.clone()))
                .collect::<Vec<_>>();
            expected.sort_by(|left, right| {
                right
                    .0
                    .total_cmp(&left.0)
                    .then_with(|| left.1.cmp(&right.1))
            });
            let actual = state.answer();
            assert_eq!(
                actual.len(),
                expected.len().min(7),
                "seed {seed}, epoch {epoch}"
            );
            for (hit, (_distance, key)) in actual.iter().zip(expected.iter()) {
                assert_eq!(&hit.key, key, "seed {seed}, epoch {epoch}");
                assert_eq!(
                    hit.score.to_bits(),
                    dot(&query_vector, &oracle.get(key).unwrap().vector).to_bits(),
                    "seed {seed}, epoch {epoch}"
                );
            }
        }
    }
}

#[test]
fn distance_order_is_total_for_ties() {
    let mut state = SemanticTopK::new(query("model:v1", 3, ScalarPredicate::default()));
    for key in ["c", "a", "b"] {
        state
            .apply_epoch([SemanticDelta {
                record: record(
                    key,
                    "model:v1",
                    vec![0.0, 1.0, 0.0, 0.0],
                    columns("acme", 1, 1.0, false),
                ),
                weight: 1,
            }])
            .unwrap();
    }
    assert_eq!(
        state
            .answer()
            .iter()
            .map(|hit| hit.key.as_str())
            .collect::<Vec<_>>(),
        vec!["a", "b", "c"]
    );
}

#[test]
fn maintained_hybrid_answer_equals_prismdbs_real_exact_search() {
    let root = tempfile::tempdir().unwrap();
    let dim = 16;
    let engine = prism_engine::Engine::init(
        root.path(),
        prism_part::store::StoreConfig {
            format_version: prism_part::store::STORE_VERSION,
            dim,
            nlist: 8,
            pq_m: 4,
            seed: 42,
            kmeans_restarts: prism_quantizer::kmeans::KMEANS_RESTARTS,
            block_size: prism_part::format::DEFAULT_BLOCK_SIZE,
            partitions: Default::default(),
            promote: Vec::new(),
        },
    )
    .unwrap();
    let embedder = HashEmbedder::with_version(dim, "1");
    let mut events = Vec::new();
    for id in 0..96 {
        events.push(Event {
            event_id: format!("event-{id:03}"),
            tenant_id: if id % 3 == 0 { "other" } else { "acme" }.to_owned(),
            event_time: 1_000 + id,
            observed_time: 2_000,
            event_name: "agent.step".to_owned(),
            cost: id as f64,
            error: id % 7 == 0,
            body: format!(
                "{} security database operation {id}",
                if id % 4 == 0 { "urgent" } else { "routine" }
            ),
            trace_id: String::new(),
            span_id: String::new(),
            attributes: Attributes::new(),
            idempotency_key: None,
        });
    }
    engine.ingest(events.clone(), 2_000).unwrap();

    let query_text = "urgent security database operation";
    let prism_query = Query {
        text: query_text.to_owned(),
        tenant: Some("acme".to_owned()),
        time_from: Some(1_020),
        time_to: Some(1_090),
        k: 10,
        ..Query::default()
    };
    let expected = engine.exact_search(&prism_query).unwrap();
    let query_vector = embedder.embed(query_text).unwrap();
    let mut maintained = SemanticTopK::new(
        SemanticQuery::new(
            "prism-exact-gate",
            "hash-embedder:1",
            query_vector,
            10,
            ScalarPredicate {
                tenant: Some("acme".to_owned()),
                time_from: Some(1_020),
                time_to: Some(1_090),
                ..ScalarPredicate::default()
            },
        )
        .unwrap(),
    );
    let deltas = events
        .iter()
        .map(|event| SemanticDelta {
            record: record(
                &event.event_id,
                "hash-embedder:1",
                embedder.embed(&event.body).unwrap(),
                columns(&event.tenant_id, event.event_time, event.cost, event.error),
            ),
            weight: 1,
        })
        .collect::<Vec<_>>();
    maintained.apply_epoch(deltas).unwrap();
    let actual = maintained.answer();
    assert_eq!(actual.len(), expected.len());
    for (actual, expected) in actual.iter().zip(&expected) {
        assert_eq!(actual.key, expected.event.event_id);
        assert_eq!(actual.score.to_bits(), expected.score.to_bits());
    }
}

#[test]
fn bridge_embedding_is_generation_pinned_scoped_and_epoch_atomic() {
    let embedder = HashEmbedder::with_version(16, "7");
    let plan = BridgeEmbeddingPlan::new(
        "events",
        BridgeColumns {
            key: 0,
            body: 1,
            tenant: 2,
            event_time: 3,
            cost_micros: 4,
            error: 5,
        },
        &embedder,
    )
    .unwrap();
    let good = Row::new(vec![
        Value::Str("event-1".to_owned()),
        Value::Str("database security incident".to_owned()),
        Value::Str("acme".to_owned()),
        Value::Int(42),
        Value::Int(1_250_000),
        Value::Bool(true),
    ]);
    let malformed = Row::new(vec![
        Value::Str("event-2".to_owned()),
        Value::Str("bad row".to_owned()),
        Value::Str("acme".to_owned()),
        Value::Str("not-a-time".to_owned()),
        Value::Int(0),
        Value::Bool(false),
    ]);
    let embedded = plan.embed_entries(&[(good.clone(), 1)], &embedder).unwrap();
    assert_eq!(embedded[0].record.space, "hash-embedder:7");
    assert_eq!(embedded[0].record.columns.cost, 1.25);
    assert!(plan
        .embed_entries(&[(good, 1), (malformed, 1)], &embedder)
        .is_err());

    let wrong_generation = HashEmbedder::with_version(16, "8");
    assert!(matches!(
        plan.embed_entries(&[], &wrong_generation),
        Err(SemanticError::SpaceMismatch { .. })
    ));
}

#[test]
fn semantic_groups_are_incremental_bounded_and_exactly_mergeable() {
    let plan = SemanticGroupPlan::new(
        "model:v1",
        vec![vec![1.0, 0.0], vec![0.0, 1.0]],
        ScalarPredicate {
            tenant: Some("acme".to_owned()),
            ..ScalarPredicate::default()
        },
    )
    .unwrap();
    let rows = [
        record(
            "a",
            "model:v1",
            vec![1.0, 0.1],
            columns("acme", 1, 10.0, false),
        ),
        record(
            "b",
            "model:v1",
            vec![0.9, 0.2],
            columns("acme", 2, 20.0, true),
        ),
        record(
            "c",
            "model:v1",
            vec![0.1, 1.0],
            columns("acme", 3, 30.0, false),
        ),
        record(
            "filtered",
            "model:v1",
            vec![0.0, 1.0],
            columns("other", 4, 100.0, true),
        ),
    ];
    let mut whole = SemanticGroups::new(plan.clone());
    whole
        .apply_epoch(
            rows.iter()
                .cloned()
                .map(|record| SemanticDelta { record, weight: 1 }),
        )
        .unwrap();
    let mut left = SemanticGroups::new(plan.clone());
    let mut right = SemanticGroups::new(plan);
    left.apply_epoch(
        rows[..2]
            .iter()
            .cloned()
            .map(|record| SemanticDelta { record, weight: 1 }),
    )
    .unwrap();
    right
        .apply_epoch(
            rows[2..]
                .iter()
                .cloned()
                .map(|record| SemanticDelta { record, weight: 1 }),
        )
        .unwrap();
    left.merge_disjoint(&right).unwrap();
    assert_eq!(left.summaries(), whole.summaries());
    assert_eq!(whole.summaries()[0].member_keys, vec!["a", "b"]);
    assert_eq!(whole.summaries()[0].avg_cost, 15.0);
    assert_eq!(whole.summaries()[0].error_rate, 0.5);

    whole
        .apply_epoch([SemanticDelta {
            record: rows[1].clone(),
            weight: -1,
        }])
        .unwrap();
    assert_eq!(whole.summaries()[0].member_keys, vec!["a"]);
}

#[test]
fn dual_generation_migration_never_compares_scores_and_fails_closed_on_parity() {
    let predicate = ScalarPredicate::default();
    let primary = SemanticTopK::new(
        SemanticQuery::new("migrate", "model:v1", vec![1.0, 0.0], 2, predicate.clone()).unwrap(),
    );
    let candidate = SemanticTopK::new(
        SemanticQuery::new("migrate", "model:v2", vec![0.0, 1.0], 2, predicate).unwrap(),
    );
    let mut migration = GenerationMigration::new(primary, candidate).unwrap();
    let old = [
        record(
            "a",
            "model:v1",
            vec![1.0, 0.0],
            columns("acme", 1, 1.0, false),
        ),
        record(
            "b",
            "model:v1",
            vec![0.0, 1.0],
            columns("acme", 1, 1.0, false),
        ),
    ];
    let new = [
        record(
            "a",
            "model:v2",
            vec![1.0, 0.0],
            columns("acme", 1, 1.0, false),
        ),
        record(
            "b",
            "model:v2",
            vec![0.0, 1.0],
            columns("acme", 1, 1.0, false),
        ),
    ];
    migration
        .apply_epoch(
            old.iter()
                .cloned()
                .map(|record| SemanticDelta { record, weight: 1 }),
            new.iter()
                .cloned()
                .map(|record| SemanticDelta { record, weight: 1 }),
        )
        .unwrap();
    assert_eq!(migration.answer()[0].key, "a");
    assert_eq!(migration.candidate_answer()[0].key, "b");
    assert!(matches!(
        migration.cut_over(0),
        Err(SemanticError::MigrationParity { .. })
    ));
    assert_eq!(migration.phase(), MigrationPhase::Mirroring);
    migration.cut_over(2).unwrap();
    assert_eq!(migration.phase(), MigrationPhase::CutOver);
    assert_eq!(migration.answer()[0].key, "b");
}

struct PrismOneShot<'a> {
    engine: &'a prism_engine::Engine,
    query: Query,
    space: String,
}

impl OneShotSemanticSource for PrismOneShot<'_> {
    fn exact(&self, _query: &SemanticQuery) -> Result<OneShotAnswer, String> {
        self.engine
            .search(&self.query)
            .map(|result| OneShotAnswer {
                space: self.space.clone(),
                hits: result
                    .hits
                    .into_iter()
                    .enumerate()
                    .map(|(index, hit)| SemanticHit {
                        key: hit.event.event_id,
                        rank: index + 1,
                        score: hit.score,
                    })
                    .collect(),
            })
            .map_err(|error| error.to_string())
    }
}

#[test]
fn cold_one_shot_route_uses_prisms_real_rerank_path_and_equals_hot_state() {
    let root = tempfile::tempdir().unwrap();
    let dim = 16;
    let engine = prism_engine::Engine::init(
        root.path(),
        prism_part::store::StoreConfig {
            format_version: prism_part::store::STORE_VERSION,
            dim,
            nlist: 4,
            pq_m: 4,
            seed: 42,
            kmeans_restarts: prism_quantizer::kmeans::KMEANS_RESTARTS,
            block_size: prism_part::format::DEFAULT_BLOCK_SIZE,
            partitions: Default::default(),
            promote: Vec::new(),
        },
    )
    .unwrap();
    let embedder = HashEmbedder::with_version(dim, "1");
    let events = (0..64)
        .map(|id| Event {
            event_id: format!("cold-{id:03}"),
            tenant_id: "acme".to_owned(),
            event_time: id,
            observed_time: 100,
            event_name: "event".to_owned(),
            cost: id as f64,
            error: id % 9 == 0,
            body: format!(
                "{} database security event {id}",
                if id % 5 == 0 { "urgent" } else { "routine" }
            ),
            trace_id: String::new(),
            span_id: String::new(),
            attributes: Attributes::new(),
            idempotency_key: None,
        })
        .collect::<Vec<_>>();
    engine.ingest(events.clone(), 100).unwrap();
    let text = "urgent database security event";
    let semantic_query = SemanticQuery::new(
        "cold-route",
        "hash-embedder:1",
        embedder.embed(text).unwrap(),
        8,
        ScalarPredicate::default(),
    )
    .unwrap();
    let mut standing = SemanticTopK::new(semantic_query);
    standing
        .apply_epoch(events.iter().map(|event| SemanticDelta {
            record: record(
                &event.event_id,
                "hash-embedder:1",
                embedder.embed(&event.body).unwrap(),
                columns(&event.tenant_id, event.event_time, event.cost, event.error),
            ),
            weight: 1,
        }))
        .unwrap();
    let prism_query = Query {
        text: text.to_owned(),
        tenant: Some("acme".to_owned()),
        k: 8,
        nprobe: 4,
        candidates: 64,
        rerank: 64,
        ..Query::default()
    };
    let source = PrismOneShot {
        engine: &engine,
        query: prism_query,
        space: "hash-embedder:1".to_owned(),
    };
    let hot = route_exact(&standing, usize::MAX, &source).unwrap();
    let cold = route_exact(&standing, 1, &source).unwrap();
    assert_eq!(hot.route, QueryRoute::Incremental);
    assert_eq!(cold.route, QueryRoute::ColdOneShot);
    assert_eq!(hot.hits, cold.hits);
}

struct FrozenSource {
    space: String,
    hits: Vec<SemanticHit>,
}

impl OneShotSemanticSource for FrozenSource {
    fn exact(&self, _query: &SemanticQuery) -> Result<OneShotAnswer, String> {
        Ok(OneShotAnswer {
            space: self.space.clone(),
            hits: self.hits.clone(),
        })
    }
}

#[test]
fn frozen_hybrid_corpus_covers_bridge_group_migration_and_route_contracts() {
    let rows = include_str!("fixtures/m2-golden.tsv")
        .lines()
        .filter(|line| !line.starts_with('#') && !line.is_empty())
        .map(|line| {
            let fields = line.split('|').collect::<Vec<_>>();
            Row::new(vec![
                Value::Str(fields[0].to_owned()),
                Value::Str(fields[1].to_owned()),
                Value::Str(fields[2].to_owned()),
                Value::Int(fields[3].parse().unwrap()),
                Value::Int(fields[4].parse().unwrap()),
                Value::Bool(fields[5].parse().unwrap()),
            ])
        })
        .map(|row| (row, 1))
        .collect::<Vec<_>>();
    let v1 = HashEmbedder::with_version(16, "golden-v1");
    let v2 = HashEmbedder::with_version(16, "golden-v2");
    let columns = BridgeColumns {
        key: 0,
        body: 1,
        tenant: 2,
        event_time: 3,
        cost_micros: 4,
        error: 5,
    };
    let old = BridgeEmbeddingPlan::new("events", columns.clone(), &v1)
        .unwrap()
        .embed_entries(&rows, &v1)
        .unwrap();
    let new = BridgeEmbeddingPlan::new("events", columns, &v2)
        .unwrap()
        .embed_entries(&rows, &v2)
        .unwrap();
    let predicate = ScalarPredicate {
        tenant: Some("acme".to_owned()),
        min_cost: Some(5.0),
        ..ScalarPredicate::default()
    };
    let query_text = "urgent database security";
    let primary = SemanticTopK::new(
        SemanticQuery::new(
            "golden",
            "hash-embedder:golden-v1",
            v1.embed(query_text).unwrap(),
            4,
            predicate.clone(),
        )
        .unwrap(),
    );
    let candidate = SemanticTopK::new(
        SemanticQuery::new(
            "golden",
            "hash-embedder:golden-v2",
            v2.embed(query_text).unwrap(),
            4,
            predicate.clone(),
        )
        .unwrap(),
    );
    let mut migration = GenerationMigration::new(primary, candidate).unwrap();
    migration.apply_epoch(old.clone(), new.clone()).unwrap();
    migration.cut_over(0).unwrap();

    let group_plan = SemanticGroupPlan::new(
        "hash-embedder:golden-v1",
        vec![
            v1.embed("database operations").unwrap(),
            v1.embed("identity security").unwrap(),
        ],
        predicate,
    )
    .unwrap();
    let mut groups = SemanticGroups::new(group_plan);
    groups.apply_epoch(old).unwrap();

    let candidate_hits = migration.answer();
    let source = FrozenSource {
        space: "hash-embedder:golden-v2".to_owned(),
        hits: candidate_hits.clone(),
    };
    let mut route_state = SemanticTopK::new(migration_query(&v2));
    route_state.apply_epoch(new).unwrap();
    let cold = route_exact(&route_state, 1, &source).unwrap();
    assert_eq!(cold.route, QueryRoute::ColdOneShot);
    assert_eq!(cold.hits, candidate_hits);

    let mut canonical = String::new();
    for hit in candidate_hits {
        writeln!(
            canonical,
            "hit|{}|{}|{:08x}",
            hit.rank,
            hit.key,
            hit.score.to_bits()
        )
        .unwrap();
    }
    for group in groups.summaries() {
        writeln!(
            canonical,
            "group|{}|{}|{:.6}|{:.6}|{}|{}",
            group.group_id,
            group.count,
            group.avg_cost,
            group.error_rate,
            group.exemplar_key,
            group.member_keys.join(",")
        )
        .unwrap();
    }
    assert_eq!(
        canonical,
        concat!(
            "hit|1|evt-001|3f1e246c\n",
            "hit|2|evt-008|3f1e246c\n",
            "hit|3|evt-006|3f13f298\n",
            "hit|4|evt-003|3f017245\n",
            "group|0|3|10.666667|0.666667|evt-008|evt-001,evt-004,evt-008\n",
            "group|1|2|9.000000|1.000000|evt-006|evt-003,evt-006\n",
        )
    );
}

fn migration_query(embedder: &HashEmbedder) -> SemanticQuery {
    SemanticQuery::new(
        "golden",
        "hash-embedder:golden-v2",
        embedder.embed("urgent database security").unwrap(),
        4,
        ScalarPredicate {
            tenant: Some("acme".to_owned()),
            min_cost: Some(5.0),
            ..ScalarPredicate::default()
        },
    )
    .unwrap()
}
