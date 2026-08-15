# MutinyDB

**One database for continuously correct, provenance-aware agent state.** MutinyDB consolidates
Schweep's incremental compute, LoomDB's trust and branch model, PrismDB's semantic event tier, and
substrate's durable storage into one supported product and one source repository.

MutinyDB's defining operation is source recall: a write records what it derived from; every commit
becomes one compute epoch; `taint(source)` retracts that source through all standing computations;
reversible answers repair themselves; irreversible external actions remain visible first, with the
receipts and compensation required to address them.

## Current product decision

**Not approved for production. Not a software release candidate.**

The consolidation has entered **M1 bridge development**. The complete source of all four components is present
under `components/`, pinned to exact commits and trees by [`components.lock.json`](components.lock.json).
Presence and development linkage are not release admission: every component remains quarantined
until its exact release and composed product gates pass. `mutiny-bridge` now implements the first
real substrate/Loom/Schweep seam and its local M1 gates; there is still no mounted trust plane,
semantic circuit operator, supported `mutinyd` binary, or production approval.

| Component | Product role | Imported state | Admission |
| --- | --- | --- | --- |
| substrate | storage | `substrate-v1.6.0` | quarantined pending compatibility |
| LoomDB | trust, branches, provenance, policy, action gateway | `loomdb-v0.5.1` | quarantined pending mounted-oracle gates |
| PrismDB | semantic event parts, generations, exact/approximate search | unreleased snapshot `296e804` | blocked on a release and composed oracle |
| Schweep | incremental circuits, epochs, standing answers | exact unreleased snapshot `220bf6b`; C11–C13 implementation complete | blocked on the remaining scheduled-night evidence, `current-v0.1`, and composed release admission |

This distinction is enforced, not editorial. `scripts/verify_component_lock.py` recomputes the
indexed tree of every import and refuses an unreleased or blocked component marked admitted.

## Architecture

The product has five planes with directed dependencies:

```text
fleet/ops -> trust -> compute -> semantic -> storage
                         |                     ^
                         +---- bridge ----------+
```

- **Trust:** branches, envelopes, provenance, claims, policy, and propose-not-execute actions.
- **Compute:** DBSP-style standing query circuits and shared subplans.
- **Semantic:** bounded semantic operators in compute plus Prism's immutable cold/scan tier.
- **Storage:** substrate commits, pages, WAL, snapshots, and forks.
- **Fleet/ops:** tenant pools, registry, sleep/wake, resource governance, and deployment controls.

The binding details are in [`CONSOLIDATION-ROADMAP.md`](CONSOLIDATION-ROADMAP.md) and
[`docs/decisions`](docs/decisions). [MD-6](docs/decisions/MD-6.md) executes the one-repository product
topology while preserving exact source provenance and release admission.

## Current M1 gates

1. Every imported tree matches `components.lock.json`.
2. `mutiny-bridge` maps one storage commit to one compute epoch, requires a real Loom envelope,
   audits physical pages against captured logical changes, and survives every append/seal crash seam.
3. The randomized differential gate remains byte-identical to an independent direct-ingest control.
4. Schweep produces `current-v0.1` after seven qualifying scheduled nights. Development uses the
   exact merged C13 snapshot; no product release may treat that snapshot as admitted.
5. Root compatibility, M0 charter, and component provenance gates remain green.

## Run the current gates

```sh
python3 scripts/verify_component_lock.py
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-features --locked
```

The nested component workflows are retained as import provenance; root workflows own the composed
product result. No performance, security, availability, or enterprise-approval claim is inherited
merely because a component repository made one about itself.

Private during consolidation. Apache-2.0 when the product's M8 release and professional naming
clearance gates say otherwise.
