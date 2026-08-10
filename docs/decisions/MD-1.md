# MD-1 · Plane boundaries and the dependency rules

**Status:** Accepted
**Phase:** M0 · **Governs:** every crate in this repository, and every consumption edge to
`substrate`, `loomdb`, `PrismDB`, and `Current`
**Roadmap:** `CONSOLIDATION-ROADMAP.md` §2, §4 M0, §6, R-4

## Context

MutinyDB is one product assembled from four codebases with four maturities: substrate (v1.3.0,
frozen), LoomDB (v0.2 line complete, v0.3 in flight), PrismDB (S12 in flight), and Current (public,
one day old, no tags, no published crates). §2 names five planes — trust, compute, semantic,
fleet/ops, storage — and §6 names the failure this record exists to prevent, quoting substrate's
own docs: *the shared core quietly forking to serve two masters.*

That failure does not arrive as a decision. It arrives as a convenience. The compute plane needs
one field that only the trust plane has; someone adds the import; six months later the trust plane
cannot be tested without a query engine, the semantic plane cannot ship without the trust plane's
release cadence, and the four repositories have become one repository with four names. Everything
downstream of that — Loom's four model oracles proving the port changed nothing (M3), Prism's
byte-identical answers across shard layouts, substrate's frozen API — depends on the arrows between
planes being *few*, *directed*, and *impossible to reverse by accident*.

Two distinct questions have to be answered, and they are usually conflated:

1. **What may depend on what** — the shape of the arrows between planes.
2. **What makes the answer to (1) true tomorrow** — enforcement.

A boundary that lives only in a document is not a boundary. It is a preference with a footnote.

## Options considered

### Option A1 — One crate, module discipline

MutinyDB as a single crate with `mod trust`, `mod compute`, `mod semantic`, `mod storage`, and a
code-review convention about which module may `use` which.

Cheapest to start. Fails the enforcement test completely: Rust's module system makes every sibling
module reachable, `pub(crate)` is the whole crate, and the only thing standing between the layering
and its violation is whether a reviewer noticed. Also makes M3's proof impossible — Loom's oracles
run "unmodified against the mounted configuration", which requires the mounted trust plane to be a
thing that can be built and tested on its own.

### Option A2 — One crate per plane, dependency edges declared in Cargo.toml, checked by review

The natural Rust decomposition: `mutiny-trust`, `mutiny-compute`, `mutiny-semantic`, `mutiny-fleet`,
`mutiny-bridge`. Cargo refuses circular dependencies outright, so the worst violations become
compile errors for free.

Better, and it is most of the answer. But Cargo only forbids *cycles*. It says nothing about a new
edge that is acyclic and still wrong — compute depending on fleet, say, or the semantic plane
reaching into the trust plane for a tenant id. Those are the violations that actually happen,
because they compile.

### Option A3 — One crate per plane, plus a machine-checked allowed-edge matrix

A2, plus a test that reads the workspace's own dependency graph (`cargo metadata`) and compares it
against a matrix of permitted edges written down in one place. A new edge is a red CI job naming
the edge and the rule it breaks; adding it deliberately means editing the matrix, in the diff,
where a reviewer sees it.

Costs one test and one table. Converts "we agreed not to" into "you cannot, without saying so out
loud."

### Option A4 — Process isolation: one service per plane, RPC between them

Boundaries enforced by the network. Unforgeable, and wrong for this product: K-1 (commit-as-delta),
K-2 (taint-as-retraction) and K-3 (forked standing state) are all *tight* couplings by design — an
epoch is a commit, a retraction propagates through circuits, a fork shares circuit state on
copy-on-write pages. Putting an RPC hop inside those paths would cost the exact properties the
consolidation exists to produce, and M6 explicitly goes the other way: `mutinyd`, one process.

### Option B1 — Consume the sibling repositories by git path dependency

`mutiny-bridge = { path = "../loomdb/crates/loom-core" }` during development. Fast iteration; a
local edit is instantly visible on both sides.

This is precisely the forking mechanism §6 forbids. A path dependency has no version, so "which
LoomDB is this?" has no answer; a local uncommitted change becomes load-bearing; and the first time
a sibling needs a change *for MutinyDB's sake*, it lands in the sibling's main branch as an
untracked obligation. R-4 (four repos, one person and agents) makes this failure near-certain
rather than merely possible.

### Option B2 — Consume by pinned tag, always

Every sibling is a versioned dependency at an exact tag (`substrate = "1.3.0"`, or a git dependency
with `tag = "..."` and a locked revision). Upgrading is an explicit commit that changes one line and
re-runs every gate.

Slower by exactly the amount of friction that makes the version legible. §6 already mandates it;
this record makes it enforceable and states what happens when a sibling needs a change.

## Decision

**Five planes, four rules, one enforcement mechanism.**

### The planes and the crates that will carry them

| Plane | Crate (arrives at) | Owns |
| --- | --- | --- |
| Trust | `mutiny-trust` (M3) | Loom's branches, envelopes, policy, action gateway, merge engine, the evidence record |
| Compute | `mutiny-compute` (M2) | Current's circuits, epochs, memo, result stores — *and every semantic operator* |
| Semantic | `mutiny-semantic` (M2) | Prism's meaning-clustered parts, centroid index, PQ scan, embedding generations — as a **tier**, not a peer |
| Fleet/ops | `mutiny-fleet` (M7) | per-tenant pools, registry, sleep/wake, wake-on-delta |
| Storage | substrate v1.3.0 (external, frozen) | content-addressed pages, O(1) fork, WAL, sleep/wake, the commit stream |
| — | `mutiny-bridge` (M1) | the one seam: substrate commits → compute epoch inputs (MD-2) |

The semantic plane is deliberately **not** a peer of compute. §2 gives it two halves and they land
in two places: its *operators* (`≈≈`, semantic `GROUP BY`, generations) are compute-plane operators
per M2, and its *storage* (meaning-clustered immutable parts, the cold scan tier) is a storage-side
tier addressed only through compute. There is no path by which a caller reaches Prism's parts
without going through a circuit. This is what keeps one answer, one plan, one set of counters.

### The four rules

- **R1 · Downward only.** Permitted edges: `trust → compute`, `compute → semantic`,
  `compute → bridge`, `bridge → storage`, `semantic → storage`, `fleet → {trust, compute, bridge}`.
  Every other edge between plane crates is forbidden. In particular: **compute may not depend on
  trust**, and **nothing may depend on fleet**.
- **R2 · No sideways.** Two planes at the same level never import each other. Where they must
  cooperate, the lower plane defines a trait and the higher plane implements it — the dependency
  arrow stays downward while the control flow goes wherever it needs to. Fleet's wake-on-delta (M7)
  is the case this rule is written for: the registry must know which circuits a delta feeds, and it
  learns it by *observing* an interface compute exports, never by compute calling the registry.
- **R3 · Siblings by pinned tag, never by path, never by fork.** Every dependency on substrate,
  loomdb, PrismDB or Current is a versioned or tag-pinned dependency, recorded in
  `[workspace.dependencies]`, upgraded by an explicit commit. A `path = "../..."` dependency on a
  sibling repository is a CI failure. When MutinyDB needs a change in a sibling, the change is made
  in the sibling, released under its own gates, and consumed here at the new tag — the sibling's
  roadmap is never amended to suit this repository (§6: Loom v0.3 and Prism S12 continue unchanged).
- **R4 · Nothing in any sibling may depend on MutinyDB.** §6, verbatim. The arrow out of this
  repository does not exist. Current's `ARCHITECTURE.md` already builds to this rule — its
  **[MutinyDB seam]** notes are interfaces, never imports — and MD-2 is written to consume those
  seams as they are, or to name the change Current must make on its own track and under its own
  gate.

### Enforcement

Option A3. A single `crates/mutiny-planes/tests/boundaries.rs` (arriving with the first plane crate
at M1) reads `cargo metadata` and asserts that the set of edges between plane crates is exactly the
R1 matrix, that no manifest carries a path dependency outside this workspace (R3), and that every
sibling dependency names a tag or a version. A new edge fails CI by name. Until M1 there are no
plane crates and therefore no edges, so the matrix is empty and the test is not yet written — this
is stated so its absence today reads as a schedule, not an oversight.

## Consequences

- **What gets harder.** Any cooperation between trust and compute costs a trait definition in the
  lower plane. M3 (Loom's policy consulting evidence that "may now cite *any* plane's data") is the
  first hard case: policy lives in trust, the evidence lives below it, so evidence retrieval is a
  compute-plane query the trust plane issues — not a trust-plane reach into compute's internals.
- **What gets easier.** M3's exit gate becomes reachable: Loom's four model oracles run against a
  mounted trust plane precisely because the mounted trust plane is still a separately buildable
  thing. The same is true of Prism's exact-oracle discipline in M2.
- **The upgrade cost is now visible.** R3 means a substrate or Loom upgrade is a commit that
  re-runs every composed gate. That is the intended cost. The alternative is not "cheaper" — it is
  the same cost paid later, without a diff to point at.
- **One known tension, recorded now rather than discovered later.** M5 (forked standing state) puts
  compute-plane operator state onto substrate's content-addressed pages, which means the compute
  plane gains a direct storage-plane dependency that R1 does not currently permit
  (`compute → storage`). That edge is *not* granted here. If the M5 spike goes ahead, it amends this
  record with the edge and the reason; if it goes no-go, the recorded fallback (fresh circuits
  hydrated from the parent's checkpoint) needs no new edge at all. The spike decides, in writing,
  before the matrix changes.
- **Fleet is the top of the stack, which makes it the plane most likely to be violated.** Nothing
  may depend on it, and it depends on nearly everything. R2's trait-inversion rule is the whole
  defence, and the first time wake-on-delta is implemented there will be a strong temptation to
  have a circuit call the registry directly. It fails CI. That is the point.
