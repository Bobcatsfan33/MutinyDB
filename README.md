<div align="center">

# MutinyDB

### The agent-native database.

**Every answer current. Every fact accountable. Every world forkable.**

[![CI](https://github.com/Bobcatsfan33/MutinyDB/actions/workflows/ci.yml/badge.svg)](https://github.com/Bobcatsfan33/MutinyDB/actions)
[![Status](https://img.shields.io/badge/status-pre--release%20consolidation-orange.svg)](#status--what-is-and-is-not-ready)
[![License: Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)

</div>

---

Every major database of the last two decades — ClickHouse, Snowflake, Elasticsearch, MongoDB —
shares one assumption: **an answer must be recomputed to be trusted.** You ask; the engine scans;
you pay O(data), every time, forever. MutinyDB is the mutiny against that assumption.

In MutinyDB, a query is a **standing computation**: it compiles once into an incremental circuit
and stays continuously correct as changes flow through it. Keeping an answer right costs
O(change). Reading it costs a lookup. And because the storage engine's commits *are* the change
feed, there is no CDC pipeline, no lag ambiguity, and no moment where the dashboard and the
database disagree.

It is built for the client that breaks every database designed for humans: the AI agent — which
asks the same questions at machine frequency, derives facts from sources that can turn out to be
poisoned, and needs to try three hypotheses without committing to any of them.

## The query no other database can run

```sql
SELECT m.claim_id, m.confidence, e.event_id, r.revenue_impact
FROM   memory.claims    AS m
JOIN   events.telemetry AS e ON e.session_id = m.session_id
JOIN   analytics.rollups AS r ON r.account_id = m.subject_id
WHERE  e.embedding ≈≈ 'the tool call timed out and we retried'  -- meaning, ranked and bounded
  AND  r.period = '2026-Q3' AND e.cost < 0.02                   -- scalars
  AS OF BRANCH 'session-7841'                                   -- the agent's world, not main
  AND NOT TAINTED BY SOURCE 's:scraped-page-77'                 -- provenance, as a predicate
LIMIT 20;
```

Meaning, scalars, branch scope, and provenance in one plan — and the answer is *standing*: it
maintains itself as data changes, on the agent's own fork of the world.

## Three capabilities that exist nowhere else

**Commit-as-delta.** Every incremental engine's weak point is change capture; every deployment
bolts on a fragile CDC layer. Here the storage engine emits deltas as a physical byproduct of
committing: one commit = one compute epoch, audited page-against-capture, exactly-once by content
address. The durability boundary and the visibility boundary are the same boundary.

**Taint-as-retraction.** A write records what it derived from — enforced at the write path, not as
skippable middleware. When a source turns out to be poisoned, `taint(source)` retracts it through
every standing computation: reversible answers repair *themselves* by the same propagation that
keeps dashboards current, and irreversible external actions are listed first, with receipts and
registered compensations. Other databases answer "we don't know what it touched." This one
un-touches it.

**Forked standing state.** A session is a branch of the database — the *data* forks in O(1) on a
content-addressed store. An agent tries hypotheses on branches, each with its own branch-scoped,
continuously-current answers, merges the winner's divergence under policy re-run, and rewinds the
rest — auditable, never destroyed. Honest economics, per [MD-5](docs/decisions/MD-5.md): at v1
the *standing answers* fork by hydration — **O(state), measured and published**
([ledger](crates/mutiny-forks/evidence/m5-fork-cost.json)) — and O(1) fork of live answers is
post-v1 work on the engine's own track, with the spike's copy-on-write measurements as its
baseline.

## Architecture

Five planes, directed dependencies, enforced by an allowed-edge matrix read from the build graph —
not by convention:

```text
  fleet/ops ─► trust ─► compute ─► semantic ─► storage
                          │                      ▲
                          └───── bridge ────────┘
```

- **Trust** (from LoomDB): branches, write envelopes, provenance, claims, policy — and a
  propose-not-execute action gateway where the agent handle *has no execute method, by type*.
- **Compute** (from Schweep): DBSP-style standing circuits, epochs, shared subplans — the
  ten-thousandth near-duplicate query costs a sliver of the first.
- **Semantic** (from PrismDB): bounded semantic operators inside compute, plus an immutable,
  meaning-clustered cold tier with exact-oracle-measured recall.
- **Storage** (substrate): content-addressed pages, one WAL, O(1) fork/snapshot/rewind, sleep/wake
  to object storage.
- **Fleet/ops**: tenant pools, registry, sleep/wake economics — a million idle databases cost the
  price of their bytes.

Binding contracts: [`CONSOLIDATION-ROADMAP.md`](CONSOLIDATION-ROADMAP.md) ·
[`docs/decisions`](docs/decisions) · [MD-6](docs/decisions/MD-6.md) (one-repository topology with
exact source provenance).

## Built from proven parts — and quarantined until proven together

Each plane arrives from a codebase with its own public evidence record. **No claim is inherited
merely because a component repository made one about itself** — every component stays quarantined
until its exact release and the composed product gates pass, and
[`scripts/verify_component_lock.py`](scripts/verify_component_lock.py) refuses an unreleased or
blocked component marked admitted. That distinction is enforced, not editorial.

| Component | Product role | What its own gates prove | Imported state | Admission |
| --- | --- | --- | --- | --- |
| [substrate](https://github.com/Bobcatsfan33/substrate) | storage | 98 ns fork · 50,000 randomized crash-recover cycles · airgap by compile-time amputation | `substrate-v1.6.0` | quarantined pending compatibility |
| [LoomDB](https://github.com/Bobcatsfan33/loomdb) | trust | record-level merge under four model oracles · taint → two-section RecallPlan · flat-memory soaks | `loomdb-v0.5.1` | quarantined pending mounted-oracle gates |
| [PrismDB](https://github.com/Bobcatsfan33/PrismDB) | semantic | byte-identical answers across 1/2/4-shard layouts · encryption: nothing legible at rest, rotation without rewriting a part byte | snapshot `84e5a4f` + AWS KMS provider | blocked on a release, live KMS receipt, composed gates |
| [Schweep](https://github.com/Bobcatsfan33/schweep) | compute | every answer proven against a from-scratch oracle · 1,000 real SIGKILLs with 24,219 acked appends exactly-once · 2.16 GB of operator state under a 128 MiB memory ceiling | snapshot `220bf6b`, C11–C13 complete | blocked on scheduled-night evidence, `current-v0.1`, composed admission |

## What is composed and green today

- **M1 — the bridge.** One storage commit → one compute epoch; a real Loom envelope required at
  admission; write-set captured in the same substrate transaction; physical pages audited against
  captured logical changes; crash-proven across every append/seal seam. Randomized histories
  through the bridge remain **byte-identical to an independent direct-ingest control**.
- **M2 — the semantic path.** Generation-pinned embedding in the bridge, bounded incremental
  top-k, mergeable semantic grouping, dual-generation cutover, cold one-shot routing — a frozen
  hybrid corpus traverses all of it and **matches real PrismDB exact and rerank answers
  bit-for-bit**.
- **M3 — the mounted trust plane.** Loom capability checks over branch-scoped standing results;
  branch-result isolation gated; the scripted MCP demo green; **all four Loom model oracles run
  unmodified** against the mounted configuration; the agent handle contains no action gateway.
- **M4 — cross-circuit taint.** One `taint(S)` call resolves the poisoned source's downstream
  through the `mutiny_derivation` standing relation (a query, not a DAG walk, transitive across
  branches), retracts it through Schweep's C11 predicate-scoped `retract_source`, and memory
  claims, analytical rollups, and branch-scoped semantic answers all correct themselves. The
  frozen, checksummed incident corpus proves the healed world **byte-identical to an oracle that
  never ingested the source**; the two-section RecallPlan leads with the executed action's
  receipt; untainted branches and bystander tenants are untouched; taints compose; and the taint
  path killed at every seam resumes without ever half-healing.

- **M5 — forked standing state, honestly.** The spike ran first against MD-5's pre-committed
  criteria; the verdict took the fallback deliberately. A durable fork hydrates the child's
  standing answers from the parent — **O(state), measured, asserted by the gate so the O(1) claim
  cannot quietly return** — both branches maintain independently, merge follows Loom's law
  (policy re-run at merge time, all-or-nothing, marker-deduplicated so +3 never becomes +6),
  rewind returns the state accounting exactly to baseline, recovery replays the lineage to
  byte-identical answers through injected mid-fork and mid-merge crashes, and one taint call
  heals the parent **and** the fork's inherited state on the forked incident corpus.

Details: [`docs/M1-BRIDGE.md`](docs/M1-BRIDGE.md) · [`docs/M2-SEMANTIC.md`](docs/M2-SEMANTIC.md) ·
[`docs/M3-TRUST.md`](docs/M3-TRUST.md) · [`docs/M4-TAINT.md`](docs/M4-TAINT.md) ·
[`docs/M5-FORKS.md`](docs/M5-FORKS.md)

## Status — what is and is not ready

**Not approved for production. Not a software release candidate.**

Still open, named rather than implied: a supported `mutinyd` binary (M6), fleet operation (M7),
external assurance and production approval (M8) — and O(1) fork of live answers, which MD-5
deliberately moved post-v1 with its spike evidence on record. Schweep's `current-v0.1` release requires its remaining scheduled-night evidence;
PrismDB's admission requires a release and a live KMS receipt. The roadmap runs on exit gates, not
dates.

| Phase | M1 bridge | M2 semantic | M3 trust | M4 taint | M5 forks | M6 mutinyd | M7 fleet | M8 release |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| | ✅ | ✅ | ✅ | ✅ | ✅ | ◎ | ◎ | ◎ |

## The evidence culture

This system is being built fast, largely by AI, under human review — which is a legitimate reason
to distrust it, and the reason nothing here ships on enthusiasm. Every answer is proven against a
naive oracle that recomputes from scratch. Every durability claim survives randomized crash
injection with the fault count *asserted* — a harness that injected nothing fails. Every gate has
proven it can fail, via marker-grepped mutations with the catching instrument named. Every tuned
constant lives in an evidence ledger with the benchmark artifact that justifies it. Every
limitation is written down before an evaluator finds it.

Run the composed gates yourself:

```sh
python3 scripts/verify_component_lock.py
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-features --locked
```

The nested component workflows are retained as import provenance; root workflows own the composed
product result. We would rather you read the tests than the marketing.

## License

**Apache-2.0** — see [LICENSE](LICENSE). Permanently, because a durability claim nobody can audit
is worth nothing. The license is not the release: the product remains **not approved for
production** until M8's release and naming-clearance gates pass, and nothing in this repository
carries a warranty that its own gates do not prove.
