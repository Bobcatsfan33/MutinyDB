# M8 maintenance — awake compaction, the checkpoint-aware crash path, and bounded storage

This document is the durability contract for closing issue #12. It is written **before** the
seams it names exist in code, and the crash-injection matrix tests every seam it declares. The
quarantine discipline is unchanged: everything here composes the locked component snapshots
through their public APIs; no component source is modified.

## The problem, restated precisely (#12)

The manifest history is the durable queue and the sealed epoch is the offset (docs/M1-BRIDGE.md).
That law is why recovery works — and why an *awake* tenant's storage grows O(commits): every
commit adds a manifest (holding its capture page) and fresh application pages at
`seq × PAGE_STRIDE`, the head manifest references every page ever written, and nothing consumes
the queue below the offset. M7 built the bounding machinery — `Engine::compact` plus the plane
checkpoint whose membership makes bounded rehydration safe — and wired it only at sleep. The
nightly soak measured the consequence: RSS tracking O(commit-history) growth, never green.

## The maintenance operation

Maintenance runs **at a drain point on the tenant's worker** — the single writer, between jobs —
so drain is structural, exactly as sleep's is. It is never run from a commit, read, or subscribe
path (substrate's own law: GC never in a publish path). The sequence, in order, with the crash
seam after each step named `S1..S6`:

1. **Engine compact** (`S1`) — Schweep C7: compute checkpoint + Parquet snapshot at the sealed
   anchor + log truncation. Guarded by the engine's own stated preconditions (a sealed epoch
   exists and is ahead of `retained_from`); when the guard refuses, plain `checkpoint()` runs
   instead. Crash after: the engine's own recovery suite covers it; the plane checkpoint below
   has not moved, so both plane paths still work.
2. **Plane checkpoint** (`S2`) — the M7 artifact (`plane-checkpoint.json`, atomic tmp+rename):
   `commit_seq`, fork lineage, standing-state membership. **This must be durable before any
   storage truncation** — it is the object that replaces the truncated history's recovery role.
   Crash after: checkpoint present, storage untruncated; both the checkpoint path and full
   replay are valid and agree.
3. **Storage prune** (`S3`) — one ordinary WAL-committed transaction that `remove`s the
   application pages of every capture with `commit_seq ≤` the plane checkpoint's — the consumed
   prefix of the queue. The capture page (`u64::MAX`) is never touched: the resulting manifest
   still carries the head capture, so the bridge's dense-sequence law
   (`validate_parent_sequence`) is preserved for the next commit. Application pages are
   write-only after commit (only `CAPTURE_PAGE` is ever read back — the M1 audit is at commit
   time), so pruning consumed pages changes no answer. Crash after: a normal committed
   transaction; recovery replays it like any other.
4. **Collapse** (`S4`) — install a **flat root manifest**: body = the resolved page map of the
   pruned head, `parent = None`, depth 0, the head's own `created_at_ms` (so collapsing the same
   head twice is the same content-addressed manifest — idempotent by construction). This cuts
   the history edge deliberately. The install alone is volatile — the pager's head durability
   comes from the WAL — so a crash here simply loses the collapse: recovery replays the WAL back
   to the pruned head, and the next maintenance run collapses again.
5. **WAL checkpoint** (`S5`) — `DurableStore::checkpoint()`: the collapse becomes durable
   (recovery now starts at the flat root) and WAL segments behind it are truncated. Substrate's
   own crash story covers the marker write (a crash between record and marker replays from
   further back and arrives at the same place).
6. **GC** (`S6`) — `gc([head])`: liveness recomputed from the manifests themselves (substrate's
   rule — no refcount file), sweeping every pre-collapse manifest and every page only they
   referenced (consumed application pages, historical capture pages). **GC runs strictly after
   the WAL checkpoint**: sweeping manifests the WAL might still replay against would leave a
   crashed store unopenable. An interrupted sweep leaves garbage for the next run — it cannot
   delete anything live, because the live set is computed before any deletion.

The same sequence (minus the policy trigger) runs at **sleep**, so a sleeping tenant's bytes are
bounded too: sleep = drain → compact → plane checkpoint → prune → collapse → WAL checkpoint →
GC → close.

7. **The binary's allocator** (`S7`) — not a pass step but a property the passes rely on. The
   engine's compaction materializes the full live state transiently each pass (its design, its
   track), and glibc keeps those freed arena pages resident: the first post-fix nightly
   measured resident ≈ 5× live data from exactly this, while macOS's `footprint` — which does
   not count clean reclaimable pages — showed the same process flat. The obvious fix
   (`malloc_trim` at the drain point) is FFI, and this workspace **forbids unsafe, stated
   before code** — so the supported binary instead pins an allocator that purges freed pages
   back to the OS (`#[global_allocator]` mimalloc — safe code, and a stronger property:
   resident tracks live data continuously, not only at maintenance ticks). The instrument
   stays honest by construction: an allocator can only return *freed* memory, so a genuine
   leak still fires the soak's residual gate. No crash seam is added — the allocator holds no
   durable state.

## The policy

`maintenance_every` — maintenance triggers when `commit_seq − last_maintained ≥ N`. The default
N is a **measured constant, not folklore**: candidates are run under the soak workload and the
choice is recorded in `crates/mutinyd/evidence/m8-maintenance-policy.json` with the measurement
artifact (RSS shape, maintenance pause, on-disk bound per candidate). Tenants can override it in
config; `0` disables (dev only — the soak gates the default).

## The checkpoint-aware crash path

Recovery semantics change, so the full kill matrix revalidates them (the 1,000-SIGKILL
release-mode form, in the nightly — not the PR form).

- **With a plane checkpoint present** (any tenant that has ever slept *or been maintained*),
  recovery = the M7 bounded path: engine snapshot + suffix open, membership × current rows,
  capture replay strictly after `checkpoint.commit_seq`, idempotent ledger-heal reapply. A stale
  checkpoint self-heals from the suffix — this is the property the kill matrix now exercises on
  every kill that lands after a tenant's first maintenance.
- **Without a plane checkpoint**, full replay remains exactly what it was — for a tenant that
  has never been maintained and never slept.
- **Fail closed at the seam between them:** full replay against a collapsed store cannot
  reconstruct pre-collapse history, and it must not try. The bridge's dense-sequence law already
  refuses it structurally (the walk stops at the flat root; the first recovered capture is
  `R+1 ≠ 1` → sequence gap), and the plane names the refusal: a collapsed store without its
  plane checkpoint is corruption — *"refusing full replay of a collapsed store"* — never a
  silent partial rebuild. This is the same discipline as M7's missing-checkpoint refusal.

## Interaction with taint (M4) and forks (M5)

Retraction epochs are engine-native and the ledger's heals are re-applied idempotently on every
recovery path — maintenance does not touch either mechanism. What maintenance *does* change is
how much capture history exists; the gate therefore re-proves the M4 oracle across maintenance
(taint → maintain → crash → recover → answers byte-identical to the never-maintained twin) and
fork lineage across maintenance (lineage lives in the plane checkpoint, proven at M7). The M4
incident-corpus gates run unchanged in CI.

## Teeth (constructed, caught, kept as permanent tests; instruments named)

- **(a) A maintenance that drops an unsealed capture.** Constructed by collapsing a store whose
  newest capture the compute plane has not sealed (the checkpoint claims a `commit_seq` ahead of
  the truth). The catching instrument is the **kill-matrix twin comparison**: recovery cannot
  produce the acked write, and the recovered answers diverge from the never-crashed twin (or the
  refusal fires — either instrument ends red).
- **(b) A GC that reclaims a page a sleeping tenant still references.** Constructed by sweeping
  with the wrong live roots against a slept tenant's store. The catching instrument is the
  **wake-correctness gate**: the wake either refuses by name (the store cannot serve its head)
  or diverges from the never-slept twin. Silence is impossible because wake always reads the
  suffix through the head.

## A note on the instrument (found while validating this fix)

With storage bounded, the local (macOS) full-window soak still failed its shape gate — and
attribution showed why: the process's **physical footprint was flat at ~21 MB (peak 26.2)**
while `ps rss` read 66 MB and climbing, the difference being *clean, reclaimable* pages the
macOS allocator retains in empty `MALLOC_MEDIUM` zones and `ps` counts as resident. That is an
instrument artifact, not memory the OS cannot take back. The soak therefore measures
`footprint(1)` on macOS and `ps rss` on Linux (the platform the nightly gate actually runs on,
where the default allocator returns large freed blocks via `munmap`). An instrument that fails
flat processes is as dishonest as one that passes growing ones.

The shape baseline moved for the same reason: the original gate compared the last third against
the **first** third, and the first third is the warmup ramp from a cold start to the working
set — a gate that punishes reaching a working set fails flat processes too. The gate now
compares the last third against the **middle** third at 1.30×.

And the shape's subject changed once more, for a reason worth recording. With storage bounded,
the full-window soak still climbed ~8 KB per window — attributed (sampled du vs footprint, in
lockstep) to the **taint ledger**: M4's law makes it append-only, journaling every resolved
row with its envelope, so ~1 taint/second accretes ~10 KB/second of *live, queryable,
contractual data*, and the engine's compaction snapshot — and one resident working copy —
grow with it. That is a database holding data, not a process leaking; a shape gate on raw RSS
is rate-dependent (it fails at a fast local writer's 16 windows/s and passes at CI's 2/s —
same system, same correctness). The gate therefore asserts the shape of the **residual**:
resident memory minus the *measured* on-disk live-data snapshot, both printed per run. Memory
may track live data; it must never track history. The pre-fix bug still fires the residual
gate — its consumer was capture-history storage, uncorrelated with the live-data term. The
absolute budget and the storage-bound assertions are unchanged. Bounding the ledger itself
(archival/compaction of resolved recalls without breaking M4's regeneration promise) is named
on the M8 ledger, not silently deferred.

## The gate (issue #12's exit)

1. **The nightly soak goes green for the first time** — the full 1,500 s window, RSS flat by
   shape *and* budget, with `storage/manifests` and `storage/pages` bounded and measured in the
   run's output. The green run's link is the exit criterion.
2. The **kill matrix** (1,000 SIGKILLs, release) passes with maintenance on — the new recovery
   semantics revalidated, plus deterministic crash injection at `S1..S6`.
3. The **M7 fleet sim** re-runs green — wake economics unchanged or improved, the
   24,576 B/sleeper bound held.
4. The **M4 taint gates** re-green, plus the explicit taint-across-maintenance oracle above.

Per the conditional-green rule this session added to the roadmap: this gate is **conditionally
green until the nightly run reports**, and the phase record cites the run link, not the
dispatch.
