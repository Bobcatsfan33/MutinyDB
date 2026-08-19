#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
//! **The M5 gate: forked standing state, on MD-5's fallback path — stated honestly.** A session
//! branch carries its standing answers: a durable fork hydrates the child from the parent
//! (O(state), measured, never claimed O(1)); both branches maintain independently from their own
//! commits; merge re-runs policy per Loom's law and cannot double-apply shared history; rewind
//! returns the state accounting to baseline; recovery replays the commit history through the
//! lineage to byte-identical answers; and taint composes with forks — one call heals the parent
//! AND the fork's inherited standing state. `docs/M5-FORKS.md` is the contract.

use loom_core::{BranchId, SourceRef, TrustClass};
use mutiny_forks::{lineage_source, ForkEvent, ForkKind, FORKS_TABLE};
use mutiny_incident::corpus::{self, Corpus, CorpusCommit, CorpusOp, TELEMETRY};
use mutiny_incident::host::{space, Host, HostPaths, GROUPS_QUERY};
use mutiny_semantic::{ScalarColumns, SemanticDelta, SemanticRecord};
use prism_types::Embedder;
use schweep_zset::{Row, Value};
use std::collections::{BTreeMap, BTreeSet};

const FORKED_CORPUS: &str = include_str!("../fixtures/incident-corpus-forked.tsv");
const EXPECTED_POISONED: &str = include_str!("../fixtures/expected-forked-poisoned-answers.txt");
const EXPECTED_HEALED: &str = include_str!("../fixtures/expected-forked-healed-answers.txt");
const EXPECTED_REPORT: &str = include_str!("../fixtures/expected-forked-recall-report.txt");

const PIN_CORPUS: &str = "b44ac22c55ccf7b8b0fb2a48b65bb7860f2e19535cb3170f2a6ae62fef442011";
const PIN_POISONED: &str = "2c754a4890f296c8df812f0da21c4cd6733151f5359523236c91c3caf6e806ea";
const PIN_HEALED: &str = "65d86748baaae25f75eb6e43049975ad306204a916c4e2fc1ec8462e298bed49";
const PIN_REPORT: &str = "0b6c55a5904c2e3ccacfa07aab3b740db2291606aeb6e9757882256d45188e0b";

fn poison() -> SourceRef {
    SourceRef::new("web", "scraped-page-77")
}

struct World {
    _dirs: (tempfile::TempDir, tempfile::TempDir),
    paths: HostPaths,
    corpus: Corpus,
}

fn world() -> World {
    let corpus = corpus::parse(FORKED_CORPUS).expect("the forked corpus parses");
    let storage = tempfile::tempdir().expect("storage dir");
    let compute = tempfile::tempdir().expect("compute dir");
    let paths = HostPaths {
        storage: storage.path().to_path_buf(),
        compute: compute.path().to_path_buf(),
    };
    World {
        _dirs: (storage, compute),
        paths,
        corpus,
    }
}

/// The independent oracle: replay the same lifecycle — forks, merge, rewind included — but never
/// ingest what the corpus declares downstream of the sources. Built from the corpus's own
/// declarations, never from taint code.
fn oracle_answers(corpus: &Corpus, sources: &[&SourceRef]) -> String {
    let mut omit = BTreeSet::new();
    for source in sources {
        let key = format!("{}:{}", source.system, source.record_id);
        omit.extend(
            corpus
                .downstream
                .get(&key)
                .unwrap_or_else(|| panic!("corpus declares downstream of {key}"))
                .iter()
                .cloned(),
        );
    }
    let mut oracle = corpus.clone();
    oracle.retain_commits(|commit| {
        !omit.contains(&(
            commit.branch.clone(),
            commit.table.clone(),
            commit.key.clone(),
        ))
    });
    oracle.actions.clear();
    let storage = tempfile::tempdir().expect("oracle storage");
    let compute = tempfile::tempdir().expect("oracle compute");
    let paths = HostPaths {
        storage: storage.path().to_path_buf(),
        compute: compute.path().to_path_buf(),
    };
    let host = Host::build(&paths, &oracle).expect("oracle world builds");
    host.standing_answers().expect("oracle answers render")
}

/// Every member key each branch's grouping circuit currently holds — the full standing-state
/// membership, per branch. The isolation oracle's read side.
fn circuit_members(host: &Host, branch: &str) -> BTreeSet<String> {
    let token = &host.tokens[branch];
    host.agent
        .group_summaries(token, &BranchId::new(branch), GROUPS_QUERY)
        .expect("group summaries")
        .into_iter()
        .flat_map(|group| group.member_keys)
        .collect()
}

fn telemetry_commit(session: &str, branch: &str, key: &str, body: &str, cost: i64) -> CorpusCommit {
    CorpusCommit {
        session: session.to_owned(),
        branch: branch.to_owned(),
        actor: session.to_owned(),
        table: TELEMETRY.to_owned(),
        sources: vec![SourceRef::new("erp", "ledger-9")],
        key: key.to_owned(),
        row: Row::new(vec![
            Value::Str(key.to_owned()),
            Value::Str(branch.to_owned()),
            Value::Str(body.to_owned()),
            Value::Int(cost),
            Value::Bool(false),
            Value::Int(2000),
        ]),
    }
}

// ---- the gate ---------------------------------------------------------------------------------

#[test]
fn the_forked_corpus_and_its_expectations_are_frozen() {
    let pins = [
        ("incident-corpus-forked.tsv", FORKED_CORPUS, PIN_CORPUS),
        (
            "expected-forked-poisoned-answers.txt",
            EXPECTED_POISONED,
            PIN_POISONED,
        ),
        (
            "expected-forked-healed-answers.txt",
            EXPECTED_HEALED,
            PIN_HEALED,
        ),
        (
            "expected-forked-recall-report.txt",
            EXPECTED_REPORT,
            PIN_REPORT,
        ),
    ];
    let mut drifted = Vec::new();
    for (name, content, pin) in pins {
        let actual = blake3::hash(content.as_bytes()).to_hex().to_string();
        if actual != pin {
            drifted.push(format!("{name}: pinned {pin}, actual {actual}"));
        }
    }
    assert!(
        drifted.is_empty(),
        "fixtures drifted:\n{}",
        drifted.join("\n")
    );
}

#[test]
fn a_fork_inherits_a_merge_lands_once_and_a_rewind_leaves_only_audit() {
    let world = world();
    let host = Host::build(&world.paths, &world.corpus).expect("forked world builds");
    let answers = host.standing_answers().expect("answers render");
    assert_eq!(answers, EXPECTED_POISONED);

    // Inheritance is real: the fork's standing answers contain the parent's pre-fork rows.
    let inherited = circuit_members(&host, "hyp-a");
    assert!(inherited.contains("evt-a1") && inherited.contains("evt-a2"));
    // Divergence is real: the parent's post-fork row is not in the fork's state.
    assert!(!inherited.contains("evt-a3"));

    // The merge landed exactly once, with the merged row on the target.
    assert!(circuit_members(&host, "sess-a").contains("evt-h1"));
    assert!(answers.contains("(\"evt-h1\", \"sess-a\""));

    // The rewound branch carries no standing state, but its history is audit, not ash.
    assert!(!host.branches().contains(&"hyp-b".to_owned()));
    assert!(answers.contains("(\"evt-x1\", \"hyp-b\""));

    let lineage = host.lineage().expect("lineage");
    assert_eq!(lineage.active_descendants("sess-a"), vec!["hyp-a"]);
    assert!(!lineage.is_active("hyp-b"));

    // The fork economics are O(state) and are recorded as such: every hydration cloned the
    // parent's live bytes. Nothing here claims O(1); MD-5 says why.
    assert_eq!(host.fork_samples.len(), 2);
    assert!(host.fork_samples.iter().all(|(bytes, _)| *bytes > 0));
}

#[test]
fn rewind_returns_the_state_accounting_to_baseline() {
    let world = world();
    let mut host = Host::open(&world.paths, &world.corpus).expect("host opens");
    // Everything up to (and including) the first fork; the second fork is the one we measure.
    let fork_b = world
        .corpus
        .ops
        .iter()
        .position(|op| matches!(op, CorpusOp::Fork { child, .. } if child == "hyp-b"))
        .expect("the corpus forks hyp-b");
    for op in &world.corpus.ops[..fork_b] {
        host.apply_op(op).expect("op applies");
    }
    let baseline = host.operator.mounted_state_bytes().expect("accounting");

    host.apply_op(&world.corpus.ops[fork_b])
        .expect("fork hyp-b");
    let after_fork = host.operator.mounted_state_bytes().expect("accounting");
    assert!(
        after_fork > baseline,
        "the hydration clone must cost real bytes — that is the honest O(state) economics"
    );

    let freed = host
        .rewind_durable("sess-a", "sess-a", "hyp-b")
        .expect("rewind");
    assert_eq!(freed, after_fork - baseline);
    let after_rewind = host.operator.mounted_state_bytes().expect("accounting");
    assert_eq!(
        after_rewind, baseline,
        "rewind must return the mount's accounting exactly to its pre-fork baseline (C6 \
         teardown, composed)"
    );
}

#[test]
fn merge_re_runs_policy_all_or_nothing_and_remerge_is_a_no_op() {
    let world = world();
    let mut host = Host::open(&world.paths, &world.corpus).expect("host opens");
    let merge_at = world
        .corpus
        .ops
        .iter()
        .position(|op| matches!(op, CorpusOp::Merge { .. }))
        .expect("the corpus merges");
    for op in &world.corpus.ops[..merge_at] {
        host.apply_op(op).expect("op applies");
    }
    let before = host.standing_answers().expect("answers render");

    // Policy is re-evaluated at merge time, and a denied merge writes NOTHING.
    let refused = host.merge_durable(
        "sess-a",
        "sess-a",
        "hyp-a",
        "sess-a",
        TrustClass::Untrusted,
        None,
    );
    assert!(
        matches!(
            refused,
            Err(mutiny_incident::host::HostError::MergeRefused(_))
        ),
        "an Untrusted merge must be refused by the policy re-run"
    );
    assert_eq!(
        host.standing_answers().expect("answers render"),
        before,
        "all-or-nothing: the refused merge must not have written anything"
    );

    let merged = host
        .merge_durable(
            "sess-a",
            "sess-a",
            "hyp-a",
            "sess-a",
            TrustClass::VerifiedSystem,
            None,
        )
        .expect("the merge lands");
    assert_eq!(merged, 1);

    // Merging again with no new work is a no-op — Loom's merged-from memory, composed as the
    // durable marker. This is the +6-not-+3 class, dead.
    let again = host
        .merge_durable(
            "sess-a",
            "sess-a",
            "hyp-a",
            "sess-a",
            TrustClass::VerifiedSystem,
            None,
        )
        .expect("the re-merge runs");
    assert_eq!(again, 0, "a re-merge with no new divergence merges nothing");
}

/// **The isolation oracle, over circuit state.** Randomized fork/write sequences; after every
/// step, every branch's grouping circuit holds exactly the model's row set for that branch —
/// its own writes plus what it inherited at fork, never a sibling's post-fork writes.
#[test]
fn isolation_oracle_randomized_forks_and_writes_over_circuit_state() {
    for seed in 1..=8u64 {
        let corpus = Corpus {
            sessions: vec!["root-a".to_owned(), "root-b".to_owned()],
            ..Corpus::default()
        };
        let storage = tempfile::tempdir().expect("storage");
        let compute = tempfile::tempdir().expect("compute");
        let paths = HostPaths {
            storage: storage.path().to_path_buf(),
            compute: compute.path().to_path_buf(),
        };
        let mut host = Host::open(&paths, &corpus).expect("host opens");
        let mut model: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        model.insert("root-a".to_owned(), BTreeSet::new());
        model.insert("root-b".to_owned(), BTreeSet::new());
        let mut state = seed;
        let mut next = move || {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            state
        };

        for step in 0..24u64 {
            let branches: Vec<String> = model.keys().cloned().collect();
            let roll = next();
            if roll % 4 == 0 && branches.len() < 5 {
                let parent = branches[(roll >> 8) as usize % branches.len()].clone();
                let child = format!("fork-{seed}-{step}");
                host.fork_durable(&parent, &parent, &parent, &child)
                    .expect("fork");
                let inherited = model[&parent].clone();
                model.insert(child, inherited);
            } else {
                let branch = branches[(roll >> 8) as usize % branches.len()].clone();
                let key = format!("evt-{seed}-{step}");
                let body = if roll % 2 == 0 {
                    format!("urgent incident sample {key}")
                } else {
                    format!("routine operations sample {key}")
                };
                host.ingest_commit(&telemetry_commit(&branch, &branch, &key, &body, 1_000_000))
                    .expect("write");
                model.get_mut(&branch).expect("branch in model").insert(key);
            }

            for (branch, expected) in &model {
                let actual = circuit_members(&host, branch);
                assert_eq!(
                    &actual, expected,
                    "seed {seed}, step {step}: branch {branch} circuit state diverged from the \
                     model — its standing answer read someone else's writes or lost its own"
                );
            }
        }
    }
}

#[test]
fn forked_state_survives_restart_to_byte_identical_answers() {
    let world = world();
    let host = Host::build(&world.paths, &world.corpus).expect("forked world builds");
    let live = host.standing_answers().expect("answers render");
    drop(host);
    let mut reopened = Host::reopen(&world.paths, &world.corpus).expect("host reopens");
    assert_eq!(
        reopened.standing_answers().expect("answers render"),
        live,
        "recovery must rebuild every branch's circuits — inheritance, merge, and rewind \
         included — to byte-identical answers"
    );

    // And again across the taint: the healed world also survives restart.
    reopened.taint(&poison()).expect("taint runs");
    let healed = reopened.standing_answers().expect("answers render");
    assert_eq!(healed, EXPECTED_HEALED);
    drop(reopened);
    let after = Host::reopen(&world.paths, &world.corpus).expect("host reopens again");
    assert_eq!(after.standing_answers().expect("answers render"), healed);
}

#[test]
fn crash_mid_fork_is_never_a_hybrid() {
    let crash_world = world();
    // The never-crashed twin.
    let twin_world = world();
    let twin = Host::build(&twin_world.paths, &twin_world.corpus).expect("twin builds");
    let expected = twin.standing_answers().expect("twin answers");

    let mut host = Host::open(&crash_world.paths, &crash_world.corpus).expect("host opens");
    let fork_at = crash_world
        .corpus
        .ops
        .iter()
        .position(|op| matches!(op, CorpusOp::Fork { child, .. } if child == "hyp-a"))
        .expect("the corpus forks hyp-a");
    for op in &crash_world.corpus.ops[..fork_at] {
        host.apply_op(op).expect("op applies");
    }
    // The fork's durable half only: the record commits, then the process dies before hydration.
    let event = ForkEvent {
        child: "hyp-a".to_owned(),
        parent: "sess-a".to_owned(),
        at_epoch: host.commit_seq + 1,
        kind: ForkKind::Fork,
    };
    host.ingest_commit(&CorpusCommit {
        session: "sess-a".to_owned(),
        branch: "sess-a".to_owned(),
        actor: "sess-a".to_owned(),
        table: FORKS_TABLE.to_owned(),
        sources: vec![lineage_source("sess-a")],
        key: "hyp-a".to_owned(),
        row: event.to_row(),
    })
    .expect("the fork record lands");
    drop(host);

    // Recovery replays the record and hydrates; the resumed fork op is a no-op; the rest of the
    // incident completes to the never-crashed twin.
    let mut recovered =
        Host::reopen(&crash_world.paths, &crash_world.corpus).expect("host reopens");
    assert!(
        recovered.tokens.contains_key("hyp-a"),
        "replay must hydrate the recorded fork — a record without state would be a hybrid"
    );
    for op in &crash_world.corpus.ops[fork_at..] {
        recovered.apply_op(op).expect("op resumes");
    }
    for action in &crash_world.corpus.actions {
        recovered.execute_action(action).expect("action");
    }
    assert_eq!(recovered.standing_answers().expect("answers"), expected);
}

#[test]
fn crash_mid_merge_completes_exactly_once() {
    // Both worlds get one extra divergence commit so the merge has two rows and a real midpoint.
    let extra = telemetry_commit(
        "sess-a",
        "hyp-a",
        "evt-h9",
        "routine follow up filed",
        1_000_000,
    );

    let control_world = world();
    let mut control = Host::open(&control_world.paths, &control_world.corpus).expect("control");
    let merge_at = control_world
        .corpus
        .ops
        .iter()
        .position(|op| matches!(op, CorpusOp::Merge { .. }))
        .expect("the corpus merges");
    for op in &control_world.corpus.ops[..merge_at] {
        control.apply_op(op).expect("op applies");
    }
    control.ingest_commit(&extra).expect("extra divergence");
    let merged = control
        .merge_durable(
            "sess-a",
            "sess-a",
            "hyp-a",
            "sess-a",
            TrustClass::VerifiedSystem,
            None,
        )
        .expect("control merge");
    assert_eq!(merged, 2);
    for op in &control_world.corpus.ops[merge_at + 1..] {
        control.apply_op(op).expect("op applies");
    }
    for action in &control_world.corpus.actions {
        control.execute_action(action).expect("action");
    }
    let expected = control.standing_answers().expect("control answers");

    // The crashing world: one of two rows lands, then the process dies.
    let crash_world = world();
    let mut host = Host::open(&crash_world.paths, &crash_world.corpus).expect("host opens");
    for op in &crash_world.corpus.ops[..merge_at] {
        host.apply_op(op).expect("op applies");
    }
    host.ingest_commit(&extra).expect("extra divergence");
    let interrupted = host.merge_durable(
        "sess-a",
        "sess-a",
        "hyp-a",
        "sess-a",
        TrustClass::VerifiedSystem,
        Some(1),
    );
    assert!(interrupted.is_err(), "the injected crash hook must fire");
    drop(host);

    let mut recovered = Host::reopen(&crash_world.paths, &crash_world.corpus).expect("reopen");
    let resumed = recovered
        .merge_durable(
            "sess-a",
            "sess-a",
            "hyp-a",
            "sess-a",
            TrustClass::VerifiedSystem,
            None,
        )
        .expect("the resumed merge completes");
    assert_eq!(
        resumed, 1,
        "the durable marker must keep the already-landed row from merging twice"
    );
    for op in &crash_world.corpus.ops[merge_at + 1..] {
        recovered.apply_op(op).expect("op resumes");
    }
    for action in &crash_world.corpus.actions {
        recovered.execute_action(action).expect("action");
    }
    assert_eq!(
        recovered.standing_answers().expect("answers"),
        expected,
        "a resumed merge must equal the never-crashed twin — never a hybrid, never a double"
    );
}

#[test]
fn taint_after_a_fork_heals_parent_and_inherited_state_to_the_oracle() {
    let world = world();
    let mut host = Host::build(&world.paths, &world.corpus).expect("forked world builds");
    assert!(
        circuit_members(&host, "hyp-a").contains("evt-a1"),
        "the fork must hold the inherited poisoned row before the taint"
    );

    let outcome = host.taint(&poison()).expect("taint runs");
    let report = outcome.report.to_string();
    assert_eq!(report, EXPECTED_REPORT);
    let cannot = report.find("CANNOT BE UNDONE").expect("irreversible first");
    let healed_at = report.find("ALREADY HEALED").expect("healed section");
    assert!(cannot < healed_at);

    let healed = host.standing_answers().expect("answers render");
    assert_eq!(healed, EXPECTED_HEALED);
    assert!(
        !circuit_members(&host, "hyp-a").contains("evt-a1"),
        "one taint call must heal the fork's INHERITED standing state (M4 × M5)"
    );
    // The bystander's world and the rewound branch's audit trail are untouched.
    assert!(circuit_members(&host, "sess-b").contains("evt-b1"));
    assert!(healed.contains("(\"evt-x1\", \"hyp-b\""));

    let oracle = oracle_answers(&world.corpus, &[&poison()]);
    assert_eq!(
        healed, oracle,
        "the healed forked world must be byte-identical to the world that never ingested the \
         source — forks, merge, and rewind replayed identically"
    );
}

// ---- teeth ------------------------------------------------------------------------------------

/// **Tooth A: a fork that shares operator state by reference.** Simulated by applying a parent's
/// post-fork write into the child's store as a shared-state implementation would. The catching
/// instrument is the isolation oracle's per-branch expectation
/// (`isolation_oracle_randomized_forks_and_writes_over_circuit_state`'s assertion, run here
/// against the corpus world): the contaminated branch's circuit state no longer equals its model.
#[test]
fn tooth_a_a_shared_reference_fork_is_caught_by_the_isolation_oracle() {
    let world = world();
    let mut host = Host::open(&world.paths, &world.corpus).expect("host opens");
    let fork_b = world
        .corpus
        .ops
        .iter()
        .position(|op| matches!(op, CorpusOp::Fork { child, .. } if child == "hyp-b"))
        .expect("fork position");
    for op in &world.corpus.ops[..=fork_b] {
        host.apply_op(op).expect("op applies");
    }

    // The model: what hyp-a legitimately holds after inheriting at fork.
    let expected = circuit_members(&host, "hyp-a");

    // The bug: a parent write leaks into the child through shared state.
    let leak = telemetry_commit(
        "sess-a",
        "sess-a",
        "evt-leak",
        "urgent leaked write",
        2_000_000,
    );
    host.ingest_commit(&leak).expect("parent write");
    let vector = host
        .embedder
        .embed("urgent leaked write")
        .expect("embedding");
    let record = SemanticRecord::new(
        "evt-leak",
        space(),
        vector,
        ScalarColumns {
            tenant: "acme".to_owned(),
            event_time: 2000,
            cost: 2.0,
            error: false,
        },
    )
    .expect("record");
    let token = &host.tokens["hyp-a"];
    host.agent
        .apply_group_epoch(
            token,
            &BranchId::new("hyp-a"),
            GROUPS_QUERY,
            [SemanticDelta { record, weight: 1 }],
        )
        .expect("the simulated shared-reference leak applies");

    let contaminated = circuit_members(&host, "hyp-a");
    assert_ne!(
        contaminated, expected,
        "the instrument must fire: the fork's circuit state contains a write it never made \
         and never inherited"
    );
    assert!(contaminated.contains("evt-leak"));
}

/// **Tooth B: a merge that double-applies shared history — the Loom +6-not-+3 class.** Simulated
/// by re-applying the already-merged row at the engine door, as a merge that bypassed the durable
/// marker would. The catching instrument is the merge gate's totals comparison: the target's
/// rollup no longer matches the merged expectation.
#[test]
fn tooth_b_a_double_applied_merge_is_caught_by_the_totals_instrument() {
    let world = world();
    let host = Host::build(&world.paths, &world.corpus).expect("forked world builds");

    let totals = |host: &Host| -> (i64, i64) {
        for row in host.standing_rows("cost_by_branch").expect("rollup") {
            if let (Some(Value::Str(branch)), Some(Value::Int(total)), Some(Value::Int(events))) =
                (row.get(0), row.get(1), row.get(2))
            {
                if branch == "sess-a" {
                    return (*total, *events);
                }
            }
        }
        (0, 0)
    };
    let expected = totals(&host);

    // The bug: the merged row lands a second time, bypassing the marker and the front door.
    let merged_again = Row::new(vec![
        Value::Str("evt-h1".to_owned()),
        Value::Str("sess-a".to_owned()),
        Value::Str("routine backup verified after takeover check".to_owned()),
        Value::Int(3_000_000),
        Value::Bool(false),
        Value::Int(1003),
    ]);
    let mut host = host;
    host.engine
        .ingest(
            "acme/events/telemetry",
            TELEMETRY,
            "tooth-b-double-apply",
            vec![(merged_again, 1)],
        )
        .expect("the bypass lands");
    host.engine.seal().expect("seal");

    let doubled = totals(&host);
    assert_ne!(
        doubled, expected,
        "the instrument must fire: the target's rollup no longer matches the merged expectation"
    );
    assert_eq!(
        doubled,
        (expected.0 + 3_000_000, expected.1 + 1),
        "and it is exactly the double-count: +3 became +6"
    );
}
