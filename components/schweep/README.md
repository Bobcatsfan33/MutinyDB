# Schweep

[![CI](https://github.com/Bobcatsfan33/schweep/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/Bobcatsfan33/schweep/actions/workflows/ci.yml)

**The incremental-first query engine.** Every major database of the last two decades —
ClickHouse, Snowflake, Elasticsearch, MongoDB, Postgres — shares one assumption: a query is a
one-shot program. You ask; the engine reads the data, computes the answer, returns it, and
forgets everything. Ask again after three rows changed and it recomputes everything from
scratch. The cost of a question is O(data), every time. Schweep inverts the assumption: a query
is a *standing computation*. The first time a query is asked, Schweep compiles it into a
dataflow circuit and runs the data through it once. From then on the circuit stays alive: every
batch of changes (a *delta*) flows through it, and the circuit updates its answer incrementally.
The cost of keeping an answer correct is O(change), and the cost of reading an answer is a
lookup. A one-shot query is just the degenerate case: a circuit fed one big delta (the whole
dataset) and then torn down — same machinery, one code path.

**The one-sentence pitch:** every answer, current.

Schweep is the compute plane of a future database called MutinyDB, but it is a **standalone
engine**: it has no dependency on any sibling system, and none may be added.

## Status: C13 release-candidate hardening in progress; the v0.1 tag is evidence-blocked

Schweep is near the beginning. Sprints are numbered C0–C13 and a sprint is complete only when its
exit gate is green in CI. There are no dates.

**What exists today:** the whole query surface `docs/SEMANTICS.md` defines, reachable from **SQL
text**. A scan or an INNER equi-join, an optional `WHERE`, `GROUP BY` with
`SUM`/`COUNT`/`MIN`/`MAX`/`AVG` and `HAVING`, a projection, and `DISTINCT` — compiled to a circuit
that maintains its answer from deltas and never re-reads the input. Every answer is checked against a
from-scratch recomputation at every sealed epoch, over **all 4,400** randomized scenarios the generator
produces: 24,747 answer comparisons, zero divergences, and the scenarios are full of retractions,
weight multiplicities, same-epoch updates, and expressions that raise. The SQL door is checked the same
way over the 2,028 of those scenarios that have a SQL form, and I-6 asserts that both doors compile to
structurally identical plans with identical execution counters.

Anything SQL has that this dialect does not is refused **by name**: 60 such constructs are in
`crates/schweep-sql/tests/dialect.rs`, each with the message that must name it.

**Many queries, one dataflow.** Standing queries that overlap share the circuitry they have in
common: register two queries with the same `WHERE` and the filter is stepped once per epoch, not
twice. Sharing is asserted to be invisible — the same battery run with sharing on and off gives
byte-identical answers — *and* asserted to actually happen, because a memo that quietly stopped
sharing would still be correct: 64 operator steps instead of 104 over the gate's battery.

**The log does not grow forever.** Compaction replaces a prefix of it with a Parquet snapshot of the
accumulated input, published-then-swapped so that a crash at any point leaves the old log
authoritative. Nothing downstream can tell: a standing query mid-flight, a query registered after the
compaction, and a one-shot asked at the end all produce byte-identical answers — checked against a
from-scratch recomputation, four materializations at a time. The snapshots are ordinary Parquet, so the
ground truth is readable by tools that are not us.

**One-shot queries** run through the same machinery as standing ones — the same binder, the same
operators, one big delta, torn down after — because a second execution path would be a second set of
answers to keep right.

**Operator state does not have to fit in memory.** It lives in redb files, one per operator, behind the
`StateBackend` trait frozen at C4 and amended additively by D-25 with a bounded visitor. In CI, a job runs
the engine under a **fixed 128 MiB cgroup ceiling**,
sampling resident memory throughout: **2.16 GB of operator state — sixteen times the ceiling — in a
process whose resident memory peaks at 14.3 MiB**, a ratio of 144 to 1, with memory growing 0.7% while
state grew 1,500%. The same scenarios on either backend give byte-identical answers and
byte-identical logical state.

`EXPLAIN STATE` reports what every operator of every query is holding, and a gate checks the report
against the backends themselves rather than trusting it.

**There is a server.** `schweepd` is one process over the embedded engine, reached over HTTP: ingest, seal,
register, read, subscribe, one-shot, transaction. Two things make that claim worth reading. First, the
**differential harness runs over the socket**: 2,028 generated scenarios, 11,544 answer comparisons, every
answer checked byte for byte against the oracle through a real listener — and the network, SQL and typed
doors are proven to compile to one plan and do the same work, counter for counter. Second, the server is
**killed for real**: `SIGKILL` at 1,000 random points under concurrent ingest, read and subscribe load, and
after every one of them each acknowledged batch is applied in exactly one epoch and the recovered state is
byte-identical to a never-crashed twin, emission counters included. The subscriber is killed too, as a real
process, and resumes from its token with no epoch delivered twice and none lost.

**The C10 performance sprint is complete.** The retained log is paged; server recovery and registration
stream a compacted Parquet snapshot plus its retained suffix; prefix probes and aggregate folds have
bounded intermediate memory; and `consolidate()` is a stable sort plus a linear merge rather than one
B-tree insertion per row. `EXPLAIN MAINTENANCE` exposes measured work counters through the embedded API
and `GET /explain-maintenance`.
`schweep-oracle` remains deliberately slow because its job is to be obviously correct, not quick.

**C11 source-scoped retraction is complete.** Every batch's `source_id` survives compaction in an
authenticated snapshot-v2 `PROVENANCE` ledger. `POST /retract-source` resolves that source's current net
contribution, optionally applies ordinary SQL `WHERE` semantics to one table, and feeds the negative
delta through the same epoch path as any other change. The generated transaction remains attributed to
the source, so a completed retry is a no-op. The C11 differential gate exercises 128 seeded histories,
four query families, shared memo registrations, compaction, and restart against an oracle rebuilt
without the source.

**C12's bounded accelerator spike returned `GO` for later design work, not for production code.** The
criteria were committed before the implementation. On the recorded Apple M2, one fused Metal integer
filter/sum was 89.85x faster at 1 million rows and 85.98x faster at 10 million rows by paired median,
including buffer copies, command setup, synchronization, and final reduction. Three warm-ups and all 66
measured executions matched the current C10 CPU one-shot result exactly. The magnitude identifies the
cost of the general one-shot circuit and an opportunity for a specialized cold path; it does not prove
wider SQL semantics or another GPU platform. CPU remains the only shipped path.

**C13 has frozen the release-candidate surface, not released it.** The supported API and patch-level
compatibility promise are in [`docs/current-api.md`](docs/current-api.md). Ten separately named CI jobs
map I-1 through I-10 to executable gates; scheduled jobs add a 44,000-seed differential sweep and a
100,000-cycle crash sweep. Both extended populations are green on the merged C13 commit in hosted
run [`31906947809`](https://github.com/Bobcatsfan33/schweep/actions/runs/31906947809): 248,321
differential comparisons with zero divergence and 100,000 crash/recovery cycles with all 26 named
seams exercised. The manual dispatch proves those populations but does not count toward the scheduled
night streak. The complete pre-C13 GitHub CI history—36 runs through main run
`31903930881`, not an invented 50—was audited, and all four historical failures map to fixes and later green proof in
[`testing/evidence/c13-ci-audit.json`](testing/evidence/c13-ci-audit.json). Arrow Flight remains out of
v0.1 by D-29 because the repository has no workload evidence that transport is the current bottleneck.
The `current-v0.1` release workflow fails closed until seven different scheduled nights have both the
full-sync crash job and server soak green. Four nights currently qualify; the tag does not yet exist.

**Numbers we publish:** each traces to a committed artifact in
`testing/evidence/` — `c8-state-costs.json` and `c9-bounds.json` (deterministic, both recomputed by a
test), `c8-cache-sweep.json`, `c9-memo-ceiling.json` and `c9-soak.json` (machine-dependent, and labelled as
such), `c10-benchmarks.json`, and `c12-accelerator.json`. On the recorded 8-core Apple-arm64 host, using
the **slowest** of 11 release rounds: a 10,000-row maintenance batch cost **3.176 µs per changed row**, a compact 128-row answer
over 100,000 retained input rows cost **18.068 µs per standing read**, and the 10,001st member of a
10,000-query shared swarm cost **1.786 ms for the marginal query**. In the paired TPC-H SF0.1 projection,
Schweep one-shot was **89.46× slower than DuckDB** at each engine's slowest round. That comparison uses
600,572 real `lineitem` rows but is explicitly not the official TPC-H query suite; Schweep's current
dialect cannot express that suite. Full samples, medians, ranges, machine, method, and caveats live in
[`testing/evidence/c10-benchmarks.json`](testing/evidence/c10-benchmarks.json), and a test pins every
README quotation to that artifact.

See [`docs/PROGRESS.md`](docs/PROGRESS.md) for exactly what is proven and by which test.

## Architecture of record

[`ARCHITECTURE.md`](ARCHITECTURE.md) is the architecture of record. It defines the glossary (§1),
the binding decisions D-1…D-9 (§3), the invariants I-1…I-10 (§4), the crate map (§5), the sprint
gates (§6), the testing strategy (§7), and the non-goals (§8). If code and that document
disagree, the document wins; a genuine error in it is corrected by a superseding note in
[`docs/DECISIONS.md`](docs/DECISIONS.md) first, never by quietly diverging code.

Contributors start with [`CONTRIBUTING.md`](CONTRIBUTING.md) and
[`docs/SEMANTICS.md`](docs/SEMANTICS.md).

## The theory is not ours

Schweep implements the **DBSP** model of incremental computation (Budiu, McSherry, Ryzhyk,
Tannen — VLDB 2023), in which every relational operator has an incremental form that consumes
deltas and emits deltas, and any composition of them is itself incremental. We did not discover
the theory. The work here is building a general-purpose, enterprise-grade, evidence-obsessed
engine on it — and proving every answer against a naive reference implementation that recomputes
from scratch, every time, in CI.

## Repository layout

```
crates/schweep-zset/     Z-set batches over Arrow; weight algebra; consolidation
crates/schweep-plan/     the logical plan, the binder, the scalar expression library
crates/schweep-oracle/   the naive reference engine — the spec, and the arbiter of disputes
crates/schweep-ops/      circuit operators: filter, project, equi-join, aggregate, distinct
crates/schweep-circuit/  the circuit: DAG wiring, epochs, step scheduler, result stores
crates/schweep-sql/      SQL -> binder -> logical plan -> the incrementalizer -> circuit plan
crates/schweep-memo/     canonicalization, structural hashing, the standing-query registry
crates/schweep-batch/    one-shot queries, Parquet snapshots, log compaction, bootstrap
crates/schweep-server/   schweepd: the endpoints, admission, subscriptions, and a client for them
testing/soak/            the soak harness: RSS sampled across a run, at a fixed memory ceiling
crates/schweep-state/    the StateBackend trait, MemBackend, and the order-preserving key codec
crates/schweep-log/      the input log: a directory of files, epoch sealing, exactly-once admission
testing/crash/           the crash harness: named seams, byte faults, recovery vs an uncrashed twin
testing/differential/    the oracle harness: seeded scenarios, engine vs oracle, every epoch
testing/evidence/        the ledger, and the artifacts its entries cite
docs/                    SEMANTICS.md, PROGRESS.md, DECISIONS.md
```

`schweep-plan` is not in `ARCHITECTURE.md` §5's crate map; it was added in C1 and the reason is
recorded as **D-14** in [`docs/DECISIONS.md`](docs/DECISIONS.md), before the code moved.

## Known limitations (sourced from open issues, 2026-08-15)

This is the honesty boundary for v0.1. The linked issues—not memory or marketing—are the source of the
list, and closing one requires both an implementation and its evidence gate.

- Stored non-integer arithmetic is absent: `Float64` is result-only from `AVG`; exact `Decimal128`
  semantics remain [#4](https://github.com/Bobcatsfan33/schweep/issues/4).
- Retained subscription deltas are not durable across restart
  ([#5](https://github.com/Bobcatsfan33/schweep/issues/5)); streaming operator state is not checkpointed
  ([#6](https://github.com/Bobcatsfan33/schweep/issues/6)).
- Compaction is callable and crash-safe but has no automatic operating policy
  ([#7](https://github.com/Bobcatsfan33/schweep/issues/7)). The shared memo is rebuilt per registration,
  so cold registration remains O(data) per query ([#8](https://github.com/Bobcatsfan33/schweep/issues/8)).
- SQL is intentionally narrow and the benchmark is one supported TPC-H-shaped projection, not the
  official suite ([#9](https://github.com/Bobcatsfan33/schweep/issues/9)).
- Process-death coverage is extensive, but a real power-cut storage lab remains
  [#10](https://github.com/Bobcatsfan33/schweep/issues/10).
- A source retraction must fit current admission bounds; resumable large recalls remain
  [#11](https://github.com/Bobcatsfan33/schweep/issues/11).
- The Metal result authorizes design only. A portable, admitted, fault-safe accelerator boundary remains
  [#12](https://github.com/Bobcatsfan33/schweep/issues/12), and columnar/Flight transport remains
  [#13](https://github.com/Bobcatsfan33/schweep/issues/13).
- Dedup tokens and epoch spans legitimately grow with history
  ([#14](https://github.com/Bobcatsfan33/schweep/issues/14)). Snapshot v1 is readable but cannot honestly
  reconstruct discarded source ownership ([#15](https://github.com/Bobcatsfan33/schweep/issues/15)).
- Standalone HTTP is loopback-only, plaintext, and unauthenticated; authenticated/encrypted remote
  transport belongs at the composed product boundary ([#16](https://github.com/Bobcatsfan33/schweep/issues/16)).
- Backup and restore have tested storage primitives but not a complete operator workflow with drill
  receipts ([#17](https://github.com/Bobcatsfan33/schweep/issues/17)).

`docs/DURABILITY.md` carries the exact failure-coverage table. `docs/current-api.md` defines what v0.1
does promise despite these limitations.

## License

Apache-2.0, permanently — see [LICENSE](LICENSE). The engine is open because a correctness claim
nobody can audit is worthless.
