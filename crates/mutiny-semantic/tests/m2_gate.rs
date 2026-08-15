#![allow(clippy::panic, clippy::unwrap_used)]

use mutiny_semantic::{
    ScalarColumns, ScalarPredicate, SemanticDelta, SemanticError, SemanticQuery, SemanticRecord,
    SemanticTopK,
};
use prism_types::vector::dot;
use prism_types::{Attributes, Embedder, Event, HashEmbedder, Query};
use std::collections::BTreeMap;

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
