# MD-4 · Naming and trademark sweep: "MutinyDB" and "Current"

**Status:** Accepted
**Phase:** M0 (the sweep and the rules) · **Executed at:** M8 (public naming, repo topology)
**Roadmap:** `CONSOLIDATION-ROADMAP.md` §4 M0, §4 M8, R-5

> **This is a clearance *screening*, not a legal opinion.** No attorney was involved. It covers the
> USPTO's public record as surfaced by third-party mirrors, the package registries this product
> would publish to, GitHub, and DNS — as of the sweep date below. It does **not** cover a full
> USPTO knock-out search, state registrations, common-law use, EUIPO/WIPO, or likelihood-of-confusion
> analysis. A professional clearance search is a **named prerequisite** before first public use in
> commerce and before any application is filed. What this record can do is tell you where the
> obvious walls are while moving them is still cheap.

**Sweep date:** 2026-08-10. **Method:** crates.io / npm / PyPI registry APIs; GitHub repo, user and
search APIs; `dig` and `whois`; web search of the USPTO public record via third-party mirrors and of
the live product landscape.

## Context

R-5 states the risk in one line: *"The name. MD-4 trademark/collision sweep before anything public —
the FlockDB lesson, learned once, applied twice."* The lesson was learned when a component's name
turned out to be someone else's; the roadmap's instruction is to not learn it a second time by
accident, across **two** names — the product being assembled here, and the engine that is already
public.

The second of those is time-sensitive in a way the roadmap's §4 M0 does not spell out. Current went
public on 2026-08-10 (`github.com/Bobcatsfan33/Current`, public, created that day) with **no tags
and no published crates**. Every day of public existence, every tag, and above all every crates.io
publish makes a rename more expensive and less reversible. A name sweep that reports after the
crates are published has reported too late.

## Options considered

### Option A1 — Ship the product as "Mutiny"

Short, memorable, and it is the thesis word.

The word alone is heavily occupied in and adjacent to this market:

| Use | What it is | Why it matters |
| --- | --- | --- |
| **Mutiny / MutinyHQ** (mutinyhq.com) | B2B website-personalization SaaS, YC S18, backed by Sequoia, Tiger Global, Insight | An active, well-funded software company using the bare word. Software/SaaS sits in the same trademark classes (9 and 42) a database would file in. This is the blocking one. |
| **SmallRye Mutiny** (`io.smallrye.reactive:mutiny`) | Event-driven reactive library for Java; the reactive API throughout Quarkus | Not a registered-mark problem; a *mindshare* problem, in exactly this product's audience. "Mutiny" plus "async/streams/data" already means something to backend developers. |
| **Mutiny** (Hex/Elixir, `newaperio/mutiny`) | "Simple database immutability" — Ecto/Postgres migration helpers | Small, but the closest semantic neighbour: it is a *database* library called Mutiny. |
| **Mutiny Wallet** (mutinywallet.com) | Bitcoin Lightning wallet; ceased operations end of 2024, team moved to OpenSecret | Winding down reduces the conflict; abandonment is not automatic and is not something this sweep can determine. |
| **MUTINY**, Mutiny LLC (USPTO serial 90874795) | Video cables and interface circuits for video cameras; first use 2019/2021 | A registration on the exact word in **class 9** — which is also where downloadable software lives. |
| Mutiny Industries LLC | "MUTINY ON THE MAYFLOWER" | Noted for completeness; not adjacent. |

Taking the bare word means competing for it with a funded SaaS company and a widely-deployed Java
library, in the same audience, forever.

### Option A2 — Ship the product as "MutinyDB", and never as bare "Mutiny"

The compound string is, as far as this sweep can see, **unoccupied**:

| Surface | Result |
| --- | --- |
| crates.io `mutinydb` | free (`mutiny` is taken — v0.2.0, process-monitoring test tools) |
| npm `mutinydb` | free (`mutiny` taken) |
| PyPI `mutinydb` | free (`mutiny` taken) |
| GitHub repo search, `mutinydb` in name | **0 results** |
| GitHub org/user `mutinydb` | free (`mutiny` taken) |
| mutinydb.com | unregistered ("No match") |
| mutinydb.org | unregistered ("Domain not found") |
| mutinydb.io / mutinydb.dev | no A record, no NS record — no evidence of registration |
| Web search, "MutinyDB" as a product | nothing; results are the Elixir `Mutiny` library and the dictionary word |
| mutiny.dev | **registered** (resolves; GoDaddy nameservers) — do not plan on it |

The `-DB` suffix does real work here: it moves the mark out of the marketing-SaaS and reactive-Java
neighbourhoods and into a category where nothing is using it, and it is the difference between a
descriptive collision and a distinct compound.

### Option A3 — Rename the product entirely

Abandon "Mutiny" for something with no neighbours at all.

Not warranted by the evidence. The compound is clear, the thesis is genuinely carried by the word
("a mutiny against the assumption that an answer must be recomputed to be trusted"), and the product
is private until M8 — so the option to change costs nothing to keep open, and nothing found here
forces it.

### Option B1 — Keep "Current" as the public engine name

It is already public, the architecture document is written around it, and the one-sentence pitch —
"every answer, current" — is built on the word.

The evidence against is the strongest thing in this record:

| Finding | Detail |
| --- | --- |
| **CURRENT is a registered mark on downloadable software** | Finco Services, Inc. (d/b/a Current, the neobank at current.com) — serials 88489787 and 88489765, filed 2019-06-26, covering *downloadable computer software* for processing electronic payments and transferring funds; an earlier 2016 filing covers a mobile software application. Trademarkia lists 17 marks owned by Finco. Class 9 software, on the exact word. |
| **"Current" is the data-infrastructure category's event brand** | Confluent's *Current* — the successor to Kafka Summit and its flagship data-streaming conference (current.confluent.io; Current 2026 San Francisco, Nov 4–5, Moscone West; Current London). A data engine named Current is competing for its own category's search results against the conference that category attends. |
| **crates.io `current` is taken** | v1.0.1, "a library for setting current values for stack scope". The engine cannot publish under its own name. (`current-zset`, `current-sql`, `current-oracle` are free today — which is exactly the surface a rename would have to reclaim later.) |
| **It is a dictionary word** | Weak as a mark, near-impossible to search for, and it collides with the ordinary English word in every sentence the product's own documentation writes about answers being current. |

### Option B2 — Keep "Current" internally, rename only at public launch

Let the repository and crates keep the name, and pick a public name at M8.

This is the option that looks cheap and is not. The crates *are* the public surface for a Rust
engine: the first `cargo publish` fixes `current-*` in a global namespace, the first tag fixes it in
every downstream lockfile, and the architecture document, the READMEs and every gate name accumulate
the word. "Rename later" is a plan to pay more, later, for the same thing.

### Option B3 — Rename Current before its v0.1 tag and before its first crates.io publish

Treat today's state — one public repo, one day old, zero tags, zero published crates — as the last
cheap moment, and act while the rename is a find-and-replace rather than a migration.

## Decision

**A2 for the product. B3 for the engine, with the decision executed on Current's own track.**

### The product is **MutinyDB**, under four rules

1. **Always the compound.** "MutinyDB" is the name, in prose, in the repository, in the wordmark, in
   package names, and in any future filing. "Mutiny" alone is **never** the product name — not in
   headlines, not in a logo, not as a shortened form in documentation. This is what keeps distance
   from MutinyHQ and SmallRye Mutiny.
2. **Never claim the bare word anywhere.** No `mutiny` package on any registry (all three are taken
   in any case), no `mutiny.*` domain, no `@mutiny` handle.
3. **Reserve the compound before M8 opens the repository.** `mutinydb` on crates.io, npm and PyPI;
   the `mutinydb` GitHub organisation; mutinydb.com and mutinydb.org, plus .dev/.io if a registrar
   confirms they are free. Reservation is cheap; recovering a name someone squatted after your
   launch announcement is not.
4. **Clear it properly before first public use.** A professional clearance search — full USPTO,
   common-law, and at minimum EUIPO — is a prerequisite of the M8 naming step, not a follow-up to it.
   Two neighbours it must specifically address: the class-9 MUTINY registration (hardware, but the
   same class as downloadable software) and MutinyHQ's use in software services.

### "Current" is not clearable as a public product name in this category

The finding is recorded plainly because a hedged finding will be read as permission: a registered
class-9 software mark on the exact word, the category's own flagship event brand using it, the
crates.io name already gone, and a dictionary word that cannot be searched for. Any one of those is
survivable. Together they mean the engine cannot own its name in its own market.

**Trigger, and who pulls it.** The rename must land **before `current-v0.1` is tagged and before the
first `cargo publish`**, whichever comes first. MD-1 R4 forbids this repository from reaching into
Current, so this record does not rename anything: it records the evidence and the deadline, and the
decision is taken on Current's own track, as a D-record in Current's `docs/DECISIONS.md` under its
own supersession discipline. **M8's naming step blocks on that D-record existing and being
executed** — that is the gate that makes this finding binding rather than advisory.

**Screening criteria for the replacement** (the candidate itself belongs in a follow-on record —
MD-6, since MD-5 is reserved by the roadmap for the M5 fork fallback): not a dictionary word; free
on crates.io as both the bare name and the `<name>-*` prefix; no registered mark in classes 9 or 42;
no active project in data infrastructure; and searchable — a name whose first page of results is the
project itself.

**In the meantime**, MutinyDB's own documents refer to *the compute plane* wherever the sentence
does not specifically need the engine's name. That keeps this repository's exposure to the rename
down to a handful of proper nouns.

## Consequences

- **MutinyDB's public surface has to be reserved, not just chosen.** Rule 3 is a task with a
  deadline (M8), and it is the kind of task that gets skipped because it is administrative. It is on
  the M8 gate for that reason.
- **A finding this record cannot make.** Whether MutinyDB is *registrable* — as opposed to
  apparently unoccupied — is a question for counsel. This sweep can only show that nobody visible is
  standing on the name today.
- **The Current rename will cost something no matter when it happens, and it costs least now.**
  Today: one repository, no tags, no crates, one architecture document. After C13: a tagged v0.1, a
  published crate family, a benchmark suite whose artifacts quote the name, and users with
  lockfiles. This record's only real leverage is the date on it.
- **The consolidation is insulated either way.** MD-1's plane vocabulary means MutinyDB depends on
  "the compute plane" far more than on the string "Current", so the rename lands as a dependency
  bump and a proper-noun sweep rather than a refactor.
- **Re-run the sweep before M8.** Registry and domain availability is a fact with a shelf life; the
  three-year-old sweep in a repository is the one that finds the name was taken last spring. The M8
  step re-runs it and re-dates this record.

## Addendum — 2026-08-11: the engine's name is resolved

The rename this record demanded has a name. **The compute engine becomes SCHWEEP.**

**The sweep that chose it** (conducted on the engine's track; candidates and outcomes returned to
this record):

| Candidate | Outcome |
| --- | --- |
| **Weft** | **Rejected.** `WeaveMindAI/weft` — "a programming language for AI orchestrations", Rust, 1,824 stars — is an active project in an adjacent category with real mindshare, and `weft` is taken on npm. The bare word fails for the same reason bare "Mutiny" does: someone is already standing on it, in this audience. |
| Heddle · Artesian · Seiche · Millrace · Freshet · Weir · Oxbow | Occupied. |
| **Schweep** | **Chosen.** Clean on crates.io, npm and GitHub. |

**Independently confirmed here, 2026-08-11**, against this record's own rule that an availability
finding is only as good as its date:

| Surface | Result |
| --- | --- |
| crates.io `schweep`, `schweep-core` | free — and unlike `current`, the bare name *and* the prefix are both available |
| npm `schweep` · PyPI `schweep` | free |
| GitHub org/user `schweep` | free |
| GitHub repo search, `schweep` in name | 5 results, **all 0 stars**, all unrelated hobby repositories (`BenDobbe/Schweep`, `Kerol4/Schweeps`, and three supermarket/minesweeper toys). Recorded as found rather than rounded to "zero": the name is unclaimed, not unheard-of. |
| schweep.dev · schweep.io | no A record, no NS record — no evidence of registration |
| schweep.com | **registered** (resolves; GoDaddy nameservers) — do not plan on it |

Schweep clears every criterion this record set for the replacement: not a dictionary word, free as
both the bare crate name and the `schweep-*` prefix, nothing in data infrastructure using it, and
searchable — the first page of results will be the project itself. The class-9/42 registered-mark
question is unchanged in kind and still belongs to the professional clearance search that Rule 4
makes a prerequisite of M8; Schweep goes into that search alongside MutinyDB.

**When it takes effect.** The rename executes on the engine's own track as its **D-21**, after its
current sprint lands. MD-1 R4 keeps that out of this repository's hands, correctly, and nothing here
anticipates it: the engine is still named Current until D-21 lands, and the trigger MD-2 now names
(`schweep-v0.1` at the engine's C13 freeze) is a tag that does not yet exist.

**Why this repository's own references are not being rewritten today.** Deliberate deferral, not an
oversight. Every "Current" in MD-1, MD-2, MD-3 and the README still names a real engine under its
real present name, and rewriting them now would (a) churn four documents against a rename that has
not landed, (b) leave this repository describing an engine whose repository, crates and architecture
document still say Current, and (c) have to be redone anyway if D-21 slips or the name moves again.
The sweep is recorded; the churn happens **once, at M1 open**, against the renamed engine — and M1's
session inherits it as a first task rather than discovering it.

## Sources

Registry, DNS and GitHub results were obtained directly from the crates.io, npm, PyPI and GitHub
APIs and from `dig`/`whois` on the sweep date. Trademark and product-landscape findings:

- [MUTINY — Mutiny, LLC, USPTO serial 90874795](https://uspto.report/TM/90874795)
- [Mutiny Industries LLC trademarks](https://uspto.report/company/Mutiny-Industries-L-L-C)
- [Mutiny (MutinyHQ) — B2B website personalization](https://www.mutinyhq.com/overview) ·
  [Launch HN: Mutiny (YC S18)](https://news.ycombinator.com/item?id=21346414)
- [SmallRye Mutiny](https://github.com/smallrye/smallrye-mutiny) ·
  [Mutiny — Async for mere mortals (Quarkus)](https://quarkus.io/guides/mutiny-primer)
- [newaperio/mutiny — simple database immutability (Elixir)](https://github.com/newaperio/mutiny)
- [Mutiny Wallet is shutting down](https://blog.mutinywallet.com/mutiny-wallet-is-shutting-down/) ·
  [shutdown timeline](https://www.nobsbitcoin.com/mutiny-wallet-shutdown-timeline-announced/)
- [CURRENT — Finco Services, Inc., USPTO serial 88489787](https://uspto.report/TM/88489787) ·
  [CURRENT, serial 86882027 (Justia)](https://trademarks.justia.com/868/82/current-86882027.html) ·
  [Finco Services trademark portfolio](https://www.trademarkia.com/owners/finco-services-inc)
- [Current (financial services company)](https://en.wikipedia.org/wiki/Current_(financial_services_company))
- [Current 2026 — Confluent](https://current.confluent.io/) ·
  [The Future of Current](https://www.confluent.io/blog/future-of-current-data-streaming-community/)

Addendum (2026-08-11):

- [WeaveMindAI/weft — a programming language for AI orchestrations](https://github.com/WeaveMindAI/weft)
  (1,824 stars, Rust, at time of check)
- Schweep availability was checked directly against the crates.io, npm, PyPI and GitHub APIs and
  `dig`, on the addendum date.

## Addendum — 2026-08-14: the rename is executed in the consolidated source

Schweep's D-21 rename landed before a v0.1 tag or crate publication. MD-6 now imports the renamed
source tree directly into MutinyDB, so the earlier deliberate deferral no longer describes the live
repository. MD-1, MD-2, MD-3, the roadmap, workspace comments, and CI labels are rewritten now,
during the M0 reset, rather than waiting for M1. Historical discussion in this record keeps the old
name where it describes the choice that was made.
