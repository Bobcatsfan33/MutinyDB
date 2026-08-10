# MD-2 · The Delta Bridge contract

**Status:** Accepted
**Phase:** M0 (contract) · **Built at:** M1 (`mutiny-bridge`), needs Current **C4**
**Roadmap:** `CONSOLIDATION-ROADMAP.md` §3 K-1, §4 M1
**Depends on:** MD-1 R1 (`compute → bridge → storage`), MD-1 R3 (siblings by pinned tag)

## Context

K-1 is the keystone: *every incremental engine's weak point is change capture; substrate emits
deltas as a physical byproduct of committing.* This record fixes the exact mapping from a substrate
commit plus a Loom write envelope to a Current epoch input, so that M1 builds a translation and not
a design.

The three sides, as they exist today at their pinned versions — read from the code, not from
memory:

**substrate v1.3.0.** `commit(txn) -> ManifestId`, where `ManifestId` is a 32-byte content address.
A `Manifest` carries `parent: Option<ManifestId>` (the history edge — the commit DAG that branch
trees walk), `created_at_ms: u64` (wall clock, *explicitly never used for an internal decision*),
`schema_version: u32`, `page_size`, `depth`, `page_count`, and a page map held in a `BTreeMap` so
that replaying the same WAL twice produces byte-identical manifests. Page maps diff in O(changed).
One store has one WAL, and therefore one total order over its commits.

**LoomDB v0.2.** `WriteEnvelope { actor, session, branch, context_hash: [u8; 32], delegation:
Vec<ActorId>, derived_from: Vec<SourceRef>, intent: String, policy: Option<PolicyDecisionId>,
signature }`, with a canonical, length-prefixed `signing_bytes()` that deliberately excludes the
signature. `SourceRef { system, record_id }`, displayed `system:record_id`. **No write exists
without an envelope** — enforced at the write entry point, not as middleware, since L1. And
`derived_from` is *engine-captured*: callers may add external sources, they may not omit what they
read, because an agent must not be able to launder a derivation by declining to mention it.

**Current, at C4.** `Log::append(source_id: &str, table: &str, entries: Vec<(Row, i64)>,
dedup_token: &str) -> Result<Ack>`, `Ack::{Appended, DroppedAsReplay}`; the same token with the
same content is a replay that is acknowledged and dropped, the same token with different content is
refused loudly (I-4). `seal_epoch() -> Result<Epoch>`; epochs are dense integers from 1.
`EpochDeltas` holds `BTreeMap<String, Vec<(Row, i64)>>` — per table, ordered, because iteration
order must be a function of the data alone (I-2). A retraction is a negative weight through the
same code path (I-5). And `Batch.source_id` already carries the comment naming this seam.

Three questions have to be answered exactly, or M1 will answer them by accident.

## Options considered

### Option A1 — Change capture from the manifest page diff (physical)

Diff the committed manifest against its parent, decode the changed pages, and derive row-level
Z-set entries from the bytes.

Maximally faithful to "the commit *is* the delta", and it needs nothing from the layer above. It
also requires the bridge to contain a second, independent decoder for every table's physical
layout. That decoder is a fork of the storage format's meaning, living in a different crate from
the writer, and it will drift — silently, since both sides will keep passing their own tests. It
also cannot recover *intent*: a page diff knows bytes changed, not that a row was updated (which in
Z-set terms is −1 for the old row and +1 for the new one, and getting that wrong is a correctness
bug that only shows up through an aggregate).

### Option A2 — Change capture from the logical write set, captured at commit time

The layer that performs the write already knows the logical records: it has the rows and the
envelope. Capture the write set in the same transaction that produces the commit, key it by the
resulting `ManifestId`, and hand it to the bridge.

One decoder, owned by the writer. Intent survives (an update is emitted as the pair it is). The
risk is the mirror image of A1's: nothing structurally guarantees that the captured write set
*covers* what the commit actually changed. A bug that drops a write from the capture produces a
commit whose deltas are incomplete, and no test that looks only at the capture will see it.

### Option A3 — A2, with the manifest diff as a completeness audit

Logical write set is authoritative; the manifest page diff is computed alongside and asserted to be
*explained by* the write set — every changed logical page is accounted for by at least one captured
record, and no captured record claims a page the commit did not touch.

Costs one O(changed) diff per commit, which is the cheapest operation substrate has. Buys the one
property A2 cannot give itself: a dropped write is a loud failure at the seam rather than a quiet
divergence three planes later.

### Option B1 — One epoch per batch of commits (time- or size-triggered)

Group commits into epochs by a sealing policy, as a streaming system would.

Throughput-friendly and wrong for this product. It re-introduces the lag ambiguity K-1 exists to
delete, it makes "as of commit X" un-askable, and it gives MD-3's `AS OF` nothing exact to bind to.

### Option B2 — Epoch = commit, one epoch stream per tenant store

Each commit to a tenant's substrate store becomes exactly one Current epoch, in WAL order. `AS OF`
resolves against a real commit; the durability boundary and the visibility boundary are the same
boundary.

The cost is the write-amplification risk R-3 already names: a tenant committing at high frequency
pays per-commit circuit maintenance. That is a real cost, it belongs to `EXPLAIN MAINTENANCE` and
admission control, and it is the cost of the property.

### Option B3 — One epoch clock per branch

Since a session is a branch (a substrate fork), give each branch its own epoch stream.

Rejected for v1. Current is single-writer with one epoch clock (its §8 non-goals), and per-branch
clocks would make cross-branch queries a distributed-time problem before M5 has even proven the
fork mechanism. A tenant store has one WAL and therefore one total order over every commit on every
branch in that store — that order is the epoch clock, and `branch` becomes a tag on the delta
rather than a second clock.

### Option C1 — `source_id` = the single source the write derived from

Map `envelope.derived_from[0]` into Current's `source_id`, and taint by retracting that source.

Simple, and lossy in exactly the case that matters. `derived_from` is a *vector* because an agent's
claim routinely derives from several reads. Picking one makes `taint(S)` miss every fact where S was
not the first source — which is the flagship gate (M4) failing quietly, in the direction that says
"you are clean" when you are not.

### Option C2 — One append per (row, source), splitting weights

Emit the row once per contributing source with fractional or divided weights so each source can be
retracted independently.

Breaks the Z-set algebra: weights are integers with a defined meaning, and a row half-retracted is
not a state the model has. Also multiplies the delta volume by the fan-out of derivation.

### Option C3 — `source_id` names the ingest channel; derivation is its own maintained relation

`source_id` identifies *where the delta entered* (`<tenant>/<plane>/<table>`), and the
envelope-to-source edges are ingested as an ordinary relation, `mutiny_derivation`, maintained by
the same bridge on the same epoch clock. `taint(S)` then resolves the affected keys through that
relation — itself a standing query, incrementally maintained — and issues source-scoped retraction
over those keys via Current's C11 `retract_source(source_id, predicate?)`.

Multi-source derivation survives intact; the resolution of "what is downstream of S" becomes engine
physics rather than a DAG walk, which is precisely K-2's claim; and the derivation edges become
queryable, which is what MD-3's `TAINTED BY` predicate reads.

The cost is one extra relation on the write path and a dependence on C11's optional `predicate`
parameter actually landing. Both are named below.

## Decision

**A3 + B2 + C3.** The bridge is a translation with an audit, the epoch is the commit, and
provenance is a relation.

### The wire record

One `BridgeDelta` per (commit, table). This is the exact schema; field order is the canonical
encoding order, and it is length-prefixed per field for the same reason Loom's `signing_bytes()` is
(so that no two different records can produce the same bytes).

```rust
struct BridgeDelta {
    // ---- identity: what commit this is, on which clock -----------------------------------
    tenant:        TenantId,            // one substrate pool per tenant (M7); the epoch clock's scope
    commit:        ManifestId,          // [u8; 32], substrate's content address for the commit
    commit_seq:    u64,                 // position in the tenant store's WAL; becomes the epoch number
    parent_commit: Option<ManifestId>,  // Manifest::parent — the history edge, not the overlay base
    branch:        BranchId,            // Loom's branch = the substrate fork this commit landed on
    schema_version: u32,                // Manifest::schema_version, copied, not inferred

    // ---- payload: the change itself ------------------------------------------------------
    table:   TableName,
    entries: Vec<(Row, i64)>,           // Z-set. An update is (-1 old, +1 new). No delete channel.

    // ---- provenance: why this change is allowed to exist ---------------------------------
    envelope:      EnvelopeId,          // blake3(WriteEnvelope::signing_bytes()); the envelope itself
                                        // stays in Loom's store — this is a reference, not a copy
    actor:         ActorId,
    session:       SessionId,
    derived_from:  Vec<SourceRef>,      // copied verbatim from the envelope; engine-captured
    policy:        Option<PolicyDecisionId>,

    // ---- admission ------------------------------------------------------------------------
    source_id:    String,               // "<tenant>/<plane>/<table>" — the ingest channel
    dedup_token:  String,               // "<commit_hex>/<table>" — see R3

    // ---- semantic, M2 and later; absent at M1 --------------------------------------------
    embedding: Option<(GenerationId, Vec<f32>)>,  // generation-pinned per Prism's ingestion contract
}
```

And the relation the bridge maintains alongside every payload table — an ordinary Current table,
ingested through the ordinary path, on the same epoch:

```
mutiny_derivation(
    tenant       TEXT NOT NULL,
    branch       TEXT NOT NULL,
    table_name   TEXT NOT NULL,
    row_key      BYTES NOT NULL,   -- the payload row's primary key, canonically encoded
    source_system TEXT NOT NULL,   -- SourceRef.system
    source_record TEXT NOT NULL,   -- SourceRef.record_id
    envelope     BYTES NOT NULL    -- EnvelopeId, so the audit narrative can cite the envelope
)
```

One row per (payload row, contributing source). Its weights move with the payload's: a retraction of
the payload row retracts its derivation edges by the same −1, through the same code path, because
they are ingested in the same delta. Nothing about it is special-cased, which is the only way it
stays true.

### The rules

- **R1 · Epoch = commit.** `epoch = commit_seq` within the tenant's store, dense from 1, in WAL
  order. Every table touched by one commit is appended before that epoch is sealed, and the bridge
  is the only sealer. A commit never spans two epochs; an epoch never contains two commits.
- **R2 · Envelope required at admission.** A delta without a resolvable `envelope` is refused at the
  bridge, by name, before it reaches the log. Loom's I-7 discipline, now engine-wide: a bypassable
  audit trail is worse than none, because it is believed. There is no "envelope optional" mode, not
  behind a feature flag and not for tests — test fixtures construct real envelopes.
- **R3 · Exactly-once by content address.** `dedup_token = "<commit_hex>/<table>"`. This is not an
  arbitrary token: it is derived from substrate's content address, so a commit replayed after a
  crash presents the *same* token with the *same* content and Current answers `DroppedAsReplay`
  (I-4). If the same token ever arrives with different content, Current refuses loudly — which is
  exactly right, because that means a `ManifestId` collided or the write-set capture is
  nondeterministic, and both are catastrophic rather than retryable.
- **R4 · Determinism at the seam.** Tables within a commit are appended in `BTreeMap` order,
  entries within a table in canonical row order, `derived_from` sorted by `(system, record_id)`.
  No wall clock and no randomness anywhere in the bridge (Current D-6, substrate's own rule).
  `Manifest::created_at_ms` is carried into the catalog for `AS OF TIME` (MD-3) and is **never**
  used to make a bridge decision — substrate's manifest doc says why, and the reason is that an
  operator moving the clock must not be able to break the engine.
- **R5 · The completeness audit (A3).** For every commit, the manifest page diff against
  `parent_commit` is computed and reconciled against the captured write set. An unexplained changed
  page fails the commit at the seam. This runs in the M1 gate and in the nightly composed run; if
  it proves too costly on the hot path in production, it may be sampled — but the sampling rate is
  a ledgered constant with a benchmark receipt, not a shrug.
- **R6 · `source_id` is a channel, not a source.** `"<tenant>/<plane>/<table>"`. The *sources* are
  in `derived_from` and in `mutiny_derivation`. This distinction is the whole of C3 and the single
  easiest thing to get wrong later: anything that reads `source_id` and calls it provenance is a
  bug.
- **R7 · Branch is a tag, not a clock.** Every delta carries `branch`; branch-scoped circuits filter
  on it. One epoch clock per tenant store (B2/B3), for as long as Current is single-writer.
- **R8 · The bridge is the only writer into the compute plane.** No plane appends to the log
  directly. MD-1 R1 makes this structural (`compute → bridge`, and nothing else reaches storage),
  and it is what makes R2 unbypassable.

### What this asks of Current, on Current's own track

Recorded here because MD-1 R4 forbids MutinyDB from reaching into Current, and an unrecorded
dependency is a surprise waiting for M1:

1. **C4 as shipped is sufficient for R1–R4, R6–R8.** `append` + `seal_epoch` + content-addressed
   dedup already give exactly-once and the epoch boundary; `Batch.source_id` already exists.
2. **C11's optional `predicate` parameter is load-bearing for R6/C3.** `retract_source(source_id,
   predicate?)` with the predicate able to name a key set is what makes taint-over-multi-source
   derivation work. Its architecture entry already includes it; if C11 ships without it, MD-2 is
   amended and M4 gets more expensive, not impossible.
3. **A multi-append-then-seal admission API is desirable but not required.** R1 can be satisfied by
   the bridge calling `append` N times and then `seal_epoch`, provided nothing else can seal
   concurrently — which R8 guarantees. If Current later offers a transactional multi-table append,
   the bridge adopts it and R1 gets cheaper to prove.

### The M1 exit gate this contract must survive

Unchanged from the roadmap, restated in this record's terms: a randomized commit history replayed
through the bridge yields byte-identical Current state to a direct-ingest control, and the crash
matrix is green across the seam — kill the process at every boundary between "substrate commit
durable" and "epoch sealed", and prove the recovered state equals the never-crashed twin. R3 is what
makes that provable: after a crash, the bridge re-offers the same tokens and Current drops the
replays.

## Consequences

- **The write path grows a second relation.** Every payload delta carries its derivation edges,
  with fan-out equal to `derived_from.len()`. For observation-heavy tenants this is small; for
  claim-heavy agent workloads it is not, and it is the honest cost of K-2 being physics instead of a
  DAG walk. It must appear in `EXPLAIN MAINTENANCE` (R-3's mitigation) from the moment that exists.
- **`taint(S)` becomes a two-step, and both steps are engine-native.** Resolve keys through
  `mutiny_derivation` (a standing query), then retract those keys through the ordinary path. No
  provenance walker, no recall planner in the hot path. The RecallPlan (M4) is generated from the
  propagation receipt, and its two-section honesty is untouched: irreversible actions still cannot
  be retracted by any engine and are still listed first.
- **The bridge holds the only nondeterminism in the system, and it holds none.** Current's D-6 puts
  all nondeterminism at the ingest boundary; MD-2 R4 removes it from there too, because the epoch
  assignment is not a choice — it is the commit sequence. Downstream of a sealed epoch, MutinyDB is
  a pure function of the tenant's WAL, which is what makes both the oracle and crash recovery
  meaningful across the composed system.
- **Open, and deliberately not decided here.** (a) The canonical `row_key` encoding for
  `mutiny_derivation` — it must be stable across schema versions, and that is a schema-evolution
  question that belongs with the catalog work in M2, not with the bridge. (b) Whether the write-set
  capture lives in a substrate-side transaction hook or a Loom-side write interceptor; both satisfy
  A2, the choice is an M1 implementation detail, and the audit in R5 keeps either honest.
  (c) Cross-tenant queries have no epoch clock and are out of scope until the fleet plane exists.
