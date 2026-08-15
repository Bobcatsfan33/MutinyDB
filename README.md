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

The consolidation is in its **M0 reset**. The complete source of all four components is now present
under `components/`, pinned to exact commits and trees by [`components.lock.json`](components.lock.json).
Presence is not admission: every component is quarantined until its exact release and composed
product gates pass. There is still no Delta Bridge, mounted trust plane, semantic circuit operator,
or supported `mutinyd` binary.

| Component | Product role | Imported state | Admission |
| --- | --- | --- | --- |
| substrate | storage | `substrate-v1.6.0` | quarantined pending compatibility |
| LoomDB | trust, branches, provenance, policy, action gateway | `loomdb-v0.5.1` | quarantined pending mounted-oracle gates |
| PrismDB | semantic event parts, generations, exact/approximate search | unreleased snapshot `296e804` | blocked on a release and composed oracle |
| Schweep | incremental circuits, epochs, standing answers | unreleased snapshot `c4b6268`; C10 complete | blocked on C11–C13 and `schweep-v0.1` |

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

## What must be green before M1

1. Every imported tree matches `components.lock.json`.
2. The imported LoomDB snapshot is rebased from its untagged substrate revision to the released
   monorepo substrate tree, with all supported Loom gates green.
3. Schweep completes C11–C13 and produces `schweep-v0.1` under its own gates.
4. The root compatibility workflow builds the supported component configurations without using a
   quarantined component in a product binary.
5. The M0 charter and decision records remain green.

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
