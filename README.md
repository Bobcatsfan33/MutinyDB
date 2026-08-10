# MutinyDB

**Private.** The consolidation of **Current** (compute), **LoomDB** (trust) and **PrismDB**
(semantic) on **substrate** (storage), into one agent-native enterprise database.

`CONSOLIDATION-ROADMAP.md` is the document of record: the four planes, the three keystone
unifications (commit-as-delta, taint-as-retraction, forked standing state), the phases M0–M8 and
their exit gates. Read it before anything here.

## Where this is

**M0 — charter and contracts.** Complete.

| M0 exit condition | State |
| --- | --- |
| MD-1…MD-4 merged | [MD-1](docs/decisions/MD-1.md) planes and dependency rules · [MD-2](docs/decisions/MD-2.md) the Delta Bridge contract · [MD-3](docs/decisions/MD-3.md) the unified SQL surface · [MD-4](docs/decisions/MD-4.md) the naming sweep |
| CI skeleton (fmt / clippy `-D warnings` / test / no-egress) green | `.github/workflows/ci.yml` |

**M1 has not started, and cannot.** It needs Current's C4 gate consumed *by pinned tag* (§6), and
Current carries no tags. Nothing in this repository builds a bridge, an operator, or a mounted
plane; the workspace holds one crate, `mutiny-charter`, which contains no engine code — it is the M0
gate expressed as a test.

Loom v0.3 and Prism S12 continue on their own roadmaps, unchanged. Nothing here pauses them, and
nothing in any sibling repository may depend on this one (§6, MD-1 R4).

## Layout

```
CONSOLIDATION-ROADMAP.md    the document of record — phases, gates, risks
docs/decisions/             MD-#: binding decision records, options weighed
crates/mutiny-charter/      the M0 gate: the records are present, well-formed, accepted
.github/workflows/ci.yml    fmt · clippy -D warnings · test · no-egress
```

## Running the gate

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test  --workspace --all-features --locked
```

## House rules, inherited

The invariants of the four planes are not restated here; they are enforced where they live. What
this repository adopts from day one, before it has code to break them with:

- **Nothing is skipped "for now."** A disabled test means the phase is not done.
- **No phase starts before its named gates** (§6, R-4). M1 needs C4-by-tag; M2 needs C5–C6; M4 needs
  M1 + M3 + C11.
- **Siblings by pinned tag, never by path, never by fork** (MD-1 R3).
- **Honesty.** No performance number without a committed reproducible artifact; every known weakness
  written down before someone else finds it.

Apache-2.0 when it goes public. Private until M8 says otherwise.
