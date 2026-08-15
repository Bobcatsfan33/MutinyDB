# MD-6 · One official repository and provenance-gated component imports

**Status:** Accepted
**Phase:** M0 reset · **Governs:** repository topology, source admission, component provenance,
and the transition from the four development repositories to one supported MutinyDB product
**Supersedes:** MD-1 R3 and R4, `CONSOLIDATION-ROADMAP.md` §6, and MD-4's deferred M8 topology step

## Context

MD-1 chose independently released sibling repositories consumed by exact tags. That was coherent
when the goal was to preserve four standalone products. The product direction is now different:
PrismDB, LoomDB, Schweep, and substrate are to become one official database offering in this
repository. Continuing to develop the supported product across four release trains would preserve
the exact integration and documentation drift the consolidation is meant to eliminate.

The current source also disproves the old baseline. LoomDB is v0.5 rather than v0.3 and resolves an
untagged substrate revision beyond the then-current v1.5.0 release. That dependency was subsequently
closed by `substrate-v1.6.0` and `loomdb-v0.5.1`. PrismDB has moved beyond S12 but has no tag.
Schweep is at C10 and has no v0.1 release. MutinyDB cannot honestly call any of those an admitted
product dependency merely because their source has been copied here.

Two properties must therefore hold at once:

1. The complete source must live in one repository so one change, gate, release and support policy
   define the product.
2. An imported development snapshot must not become supported product code until its own release
   and composed-product gates pass.

## Options considered

### Option A — Keep the sibling/tag model and make MutinyDB an integration shell

This preserves independent histories and release discipline, but it does not produce one official
repository. Cross-repository changes remain non-atomic, compatibility is a convention, and every
urgent product fix has four places to drift.

### Option B — Git submodules

Submodules retain exact commits but make the official checkout depend on nested repositories and
special client behavior. They also allow the product commit to remain green while a component's
actual branch, review policy, or availability changes elsewhere. An offline source bundle would
not be self-contained.

### Option C — Copy the latest directories and treat them as new source

This is operationally simple and destroys provenance. A buyer, reviewer, or future maintainer could
not answer which upstream commit produced a directory or distinguish an intentional product change
from an incomplete copy.

### Option D — Exact monorepo imports with a machine-checked provenance and admission lock

Import each complete source tree under `components/<name>`, record repository, commit, original
tree, current tree, release tag, phase, admission state, and blockers in `components.lock.json`, and
make CI recompute every current tree. A component may be present but quarantined. Admission requires
an exact release tag, zero named blockers, and the phase-specific composed gates.

## Decision

**Option D. MutinyDB is the one official product repository.**

The initial imports are exact snapshots:

| Component | Imported revision | Release state | Initial admission |
| --- | --- | --- | --- |
| substrate | `44480f5` / `substrate-v1.6.0` | released | quarantined pending composed compatibility |
| LoomDB | `9c2934b` / `loomdb-v0.5.1` | released | quarantined pending mounted-oracle gates |
| PrismDB | `296e804` | no release | quarantined |
| Schweep | `c4b6268` | no release; C10 complete | quarantined |

`components.lock.json` is authoritative. Its verifier fails if an imported tree changes without a
lock update, a nested repository appears, a release-less component is marked admitted, or an
admitted component retains blockers. A source import is therefore visible without being confused
with a supported product dependency.

MD-1's plane-edge matrix remains binding. Only its repository rules change:

- **R3 is replaced.** Product crates use workspace-local path dependencies whose source is inside
  this repository. Paths outside the repository remain forbidden.
- **R4 is replaced.** The legacy repositories remain development-history sources during migration;
  after their final import they become archived or read-only mirrors. The supported release is made
  only from MutinyDB.
- Component changes after import land here first. During migration, a change needed to close a
  component's own unfinished release gate is also returned to that component repository, released
  there, then re-imported with both the source and current trees recorded.
- No product binary may link a quarantined component. The component-integrity gate can be green
  while product admission remains red; those are deliberately different claims.

## Consequences

- The repository becomes materially larger, because an offline, auditable product checkout is now
  a feature rather than an inconvenience.
- Nested component workflows are retained as provenance but do not execute from their nested paths;
  MutinyDB's root workflows must carry the composed gates.
- Component history before import remains reachable by the recorded repository and commit. Product
  history after import is atomic here.
- LoomDB's untagged substrate dependency is now a named migration defect. The monorepo rebases it to
  the imported released substrate tree and proves the change through Loom's supported flavour and
  oracle gates before either component is admitted.
- MD-4's public naming and professional-clearance gate remains external and open. Only its deferred
  repository-topology decision is executed early by this record.
- This decision does not claim M1 has started. It changes where source lives and how admission is
  proven; the Delta Bridge still begins only when the compute release gate exists.
