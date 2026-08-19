#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
//! **The M5 spike: copy-on-write operator state on substrate pages.** MD-5 committed the go/no-go
//! criteria before this ran; this file is the attempt, kept permanently as the evidence behind the
//! verdict and as the re-runnable baseline for the post-v1 CoW path.
//!
//! Layout under test — a live standing top-k plus a scalar rollup, hosted on `loom-branch::Tree`
//! (the imported CoW B-tree over substrate pages) rather than a second, invented page B-tree:
//!
//! ```text
//!   r/<row-key>                  -> (score bits, cost)      the row store
//!   k/<score-asc-bytes><row-key> -> ()                      the rank index (top-k = last k)
//!   a/cost                       -> i64                     the rollup accumulator
//! ```
//!
//! Fork = a substrate manifest reference: the child *is* `pager.fork(&head)`, sharing every page
//! copy-on-write. Run with `--nocapture` to see the measurements; `M5_FREEZE=1` writes the
//! evidence ledger to `evidence/m5-spike.json`.

use loom_branch::Tree;
use loom_core::{Record, Value};
use std::collections::BTreeMap;
use std::time::Instant;
use substrate_pager::{Manifest, ManifestBody, ManifestId, PageStore, Pager, StoreConfig};

const TOP_K: usize = 5;
const DIVERGENT_UPDATES: usize = 200;

/// IEEE-754 order-preserving ascending encoding for an f32 score.
fn score_asc_bytes(score: f32) -> [u8; 4] {
    let bits = score.to_bits();
    let asc = if bits & 0x8000_0000 != 0 {
        !bits
    } else {
        bits | 0x8000_0000
    };
    asc.to_be_bytes()
}

fn row_key(name: &str) -> Vec<u8> {
    let mut key = b"r/".to_vec();
    key.extend_from_slice(name.as_bytes());
    key
}

fn rank_key(score: f32, name: &str) -> Vec<u8> {
    let mut key = b"k/".to_vec();
    key.extend_from_slice(&score_asc_bytes(score));
    key.extend_from_slice(name.as_bytes());
    key
}

struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0
    }

    fn score(&mut self) -> f32 {
        ((self.next() >> 40) as f32) / (1u64 << 24) as f32
    }

    fn cost(&mut self) -> i64 {
        (self.next() % 1_000_000) as i64
    }
}

/// One page-backed standing state: a branch head plus the pager it lives on.
struct PageState<'a> {
    pager: &'a Pager,
    head: ManifestId,
}

impl<'a> PageState<'a> {
    fn new(pager: &'a Pager) -> PageState<'a> {
        PageState {
            pager,
            head: pager.head(),
        }
    }

    /// The K-3 fork: a manifest reference. Returns the child and how many pages the commit that
    /// created this fork wrote (none — the fork itself commits nothing).
    fn fork(&self) -> PageState<'a> {
        PageState {
            pager: self.pager,
            head: self.head,
        }
    }

    /// Apply one batch of row upserts in one commit; returns pages written by the commit.
    fn apply(&mut self, rows: &[(String, f32, i64)]) -> usize {
        let store = self.pager.fork(&self.head).expect("fork for write");
        let mut tree = Tree::open(&*store).expect("tree opens");
        for (name, score, cost) in rows {
            // Retract any previous version of the row from the rank index first.
            if let Some(Record::Value(Value::Blob(bytes))) =
                tree.get(&row_key(name)).expect("row read")
            {
                let old_score = f32::from_bits(u32::from_be_bytes(
                    bytes[0..4].try_into().expect("stored score"),
                ));
                let old_cost =
                    i64::from_be_bytes(bytes[4..12].try_into().expect("stored cost"));
                tree.remove(&rank_key(old_score, name)).expect("rank retract");
                add_to_rollup(&mut tree, -old_cost);
            }
            let mut blob = score.to_bits().to_be_bytes().to_vec();
            blob.extend_from_slice(&cost.to_be_bytes());
            tree.insert(row_key(name), Record::Value(Value::Blob(blob)))
                .expect("row insert");
            tree.insert(rank_key(*score, name), Record::Value(Value::Blob(Vec::new())))
                .expect("rank insert");
            add_to_rollup(&mut tree, *cost);
        }
        let mut txn = store.begin().expect("txn");
        tree.flush(&mut txn).expect("tree flush");
        self.head = store.commit(txn).expect("commit");
        let manifest: Manifest = self.pager.manifest(&self.head).expect("manifest");
        match manifest.body {
            ManifestBody::Overlay { changes, .. } => changes.len(),
            ManifestBody::Flat(pages) => pages.len(),
        }
    }

    /// The standing top-k answer, highest score first, rendered deterministically.
    fn top_k(&self) -> String {
        let store = self.pager.fork(&self.head).expect("fork for read");
        let mut tree = Tree::open(&*store).expect("tree opens");
        let ranked = tree.scan_prefix(b"k/").expect("rank scan");
        let mut out = String::new();
        for (key, _) in ranked.iter().rev().take(TOP_K) {
            let name = String::from_utf8_lossy(&key[6..]);
            out.push_str(&name);
            out.push('\n');
        }
        out
    }

    fn rollup(&self) -> i64 {
        let store = self.pager.fork(&self.head).expect("fork for read");
        let mut tree = Tree::open(&*store).expect("tree opens");
        match tree.get(b"a/cost").expect("rollup read") {
            Some(Record::Value(Value::Blob(bytes))) => {
                i64::from_be_bytes(bytes[..8].try_into().expect("rollup bytes"))
            }
            _ => 0,
        }
    }
}

fn add_to_rollup(tree: &mut Tree<'_>, delta: i64) {
    let current = match tree.get(b"a/cost").expect("rollup read") {
        Some(Record::Value(Value::Blob(bytes))) => {
            i64::from_be_bytes(bytes[..8].try_into().expect("rollup bytes"))
        }
        _ => 0,
    };
    tree.insert(
        b"a/cost".to_vec(),
        Record::Value(Value::Blob((current + delta).to_be_bytes().to_vec())),
    )
    .expect("rollup write");
}

/// The independent model: plain in-memory maps, no shared code with the page layout.
#[derive(Clone, Default)]
struct Model {
    rows: BTreeMap<String, (f32, i64)>,
}

impl Model {
    fn apply(&mut self, rows: &[(String, f32, i64)]) {
        for (name, score, cost) in rows {
            self.rows.insert(name.clone(), (*score, *cost));
        }
    }

    fn top_k(&self) -> String {
        let mut ranked: Vec<(&String, f32)> = self
            .rows
            .iter()
            .map(|(name, (score, _))| (name, *score))
            .collect();
        // Highest score first; ties by the rank index's byte order (key ascending within a score,
        // reversed by the descending read).
        ranked.sort_by(|a, b| {
            score_asc_bytes(b.1)
                .cmp(&score_asc_bytes(a.1))
                .then_with(|| b.0.cmp(a.0))
        });
        let mut out = String::new();
        for (name, _) in ranked.into_iter().take(TOP_K) {
            out.push_str(name);
            out.push('\n');
        }
        out
    }

    fn rollup(&self) -> i64 {
        self.rows.values().map(|(_, cost)| cost).sum()
    }
}

fn build_state<'a>(pager: &'a Pager, entries: usize, seed: u64) -> (PageState<'a>, Model) {
    let mut state = PageState::new(pager);
    let mut model = Model::default();
    let mut lcg = Lcg(seed);
    let mut batch = Vec::new();
    for index in 0..entries {
        batch.push((format!("row-{index:06}"), lcg.score(), lcg.cost()));
        if batch.len() == 512 {
            state.apply(&batch);
            model.apply(&batch);
            batch.clear();
        }
    }
    if !batch.is_empty() {
        state.apply(&batch);
        model.apply(&batch);
    }
    (state, model)
}

/// Median fork-and-open latency in nanoseconds, over `iterations` forks.
fn fork_latency_ns(state: &PageState<'_>, iterations: usize) -> u128 {
    let mut samples = Vec::with_capacity(iterations);
    for _ in 0..iterations {
        let started = Instant::now();
        let child = state.fork();
        let store = child.pager.fork(&child.head).expect("fork");
        let tree = Tree::open(&*store).expect("open");
        let _ = tree.len();
        samples.push(started.elapsed().as_nanos());
    }
    samples.sort_unstable();
    samples[samples.len() / 2]
}

#[test]
fn the_spike_measures_the_cow_layout_against_md5_criteria() {
    let pager = Pager::in_memory(StoreConfig::default()).expect("in-memory substrate");

    // ---- C1: fork latency must be flat in state size -----------------------------------------
    let (small, _) = build_state(&pager, 1_000, 1);
    let small_fork_ns = fork_latency_ns(&small, 101);

    let (medium, _) = build_state(&pager, 10_000, 2);
    let medium_fork_ns = fork_latency_ns(&medium, 101);

    let (mut large, mut large_model) = build_state(&pager, 50_000, 3);
    let large_fork_ns = fork_latency_ns(&large, 101);

    println!("C1 fork+open median ns: 1k={small_fork_ns} 10k={medium_fork_ns} 50k={large_fork_ns}");
    let c1_pass = large_fork_ns <= small_fork_ns.saturating_mul(2);

    // ---- C2: pages written per single-row update at 50k entries ------------------------------
    let mut lcg = Lcg(4);
    let mut pages_per_update = Vec::new();
    for index in 0..DIVERGENT_UPDATES {
        let update = vec![(format!("upd-{index:06}"), lcg.score(), lcg.cost())];
        pages_per_update.push(large.apply(&update));
        large_model.apply(&update);
    }
    pages_per_update.sort_unstable();
    let c2_median = pages_per_update[pages_per_update.len() / 2];
    let c2_max = *pages_per_update.last().expect("updates ran");
    println!("C2 pages/update at 50k: median={c2_median} max={c2_max}");
    // MD-5's C2 wording fixed a bound without naming the statistic. Both readings are recorded
    // and neither is silently chosen: the verdict quotes median AND worst case.
    let c2_median_pass = c2_median <= 16;
    let c2_worst_pass = c2_max <= 16;

    // ---- C3: divergent forks answer correctly; the idle side is byte-identical --------------
    let parent_before = (large.top_k(), large.rollup());
    let mut child = large.fork();
    let mut child_model = large_model.clone();

    // Child-only writes first: the parent must not move.
    let mut child_lcg = Lcg(5);
    for index in 0..DIVERGENT_UPDATES {
        let update = vec![(format!("child-{index:06}"), child_lcg.score(), child_lcg.cost())];
        child.apply(&update);
        child_model.apply(&update);
    }
    let parent_idle_ok =
        large.top_k() == parent_before.0 && large.rollup() == parent_before.1;

    // Then interleaved divergence on both sides, checked against the models at every step.
    let mut parent_lcg = Lcg(6);
    let mut c3_pass = parent_idle_ok;
    for index in 0..DIVERGENT_UPDATES {
        let parent_update =
            vec![(format!("parent-{index:06}"), parent_lcg.score(), parent_lcg.cost())];
        large.apply(&parent_update);
        large_model.apply(&parent_update);
        let child_update =
            vec![(format!("late-{index:06}"), child_lcg.score(), child_lcg.cost())];
        child.apply(&child_update);
        child_model.apply(&child_update);

        c3_pass &= large.top_k() == large_model.top_k()
            && large.rollup() == large_model.rollup()
            && child.top_k() == child_model.top_k()
            && child.rollup() == child_model.rollup();
    }
    println!("C3 divergent correctness: idle-side-identical={parent_idle_ok} all-steps={c3_pass}");

    // ---- The measurements stand whatever the verdict; the criteria assert what MD-5 fixed ----
    assert!(
        c3_pass,
        "C3 is disqualifying regardless of the others: the page layout answered wrongly"
    );
    println!(
        "MD-5 criteria: C1 {} · C2 median {} / worst-case {} · C3 {} · C4 assessed in writing in MD-5",
        verdict(c1_pass),
        verdict(c2_median_pass),
        verdict(c2_worst_pass),
        verdict(c3_pass),
    );

    if std::env::var("M5_FREEZE").is_ok() {
        let ledger = format!(
            "{{\n  \"spike\": \"M5 copy-on-write operator state on substrate pages\",\n  \
             \"layout\": \"loom-branch Tree over substrate_pager::Pager (in-memory), row store + rank index + rollup\",\n  \
             \"fork_open_median_ns\": {{ \"entries_1k\": {small_fork_ns}, \"entries_10k\": {medium_fork_ns}, \"entries_50k\": {large_fork_ns} }},\n  \
             \"pages_per_single_row_update_at_50k\": {{ \"median\": {c2_median}, \"max\": {c2_max}, \"budget\": 16 }},\n  \
             \"divergent_updates_per_side\": {DIVERGENT_UPDATES},\n  \
             \"criteria\": {{ \"C1\": \"{}\", \"C2_median\": \"{}\", \"C2_worst_case\": \"{}\", \"C3\": \"{}\", \"C4\": \"assessed in MD-5\" }},\n  \
             \"machine\": \"darwin-arm64 dev host, unloaded, cargo test (debug)\"\n}}\n",
            verdict(c1_pass),
            verdict(c2_median_pass),
            verdict(c2_worst_pass),
            verdict(c3_pass),
        );
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("evidence")
            .join("m5-spike.json");
        std::fs::create_dir_all(path.parent().expect("evidence dir")).expect("mkdir");
        std::fs::write(path, ledger).expect("ledger written");
    }
}

fn verdict(pass: bool) -> &'static str {
    if pass {
        "PASS"
    } else {
        "FAIL"
    }
}
