# MD-3 · The unified SQL surface

**Status:** Accepted
**Phase:** M0 (grammar and phasing) · **Built across:** M2, M3, M4, M6
**Roadmap:** `CONSOLIDATION-ROADMAP.md` §2, §4 M2/M3/M4/M6
**Depends on:** MD-1 (semantic operators live inside compute), MD-2 (`mutiny_derivation`, epoch =
commit)

## Context

MutinyDB inherits three query surfaces. Current has a dialect ladder and a binder that refuses
anything outside it *by name* (its §5.6, D-4), with I-6 — the same-door law — asserting that SQL and
the typed API compile to identical plans and identical execution counters. PrismDB has a minimal
SQL subset with `embedding ≈≈ 'text'` (and the ASCII alias `~~`, because a Unicode operator that
some client mangles is an operator that gets worked around), plus S9's `GROUP BY
semantic_cluster(embedding, k)`, `NOVELTY(embedding) AGAINST (baseline)` and `SEMANTIC_DIFF(a, b, k)`
— gated at the engine level, with the keyword grammar deliberately following the semantics rather
than leading them. LoomDB's surface is branches, taint and RecallPlans, reached through MCP and a
typed API rather than through SQL at all.

M6's exit gate is "same-door law across all three doors (SQL, typed, MCP): identical plans,
identical counters". That gate is only reachable if there is exactly one dialect, decided before the
first operator is written, with each construct assigned to the phase that can actually prove it.
The failure this record prevents is the ordinary one: three surfaces converge into one binder,
each brings its own defaults, and the product ends up with two meanings for `LIMIT` and no one
willing to break either.

## Options considered

### Option A1 — Three doors, three dialects, one process

Ship `mutinyd` with Current's SQL for relational queries, Prism's SQL for semantic ones, and Loom's
MCP for trust operations, sharing only the storage beneath.

Cheapest, and it forfeits the product. A hybrid standing query (`≈≈` *and* scalar predicates, M2's
exit gate) has no door to be asked at, and the same-door law becomes three same-door laws that say
nothing about each other.

### Option A2 — One dialect, owned by MutinyDB, lowering into Current's plan algebra

A single binder in this repository: Current's dialect ladder as the base, extended with the
semantic, temporal and provenance constructs, lowering to Current's logical plan plus a small set of
new plan nodes that the incrementalizer knows how to rewrite.

This is the shape M6 requires. The cost is a real one and worth naming: MutinyDB's binder must track
Current's binder as Current's dialect grows, or the two drift — a public engine and a private
consolidation with the same name for different behaviour, which is the MD-4 problem wearing a
different hat.

### Option A3 — Push the whole unified dialect upstream into Current

Make Current's binder speak `≈≈`, `AS OF BRANCH` and `TAINTED BY` natively; MutinyDB just supplies
the implementations.

Rejected by MD-1 R4 and by Current's own architecture: Current is standalone and *nothing in it may
depend on substrate, LoomDB or PrismDB*. `AS OF BRANCH` has no meaning without a fork; `TAINTED BY`
has no meaning without envelopes. Pushing them upstream would make Current's dialect describe
capabilities Current does not have — the exact "two masters" fork §6 forbids.

### Option B1 — `≈≈` as a boolean predicate with a similarity threshold

`WHERE embedding ≈≈ 'a failure'` means "similarity above some cutoff", the way a `LIKE` behaves.

Reads naturally and is a trap. It requires a magic constant nobody can defend, it makes the result
set's size a function of an embedding model's calibration, and under incremental maintenance it
means a model generation change silently changes the membership of every standing answer.

### Option B2 — `≈≈` as a ranking that requires a bound

`≈≈` orders by similarity and is **refused by name unless the query bounds it** — a `LIMIT`, or an
explicit `WITH SIMILARITY > x` written by the asker who then owns the constant.

Matches how Prism's own tests use it (`… ≈≈ 'the tool call timed out' LIMIT 10`), keeps the
incremental top-k operator's state bound declarable (Current I-9), and refuses the unbounded case
loudly instead of scanning a cold tier by accident.

### Option C1 — `AS OF` uniformly available on standing and one-shot queries

Let any query carry `AS OF`, standing ones re-pinning as time moves.

Incoherent for half the cases: a standing query is *by definition* maintained at the latest sealed
epoch, so a standing query pinned to epoch 40 is a one-shot with extra machinery, while a standing
query pinned to a branch is a perfectly ordinary standing query with a scope.

### Option C2 — `AS OF` splits into scope and pin

**Scope** (`AS OF BRANCH b`) selects which world the query runs in and is legal for standing
queries. **Pin** (`AS OF EPOCH n` / `AS OF COMMIT '<hex>'` / `AS OF TIME t`) fixes an instant and is
legal only for one-shot reads — with the one exception that a pin against a *retained* epoch of an
already-registered standing query is answered from its result store (Current C6's read-at-epoch),
because that is a read, not a second circuit.

### Option D1 — `TAINTED BY` computed as a recursive closure at query time

Walk `mutiny_derivation` transitively when the predicate is evaluated.

Impossible in v1 and worth being explicit about: Current's D-3 puts recursive and iterative circuits
out of scope, so there is no incremental transitive closure to compile to. A one-shot-only
`TAINTED BY` would then behave differently from every other predicate in the dialect.

### Option D2 — Derivation edges are transitively closed at write time

When a write's read set includes a fact, the bridge records not just that fact as a source but the
sources *that fact* already carries (MD-2's `mutiny_derivation`, written pre-closed). `TAINTED BY`
is then a single equi-join against a relation — an ordinary, incrementally maintainable predicate,
with no recursion anywhere.

Pays in edge fan-out on the write path for a property nothing else can buy: taint is a join, so it
is incremental, so M4's flagship gate is reachable without a nested clock. This also happens to be
what makes MD-2's `taint(S)` a two-step of ordinary operations rather than a graph walk.

## Decision

**A2 + B2 + C2 + D2.** One dialect, owned here, phased against the gates that can prove each rung.

### The surface

**Base.** Current's dialect ladder verbatim (its §5.6): (1) `SELECT`/`WHERE`/projection with the
scalar expression library; (2) inner equi-`JOIN`; (3) `GROUP BY` + `SUM/COUNT/MIN/MAX/AVG` +
`HAVING`; (4) `DISTINCT`, `UNION ALL`, `ORDER BY`/`LIMIT` at read time; (5) `LEFT JOIN` and
decorrelatable subqueries. Three-valued logic from rung 1. Every ordering carries a total tiebreak
(D-7). Anything outside the ladder is refused **by name**, never silently accepted.

**Semantic.**

```sql
SELECT event_id FROM events
 WHERE embedding ≈≈ 'the tool call timed out'      -- ASCII alias: ~~
   AND cost < 0.02
 LIMIT 10;

SELECT count(*) FROM events
 WHERE ts > ?
 GROUP BY semantic_cluster(embedding, 8);
```

- `≈≈` ranks; it does not filter. A query using `≈≈` without a bound (`LIMIT n`, or an explicit
  `WITH SIMILARITY > x`) is refused by name. No default cutoff exists, at any layer.
- The right operand is text, embedded at bind time in the **query's pinned generation**. Mixing
  generations in one comparison is refused, not merged — Prism's rule, carried over intact: no
  score-space merging without a rank bridge.
- `semantic_cluster`'s `cluster_id` is scoped to the one query that produced it and means nothing
  outside it; group order is size-descending with the exemplar tiebreak; `k` is capped by policy and
  a query over the cap is refused with a named limit, never silently clamped. `NOVELTY … AGAINST`
  and `SEMANTIC_DIFF` follow at the same rung.

**Temporal.**

```sql
SELECT ... FROM t AS OF BRANCH 'hypothesis-3';                 -- scope; legal standing
SELECT ... FROM t AS OF COMMIT 'b3f1…';                        -- pin; exact, unambiguous
SELECT ... FROM t AS OF EPOCH 4172;                            -- pin; epoch = commit_seq (MD-2 R1)
SELECT ... FROM t AS OF BRANCH 'main' TIME '2026-08-09T14:00Z';-- pin, resolved at bind time
```

- `AS OF COMMIT` is the exact form and the one an audit narrative should quote: a `ManifestId` is a
  content address, so it names one world and cannot drift.
- `AS OF TIME t` resolves at bind time to *the last commit in WAL order whose `created_at_ms` ≤ t*
  — WAL order, not timestamp order, because MD-2 R4 inherits substrate's rule that `created_at_ms`
  is wall clock and never steers a decision. If the scanned range is non-monotonic in
  `created_at_ms` (an operator moved the clock), the bind **refuses and names the two commits**
  rather than picking one. The resolved commit is echoed in `EXPLAIN`, so a reader always sees which
  world answered.
- A pin against an epoch that compaction has discarded, with no snapshot covering it, is refused —
  never silently answered from the nearest surviving epoch.
- `AS OF BRANCH` is a scope and composes with everything, including standing registration. Pins do
  not: a standing registration carrying a pin is refused by name, except the read-at-epoch case in
  C2 above.

**Provenance.**

```sql
SELECT * FROM claims WHERE TAINTED BY 'crm:acct-42';
```

- `TAINTED BY <source-literal>` is a predicate over the transitively-closed `mutiny_derivation`
  relation (D2), with the source spelled in Loom's `system:record_id` form. It is an ordinary
  predicate: it composes with joins and aggregates, it is incrementally maintained, and it works in
  standing queries.
- It is a **query**, not a mutation. The mutation is `taint(S)` (MD-2, M4), which is not SQL — it is
  a trust-plane operation that produces a RecallPlan and a receipt. That separation is deliberate: a
  `DELETE`-shaped SQL statement that quietly suspends accounts is exactly the affordance MutinyDB
  should not have.

**Explain.** `EXPLAIN` reports the plan and the resolved `AS OF` world; `EXPLAIN STATE` (Current C8)
reports per-operator state; `EXPLAIN MAINTENANCE` (R-3's mitigation) reports the per-epoch
maintenance cost of a standing query, including the derivation-edge fan-out MD-2 introduces. A
standing query whose maintenance cost cannot be declared is admitted only explicitly (I-9).

### Phasing

| Construct | Lands in | Needs | Proven by |
| --- | --- | --- | --- |
| Ladder rungs 1–5 | inherited | Current C5 | Current's binder corpus + differential harness |
| `≈≈` (bounded), embedding-at-ingest | **M2** | Current C5–C6 | hybrid standing query equals Prism's one-shot answer at every epoch, frozen golden corpus |
| `GROUP BY semantic_cluster`, `NOVELTY`, `SEMANTIC_DIFF` | **M2** | Current C5–C6 | Prism's engine-level gates, re-run through the SQL door |
| Generation pinning + refusal to merge score spaces | **M2** | — | two generations live; cross-space merge refused |
| `AS OF BRANCH` (scope) | **M3** | Current C6 | Loom's isolation oracle over branch-scoped result stores |
| `AS OF COMMIT` / `AS OF EPOCH` (pin) | **M3** | Current C6–C7 | read-at-epoch equals a one-shot over the same world |
| `AS OF TIME` (pin, bind-time resolution) | **M3** | C7 | the non-monotonic-clock refusal test |
| `TAINTED BY` | **M4** | M1 + M3 + Current C11 | the frozen incident corpus: rows returned equal the oracle's downstream-of-S set |
| `EXPLAIN MAINTENANCE` | **M4** | C8 | numbers reconcile with measured per-epoch cost within a stated tolerance |
| Same-door law across SQL / typed / MCP | **M6** | C9 | identical plans and identical counters through all three doors |

Nothing is bound before its phase. A construct that parses but is not yet implemented is refused by
name, with the phase in the message ("`TAINTED BY` is M4") — never accepted and ignored.

### The rule that keeps the dialect honest

Adopted verbatim from Current's working agreements, because it is the rule that catches the bug the
differential harness cannot: **every dialect change adds a row to the binder corpus (text ↔ expected
plan) and every refusal adds a row to the refusal corpus.** I-6 makes both doors compile to identical
plans, so a binder that maps text to a *valid but wrong* plan produces the same wrong plan through
every door, the same answer as the oracle for the query it actually compiled, and a green sweep. The
corpus is the only thing standing between "the text means what MD-3 says" and "the text means
whatever the binder does."

## Consequences

- **MutinyDB's binder must track Current's.** A2's stated cost. Mitigation: the base ladder is
  consumed from Current's crates at a pinned tag (MD-1 R3) rather than reimplemented, so a divergence
  is a compile error at upgrade time rather than a semantic drift. Only the four extensions above are
  written here.
- **`≈≈` without a bound is refused, and someone will find that annoying.** It is the right
  annoyance. The alternative is a default threshold that silently defines every semantic answer in
  the product, chosen once, by whoever wrote the constant.
- **D2 moves cost from read to write.** Pre-closing derivation edges makes `TAINTED BY` a join and
  makes M4 reachable without recursive circuits, and it makes the write path's fan-out a function of
  derivation depth. High-derivation-depth tenants — long agent chains where each claim cites the
  previous — are the worst case, and the number belongs in `EXPLAIN MAINTENANCE` before anyone is
  surprised by it. If it proves untenable, the fallback is edge compaction (collapse chains through
  facts that are themselves fully retracted), which is a change to the bridge, not to this grammar.
- **`AS OF TIME` can refuse.** A clock that went backwards makes "as of 14:00" genuinely ambiguous,
  and this dialect says so instead of guessing. Callers who cannot tolerate a refusal have
  `AS OF COMMIT`, which is exact by construction.
- **Loom's MCP surface is not replaced by SQL.** Trust operations (open a branch, propose an action,
  taint a source) stay typed operations with envelopes and policy decisions attached. M6 makes the
  three doors agree on *queries*; it does not turn governance into a statement.
