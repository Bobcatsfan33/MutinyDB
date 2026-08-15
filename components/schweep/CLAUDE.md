# CLAUDE.md — how work is done in this repository

`ARCHITECTURE.md` is the architecture of record. It governs everything: the glossary (§1), the
binding decisions D-1…D-9 (§3), the invariants I-1…I-10 (§4), the crate map (§5), the sprint
gates (§6), the testing strategy (§7), and the non-goals (§8). **If your code would contradict
it, the document wins.** If the document is truly wrong, record a superseding note in
`docs/DECISIONS.md` *first*, then change the code.

Read `ARCHITECTURE.md` before working in this repository. This file is a working companion to
it, not a replacement for it.

---

## The invariants (§4, verbatim)

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

## Working agreements (§10, verbatim)

Read `docs/SEMANTICS.md` before touching operator code; semantics change in the doc first,
oracle second, engine third. Every PR names the invariant it preserves, the scenario family
that covers it, and the gate that proves it. No `unwrap`/`expect`/panic in library code. No
wall clock, no randomness outside the seeded generators. If a test is skipped "for now," the
sprint is not done — stop and re-run. When the differential harness disagrees with you, the
harness is right until proven otherwise, and "proven otherwise" means the oracle had a bug,
which you fix first. And the standing rule inherited from every sibling repository: **we would
rather you read the tests than the marketing.**

---

## Sprint protocol

- **One sprint per session.** A session works on exactly one sprint from §6. Sprint scope is
  what §6 says it is — not less, and emphatically not more.
- **A sprint is done only when its exit gate is green in CI.** Not green locally; green in CI.
  "Green" means every job: fmt, clippy `-D warnings`, tests, and the no-network job.
- **Never start the next sprint in the session that finished one.** Finishing a sprint ends the
  session's work. The next sprint begins in a new session, with a fresh reading of
  `ARCHITECTURE.md` §6.
- **Nothing is skipped "for now."** If a test is disabled, ignored, or marked TODO, the sprint
  is not done. Stop, fix it, re-run the gate.
- **`docs/PROGRESS.md` is updated as part of the sprint**, stating what is proven and by which
  named test. A claim in `PROGRESS.md` without a test that proves it is a violation of I-10.
  **Its status table at the top counts.** Four sprints once ran green while that table still said
  "not started", because the table was treated as something to update afterwards and the sections were
  treated as the real work. A sprint that adds its section without adding its row has not finished.

### Sprint order (§6)

C0 oracle + harness + rules · C1 linear operators + first circuit · C2 join · C3 aggregates and
distinct · C4 durability · C5 SQL frontend + incrementalizer · C6 memo · C7 one-shot + Parquet +
compaction · C8 state spill · C9 schweepd · C10 performance · C11 source-scoped retraction ·
C12 accelerator spike · C13 hardening and v0.1 freeze.

Serial by default. Do not pull work forward from a later sprint because it is convenient.

---

## Hard rules that bite in day-to-day code

These are consequences of the invariants and the decisions, restated where they get violated.

1. **No `unwrap`, `expect`, or panic in library code.** Return `Result`. Panicking is acceptable
   only in tests and test harnesses. (§10)
2. **No `unsafe`.** Forbidden until C10, and then only under the D-1 inventory-and-safety-argument
   discipline. Every crate carries `#![forbid(unsafe_code)]` until that sprint.
3. **No wall clock, no randomness outside the seeded generators.** No `SystemTime::now()`, no
   `Instant::now()`, no `rand::thread_rng()`, no un-seeded anything, anywhere in engine or oracle
   code. Randomness lives in the scenario generator and is seeded and reproducible. (D-6, I-2)
4. **No dependence on iteration order of a hash map.** Use `BTreeMap`/`BTreeSet`, or sort
   explicitly before emitting. Two runs must produce byte-identical output. (I-2)
5. **Never special-case a negative weight.** If you write `if weight < 0` inside an operator —
   outside MIN/MAX multiset bookkeeping or the sign logic in `distinct` — you are re-deriving a
   bug. (I-5)
6. **Every ordering has a total tiebreak:** the declared sort keys, then all remaining columns in
   schema order. "Nondeterministic order" is not a thing this engine is allowed to have. (D-7)
7. **Semantics change in the doc first**, `docs/SEMANTICS.md`, then the oracle, then the engine.
   In that order, every time. (§5.6, §10)
8. **The oracle is the spec.** When the harness disagrees with you, it is right until you prove
   the oracle had a bug — and then you fix the oracle first. (§5.1, I-1)
9. **No performance number anywhere** — README, commit message, docs, comment — without a
   committed reproducible benchmark artifact. No tuned constant that steers behavior without an
   entry in `testing/evidence/registry.json`. (I-10)
10. **The scenario generator emits retractions from day one.** Never weaken the generator to make
    an implementation pass. The generator defines the bar. (§6 C0 pitfalls)
11. **`crates/schweep-sql/tests/binder.rs` is the semantic gate for the SQL door.** Every dialect
    change adds a row to its text↔expected-plan corpus, and every refusal adds a row to
    `crates/schweep-sql/tests/dialect.rs`. *The differential harness cannot do this job.* I-6 makes
    the two doors compile to identical plans, so a bug that binds SQL text to a **valid but wrong**
    plan produces the same plan through both doors, the same answer as the oracle for the query it
    actually compiled, and a green sweep. The only thing standing between "the text means what
    S-11…S-36 say" and "the text means whatever the binder does" is that corpus. Never add dialect
    surface without it. (C5's flag, recorded because the next dialect session will need it)

## Commit style

Small, honest commits. State what is proven, and by which test. A commit that adds a capability
without a test that pins it should say so out loud rather than imply coverage it does not have.
