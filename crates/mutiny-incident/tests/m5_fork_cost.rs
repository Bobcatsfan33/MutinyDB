#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
//! **The fork-cost measurement (M5 deliverable): O(state), published in worst-honest form.**
//! MD-5's verdict shipped the fallback — a fork hydrates the child by cloning the parent's live
//! standing state — so the number that tells the truth about it is a cost that *grows with
//! state*, and this gate asserts exactly that, so the O(1) claim cannot quietly reattach itself.
//! `M5_FREEZE=1` writes the ledger artifact to `crates/mutiny-forks/evidence/m5-fork-cost.json`.

use loom_branch::{Loom, MAIN};
use loom_core::{BranchId, SessionId, TenantId};
use loom_policy::{Effect, Match, PolicyRule, PolicySet};
use mutiny_incident::host::{space, EMBED_DIM, EMBED_VERSION, GROUPS_QUERY, TOPK_QUERY, TOPK_TEXT};
use mutiny_semantic::{
    ScalarColumns, ScalarPredicate, SemanticDelta, SemanticGroupPlan, SemanticGroups,
    SemanticQuery, SemanticRecord, SemanticTopK,
};
use mutiny_trust::mount;
use prism_types::{Embedder, HashEmbedder};
use std::sync::Arc;
use std::time::Instant;

const SIZES: [usize; 3] = [1_000, 10_000, 50_000];
const FORKS_PER_SIZE: usize = 9;

fn allow_all() -> PolicySet {
    PolicySet::new(
        "m5-fork-cost",
        vec![PolicyRule {
            actor: Match::Any,
            label: Match::Any,
            purpose: Match::Any,
            action: Match::Any,
            effect: Effect::Allow,
        }],
    )
}

#[test]
fn fork_cost_is_o_state_measured_and_said_out_loud() {
    let embedder = HashEmbedder::with_version(EMBED_DIM, EMBED_VERSION);
    let db = Arc::new(Loom::in_memory(TenantId::new("acme")).expect("loom"));
    let (agent, operator) = mount(Arc::clone(&db), "acme", allow_all(), Vec::new());
    operator
        .install_standing(
            &BranchId::new(MAIN),
            SemanticTopK::new(
                SemanticQuery::new(
                    TOPK_QUERY,
                    space(),
                    embedder.embed(TOPK_TEXT).expect("query vector"),
                    3,
                    ScalarPredicate::default(),
                )
                .expect("query"),
            ),
        )
        .expect("install top-k");
    operator
        .install_groups(
            &BranchId::new(MAIN),
            GROUPS_QUERY,
            SemanticGroups::new(
                SemanticGroupPlan::new(
                    space(),
                    vec![
                        embedder.embed("urgent security incident").expect("anchor"),
                        embedder.embed("routine operations").expect("anchor"),
                    ],
                    ScalarPredicate::default(),
                )
                .expect("plan"),
            ),
        )
        .expect("install groups");

    let (session, token) = agent
        .open_session_named(SessionId::new("measure-parent"))
        .expect("session");
    let parent = session.branch.clone();

    let mut measured: Vec<(usize, usize, u128)> = Vec::new();
    let mut populated = 0usize;
    for (level, size) in SIZES.into_iter().enumerate() {
        // Grow the parent's standing state to `size` rows, one epoch per growth step.
        let deltas: Vec<SemanticDelta> = (populated..size)
            .map(|index| {
                let key = format!("row-{index:06}");
                let body = if index % 3 == 0 {
                    format!("urgent security sample {index}")
                } else {
                    format!("routine operations sample {index}")
                };
                SemanticDelta {
                    record: SemanticRecord::new(
                        key,
                        space(),
                        embedder.embed(&body).expect("vector"),
                        ScalarColumns {
                            tenant: "acme".to_owned(),
                            event_time: index as i64,
                            cost: 1.0,
                            error: false,
                        },
                    )
                    .expect("record"),
                    weight: 1,
                }
            })
            .collect();
        agent
            .apply_semantic_epoch(&token, &parent, TOPK_QUERY, deltas.clone())
            .expect("top-k grows");
        agent
            .apply_group_epoch(&token, &parent, GROUPS_QUERY, deltas)
            .expect("groups grow");
        populated = size;

        let state_bytes = operator.branch_state_bytes(&parent).expect("accounting");
        let mut samples = Vec::with_capacity(FORKS_PER_SIZE);
        for fork in 0..FORKS_PER_SIZE {
            let child = format!("measure-{level}-{fork}");
            let started = Instant::now();
            let (branch, _) = agent.branch(&token, &parent, &child).expect("fork");
            samples.push(started.elapsed().as_nanos());
            // Tear the measured fork down again so the next sample clones the same parent.
            operator.rewind_branch(&branch).expect("teardown");
        }
        samples.sort_unstable();
        measured.push((size, state_bytes, samples[samples.len() / 2]));
        println!(
            "fork cost at {size} rows: state={state_bytes} bytes, median={} ns",
            samples[samples.len() / 2]
        );
    }

    // The gate's honest tooth against the O(1) claim: the measured cost must grow with state.
    let smallest = measured[0].2;
    let largest = measured[measured.len() - 1].2;
    assert!(
        largest >= smallest.saturating_mul(3),
        "the fallback fork is O(state) by construction; a flat measurement here would mean the \
         hydration is no longer copying, and the MD-5 economics statement would be stale: \
         {measured:?}"
    );

    if std::env::var("M5_FREEZE").is_ok() {
        let rows = measured
            .iter()
            .map(|(size, bytes, ns)| {
                format!(
                    "    {{ \"rows\": {size}, \"state_bytes\": {bytes}, \"fork_median_ns\": \
                     {ns}, \"ns_per_row\": {:.1} }}",
                    *ns as f64 / *size as f64
                )
            })
            .collect::<Vec<_>>()
            .join(",\n");
        let rows = format!("{rows}\n");
        let worst = measured
            .iter()
            .map(|(size, _, ns)| *ns as f64 / *size as f64)
            .fold(0.0f64, f64::max);
        let ledger = format!(
            "{{\n  \"measurement\": \"M5 fork cost — MD-5 Option B hydration clone (top-k + \
             groups, {EMBED_DIM}-dim vectors)\",\n  \"claim\": \"fork cost is O(state); O(1) fork \
             of live answers is post-v1 (MD-5)\",\n  \"samples\": [\n{rows}  ],\n  \
             \"worst_honest_ns_per_row\": {worst:.1},\n  \"forks_per_size\": {FORKS_PER_SIZE},\n  \
             \"machine\": \"darwin-arm64 dev host, unloaded, cargo test (debug)\"\n}}\n"
        );
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../mutiny-forks/evidence")
            .join("m5-fork-cost.json");
        std::fs::write(path, ledger).expect("ledger written");
    }
}
