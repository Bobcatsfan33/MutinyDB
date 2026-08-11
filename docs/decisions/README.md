# Decision records

Binding. A record supersedes only in writing, by another record that names it.

Every record carries `**Status:**`, then `## Context`, `## Options considered` (at least two
options, each weighed, including the ones that were rejected and why), `## Decision`, and
`## Consequences` — in that order. The order is the contract: a reader finds the options before the
decision and the costs after it. The shape is enforced by `crates/mutiny-charter`, which fails CI if
a record goes missing, reverts to a draft, decides before it weighs, or weighs only one option.

| Record | Decides | Phase | Status |
| --- | --- | --- | --- |
| [MD-1](MD-1.md) | Plane boundaries and the dependency rules | M0 | Accepted |
| [MD-2](MD-2.md) | The Delta Bridge contract — substrate commit + Loom envelope → Current epoch input | M0, built M1 | Accepted |
| [MD-3](MD-3.md) | The unified SQL surface, phased | M0, built M2–M6 | Accepted |
| [MD-4](MD-4.md) | Naming and trademark sweep: "MutinyDB" and "Current" | M0, executed M8 | Accepted, addendum 2026-08-11 |

**MD-4's addendum** resolves the engine's name: it becomes **SCHWEEP**, by the engine's own D-21.
This repository's "Current" references are rewritten once, at M1 open, not now.

**Reserved.** `CONSOLIDATION-ROADMAP.md` §4 M5 reserves **MD-5** for the forked-standing-state
fallback, to be written when the M5 spike returns its verdict. **MD-6** is the first free number —
no longer nominated for the rename, which MD-4 has now settled.
