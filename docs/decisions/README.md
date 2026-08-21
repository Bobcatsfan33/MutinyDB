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
| [MD-5](MD-5.md) | Forked standing state: the spike, its criteria, and the fallback taken | M5 | Accepted; verdict NO-GO for v1, Option B |
| [MD-6](MD-6.md) | One official repository; exact, quarantined component imports | M0 reset | Accepted; partially supersedes MD-1 and roadmap §6 |
| [MD-7](MD-7.md) | KMS custody — deferred to first enterprise adoption; the release-vs-production split | M8 hardening | Accepted |

**MD-4's addenda** resolve and execute the engine rename to **SCHWEEP**. The 2026-08-14 M0 reset
rewrites the live MD-1…MD-3 references because MD-6 imports the renamed source before M1.

**MD-5** was written in two committed steps, deliberately: the go/no-go criteria before the spike
ran, the verdict after — so neither could bend to the other. **MD-7** applies PrismDB's own
`DATA-01`/`EXT-SCALE` split-and-defer discipline to key custody: release-admissible with the
posture stated precisely, production-blocked until a real key service produces the receipts.
**MD-8** is the first free number.
