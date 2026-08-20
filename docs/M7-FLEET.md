# M7 the fleet plane — many tenants, sleep/wake economics, wake-on-delta

The M6 per-tenant pool structure becomes fleet mechanics: a durable registry of tenants that can
be added, removed, slept, and woken without restarting `mutinyd`; a sleeping tenant that is bytes
on the storage backend plus a registry row and **nothing resident**; a wake whose cost is
O(checkpoint + suffix), not O(history); and wake-on-delta — an arriving delta wakes the tenant it
belongs to and only the circuits it feeds, with the delta→circuit mapping **observed from the
compute plane**, exactly as MD-1 R2 anticipated. The quarantine discipline is unchanged: this is
still the composed-development form of the product; nothing here alters any component's release
admission.

## The prerequisite: bounded wake (the economics are impossible without it)

M6's recovery replays the full manifest capture history — correct, and O(history). The kill
matrix measured what that costs as history grows: 1,000 kill/recover cycles over ~3,000
accumulated commits took 5,513 s in release mode, the growth dominated by recovery replay. That
is the baseline this phase must beat, and the sleep/wake path now does:

- **The engine side already had the machinery**: `Engine::compact()` (Schweep C7) checkpoints,
  writes the Parquet snapshot at the sealed anchor, and truncates the retained log — so a
  reopened engine hydrates snapshot + suffix, never the full log. Sleep calls it.
- **The plane side gains a checkpoint**: `plane-checkpoint.json` in the tenant's directory,
  written atomically at sleep, holding `commit_seq`, the fork-lineage events, and the
  **standing-state membership** — for every active branch, the `(table, key)` set its semantic
  operators hold. Membership suffices because of an invariant M4/M5 established: every record in
  any branch's standing store corresponds to a **live row in the engine's current tables**
  (heals cascade to every holder; rewinds drop stores), and semantic records are a deterministic
  function of row content. So wake rebuilds each branch's stores from membership × current rows —
  O(current data) — with no serialized operator state to drift from the log.
- **Wake** = open the engine (snapshot + suffix), read the checkpoint, recreate active branches,
  hydrate their stores from membership, replay only the captures **after** the checkpoint's
  `commit_seq` (a fresh sleep leaves none; a stale checkpoint self-heals from the suffix), then
  re-apply the taint ledger's heals — idempotent, retract-by-key, skip-absent, so re-applying
  all of them is safe by construction.
- **Fail closed**: a tenant the registry says was slept-with-checkpoint but whose checkpoint file
  is missing does **not** silently fall back to full replay — it refuses by name, because a
  missing artifact the contract promised is corruption, not an inconvenience. The full-history
  replay remains exactly one thing: the **crash path** for a tenant that was awake when the
  process died (registry state `awake`), where no checkpoint was promised.

The gate builds a long history (writes and taints), sleeps, wakes, and asserts byte-identical
standing answers against a never-slept twin **and** a measured wake budget; the bounded-vs-full
cost ratio is printed alongside.

## The registry

`fleet-registry.json` under the data root, written atomically (tmp + rename): one row per tenant
— its full `TenantConfig`, its state (`awake` / `asleep`), and whether a sleep checkpoint was
recorded. The registry is the durable fleet truth:

- **Register / remove / sleep / wake are operator-door operations** (`POST /fleet/…`, bearer
  token), applied without restarting the process. The static config's `tenants` list seeds the
  registry on first boot; thereafter the registry is authoritative.
- **A crashed `mutinyd` recovers its fleet**: on start every registry row is present and
  serveable; nothing is resident until touched. Rows marked `asleep` wake through their
  checkpoint; rows marked `awake` (the process died under them) wake through full replay — the
  M6 crash discipline, unchanged and already gated by the kill matrix.
- **Removal is teardown with byte accounting**: the M5 rewind discipline at fleet granularity.
  The tenant's worker stops, its directory is deleted, its registry row is removed, and the gate
  asserts the tenant's on-disk bytes return to zero and the resident-plane count to its prior
  baseline.

## Sleep

`drain → checkpoint → close → bytes`: the tenant's worker finishes its queue (single-writer, so
drain is structural), the engine compacts (checkpoint + snapshot + log truncation), the plane
checkpoint is written, and everything is dropped — engine, state store, substrate handles. A
sleeping tenant is **files on the storage backend plus one registry row**; the gate asserts the
resident footprint (no worker, no open plane, no engine) rather than describing it, and the
simulation asserts it at scale with measured RSS.

## Wake-on-delta

A request arriving for a **registered, sleeping** tenant wakes it and is then served — the wake
is the admission boundary's job, not the client's. Selectivity is structural and gated at two
granularities:

- **Tenant-granular**: a delta for tenant A wakes A. B stays a registry row. The simulation
  counter-asserts this: wake K of 10,000 and the resident count is exactly K + the baseline.
- **Circuit-granular**: within a woken tenant, a delta to table T moves only the circuits that
  read T. The gate asserts it from the outside: the subscription delta for every unrelated
  standing query is **empty** for that epoch, and the delta→circuit mapping predicts exactly
  which handles may emit — a prediction the gate cross-checks against what actually emitted.
  (Every registration still *hydrates* at engine open — per-circuit lazy hydration inside the
  engine is engine-track work; what the mapping bounds today is the per-delta maintenance, which
  is the recurring cost.)

**The mapping is an exported observation, as MD-1 R2 anticipated.** The fleet plane reads the
compute plane's own persisted registration file (`schweep_server::Registry::load`) and binds each
registered SQL text through the compute plane's public binder (`schweep_sql::bind_sql`), walking
the bound `Source` tree for its table set; configured semantic operators contribute their
configured tables. Compute never calls the registry — the fleet **observes** interfaces compute
already exports. This adds one reviewed dependency edge (`mutinyd → schweep-sql`,
observation-only), recorded in the boundary matrix with this paragraph as its reason.
`GET /fleet/mapping?tenant=` serves the mapping, so an operator can ask "what would this delta
wake" without reading source.

**Taint composes with sleep** (M4 × M7): `taint(S)` against a sleeping tenant is an operator
request like any other — it wakes the tenant, resolves and heals through the ordinary M4 path,
and the tenant can be re-slept; the gate runs the M4-style oracle comparison across the full
sleep → taint → re-sleep → wake cycle.

## The simulation

N sleeping tenants on one host (N = 10,000 in the published run), each registered, written to,
and slept; the gate asserts flat measured RSS while they sleep, wakes a random subset via
deltas, asserts selectivity by count, and publishes **p50/p99 wake-to-first-answer** with the
storage backend named. The published run executes in CI under a **cgroup memory ceiling** so the
number means something; the evidence ledger (`crates/mutinyd/evidence/m7-fleet-sim.json`) records
the numbers, the scale, the ceiling, and the backend. `M7_SIM_TENANTS` scales the same test down
for the PR gate.

## Wide-area wake, honestly

The roadmap's "known 1020 ms item" was **closed on the component track** while this repository
was mid-consolidation, and the composed system inherits the result rather than re-measuring it
badly: substrate ≥ v1.4.2 records the warm set a session faults and carries it in the sleep
token; wake hydrates it in **one concurrent `get_batch`** (`TieredCas::get_batch`, coalesced
object-storage GETs); substrate v1.5.0's `WarmPool` holds keep-alive connections so the hydrate
pays no handshake tax. Measured against a deliberately extreme intercontinental endpoint
(Sydney, RTT ≈ 226 ms, `wake-latency-widearea.yml` on the component's track): cold first-ever
wake ~4 RTT (pointer-chasing is inherently serial); hot re-wake with no pool 2.88 RTT; **with
the warm pool ~1 RTT at the median (p50 179 ms), p99 ~2 RTT (345 ms)**. The honest SLA is
therefore **"wake ≈ 1 RTT to your object store"** — topology, not code — and the deployment
recommendation is in-region co-location, where even the 2-RTT tail clears 250 ms with margin.

**The composed disposition, stated rather than implied**: `mutinyd`'s tenant stores currently run
on the **local filesystem** (the imported Loom flavor is air-gapped by this repository's own
pinning, and `substrate-store`'s remote tier is not yet wired under the composed WAL), so every
number the M7 simulation publishes is a **same-host, local-disk number and is labeled as such**.
Wiring the remote tier under the composed store is deployment-surface work that inherits the
measured ≈1-RTT property above; it is **open for the composed path** and named on M8's ledger —
not claimed.

## Teeth (instruments named; constructed, caught, and kept as permanent tests)

- **(a) A wake-on-delta that wakes every tenant.** Answers stay right, so the catching instrument
  is the **counter half of the selectivity gate**: resident-plane count after K deltas must be
  exactly baseline + K; the tooth wakes all and the count instrument fires.
- **(b) A sleep that skips the checkpoint.** Two failure shapes, both caught: a sleep that
  recorded `asleep` but left **no checkpoint** is refused by name at wake (fail-closed — the
  wake-correctness gate's refusal arm); a checkpoint written from **incomplete state** (the real
  bug class: membership missing a branch's rows) wakes into answers that differ from the
  never-slept twin, and the byte-compare instrument fires.
- **(c) A registry that forgets a sleeping tenant on restart.** The fleet-recovery gate
  enumerates the registry after restart and wakes every row; the tooth drops a sleeping tenant's
  row and the enumeration instrument fires.

M7 is a composition milestone, not release admission. M8 (hardening, audit, naming, release, and
the component-track items — the engine's remaining qualifying nights, PrismDB's release tag and
KMS receipt, trademark clearance) remains open.
