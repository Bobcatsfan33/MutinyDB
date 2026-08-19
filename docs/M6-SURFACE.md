# M6 one surface — `mutinyd`

One process composes every plane behind one admission boundary and three doors. This document is
the wire contract; it is written before the code and the code is held to it. `mutinyd` **absorbs**
its parents — schweepd's HTTP door, admission discipline, and resume tokens (Schweep D-23);
loomd's MCP door and its propose-not-execute law; Prism's per-tenant quota and round-robin
fairness — it does not reinvent them, and where it deviates, the deviation is stated here with its
reason.

**Quarantine notice, first.** Every component this binary links is release-quarantined
(`components.lock.json`, 0 admitted). `mutinyd` is therefore the *composed-development form* of
the product binary: it is built, gated, and documented as the supported surface **shape**, and it
becomes a supported, distributable artifact only when M8's release gates and the lock's blockers
clear. It says so in `--help` and in `/health`. Nothing in this phase publishes an image or a
binary.

## The versioning promise (an enterprise trust artifact, from day one)

- The surface described here is **`v0`**, reported in `/health`, in MCP `initialize`
  (`serverInfo`), and in `mutinyd --help`.
- Until v1.0, changes to this surface are **additive only**: new endpoints, new tools, new
  response fields. Nothing documented here is renamed, removed, re-typed, or re-numbered except
  with a version bump, and a version bump ships with migration notes.
- Error kinds, their status codes, and their retryability (below) are part of the promise.
- The durable formats beneath the surface (substrate manifests + capture page, the engine log,
  the system relations `mutiny_derivation` / `mutiny_taint_ledger` / `mutiny_forks`) carry their
  components' own compatibility promises; `mutinyd` adds no format of its own to the wire —
  bodies are the log's frame encoding and the canonical rendering the differential gates already
  compare, exactly as schweepd's D-23 chose.

## Doors

Three doors, one engine, one admission boundary. **Arrow Flight is not a door**: Schweep's D-29
froze HTTP as the v0.1 transport and kept Flight an evaluated extension, so the component never
proved it and `mutinyd` does not ship it. It arrives, if ever, from the engine's own track.

### 1 · HTTP — the SQL door and the typed door

Hand-written HTTP/1.1 exactly as schweepd's wire (one request per connection, `Content-Length`
bodies, no wall-clock headers). All paths are tenant-scoped: `/v1/<tenant>/…`.

**The SQL door** (per MD-3's shipped phases):

| Method, path | Body / params | Answer |
| --- | --- | --- |
| `POST /v1/<t>/sql/register` | body = SQL text; `?unbounded=<reason>` for I-9 admission | handle number |
| `POST /v1/<t>/sql/deregister?handle=` | — | `ok` |
| `GET /v1/<t>/sql/read?handle=` | `&format=frames` for the log-frame body | `epoch N` + canonical rendering |
| `GET /v1/<t>/sql/oneshot?sql=` | or SQL in the body | canonical rendering |
| `GET /v1/<t>/sql/subscribe?handle=&from=` | — | resume token + per-epoch deltas (below) |
| `GET /v1/<t>/sql/plan?handle=` | — | the plan's s-expression rendering |

The dialect served is Schweep's ladder, verbatim, through the engine's own binder — the inherited,
gated rungs. The MD-3 extension constructs are **refused by name with their phase** before the
engine sees them: a query containing `≈≈`/`~~`, `AS OF`, `TAINTED BY`, `semantic_cluster`,
`NOVELTY`, or `SEMANTIC_DIFF` is answered `Refused` with a message naming the construct and the
door that serves its semantics today (the typed and MCP doors' standing semantic operators, per
M2/M3's gates). MD-3's rule stands: never accepted-and-ignored. The SQL *text* forms of those
extensions land when the MutinyDB binder does, additively, with MD-3's binder-corpus discipline.

**The typed door** — structured operations, JSON bodies, same engine calls as the SQL door for
queries (`{"sql": …}` — the typed door types the *operation*; the typed query *algebra* is the
engine's own Rust API, which Schweep's I-6 corpus already proves plan-identical to SQL):

| Method, path | Purpose |
| --- | --- |
| `POST /v1/<t>/write` | one storage commit through the M1 front door: `{actor, session, branch, intent, sources[], table, rows[][]}` — the envelope is **required and constructed here**; there is no write path that omits it (MD-2 R2, unbypassable by construction) |
| `POST /v1/<t>/session/open` | `{session}` → capability token (Loom's, serialized) |
| `POST /v1/<t>/branch/fork` | `{session, from, child}` — M5's durable fork |
| `POST /v1/<t>/branch/merge` | `{session, child, into}` — Loom's merge law, policy re-run, all-or-nothing |
| `POST /v1/<t>/branch/rewind` | `{session, child}` — recorded, then torn down |
| `POST /v1/<t>/query/register` · `/query/read` · `/query/oneshot` · `/query/subscribe` · `/query/plan` | the same engine operations as the SQL door, JSON-typed |
| `GET /v1/<t>/semantic/answer?branch=&query=` · `GET /v1/<t>/semantic/groups?branch=&group=` | branch-scoped standing semantic answers (M2/M3 operators) |
| `GET /v1/<t>/health` | epoch, registrations, pending, admission counters |

**The operator door** — same HTTP surface, gated by the configured operator bearer token
(`Authorization: Bearer …`). Possession of the token is the wire form of M3's type separation:

| Method, path | Purpose |
| --- | --- |
| `POST /v1/<t>/action/execute` | execute a proposal through Loom's gateway (kill switch, evidence, policy, idempotency, receipts). **No agent door has this route**: not in MCP's tool list, not without the token. |
| `POST /v1/<t>/taint` | `{system, record}` → M4's taint: resolve, journal, heal, retract; answers the two-section RecallReport |
| `POST /shutdown` | graceful: drain every tenant queue, checkpoint every engine, report what drained |

**A deliberate deviation from loomd, recorded:** loomd's `audit.taint` was an agent tool because
Loom's taint is a *dry run* (AT-024: nothing executes on a signal). MutinyDB's `taint(S)` **heals**
— it retracts standing state — so it moved behind the operator boundary. An agent that could
trigger retraction on a signal would be the weapon AT-024 warns about, wearing a bandage.

### 2 · MCP — the agent door

JSON-RPC 2.0 (`initialize`, `tools/list`, `tools/call`), absorbed from loomd: served over
`POST /v1/<tenant>/mcp` and, for MCP-standard clients, on stdio via `mutinyd --mcp-stdio <tenant>`.
Error codes are loomd's, unchanged: `-32600/-32601/-32602` protocol, `-32000 DENIED` (a decision,
not a malformed call), `-32001 RESOURCE_EXHAUSTED` (admission).

Tools (the absorption map from loomd is stated so nothing is quietly dropped):

| Tool | Absorbs | Meaning here |
| --- | --- | --- |
| `session.open` | loomd `session.open` | open a session; returns branch + capability token |
| `write` | loomd `observe` / `claim.assert` | one enveloped commit into a configured table; observations and claims are rows in tables now, sources carried verbatim |
| `branch.fork` / `branch.merge` / `branch.rewind` | loomd `branch.*` | M5's durable lifecycle |
| `query.register` / `query.read` / `query.oneshot` / `query.subscribe` / `query.plan` | loomd `read` / `retrieve`, generalized | standing queries over the composed engine |
| `semantic.answer` / `semantic.groups` | — | branch-scoped standing semantic answers |
| `action.propose` | loomd `action.propose` | **propose only.** There is no `action.execute` tool, by construction — the M3 law at the MCP boundary. |

Loom's own record-level MCP surface (`loomd`) remains what it was in the component; the M3 gate
still runs its demo verbatim. `mutinyd` is the composed product's door, not a replacement for the
component's.

### 3 · The error taxonomy (D-23, inherited whole)

| Kind | HTTP | MCP code | Retry? |
| --- | --- | --- | --- |
| `Refused` — outside the dialect/contract, malformed | 400 | `-32602` | no |
| `NotFound` — unknown handle, table, tenant, path | 404 | `-32601` | no |
| `Rejected` — a conflict: dedup token reused with different content, unbindable plan, policy/capability denial | 409 | `-32000` | no |
| `Overloaded` — admission refused; quota window or queue full | 429 | `-32001` | **yes — the only retryable kind** |
| `Internal` — a bug or I/O failure | 500 | `-32603` | no |

The kind is the first line of every error body, so a client that logs only bodies still learns
which it was.

## The admission boundary (one, composed)

Every request through every door passes the same gate before it touches a plane:

- **Per-tenant quotas, Prism's discipline**: a windowed `requests_per_sec` and `bytes_per_sec`
  per tenant (config), charged per request; over quota answers `Overloaded`. Quota exhaustion is
  a *statement about the window*, so it is the retryable kind.
- **Round-robin fairness, structurally**: each tenant has its own bounded queue and its own
  worker; a request is parsed, charged, and enqueued, and the tenant's worker executes serially —
  Schweep's one-thread-one-engine determinism per tenant, Prism's
  "the quiet tenant's latency does not change when the loud tenant gets 1,000× louder" across
  tenants. A full queue answers `Overloaded` with the depth named.
- **Envelopes required on every write** — the only write path is `commit_with_capture` (MD-2 R2);
  there is no bypass flag, no test mode, and the same-door counter gate exists to catch a door
  that grew one.
- Admission is **counted per door and per tenant** (`admitted`, `refused`), and those counters
  are part of the same-door law's instrument: every door's operations must appear in the
  admission ledger, so a door that bypasses the boundary shows up as a counter shortfall, not as
  a hunch.

## Resume tokens (D-23 semantics, unchanged)

`subscribe?handle=&from=T` answers `token N` plus every retained epoch delta in `(T, …]`; `N` is
the next `from` to ask. The server holds **no cursor** — the token is the client's, so a
subscriber that crashes resumes at its own token and sees exactly the epochs it has not consumed:
no duplicates, no gaps. A token behind the retained ring is `Rejected` by name, never served a
re-baseline pretending to be a delta. The subscriber gate asserts all three properties, and its
off-by-one tooth proves the instrument fires.

## The epoch clock, composed (a contract note MD-2 R1 readers need)

MD-2 R1's epoch=commit bijection is exact through the ingest phase. Two operations mint
**engine-native epochs** with no storage commit behind them: taint's journal/retraction epochs
(M4) — and nothing else. From the first such epoch, engine epochs run **ahead** of storage commit
sequences; storage sequences stay dense on their own clock (the capture chain), the engine clock
stays dense on its own, and recovery replays the full capture history idempotently (offer every
batch; dedup drops what already landed; seal what is pending) rather than comparing the two
clocks. `mutinyd` therefore accepts writes after a taint — unlike the frozen M4/M5 dev host,
which deliberately refuses them to pin the strict phase. `AS OF EPOCH` (unshipped SQL) will mean
the **engine** epoch when it lands; `AS OF COMMIT` remains the exact, content-addressed form an
audit should quote. Re-unifying the two clocks (retraction riding a storage commit) is
engine-track work, named for M8's ledger rather than implied away.

## Graceful shutdown

`POST /shutdown` (operator): stop admitting, drain every tenant queue, checkpoint every engine
(`Engine::shutdown` = checkpoint + drain report), then exit. The response reports per-tenant
`epoch / pending_appends / registrations`, so an operator asserts the drain rather than assuming
it. SIGKILL needs none of this — that is the kill matrix's job to prove.

## Observability — answering "why is this answer late" without reading source

`GET /metrics` (Prometheus text) and structured JSON traces on stderr, one line per event.

| Signal | Names |
| --- | --- |
| Admission | `mutiny_admitted_total{tenant,door}`, `mutiny_refused_total{tenant,door,kind}`, `mutiny_queue_depth{tenant}` |
| Epochs | `mutiny_epochs_sealed_total{tenant}`, `mutiny_engine_epoch{tenant}`, `mutiny_storage_commits_total{tenant}` |
| Circuit maintenance | `mutiny_registrations{tenant}`, plus the engine's own `explain-state` / `explain-maintenance` served per tenant |
| Taint | `mutiny_taint_runs_total{tenant}`, `mutiny_taint_rows_resolved_total{tenant}`, `mutiny_taint_semantic_healed_total{tenant}` |
| Traces | `{"event":"admission"|"epoch_sealed"|"taint"|"shutdown", tenant, door, …}` on stderr; wall-clock timestamps are permitted here and only here — logs are operational, never an engine input (D-6 holds inside the boundary) |

The runbook, in one paragraph: an answer is late because (1) the request never got in — read
`refused_total` and the quota window; or (2) it is queued — read `queue_depth` for the tenant; or
(3) the epoch has not sealed — compare `engine_epoch` against the writer's ack; or (4) the
circuit is expensive — read `explain-maintenance` for the handle and the per-epoch cost MD-3's
`EXPLAIN MAINTENANCE` documents. Each step is a metric read, not a source read.

## Retired at M6, visibly

- **The M4 dev demo binary** (`mutiny-incident`'s `incident_demo` example) is retired. The
  supported form of the flagship demo is the **scripted agent driving the incident corpus
  end-to-end over MCP against `mutinyd`** — the same story, through the product door, with the
  operator's approval and taint arriving through the operator door because that is where they
  live now. The corpus, its fixtures, and every M4/M5 gate remain untouched.
- The quickstart (README, verbatim, CI-run) is the supported five-minute path; the build that
  precedes it is stated honestly where it appears.

M6 is a composition milestone, not release admission. The quarantine notice at the top governs;
M7 (fleet) and M8 (hardening, audit, naming, release) remain open.
