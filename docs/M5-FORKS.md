# M5 forked standing state

A session branch carries its standing answers. A fork gives the child the parent's live standing
state and both sides maintain independently from their own commits; a merge lands the child's
divergence on the parent under Loom's law; a rewind tears the child's state down to the byte; a
restart rebuilds every branch to byte-identical answers; and a taint after a fork heals the
parent **and** the fork's inherited state from one call. Implemented in `mutiny-forks` (the pure
lineage model and the spike evidence) and the composed host, per the path MD-5's spike selected.

## The economics, first, because they are the honest headline

**Fork cost is O(state), not O(1).** The spike proved copy-on-write operator state on substrate
pages viable in the small (fork flat at 3.5–4.8 µs across 1k–50k entries; answers correct), but
adopting it end-to-end at v1 fails MD-5's C4: Schweep's `StateBackend` froze at C4 without a fork
operation and circuit instantiation is engine-global, and MD-1 R3/R4 forbid bending a sibling's
track. So M5 ships the pre-decided fallback: **hydration by clone** — measured at 0.77 ms for a
1k-row branch and 26 ms for a 50k-row branch (40 MB of standing state, ~520–770 ns/row
worst-honest; `crates/mutiny-forks/evidence/m5-fork-cost.json`). The fork-cost gate *asserts* the
cost grows with state, so the O(1) claim cannot quietly reattach itself. O(1) fork of live
answers moves to post-v1 on the engine's own track, re-opened by amending MD-5 — the spike's
numbers are its baseline.

## The lifecycle, durably

Every lifecycle event is an ordinary commit through the M1 front door, on the tenant's one epoch
clock, into the `mutiny_forks` relation (`child, parent, at_epoch, kind`):

- **Fork** = one commit on the parent's timeline (`kind=fork`), then the child's standing
  operators are hydrated from the parent's live state and the child's capability token minted
  through Loom. Inheritance is real and gated: the child's top-k and groupings hold the parent's
  pre-fork rows and never its post-fork ones.
- **Rewind** = one commit (`kind=rewind`), then teardown. The mount's state accounting returns
  **exactly** to its pre-fork baseline — the C6 teardown discipline, composed — while the
  branch's committed history remains: auditable, never destroyed.
- **Merge** follows Loom's law, composed: the child's post-fork divergence becomes **new commits
  on the target**; every merged write is re-evaluated against policy **at merge time**,
  all-or-nothing (a denied merge writes nothing); each merged row carries its **own** original
  sources (Loom I-2's per-key rule, never the union) plus a durable merge marker
  (`loom:merge/<child>` in the derivation relation) — the composed analog of Loom's merged-from
  memory. A re-merge with no new work merges zero rows, and a crash-resumed merge completes
  exactly the rows the marker says are missing. That is where the Loom +6-not-+3 double-count
  class dies, and the gate proves it in both directions.

## Durability is lineage replay

Recovery replays the manifest capture history oldest-first: payload rows rebuild each branch's
standing state; a fork record clones the parent's replayed state at exactly that point — so
inheritance falls out of replay order; a rewind record tears it down again; then the taint
ledger's heals are re-applied through the lineage. There is no serialized per-branch state file
to drift from the log. The gate kills the process mid-fork (record durable, hydration lost) and
mid-merge (one of two rows landed) and proves the resumed world byte-identical to a never-crashed
twin — never a hybrid, never a double.

## Taint composes with forks (M4 × M5)

The taint heal now cascades through the lineage: healing the writing branch also heals every
**active** descendant's standing state, retract-by-key, skip-absent — so the parent's pre-fork
poisoned row leaves the fork's *inherited* answers from the same one call. This supersedes the
M4 known-limit on inherited materialized state, for branches with recorded lineage. The forked
incident corpus (`incident-corpus-forked.tsv` — fork, diverge, merge, rewind, one executed
action, then the taint) is committed, checksum-pinned, and permanent, and its healed world is
byte-identical to an oracle that replayed the same lifecycle without ever ingesting the source.

## Isolation is structural, and the oracle runs over circuit state

The M3 isolation gate extends from data to circuits: a randomized model gate drives fork/write
sequences over up to five branches and asserts after every step that each branch's grouping
circuit holds exactly its model set — own writes plus fork-time inheritance, never a sibling's.
Teeth, with their catching instruments named: **(a)** a fork that shares operator state by
reference (a parent write leaked into the child's store) is caught by the isolation oracle's
per-branch expectation; **(b)** a merge that double-applies shared history at the engine door is
caught by the totals instrument — +3 became +6, and the gate sees exactly that.

## Scope, stated rather than implied

The branch-carried standing state at M5 is the trust-mounted result stores (top-k and
grouping/aggregate operators), per M3's law. The engine's tenant-global SQL circuits remain
single-clock and branch-*tagged* (MD-2 B3/R7): their per-branch rows are data scoping, not
carried state, and a rewound branch's rows remain visible there as audit. Lifecycle commits keep
the epoch=commit bijection intact; taint retraction epochs remain the one engine-native
exception, so the composed corpora taint last (the M4 limit, unchanged). Loom's own record-level
merge and its four model oracles are untouched and still gated at M3.

M5 is a composition milestone, not release admission. The unified process and doors (M6), fleet
behavior (M7), and external evidence (M8) remain open; components stay quarantined in
`components.lock.json`, unchanged by this phase.
