# Schweep — Architecture of Record and Build Roadmap

> **Status:** Authoritative. This document is the architecture of record for Schweep, the
> incremental-first query engine, and the sprint roadmap for building it. It is written to be
> handed to a team of junior developers: every term is defined, every sprint says exactly what to
> build and how to know it is done, and every rule states the failure it prevents.
>
> **Sprints, not timelines.** A sprint (C0–C13) is complete when its exit gate is green in CI.
> There are no dates in this document and none should be added.
>
> **The project was named `Current` until 2026-08-11.** It is now **Schweep**, for the reasons in
> `docs/DECISIONS.md` **D-21**: the old name was trademarked in its own class, owned as an event brand by
> a large vendor in this exact category, and taken on crates.io. Nothing technical changed with it — every
> invariant, decision and gate is the same one — and the tagline *every answer, current* stays, because it
> was always the adjective. Renamed before anything was published, which is why it cost a grep.
>
> **Schweep is a standalone engine.** It has no dependency on substrate, LoomDB, or PrismDB.
> Consolidation into MutinyDB is a separate roadmap in the MutinyDB repository. Nothing in this
> document may take a dependency on those systems — the seams MutinyDB will need are called out
> where they occur (marked **[MutinyDB seam]**), and they are designed as *interfaces*, never as
> imports.

---

## §0 — What Schweep is, in one page

Every major database of the last two decades — ClickHouse, Snowflake, Elasticsearch, MongoDB,
Postgres — shares one assumption: **a query is a one-shot program.** You ask; the engine reads
the data, computes the answer, returns it, and forgets everything. Ask again after three rows
changed and it recomputes everything from scratch. The cost of a question is O(data), every time.

Schweep inverts the assumption: **a query is a standing computation.** The first time a query is
asked, Schweep compiles it into a dataflow *circuit* and runs the data through it once. From then
on, the circuit stays alive: every batch of changes (a *delta*) flows through it, and the circuit
updates its answer incrementally. The cost of keeping an answer correct is O(change), and the cost
of reading an answer is a lookup. A one-shot query is just the degenerate case: a circuit fed one
big delta (the whole dataset) and then torn down — same machinery, one code path.

Why this wins, in three sentences. Dashboards, monitors, and AI agents ask the same shapes of
questions over and over — a workload where recomputation is almost pure waste, and where Schweep's
answers are already sitting there, current. Data grows roughly 10× per decade while the *rate of
change* grows far slower, so scan engines get slower every year on the same workload and Schweep
does not. And because near-identical queries share their circuitry (§5.7), the ten-thousandth
concurrent query costs a sliver of the first — the concurrency regime AI agents are creating is
the regime where this architecture's lead widens.

The mathematics is not invented here. Schweep implements the **DBSP** model of incremental
computation (Budiu et al.; see §9), in which every relational operator has an incremental form
that consumes deltas and emits deltas, and any composition of them is itself incremental. Our job
is not to discover the theory. Our job is to build the first *general-purpose, enterprise-grade,
evidence-obsessed* engine on it — and to prove every answer against a naive reference
implementation that recomputes from scratch, every time, in CI.

**The one-sentence pitch:** every answer, current.

---

## §1 — Glossary (read this before anything else)

Every term below has exactly one meaning in this codebase. Do not use these words loosely in
code, comments, commits, or docs.

- **Z-set** — the universal data representation: a multiset of rows where each row carries an
  integer **weight**. Weight +1 means "one copy of this row exists"; +3 means three copies; −1
  means "one copy is removed." A Z-set with mixed signs is a *change*; a Z-set with all
  non-negative weights can represent *state*. All data moving through Schweep — inputs, outputs,
  intermediate results — is Z-sets. There is no separate "delete" or "update" machinery anywhere:
  an update is a −1 for the old row and a +1 for the new one, in the same Z-set.
- **Delta (Δ)** — a Z-set representing "what changed" between two epochs.
- **Epoch** — the global unit of time and atomicity. Ingest assembles input deltas; when an epoch
  is **sealed**, all deltas in it become visible to circuits *together*, and the circuit **step**
  for that epoch runs to completion before any answer reflects it. Answers are always "as of
  epoch N" — never a mixture of N and N+1. Epochs are dense integers starting at 1.
- **Circuit** — the compiled form of a query: a directed acyclic graph of operators through which
  deltas flow. One step of a circuit consumes one epoch's input deltas and produces one epoch's
  output deltas.
- **Operator** — a node in a circuit. **Linear** operators (filter, map, project) pass deltas
  through directly: the delta of the output is the operator applied to the delta of the input —
  they need no state. **Bilinear** operators (join) and **aggregation** operators need **operator
  state** (see below) to compute output deltas from input deltas.
- **Operator state** — the indexed data an operator must remember between steps (e.g., a join
  must remember every row it has seen on each side, indexed by join key). State is the engine's
  memory cost, and every operator must declare and account for its state bound (invariant I-9).
- **Integral / integrate** — summing all deltas of a stream from epoch 1 to N, yielding the full
  state as of N. `integrate(deltas) = current contents`. The **result store** for a standing
  query is the maintained integral of its output stream.
- **Standing query** — a query registered with the engine whose circuit stays resident; its
  result store is continuously maintained and readable at any sealed epoch.
- **One-shot query** — a query executed by building its circuit, feeding the integral of all
  inputs as a single delta, reading the output, and discarding the circuit.
- **Retraction** — a delta whose weights are negative: the removal of previously ingested facts.
  Retractions are ordinary deltas and flow through the same code path as insertions.
  **[MutinyDB seam]** — taint-and-recall in MutinyDB will be implemented as source-scoped
  retraction; Schweep only needs retraction to be *correct*, which the Z-set model gives by
  construction.
- **Memo** — the registry of live circuits and their sub-circuits, keyed by a structural hash of
  the plan, enabling shared sub-computations across queries (§5.7).
- **Oracle** — the naive reference implementation (§5.1): recomputes every query from scratch
  over the full input at every epoch. Slow, obviously correct, and the arbiter of every
  correctness dispute in this codebase.
- **The ledger** — `testing/evidence/registry.json`: every tuned constant in the engine, with the
  benchmark artifact that justifies it. A constant not in the ledger may not steer behavior.

---

## §2 — System overview

```
                         ┌────────────────────────────────────────────┐
   SQL (text)  ────────► │  FRONTEND   schweep-sql                    │
   Typed API   ────────► │  parse → bind → logical plan               │
                         │  → INCREMENTALIZE → circuit plan           │
                         └───────────────┬────────────────────────────┘
                                         │ circuit plan (hashable, comparable)
                         ┌───────────────▼────────────────────────────┐
                         │  MEMO        schweep-memo                  │
                         │  structural-hash subplans; attach to       │
                         │  existing circuitry or instantiate new     │
                         └───────────────┬────────────────────────────┘
                                         │
   deltas in             ┌───────────────▼────────────────────────────┐   deltas out
   ────────────────────► │  RUNTIME     schweep-circuit               │ ────────────────►
   (epoch-sealed,        │  steps circuits one epoch at a time;       │   result stores
    exactly-once,        │  operators from schweep-ops;               │   (maintained
    from schweep-log)    │  state in schweep-state                    │    integrals) +
                         └───────────────┬────────────────────────────┘   subscriptions
                                         │
                         ┌───────────────▼────────────────────────────┐
                         │  STORAGE (deliberately boring)             │
                         │  schweep-log: append-only input log        │
                         │  schweep-state: durable operator state     │
                         │  checkpoints; Parquet ground truth (C7)    │
                         └────────────────────────────────────────────┘

   In parallel with ALL of the above, always:
                         ┌────────────────────────────────────────────┐
                         │  ORACLE      schweep-oracle                │
                         │  naive full recompute at every epoch;      │
                         │  differential harness asserts equality     │
                         └────────────────────────────────────────────┘
```

Storage is *deliberately boring*: an append-only log plus durable indexed state plus (from C7)
Parquet files. All of Schweep's novelty lives in the compute model. Resist every temptation to
innovate in storage — that is a different product's job, and boring storage is what makes this
engine auditable. **[MutinyDB seam]** — in MutinyDB, substrate replaces the boring storage
underneath; that is why `schweep-log` and `schweep-state` must sit behind traits (§5.4, §5.5)
rather than being called concretely from operators.

---

## §3 — Decision records (binding; supersede only in writing)

- **D-1 · Language: Rust, stable toolchain.** Edition 2021 or later. No nightly features. The
  engine is a long-lived server holding irreplaceable state; memory safety is not negotiable.
  `unsafe` is forbidden until the first hot-loop sprint (C10), and then only with the
  inventory-and-safety-argument discipline: every `unsafe` block gets a comment stating the
  invariant that makes it sound and a test that exercises it.
- **D-2 · Data representation: Z-sets over Arrow columnar batches.** A Z-set batch is an Arrow
  `RecordBatch` plus an aligned `i64` weight column. Arrow gives us columnar layout, zero-copy
  slicing, a typed schema system, and a wire format (Flight, C9) for free. We do not design a
  bespoke row format.
- **D-3 · The incremental model is DBSP.** We implement the DBSP operator algebra: linear
  operators pass deltas; join is bilinear (ΔJ = ΔA ⋈ B + A ⋈ ΔB + ΔA ⋈ ΔB); aggregates maintain
  per-group state; `integrate`/`differentiate` convert between streams of deltas and streams of
  states. Recursive/iterative circuits (transitive closure etc.) are **out of scope for v1** —
  the nested-clock machinery they need is the single biggest complexity cliff in this design
  space, and no target workload in §8 needs it.
- **D-4 · SQL parsing: the `sqlparser` crate (sqlparser-rs).** We do not write a parser. We own
  everything after the AST: binder, logical plan, incrementalizer. The dialect is *ours* and
  defined in §5.6 — sqlparser accepting a construct does not mean Schweep supports it; the binder
  refuses, by name, anything outside the dialect.
- **D-5 · Operator state: pluggable behind a trait, first implementation embedded LSM (RocksDB
  via `rust-rocksdb`) plus an in-memory implementation for tests.** Writing our own LSM is
  explicitly deferred; it is an optimization with a known interface, not a research problem, and
  it must not block the correctness milestones. The trait boundary (`StateBackend`, §5.5) is
  frozen at C4 exit so a custom backend can slot in later without touching operators.
- **D-6 · Time: there is no wall clock inside the engine.** Operators, planner, and runtime never
  call `now()` or generate randomness. All nondeterminism enters the system in exactly one place:
  the ingest boundary, where events are assigned to epochs. Everything downstream of a sealed
  epoch is a pure, deterministic function of the log. This is what makes the oracle meaningful,
  replicas byte-identical, and crash recovery provable (invariant I-2).
- **D-7 · Ordering and ties.** Z-sets are unordered; `ORDER BY` is resolved at result-store read
  time. Every ordering has a total tiebreak: the declared sort keys, then all remaining columns
  in schema order. Two engines (or the engine and the oracle) must never disagree on output
  order. "Nondeterministic order" is not a thing this engine is allowed to have.
- **D-8 · CPU first; GPU behind a falsifiable spike gate (C12).** The tensor-compiler cold path
  from the design thesis is real but it is *phase two*. Correctness of the incremental model —
  provable against the oracle — is phase one, and it is CPU-shaped work. C12 defines what the GPU
  spike must demonstrate before any GPU code is written in earnest.
- **D-9 · Apache-2.0, permanently.** Same posture as the sibling repositories: the engine is open
  because a correctness claim nobody can audit is worthless.

---

## §4 — Invariants (the laws)

Every ticket names the invariant it preserves; a change that violates one is rejected however
good it is otherwise. Each law states the failure it prevents.

- **I-1 · The oracle law.** At every sealed epoch, for every registered query, the incremental
  answer must equal the oracle's full recomputation, byte for byte, including order (D-7).
  *Prevents:* the incremental engine drifting from SQL semantics in ways no unit test would
  catch. This is the load-bearing invariant of the whole project.
- **I-2 · Determinism.** The state and every answer at epoch N is a pure function of the log
  prefix up to N. No wall clock, no randomness, no dependence on thread scheduling or map
  iteration order anywhere downstream of the ingest boundary. *Prevents:* unverifiable recovery,
  diverging replicas, flaky tests, and answers that change when nothing changed.
- **I-3 · Epoch atomicity.** A reader sees the world as of a sealed epoch — never a partial
  epoch, never a mixture of two. *Prevents:* the classic streaming-system bug where a dashboard
  shows a join of "orders as of now" against "customers as of slightly earlier."
- **I-4 · Exactly-once ingest.** An acknowledged input batch is applied in exactly one epoch,
  survives crashes, and is never applied twice. Replays are detected and suppressed at the log.
  *Prevents:* silent double-counting — the incremental engine's equivalent of data corruption.
- **I-5 · Retraction symmetry.** Negative weights flow through every operator by the same code
  path as positive ones. No operator may special-case deletion. *Prevents:* the "inserts work,
  deletes drift" class of bug that kills incremental systems; also, this symmetry is the
  property MutinyDB's taint-as-retraction will stand on.
- **I-6 · Same door.** SQL and the typed API compile to the same circuit plan and run the same
  code; gate tests assert identical results *and* identical execution counters through both.
  *Prevents:* two dialects of behavior wearing one name.
- **I-7 · Crash equals replay.** Recovery = load last checkpoint + replay log suffix, and the
  recovered state is byte-identical to a process that never crashed (provable because of I-2).
  Crash-injection tests kill the process at randomized boundaries and assert exactly this.
  *Prevents:* recovery paths that "usually work."
- **I-8 · Memo transparency.** Whether a subplan is shared or private may change counters and
  cost, never a result byte. Every memo gate runs the same query shared and unshared and asserts
  identical answers. *Prevents:* cross-query contamination — the catastrophic failure mode of
  shared computation.
- **I-9 · No unbounded state without a declaration.** Every stateful operator declares its state
  bound as a function of its input (e.g., join state is O(|A| + |B|)); the runtime accounts
  actual state against declarations, and an operator exceeding its declaration is a bug, not a
  tuning problem. Unbounded-by-nature constructs (e.g., aggregation over an unbounded key space)
  must be admitted explicitly at query registration. *Prevents:* the slow memory death that
  takes down standing-query systems in month three.
- **I-10 · Honesty.** No performance number without a committed reproducible benchmark artifact;
  every tuned constant in the ledger with its receipt; every known weakness in the README before
  a user finds it; zero-flake test policy — a flaky test is a bug in the test or a bug in the
  engine, and both block merge. *Prevents:* the credibility debt that a database, of all
  products, cannot carry.

---

## §5 — Component architecture (crate by crate)

Workspace layout (one Cargo workspace, one repository):

```
crates/
  schweep-zset/      Z-set batches over Arrow; weight algebra; consolidation
  schweep-oracle/    the naive reference engine (BUILT FIRST, C0)
  schweep-ops/       operators: linear, join, aggregate, distinct, integrate
  schweep-circuit/   circuit graph, epochs, the step scheduler
  schweep-state/     StateBackend trait; memory + RocksDB implementations; checkpoints
  schweep-log/       the input log: epochs, sealing, exactly-once admission
  schweep-sql/       sqlparser AST → binder → logical plan → incrementalizer
  schweep-memo/      structural hashing, subplan sharing, standing-query registry
  schweep-batch/     one-shot execution; Parquet ground truth (C7)
  schweepd/          the server: Arrow Flight + HTTP, subscriptions (C9)
testing/
  differential/      the oracle harness (every epoch, every query, oracle vs engine)
  crash/             crash-injection harness
  golden/            frozen corpora + expected outputs (committed, checksummed)
  evidence/          registry.json — the tuned-constant ledger
docs/
  PROGRESS.md        sprint-by-sprint status: what is proven, by which test
  DECISIONS.md       D-records beyond §3, as they accumulate
```

### §5.1 schweep-oracle — built first, on purpose

A complete, naive, in-memory implementation of the entire query surface: tables are `Vec<Row>`,
queries re-execute from scratch over full inputs at every epoch, no indexes, no incrementality,
no cleverness. Every semantic decision (null handling, ties, aggregate edge cases) is made HERE
first, in the simplest possible code, and the engine is then held to it. The oracle is also the
spec: when a question arises about what a query should return, the answer is what the oracle
returns, and if the oracle is wrong it gets fixed first. Budget for it: this is the most
important crate in the repository and the one place where "slow and obvious" is the style guide.

### §5.2 schweep-zset

`ZSetBatch`: an Arrow RecordBatch + aligned i64 weights. Operations: add (multiset union with
weight addition), negate, **consolidate** (merge duplicate rows by summing weights and drop
zero-weight rows — the single most-called function in the engine; it is where "an insert and a
delete cancel" physically happens), and iteration by (row, weight). Property tests: Z-set
addition is commutative and associative; consolidate is idempotent; negate ∘ negate = identity.

### §5.3 schweep-ops

Each operator is a struct implementing `Operator`: `step(&mut self, input_deltas) ->
output_delta`, plus `state_bound()` (I-9) and `checkpoint()/restore()` hooks (C4). The v1
operator set, in build order: **filter, map/project** (linear, stateless); **join** (equi-join,
bilinear: keeps both sides' integrals indexed by key; ΔOut = ΔA⋈B + A⋈ΔB + ΔA⋈ΔB — implement it
exactly like that, three probes, no shortcuts); **aggregate** (GROUP BY with per-group state:
SUM/COUNT/MIN/MAX/AVG — note MIN/MAX must keep a per-group multiset, not a single value, or
retractions break them: removing the current MIN must reveal the second-smallest, which you can
only do if you kept it); **distinct** (weight → sign function, stateful); **integrate** /
**differentiate**. Junior-dev pitfall, stated in code comments too: *never* handle a negative
weight as a special case inside an operator — if you find yourself writing `if weight < 0`,
outside of MIN/MAX multiset bookkeeping or sign logic in distinct, you are re-deriving a bug.

### §5.4 schweep-log

The write path and the only place time enters. `append(source_id, batch, dedup_token)` →
durable ack; epochs seal either on demand (tests) or by policy (size/interval — a *policy*
constant, ledger-entry required). Exactly-once: dedup tokens are recorded in the log; a replayed
token is acknowledged-and-dropped; a reused token with different content is refused loudly
(I-4). Deltas carry their `source_id` from birth — three lines of schema today, and the hook
**[MutinyDB seam]** taint-as-retraction and Loom's envelopes attach to later. The log trait
must be implementable by "a directory of files" (v1) without assuming it forever.

### §5.5 schweep-state

`StateBackend` trait: ordered KV with range scans, atomic multi-key write batches, and named
snapshots (checkpoints). Implementations: `MemBackend` (BTreeMap — tests, oracle) and
`RocksBackend` (D-5). Checkpoint protocol (C4): at epoch boundary, flush all operator state +
the epoch number atomically; recovery = restore checkpoint + replay log from checkpoint epoch
(I-7). The trait is frozen at C4 exit.

### §5.6 schweep-sql — the dialect and the incrementalizer

Pipeline: sqlparser AST → **binder** (names→ids, types checked, dialect enforced — anything
outside the dialect is refused with the construct named, never silently accepted) → **logical
plan** (a small algebra: Scan, Filter, Project, Join, Aggregate, Distinct, Union) → the
**incrementalizer**, which rewrites the logical plan into a circuit plan by DBSP rules — this
~500-line rewrite is the intellectual heart of the engine and must be the best-documented code
in the repository. The v1 dialect ladder (each rung gated on oracle agreement before the next):
(1) SELECT/WHERE/projection with the scalar expression library; (2) INNER equi-JOIN;
(3) GROUP BY + the five aggregates + HAVING; (4) DISTINCT, UNION ALL, ORDER BY/LIMIT at read
time; (5) LEFT JOIN and subqueries-as-joins where decorrelatable, refused where not. Nulls are
three-valued-logic from rung 1, decided in the oracle first, documented in
`docs/SEMANTICS.md` before implementation (write the doc, then the oracle, then the engine).

### §5.7 schweep-memo

Circuit plans are canonicalized and structurally hashed; registering a query attaches to
existing sub-circuits where hashes match, instantiates only the novel suffix. Reference-counted
teardown. Result stores: maintained integrals of each standing query's output, readable at any
sealed epoch, with subscription (push) delivery in C9. I-8 is the law here; the memo starts
conservative (share only exact sub-tree matches; no cross-predicate cleverness in v1).

### §5.8 schweep-batch and schweepd

`schweep-batch`: one-shot queries via the ephemeral-circuit path; Parquet ground-truth
snapshots + log compaction (C7) so the log does not grow forever. `schweepd` (C9): Arrow
Flight + HTTP, register/query/subscribe, per-source admission, graceful backpressure. Multi-node
distribution is a non-goal for v1 (§8) — schweepd is one process; replicas come later via log
shipping, exactly the flock-sync shape, and are explicitly out of v1.


---

## §6 — The sprints (C0–C13)

Serial by default. Each sprint lists: **objective · build · exit gate · pitfalls.** A sprint is
done when the gate is green in CI, `docs/PROGRESS.md` states what is proven and by which test,
and nothing was skipped "for now."

### C0 — The oracle, the harness, and the rules
**Objective:** stand up the workspace with the correctness machinery *before any engine code
exists.* **Build:** Cargo workspace + CI (fmt, clippy `-D warnings`, tests, no-network job);
`CLAUDE.md`/`CONTRIBUTING.md` carrying §4 verbatim; `schweep-zset` (batches, weights, add,
negate, consolidate + property tests); `schweep-oracle` covering dialect rungs 1–3 semantics
(filter/project/join/aggregate over Vec<Row>, epochs as replayed prefixes); the **differential
harness** in `testing/differential`: a scenario driver that feeds randomized epoch sequences
(inserts AND retractions from epoch one) to "the engine under test" behind a trait — with the
oracle temporarily on both sides to prove the harness itself. `docs/SEMANTICS.md` drafted for
rung 1–3 (nulls, ties, aggregate edge cases). **Exit gate:** CI green; harness runs
oracle-vs-oracle over 1,000 randomized scenarios; property tests for Z-set algebra pass; a
seeded scenario is reproducible byte-for-byte from its seed. **Pitfalls:** do not skip
retractions in the scenario generator "until the engine supports them" — the generator defines
the bar, and the bar includes negative weights from day one.

### C1 — Linear operators + the first real circuit
**Objective:** the smallest true incremental engine. **Build:** `schweep-circuit` v0
(single-threaded step scheduler, epochs, DAG wiring); filter/map/project operators; a hand-built
(no SQL yet) circuit API; result stores as maintained integrals. **Exit gate:** differential
harness green, engine-vs-oracle, over randomized filter/project scenarios including retractions;
I-2 gate: two runs of the same scenario produce byte-identical state and answers. **Pitfalls:**
resist adding any state to linear operators; if a linear operator seems to need state, the
design is wrong.

### C2 — Join
**Objective:** the first bilinear operator — the hardest correctness class in the engine.
**Build:** equi-join with indexed state on both sides (MemBackend); the three-term delta rule
implemented literally; state-bound declarations (I-9) and the runtime accounting that checks
them. **Exit gate:** differential harness green over join scenarios: multi-key batches,
retractions of joined rows, updates (retract+insert same epoch), weight multiplicities >1, and
the delta-delta term (both sides changing in the same epoch — write a scenario that isolates
it). **Pitfalls:** the ΔA⋈ΔB term is the one every implementer forgets; the harness must have a
scenario that fails if it is missing (both sides insert matching rows in the same epoch).

### C3 — Aggregates and distinct
**Objective:** complete the stateful core. **Build:** GROUP BY with SUM/COUNT/AVG (running
values) and MIN/MAX (per-group ordered multiset — see §5.3); DISTINCT; HAVING as post-aggregate
filter. **Exit gate:** differential green over aggregate scenarios *heavy on retractions* —
specifically: retract the current MIN and assert the second-smallest surfaces; drain a group to
zero rows and assert the group row vanishes (not zeroes); AVG over retractions lands exactly on
the oracle's value. **Pitfalls:** groups-vanish-at-zero is where naive implementations emit a
phantom (key, 0) row; the oracle decides, and the oracle says the row disappears.

### C4 — Durability: state, checkpoints, crash = replay
**Objective:** survive death. **Build:** `schweep-log` v1 (directory-of-files append log,
epoch sealing, dedup tokens, I-4); `RocksBackend`; the checkpoint protocol (§5.5); recovery.
Freeze the `StateBackend` trait at exit. **Exit gate:** crash-injection harness kills the
process at randomized points across ingest/step/checkpoint over full scenarios, ≥10,000 cycles
in CI: every recovery equals the never-crashed run byte-for-byte (I-7); every acked batch
appears exactly once (I-4); a torn checkpoint is detected and the previous one used. **Pitfalls:**
fsync discipline — write down the exact ordering (state flush → checkpoint record → log trim)
in a doc comment before implementing, and have the crash harness kill between each pair.

### C5 — SQL frontend + the incrementalizer
**Objective:** the same-door moment: SQL in, circuits out. **Build:** `schweep-sql` — binder,
logical plan, incrementalizer (§5.6), dialect rungs 1–3; scalar expression library (arithmetic,
comparison, boolean, CASE, IS NULL) implemented once, shared by oracle and engine but *tested
differentially anyway* (shared code can still be called differently). **Exit gate:** the
differential harness gains a SQL mode — randomized queries generated over randomized schemas
(a small query fuzzer: hundreds of shapes, thousands of runs) — green engine-vs-oracle; I-6
gate: typed-API and SQL doors produce identical plans (structural hash equality) and identical
counters on the gate suite; every refusal names its construct. **Pitfalls:** the fuzzer will
find null-semantics disagreements between engine and oracle — that is its job; fix the
semantics doc first, then the oracle, then the engine, in that order, every time.

### C6 — The memo: standing queries and shared circuitry
**Objective:** many queries, one dataflow. **Build:** `schweep-memo` — canonicalization,
structural hashing, subplan attach/detach with refcounts, the standing-query registry
(register/read-at-epoch/deregister). **Exit gate:** I-8 gate: a battery of overlapping queries
runs twice — sharing enabled and disabled — with byte-identical answers and a counter proof
that sharing actually shared (fewer operator-steps executed); teardown gate: deregistering a
query frees exactly its private suffix (state accounting returns to baseline); 1,000
register/deregister cycles leak nothing. **Pitfalls:** canonicalization bugs (a=b vs b=a
hashing differently) silently destroy sharing without breaking correctness — assert expected
hash-hits in tests, not just correct answers.

### C7 — One-shot queries, Parquet ground truth, compaction
**Objective:** be a database, not only a subscription engine. **Build:** `schweep-batch`:
one-shot execution through ephemeral circuits; periodic Parquet snapshots of input integrals;
log compaction (snapshot + suffix replaces prefix); bootstrap-from-snapshot for new circuits
(a new standing query hydrates from the snapshot + replays the suffix rather than the whole
log). **Exit gate:** one-shot answers equal oracle over the fuzz suite; compaction gate: answers
byte-identical before/after compaction (I-1 across a compaction is the whole point); a new
query registered mid-history produces the same result store as one registered at epoch 1
(the four-materializations discipline, Schweep edition). **Pitfalls:** compaction must be
publish-then-swap, never in-place; a crash mid-compaction leaves the old log authoritative.

### C8 — State spill and cold-start honesty
**Objective:** state larger than RAM, and honest numbers about it. **Build:** RocksBackend
tuning pass (block cache, bloom filters — every constant into the ledger with its benchmark);
state-size accounting surfaced per operator/query (`EXPLAIN STATE`); admission control at
registration for undeclarable-bound queries (I-9). **Exit gate:** a scenario with operator
state 10× RAM completes with flat memory (the soak harness arrives here — RSS sampled across
the run, leak fails the job); `EXPLAIN STATE` numbers reconcile with actual backend usage
within a stated tolerance. **Pitfalls:** this is where "it worked on the laptop" dies; the gate
runs in CI at a fixed memory ceiling (cgroup), not on whatever the runner has free.

### C9 — schweepd: the server and subscriptions
**Objective:** one process, network doors, push results. **Build:** Arrow Flight + HTTP
endpoints (ingest, register, one-shot, read-at-epoch, subscribe); subscription delivery of
result-store deltas per sealed epoch; per-source admission + backpressure (bounded queues,
never unbounded buffering); graceful shutdown = checkpoint + drain. **Exit gate:** the
differential harness runs *over the network* (same scenarios, network door) green; kill -9
under load at 1,000 random points — every ack honored on recovery, no duplicate epochs
delivered to subscribers (subscribers get a resume token; re-delivery after resume is
exactly-once per epoch); soak: 24h CI-nightly window, flat memory. **Pitfalls:** subscription
resume is where exactly-once quietly becomes at-least-once; the resume token is the epoch
number and delivery is idempotent by construction — test it by crashing the *subscriber*.

### C10 — The performance sprint
**Objective:** stop leaving 10× on the table; earn the first public numbers. **Build:**
columnar/vectorized operator inner loops over Arrow batches (this is where `unsafe` may first
appear, under the D-1 inventory discipline); consolidate() optimization (sort+merge, the
hottest path); benchmark suite: (a) maintenance cost vs change volume, (b) standing-answer read
latency, (c) one-shot vs DuckDB on TPC-H SF0.1 *reported honestly as "their game"* — the paired
measurement method, median-of-paired-rounds, worst run published; (d) **the swarm benchmark**:
10,000 near-duplicate standing queries, cost of the marginal query — this is Schweep's game,
and the benchmark that defines the product. **Exit gate:** every number in the README traces to
a committed benchmark artifact; differential + crash suites still green (performance work may
not move a result byte — I-1 is the regression net). **Pitfalls:** do not chase (c) — losing
one-shot to DuckDB is expected and stated; the product is (a), (b), (d).

### C11 — Source-scoped retraction and the lineage hook
**Objective:** the MutinyDB seam, proven inside Schweep. **Build:** `retract_source(source_id,
predicate?)` — generate the retraction delta for everything a source ever contributed (from the
log/snapshot, by source_id), feed it through the ordinary path; result-store answers reflect
the world-without-that-source. **Exit gate:** differential gate: retract-source equals the
oracle re-run with that source's inputs removed from history, over the fuzz suite — including
through joins, aggregates, and shared memo subplans. **Pitfalls:** none new if I-5 held; this
sprint exists to *prove* I-5 cashes the check MutinyDB will write, and to find out early if it
does not.

### C12 — The accelerator spike (falsifiable, bounded)
**Objective:** decide the GPU cold path with evidence, not appetite. **Build:** a bounded spike
— one fused filter+aggregate kernel over Arrow data on GPU, fed by the one-shot path, measured
against the C10 CPU implementation on the same data at three sizes. **Exit gate:** a written
verdict in `docs/DECISIONS.md`: the measured speedup, the break-even batch size, and a
go/no-go for a GPU execution phase — with the no-go criteria written *before* the spike runs.
No production GPU code ships in this sprint regardless of verdict. **Pitfalls:** sunk-cost —
the spike is allowed to fail, that is the point of running it before committing.

### C13 — Hardening and v0.1 freeze
**Objective:** the API freeze and the honesty pass. **Build:** extended fuzz sessions
(differential + crash, order-of-magnitude longer runs); the limitations section of the README
written from the open issues, not from memory; `docs/current-api.md` with the compatibility
promise; the ledger audited (every tuned constant has its receipt); zero-flake sweep (any test
that flaked in the last 50 CI runs is fixed or deleted). **Exit gate:** nightly soak green ×
a full week of runs; every invariant I-1…I-10 has a named CI job; tag `current-v0.1`.

---

## §7 — Testing strategy (how we know it is true)

**The differential harness is the product's credibility** — everything else supports it.
Structure every correctness test as: scenario (seeded, reproducible) → run engine → run oracle
→ compare byte-for-byte at every sealed epoch. New feature = new scenario family in the
generator first, then the implementation. The generator must always produce: retractions,
weight multiplicities, same-epoch retract+insert of the same row, empty epochs, empty inputs,
and (post-C5) fuzzed SQL over fuzzed schemas.

**Crash injection** (C4 onward): randomized kill points across every durability boundary,
recovery compared byte-for-byte against the uncrashed twin. **Property tests** for the algebra
(Z-sets, canonicalization). **Soaks** (C8 onward): long runs with RSS curves, leak fails the
job. **Counter gates**: same-door (I-6) and memo (I-8) assert execution counters, not just
results — counters catch divergence before answers diverge. **Zero-flake policy**: a flaky
test blocks merge exactly like a failing one.

**Benchmarks are evidence, not marketing** (I-10): paired measurement, medians of paired
rounds, full ranges published, every README number backed by a committed artifact, and the
worst supported configuration quoted alongside the best.

---

## §8 — Non-goals for v1 (write them down so nobody "helpfully" adds them)

No distributed execution (one process; replicas via log shipping are post-v1). No recursive or
iterative queries (D-3). No full ANSI SQL (the dialect ladder is the dialect; window functions
are post-v1, evaluated by evidence of need). No user-defined functions (a sandboxing problem,
not a parser problem — post-v1). No CDC source connectors beyond the native log API (adapters
are ecosystem work, not engine work). No GPU production code before the C12 verdict. No bespoke
storage engine (D-5; boring is the feature). No multi-writer: one log, one writer, one epoch
clock in v1.

## §9 — Reading list (read in this order, before C0)

1. **DBSP: Automatic Incremental View Maintenance for Rich Query Languages** (Budiu,
   McSherry, Ryzhyk, Tannen — VLDB 2023). The theory this engine implements. Read it twice.
2. **Differential Dataflow** (McSherry et al., CIDR 2013) — the ancestor; skim for intuition.
3. The Feldera and Materialize public engineering blogs — the two teams who have shipped this
   class of engine; read their postmortems especially.
4. **Apache Arrow** columnar format docs — the data layer under every operator.
5. `sqlparser-rs` docs and the DataFusion logical-plan module — prior art for the frontend
   shape (we bind and plan ourselves, but their structure is worth studying).

## §10 — Working agreements (for every contributor, junior or agent)

Read `docs/SEMANTICS.md` before touching operator code; semantics change in the doc first,
oracle second, engine third. Every PR names the invariant it preserves, the scenario family
that covers it, and the gate that proves it. No `unwrap`/`expect`/panic in library code. No
wall clock, no randomness outside the seeded generators. If a test is skipped "for now," the
sprint is not done — stop and re-run. When the differential harness disagrees with you, the
harness is right until proven otherwise, and "proven otherwise" means the oracle had a bug,
which you fix first. And the standing rule inherited from every sibling repository: **we would
rather you read the tests than the marketing.**

---

*Apache-2.0, permanently. Every answer, current.*
