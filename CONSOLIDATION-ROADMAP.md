# MutinyDB — Consolidation Roadmap

> **Status:** Authoritative sequencing for consolidating **Current** (the incremental query
> engine), **LoomDB** (the agent trust plane), and **PrismDB** (the semantic plane) — on
> **substrate** (the storage plane) — into one offering: **MutinyDB**, the agent-native
> enterprise database. FlockDB is not a component: its DuckDB kernel is retired, its fleet-plane
> concepts (registry, sleep/wake economics, per-tenant pools) are absorbed here as the operations
> layer.
>
> **Sprint milestones, no timelines.** Phases M0–M8; a phase is done when its exit gate is green.
> This repository is private during consolidation.
>
> **Prerequisite discipline:** each M-phase names the Current sprint gates (C#, from
> `Current/ARCHITECTURE.md`) it depends on. Current is built standalone and consumed here by
> pinned tag — never by branch, never by fork. The failure mode this prevents is the one
> substrate's docs/04 named: the shared core quietly forking to serve two masters.

---

## §1 — Where the inputs stand today (status snapshot at roadmap time)

- **substrate** — v1.3.0, API frozen, feature-complete. Crash-injection suite at 10k+ cycles;
  real airgap amputation (`remote` feature); `TieredStore::get_batch` (batched multi-page fetch)
  landed in v1.3.0 and is the primitive the wide-area wake fix will use.
- **LoomDB** — v0.2 line complete through L5: sessions-as-branches, record-level merge, four
  model oracles, taint → RecallPlan, policy/action gateway, loomd MCP server, multi-tenant
  signed-token router, both soaks (flat memory), no-egress + airgap CI, signed offline bundles.
  Known open items, stated in its README: wide-area wake p99 measured at ~1020 ms vs the 250 ms
  target (needs fault coalescing over get_batch — planned v0.3); retrieval default is O(entries)
  scan with opt-in per-branch HNSW whose build is super-linear (v0.3 makes it incremental).
- **PrismDB** — S0–S11 complete (including object-storage cold tier, catalog mirror with
  split-brain detection, MinIO-gated publication boundaries); S12 (distribution) in flight:
  tenant-bucket sharding with catalog-as-authority (D-071), cluster-global content-addressed
  generations (D-072), and the phased candidate/rerank search split with a frozen-monolith
  oracle (D-073) already landed. Byte-identical answers proven across 1/2/4-way shard layouts.
- **Current** — architecture of record committed (`Current/ARCHITECTURE.md`); build begins at C0.

Implication for sequencing: Loom and Prism are *far* ahead of Current in maturity. The
consolidation therefore begins with interface contracts and continues their in-flight roadmaps
(Loom v0.3, Prism S12+) unchanged — nothing here pauses them. Current catches up on its own
track; integration phases begin only at the Current gates named below.

## §2 — What MutinyDB is

One database, four planes:

```
TRUST PLANE      (from LoomDB, nearly whole)
  sessions-as-branches · write envelopes · observations vs claims ·
  policy + propose-not-execute action gateway · merge engine
  NEW HERE: taint-and-recall implemented as source-scoped RETRACTION
  through the compute plane — answers self-heal database-wide.

COMPUTE PLANE    (Current)
  every query a standing computation, O(change) · one-shot via the same
  circuits · the memo (shared circuitry across queries/agents) ·
  PrismDB's semantic operators (≈≈, semantic GROUP BY, generations)
  compiled into circuits as first-class operators.

SEMANTIC PLANE   (from PrismDB)
  meaning-clustered immutable parts as the cold/scan tier · centroid
  index + PQ scan for cold semantic queries · embedding generations as
  schema events · the exact-oracle recall discipline.

FLEET/OPS PLANE  (absorbed from FlockDB's F4–F5 concepts)
  per-tenant pools · registry · sleep/wake — sharpened to WAKE-ON-DELTA:
  an idle tenant's circuits sleep as bytes; an arriving delta wakes
  exactly the circuits it touches.

STORAGE PLANE    (substrate, frozen)
  content-addressed pages · O(1) fork/snapshot/rewind · WAL/DurableStore ·
  sleep/wake · NEW ROLE: the commit stream IS the delta feed — a commit's
  changed pages, diffed via manifests, become Current's epoch input.
```

## §3 — The three keystone unifications (why the merge is worth it)

**K-1 · Commit-as-delta.** Every incremental engine's weak point is change capture; substrate
emits deltas as a physical byproduct of committing (a commit = exactly the changed pages; a
manifest diff = O(changed)). MutinyDB feeds Current's epochs directly from substrate commits:
one durability boundary, one time axis, no CDC adapter, no lag ambiguity. Epoch = commit.

**K-2 · Taint-as-retraction.** Loom's flagship ("which of my 400,000 facts are downstream of the
poisoned source, and undo exactly those") currently requires walking its provenance DAG and
planning recalls. In MutinyDB, `taint(S)` = `retract_source(S)` pushed through Current's
circuits (Current C11 proves the primitive): every derived claim, rollup, and telemetry
aggregate corrects itself by the same propagation that keeps dashboards current. Loom's DAG and
envelopes remain — they are the *evidence and policy record* (who, why, may-it-act) — but the
mechanical recall becomes engine physics. The RecallPlan's two-section honesty is unchanged:
irreversible *actions* still cannot be retracted by any engine, and are still listed first,
with receipts.

**K-3 · Forked standing state.** substrate forks data in O(1); MutinyDB forks *live answers*:
a branch carries copy-on-write references to its parent's circuit state, so an agent's three
hypothesis branches each get their own continuously-current view for the cost of their
divergence. This is the hardest new engineering in MutinyDB (M5) and its most defensible
capability — no shipping system has it.

## §4 — Phases

### M0 — Charter and contracts *(no Current prerequisite)*
Repo: this one, private. Write the decision records that keep four codebases honest:
**MD-1** the plane boundaries and what may depend on what (trust → compute → storage; semantic
operators live inside compute; nothing depends "sideways"); **MD-2** the Delta Bridge contract
(the exact mapping from a substrate commit + Loom envelope to a Current epoch input: schema,
source_id conventions, tenant/branch tagging on every delta); **MD-3** the unified SQL surface
(Current's dialect + `≈≈` + `AS OF BRANCH/TIME` + `TAINTED BY` grammar, and which phase each
predicate lands in); **MD-4** naming/trademark sweep for "MutinyDB" and "Current" before
anything public. Continue Loom v0.3 and Prism S12 unchanged.
*Exit:* MD-1…MD-4 merged; CI skeleton (fmt/clippy/test/no-egress) green on an empty workspace.

### M1 — The Delta Bridge *(needs Current C4: log + exactly-once)*
Build `mutiny-bridge`: substrate commit stream → Current epoch inputs, per MD-2. Envelope
required on every delta at admission (Loom's I-7 discipline, now engine-wide); source_id and
branch/tenant tags stamped at birth. Prove exactly-once across the seam: a substrate commit is
reflected in exactly one epoch, crash anywhere between commit and seal included (compose
substrate's crash harness with Current's).
*Exit:* differential gate — a randomized commit history replayed through the bridge yields
byte-identical Current state to a direct-ingest control; crash matrix green across the seam.

### M2 — Semantic operators into the compute plane *(needs Current C5–C6)*
Prism's `≈≈`, embedding-at-ingest, and semantic GROUP BY become Current operators: embedding
runs in the bridge (a delta arrives with its vector, generation-pinned exactly as Prism's
ingestion contract demands); incremental top-k-by-similarity and incremental semantic grouping
as stateful operators with declared bounds; Prism's meaning-clustered parts serve as the cold
tier for one-shot semantic queries. Prism's recall contracts and exact-oracle discipline port
verbatim — recall receipts with tails, no score-space merging without a rank bridge.
*Exit:* hybrid standing query (`≈≈` + scalar predicates) maintained incrementally, equal at
every epoch to Prism's one-shot answer over the same integrated data, on the frozen golden
corpus; generation migration runs with two generations live and refuses cross-space merges.

### M3 — Trust plane over the compute plane *(needs Current C6; parallel-safe with M2)*
Loom's branches, capability tokens, policy engine, and action gateway mount over the shared
catalog: a session opens a branch (substrate fork), reads flow through branch-scoped result
stores, policy consults evidence that may now cite *any* plane's data. Loom's four model
oracles run unmodified against the mounted configuration — they are the proof the port
changed nothing.
*Exit:* the L4 scripted demo (docs/04 §3.1) runs verbatim on MutinyDB — same asserted
moments — with Loom's oracles green under fuzz.

### M4 — Taint-as-retraction *(needs M1 + M3 + Current C11)*
K-2, built: `taint(S)` resolves S's contributions via envelopes + the log, emits the
source-scoped retraction, propagates through all circuits (memory, analytics, semantic alike);
the RecallPlan is generated from the propagation receipt — irreversible actions first, from
Loom's action ledger, then the reversible writes *the engine already corrected*.
*Exit:* **the flagship gate** — the frozen incident corpus (poisoned source; sessions;
branches; claims; rollups; telemetry; one executed action): `taint(S)` yields (a) every
standing answer database-wide equal to the oracle's world-without-S, (b) a RecallPlan naming
the suspended account with its receipt first, (c) the audit narrative citing envelopes. This
demo is the company; it runs in CI from this phase forever.

### M5 — Forked standing state *(needs M3; spike first)*
K-3, built: branch = copy-on-write circuit state on substrate pages. Explicit spike gate
before the build (the operator-state-on-CAS-pages layout problem is the open research risk in
this roadmap — the spike's no-go triggers the fallback, recorded in MD-5: branches get fresh
circuits hydrated from the parent's checkpoint, correct but O(state) per fork, and O(1) fork
of live answers moves to post-v1).
*Exit (build path):* fork a session with live standing queries; both branches' answers track
their own writes and only theirs (Loom's isolation oracle, now over circuits); fork cost
measured and published honestly; merge re-runs policy per Loom's merge rules.

### M6 — One surface *(needs M2–M4; C9)*
`mutinyd`: currentd's Flight/HTTP + loomd's MCP surface, one process, one admission boundary
(Prism's quota/round-robin discipline), the unified dialect per MD-3. Same-door law across all
three doors (SQL, typed, MCP): identical plans, identical counters.
*Exit:* the M4 flagship demo driven end-to-end over MCP by the scripted agent; kill -9 matrix
across the whole stack; soak with flat memory.

### M7 — Fleet plane and wake-on-delta *(needs M6)*
Per-tenant pools (one substrate pool per tenant — Loom's isolation model, now the system's);
registry; sleep = checkpoint circuits + substrate sleep; wake-on-delta = an arriving delta
wakes only the circuits it feeds (the registry knows the mapping); the 10,000-sleeping-tenants
simulation. Wide-area wake numbers published with RTT stated, using get_batch coalescing —
closing Loom's known 1020 ms item *here*, once, for the whole system.
*Exit:* 10k-tenant sim on one host; wake-on-delta correctness (a woken circuit's answers equal
a never-slept control); honest wide-area latency table in the README.

### M8 — Harden, audit, name *(needs everything)*
Extended fuzz + crash + soak across the composed system; the evidence-ledger audit; per-tenant
encryption posture inherited from substrate P6; airgap certification (Loom's suite, extended
to the composed stack); the limitations README written before any external eyes; execute MD-4
(public naming, repo topology: what of loomdb/PrismDB/Current redirects where).
*Exit:* every M-gate re-green on the release candidate; both soaks a full nightly window;
tag `mutinydb-v0.1`.

## §5 — Risks

**R-1 · Current is the youngest plane and the critical path.** Mitigation: M0–M3 sequencing
front-loads everything that doesn't need it; Loom/Prism roadmaps continue independently; the
C-gates named per phase make "Current isn't ready" a visible fact, not a surprise.
**R-2 · Forked standing state may not yield O(1)** — pre-decided fallback in M5; the product
survives on the fallback (correctness intact, economics deferred).
**R-3 · Write amplification.** High-churn tenants with many standing queries pay per-delta
fanout; mitigation: per-query maintenance accounting (Current C8's `EXPLAIN STATE` extended
with `EXPLAIN MAINTENANCE`), admission control, and the documented demotion path to one-shot.
**R-4 · Four repos, one person + agents.** Mitigation: pinned-tag consumption everywhere,
oracle-gated integration phases, and the rule that no phase starts before its named gates.
**R-5 · The name.** MD-4 trademark/collision sweep before anything public — the FlockDB lesson,
learned once, applied twice.

## §6 — Repo topology during consolidation

`substrate` — frozen, versioned, consumed by tag (now v1.3.0). `loomdb`, `PrismDB` — active,
their own roadmaps (v0.3, S12+), consumed by tag; they re-base onto MutinyDB planes only at
the M-phase that absorbs them, with their oracles as the proof of faithful port. `Current` —
public, standalone, built per its ARCHITECTURE.md; consumed by tag from M1. `MutinyDB` (this
repo) — private: the bridge, the mounted planes, the composed gates, and the docs of record.
Nothing in any sibling repo may depend on MutinyDB.

---

*The name is the thesis: a mutiny against the one assumption every database of the last two
decades shares — that an answer must be recomputed to be trusted. Apache-2.0 when it goes
public; private until M8 says otherwise.*
