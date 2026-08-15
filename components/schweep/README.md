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

## Status: Sprint C10 implementation complete; CI-gated (with the gaps named below)

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
and `GET /explain-maintenance`. Arrow Flight remains deferred to C13, and C11–C13 have not started.
`schweep-oracle` remains deliberately slow because its job is to be obviously correct, not quick.

**Numbers we publish:** each traces to a committed artifact in
`testing/evidence/` — `c8-state-costs.json` and `c9-bounds.json` (deterministic, both recomputed by a
test), `c8-cache-sweep.json`, `c9-memo-ceiling.json` and `c9-soak.json` (machine-dependent, and labelled as
such), and `c10-benchmarks.json`. On the recorded 8-core Apple-arm64 host, using the **slowest** of 11
release rounds: a 10,000-row maintenance batch cost **3.176 µs per changed row**, a compact 128-row answer
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

**Known limitations, before you find them:** the log no longer holds batches resident, but it still keeps
one dedup token per acknowledged append and a 16-byte span per epoch; both legitimately grow with history.
Retained subscription deltas are not durable—a subscriber that falls behind across a restart must re-read
the durable answer. State can spill beyond memory, but the frozen snapshot method still returns a byte
vector, so checkpointing that state materializes it (D-18). Compaction is callable and recoverable but has
no automatic policy. A snapshot holds rows rather than source provenance, which C11's source-scoped
retraction needs.

A memo is not checkpointable because its shape is its live query set. `schweepd` rebuilds each
registration by streaming the snapshot and retained suffix (D-22), so registration remains O(data) per
query while maintenance is O(change). The SQL dialect remains narrower than the typed API for combined
group-and-project shapes. There is no stored non-integer arithmetic: `Float64` is result-only from `AVG`,
and fixed-point decimal remains Q-1. `RocksBackend` was not delivered; redb is the durable B-tree backend
(D-19), with `MemBackend` as the invariant-checking implementation. The TPC-H evidence covers one
supported projection, not the official suite. Finally, nothing here tests power loss: the 10,000-cycle
fault gate and 1,000 real `SIGKILL`s model process death, not a dying machine. `docs/DURABILITY.md` carries
the exact coverage table.

Crates named in §5 that do not appear above have not been written yet.

## License

Apache-2.0, permanently — see [LICENSE](LICENSE). The engine is open because a correctness claim
nobody can audit is worthless.
