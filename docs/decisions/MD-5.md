# MD-5 · Forked standing state: the spike, its criteria, and the path taken

**Status:** Accepted
**Phase:** M5 (spike, then build)
**Roadmap:** `CONSOLIDATION-ROADMAP.md` §3 K-3, §4 M5
**Depends on:** MD-1 (the `compute → storage` edge M5 was explicitly *not* granted in advance),
MD-2 R7 (branch is a tag, not a clock), MD-6 (exact quarantined imports; sibling tracks are not
amended to suit this repository)

## Context

K-3 claims the hardest thing in this roadmap: substrate forks *data* in O(1), and MutinyDB should
fork *live answers* — a branch carrying copy-on-write references to its parent's circuit state, so
a hypothesis fork gets its own continuously-current standing answers for the cost of its
divergence. The roadmap reserved this number for the spike's verdict and pre-decided the fallback,
because the operator-state-on-CAS-pages layout problem is the named open research risk.

What exists at the time of deciding, read from the imported trees rather than from memory:

- **substrate** forks a content-addressed store in O(1) (manifest reference; the 98 ns figure is
  substrate's own, measured on its track).
- **LoomDB** already runs a copy-on-write B-tree over substrate pages (`loom-branch::Tree`:
  `open(&dyn PageStore)`, ordered `scan_prefix`, staged writes flushed into a substrate
  transaction). Its branches *are* CoW forks of tree state. This is the strongest evidence the
  layout can work at all, and the spike leans on it rather than inventing a second page B-tree.
- **Schweep's state surface is frozen.** `StateBackend` (§5.5, D-15) froze at C4's exit with
  `snapshot() -> Vec<u8>` and `restore(&[u8])` — a checkpoint is a *file*, O(state) to freeze, and
  the trait has no fork operation. The memo instantiates circuits engine-wide against one
  `BackendFactory`; the registry, the epoch clock, and the standing handles are engine-global.
  There is no per-branch circuit instantiation anywhere on the engine's surface, and MD-2's B3
  already rejected per-branch epoch clocks for v1.
- **MD-1's known tension**, recorded at M0: putting compute-plane operator state on substrate
  pages needs a `compute → storage` edge R1 does not permit. The edge was deliberately not
  granted; a GO verdict amends MD-1, a NO-GO needs no new edge at all.
- **What a session branch actually carries today (M3/M4):** the branch-scoped standing result
  stores mounted behind the trust plane — semantic top-k and grouping/aggregate operators, cloned
  per branch, memory-resident, rebuilt from the compute plane on restart. The engine's SQL
  circuits are tenant-global and branch-*tagged* (MD-2 R7), not branch-carried.

## Options considered

### Option A — GO: copy-on-write operator state on substrate pages

Branch-carried operator state lives in a CoW ordered store on substrate pages (the Loom-Tree
layout: rows under one key prefix, a rank index under another, aggregates under a third). Fork =
substrate fork: O(1), independent of state size. The M3 mount's clone-per-branch becomes a page
fork; durability comes with it for free, because the pages are already substrate's.

The costs it must prove it can pay: every standing-state update rewrites a leaf-to-root page path
per touched index on content-addressed 4 KiB pages (write amplification, on top of the data
write the delta already made); ranking needs an order-preserving byte encoding for scores; and —
the integration cost — every circuit family a branch carries at v1 must be able to adopt the
layout *without* amending a frozen sibling surface, because MD-1 R3/R4 forbid bending Schweep's
track to MutinyDB's schedule.

### Option B — Fallback (pre-decided by the roadmap): fresh circuits hydrated from the parent's checkpoint

Fork = hydrate the child's standing operators from the parent's current materialized state — a
deep copy, O(state) per fork, exactly what the M3 mount's clone already does — and make it
*durable and honest*: the fork becomes a recorded event on the ordinary commit path, recovery
rebuilds every branch's state from the commit history through the fork lineage, and the fork cost
is measured and published as O(state) wherever the O(1) claim might otherwise be assumed. O(1)
fork of live answers moves to post-v1, on the engine's own track. No new MD-1 edge.

## Decision

**The spike decides, against criteria fixed before it runs.** The spike builds a page-backed
standing top-k (rows + score-ordered rank index) and a scalar rollup on `loom-branch::Tree` over
a substrate store, forks it, diverges both sides, and measures. The criteria are falsifiable and
all four must hold for GO; any failure takes Option B, and a fallback taken deliberately is a
result, not a failure.

- **C1 · Fork is O(1) in state size.** Median fork latency (substrate fork + opening the child's
  tree) at 50,000 state entries ≤ 2× the median at 1,000 entries. A fork whose cost tracks state
  size is not the K-3 claim.
- **C2 · Bounded maintenance amplification.** Substrate pages written per single-row
  standing-state update at 50,000 entries ≤ 16 (two indexes × a B-tree spine budget of 8). Above
  that, per-delta fan-out makes R-3's write-amplification risk the default rather than the edge
  case.
- **C3 · Correct divergent answers.** After the fork, 200 interleaved updates per side: each
  side's top-k and rollup answers equal an independent in-memory model's at every step, and the
  un-written side's answer is byte-identical to its pre-fork rendering. Any divergence is
  disqualifying regardless of C1/C2.
- **C4 · v1 integration without sibling amendment.** Every circuit family a session branch
  carries at v1 must be adoptable onto the layout without modifying a frozen Schweep surface
  (`StateBackend` §5.5; the memo/registry's engine-global instantiation) and without MutinyDB
  amending a sibling's roadmap (MD-1 R3/R4, MD-6). Assessed in writing against the spike code and
  the imported trees, not measured.

### Verdict — recorded after the spike run (evidence: `crates/mutiny-forks/evidence/m5-spike.json`)

- **C1 PASS.** Median fork-and-open latency is flat in state size: 3.46 µs at 1,000 entries,
  4.63 µs at 10,000, 3.67 µs at 50,000 (frozen run; a second run measured 3.46/3.54/4.79 — the
  jitter between runs exceeds any size trend). The 2× bound holds with a wide margin: the fork is
  a manifest reference, exactly as K-3 hopes.
- **C2 — PASS on the median, FAIL on the worst case, and the criterion did not say which.**
  Median 5 pages per single-row update; worst case 171 in 200 updates (a node-split/rebalance
  cascade rewriting a long path). The criterion as committed fixed "≤ 16" without naming the
  statistic; resolving that ambiguity *after* seeing the numbers, in the favorable direction,
  is exactly what this repository's evidence culture forbids — so both readings are recorded and
  the worst-honest reading counts C2 as failed. Either way the amplification is real: even the
  median is 5× the data write the delta already made, paid per delta per forked query.
- **C3 PASS.** 200 child-only updates left the idle parent's rendering byte-identical; 200
  further interleaved updates per side matched the independent in-memory model's top-k and rollup
  at every step. The layout answers correctly.
- **C4 FAIL.** The layout is adoptable by the branch-carried operators MutinyDB owns — the spike
  is that adoption in miniature — but a session branch's standing answers at v1 are maintained by
  circuit machinery whose state sits behind Schweep's frozen `StateBackend` (§5.5: no fork
  operation, and `snapshot() -> Vec<u8>` is an O(state) freeze by design) and whose instantiation
  is engine-global (one factory, one registry, one epoch clock per MD-2 B3). Making circuit state
  forkable is Schweep-track work — a forkable backend contract or a per-branch instantiation
  surface — and MD-1 R3/R4 forbid this repository from amending that track or waiting on an
  unmade release. Partial adoption (pages for the mutiny-owned operators, clones for the rest)
  would ship the O(1) *claim* attached to a system that cannot honor it end-to-end.

**Verdict: NO-GO for v1 — Option B, taken deliberately.** C4 fails outright and C2 fails under
the worst-honest reading; C1 and C3 stand as evidence that the layout itself is viable, so the
post-v1 path is real: when Schweep's own track grows a forkable state surface, the CoW layout
resurfaces with these numbers as its baseline — including the split-cascade worst case that any
production adoption must bound first. MD-1 is **not** amended; the fallback needs no new edge.
Everywhere the O(1) claim might be assumed — the README's K-3 paragraph, `docs/M5-FORKS.md`, the
fork-cost ledger — the shipped economics are stated as what they are: **fork cost is O(state),
measured and published.**

## Consequences

*Drafted with the criteria for the roadmap's pre-decided fallback shape; confirmed by the
verdict.*

- **Fork is O(state), and it says so.** The M5 build (Option B) records every fork and rewind as
  an ordinary commit through the M1 front door (`mutiny_forks` on the tenant's epoch clock),
  hydrates the child's standing operators from the parent's live state, and publishes the
  measured cost in worst-honest form in `crates/mutiny-forks/evidence/m5-fork-cost.json`. The
  hydration clone *is* the "checkpoint" of the fallback's wording: the parent's materialized
  state, copied at fork time.
- **Durability is by lineage replay, not by state files.** Recovery rebuilds every branch's
  standing state by replaying the manifest capture history in commit order, cloning at fork
  records, dropping at rewind records, then applying the taint ledger's heals — the M1 crash
  discipline extended through the fork lineage. There is no second serialized-state artifact to
  drift from the log.
- **Merge follows Loom's law, composed.** A merge is new commits on the target through the front
  door; each merged write re-evaluated against policy *now*, all-or-nothing; each merged row
  carries its **own** original sources (Loom I-2's per-key rule, never the union) plus a durable
  merge marker that is the composed analog of Loom's merged-from memory — so a re-merge or a
  crash-resumed merge is a no-op, not a +6 (the exact double-count class Loom's oracle caught).
- **The M4 inherited-state limit closes.** With fork lineage durable, a taint heal cascades from
  the named branch to its descendants' inherited standing state (retract-by-key, skip-absent).
  M4's known-limits entry is superseded by the M5 mechanism for branches with recorded lineage.
- **What this deliberately does not do.** No per-branch epoch clocks (MD-2 B3 stands); no
  engine-side per-branch circuits; no `compute → storage` edge; no claim of O(1) fork anywhere.
  The post-v1 CoW path is an engine-track dependency, recorded here with its spike evidence, and
  it re-opens by amending this record — not by assuming it.
