# M4 cross-circuit taint

Taint-as-retraction, composed across every plane: `taint(S)` resolves what is downstream of a
poisoned source through the `mutiny_derivation` standing relation, retracts it through the ordinary
delta path with Schweep's C11 `retract_source(source, table, predicate)`, and the memory, semantic,
and analytical circuits correct themselves by the same propagation that keeps them current. The
RecallPlan keeps Loom's two-section law unmodified — irreversible external actions first, with
receipts and registered compensations — and its reversible section now says *already healed*,
because the engine did it.

Implemented in `mutiny-taint` (the taint core, a trust-plane feature over the compute plane per
MD-1 R1's `trust → compute` edge) and `mutiny-incident` (the dev-only composed host that carries
the frozen incident corpus, the M4 gate, and the demo — the same all-planes posture the supported
`mutinyd` binary will hold at M6, and explicitly not that binary).

## Resolution is a query, not a DAG walk

`mutiny_derivation` is maintained by the M1 bridge: one row per (payload row, contributing source),
weights moving with the payload's, on the same epoch clock. Resolution registers a projection
query over that relation — `SELECT branch, table_name, row_key, envelope FROM mutiny_derivation
WHERE source_system = … AND source_record = …` — reads its typed answer through the engine's frame
door (no rendered-text parsing; the same reasoning as Schweep's D-23 frames), and deregisters it.
The affected set is whatever the relation currently nets to, which is what lets a second taint of a
different source compose: taints read state, not history.

Derivations can chain. A row derived from another MutinyDB row cites it with the reserved internal
source convention `SourceRef { system: "mutiny", record_id: "<table>/<row_key_hex>" }`; resolution
then iterates — each round one more query over the standing relation, seeded by the keys the
previous round found — until a fixed point. The rounds are bounded by `MAX_TAINT_ROUNDS = 64`
(Loom's `MAX_DERIVATION_DEPTH`, for the same reason): a chain that deep is a cycle or a bug, and
the refusal is by name, never a truncated set silently presented as complete. Source identifiers
containing quotes or control bytes are refused at the boundary before they can reach a predicate.

## The taint ledger

Before anything is healed, the resolved contamination set is written durably to
`mutiny_taint_ledger(source_system, source_record, branch, table_name, row_key, envelope)` —
an ordinary relation, through the ordinary ingest/seal path, on the tenant's ordinary epoch clock,
under a content-addressed dedup token (`taint:<system>:<record>:<blake3 of the resolved set>`), so
a resumed taint's re-write is `DroppedAsReplay`, not a duplicate.

The ledger is what makes the report durable against its own success. After the heal, the derivation
edges are gone — that is the point — so a crash after retraction but before the report would
otherwise leave an incident whose executed action can never again be tied to its source. The
RecallPlan is generated from the ledger plus the retraction receipts plus Loom's action ledger, and
it is regenerable forever. The audit narrative cites envelopes because the ledger carries them.

## The heal, in order

The ordering is the crash-consistency argument, so it is normative:

1. **Resolve** (queries over `mutiny_derivation`, transitive rounds).
2. **Journal** the resolved set to `mutiny_taint_ledger` (durable, deduplicated).
3. **Heal the volatile plane**: branch-scoped semantic standing state (top-k, groupings) retracts
   the contaminated keys through `OperatorTrustPlane::heal_semantic` — an operator power beside
   `install_standing`, applied to exactly the named branch. The retraction is the operator's
   ordinary `-1` path over its own held records (`retract_keys`); no second retraction semantics
   exists. Healing an already-healed key is a skip, not an error, which is what makes a resumed
   taint idempotent here.
4. **Retract the payload channels**, one `retract_source("<tenant>/<plane>/<table>", table,
   predicate)` per affected table in canonical order, the predicate an OR-chain of
   branch-and-key equalities — branch-scoped by construction, so an untainted branch's rows are
   never named.
5. **Retract the derivation channel last.** The edges are the resolution witness; they must
   outlive every step that might need to be resumed. A taint interrupted anywhere before this step
   re-resolves the same set (already-retracted payload nets to zero and produces a no-op receipt);
   a taint interrupted after it finds the ledger.
6. **Report**: the two-section RecallPlan below.

Retraction dedup tokens are Schweep's own (`retract:<source>:<epoch>:<table>:<hash>`), so a crash
inside step 4/5 leaves pending batches that the next `retract_source` call completes — C11's
pending-retraction path, exercised by the gate.

Why step 3 precedes step 4: the semantic state is memory-resident at M4 (durable forked operator
state is M5, and this document does not claim it early). A process death anywhere loses it and the
host rebuilds it from the engine's current tables, so a crash can never half-heal it. The one
remaining hazard is an in-process abort between the durable retraction and a later volatile heal —
in that order the retry would resolve an empty set and the semantic plane would stay poisoned
silently. Volatile first, durable second removes that state from the machine.

## The RecallPlan

Loom's law, unmodified: the plan leads with what it cannot undo. The irreversible section is built
by matching Loom's executed-action ledger (`ActionRecord::to_executed`) against the contaminated
row keys — an action is downstream iff any claim that justified it is — and each item carries the
connector's receipt, the registered compensating action or the honest `None`, and the escalation a
human is being asked to perform, in the deterministic order an incident responder can diff.

The reversible section is where M4 differs from Loom's dry run, and says so out loud: the writes
are listed **already healed** — branch, table, row key, the envelope that admitted them, and the
retraction epochs that removed them. The engine corrected them by the same propagation that keeps
answers current; there is no `execute_recall` step for them, and printing a proposal for work
already done would be its own kind of dishonesty. If the taint did not complete, no report is
produced — the error names the step, and the resume path above finishes the heal first.

## The frozen incident corpus

`crates/mutiny-incident/tests/fixtures/incident-corpus.tsv` plus expected-answer fixtures, all
committed and pinned by BLAKE3 checksum inside the gate. The corpus is the M4 gate, the demo, and
the permanent regression net; it is never regenerated by the code it tests, and a checksum mismatch
is a loud failure, not a refresh.

It contains: two external sources with one poisoned (`web:scraped-page-77`, plus
`web:scraped-page-91` for the compose gate and clean sources that must survive); two sessions;
three branches (`main`, the tainted `hyp-a`, the bystander `hyp-b`); claims (one multi-source, one
derived claim two hops from the poison via the internal source convention); telemetry; standing
rollups (per-branch aggregates); branch-scoped semantic top-k and grouping state; and one executed
action — `identity.suspend_account` through Loom's real gateway with a connector receipt —
justified by a claim downstream of the poison. Expected answers are committed for the standing
answers while poisoned, the healed state, and the RecallPlan text.

## The gate

`crates/mutiny-incident/tests/m4_gate.rs` proves, on the corpus:

- **(a) World-without-S.** After `taint(S)`, every standing answer database-wide — memory claims,
  per-branch rollups, semantic top-k and groupings on every branch — is byte-identical to an
  independent oracle host that replayed the corpus **never ingesting** anything the corpus itself
  declares downstream of S. The oracle is an independent recompute control in M1's sense: it is
  built in the test from the corpus's committed expectations, not by calling taint code.
- **(b) The plan.** The RecallPlan names the suspended account first, with its receipt and its
  registered compensation, and the healed section second — matching the committed expected text.
- **(c) Isolation.** The bystander branch's answers and a bystander tenant's entire host are
  byte-identical before and after. M3's isolation gate extends; it does not weaken.
- **(d) Composition.** A second `taint` of a different source lands on the healed world and yields
  the world without both — taints are not one-shot.
- **Crash injection.** The taint path is interrupted at every step boundary (after resolve, after
  journal, after the volatile heal, between payload channels, before the derivation retraction,
  before the report) and additionally mid-retraction with durable-but-unsealed pending retraction
  batches. Each interruption is followed by host reopen and a re-run `taint(S)`, and the result —
  answers and plan — equals the never-crashed twin. Never a half-heal.

## Teeth

The gate has proven it can fail, in the direction that lies pleasantly, and the proofs are
permanent tests rather than one-off reverted mutations:

- **Tooth A — a dropped derivation edge.** The dev host ingests the corpus with one derivation
  edge of the multi-source claim filtered out (a host-side filter; production capture cannot drop
  an edge without failing MD-2 R5's audit). `taint(S)` then reports clean for that claim while the
  world differs from the oracle — and the catching instrument is named:
  `healed_world_equals_the_never_ingested_oracle` must detect the divergence.
- **Tooth B — a plane skipped.** The retraction is performed for one payload channel only, and the
  cross-plane instrument `every_plane_heals_from_one_taint_call` must catch the unhealed plane.

## Known limits, written down before an evaluator finds them

- **Retraction epochs are engine-native.** A `retract_source` or ledger epoch does not correspond
  to a substrate commit, so MD-2 R1's strict epoch=commit bijection holds for the ingest phase and
  stops at the first taint. The dev host refuses storage commits after a taint rather than
  mislabeling them; threading incident epochs back through the storage clock is M6 unification
  work, named here rather than implied away.
- **Inherited materialized state does not carry derivation edges.** A fork clones its parent's
  standing semantic state (M3), but the derivation relation records the writing branch only, so a
  row inherited *as materialized state* before a fork is not re-attributed to the child branch.
  Contamination still crosses branches through explicit derivation citations (the corpus's
  two-hop claim does exactly that); taint over pre-fork *inherited* operator state needs the
  branch-ancestry relation and belongs with M5's durable forked state. The corpus forks its
  hypothesis branch before data lands, so nothing in the gate silently depends on the missing
  piece.
- **A crash between a resolution query's registration and its deregistration** leaves one rebuilt
  standing query over the derivation relation. It affects no answer and no gate; it is cleanup
  noise, and it is bounded by the number of crashes.
- **The interruption seams are first-class.** `TaintFaults`/`TaintSeam` plan an interruption at
  every step boundary of the heal, exactly as schweep-log's `FaultInjector` models its append/seal
  seams; the gate combines them with process kill-and-reopen and a constructed durable-but-
  unsealed pending retraction batch.

## What M4 asks of the earlier phases

- `mutiny-bridge` exposes its prepared admission batches (`prepared_batches`) so the composed host
  can drive the identical translation into the C9 engine (`schweep-server::Engine`), whose
  `ingest`/`seal` is the same N-appends-then-seal admission MD-2 ask 3 already ruled sound. The
  translation is not duplicated; only the log driver differs.
- `mutiny-trust` gains the operator-side `heal_semantic` (and its `retract_keys` counterpart in
  `mutiny-semantic`) described above. The agent surface is unchanged and still owns no execute or
  heal method.
- Schweep's C11 `retract_source` predicate was verified against the exact `220bf6b` snapshot
  (engine + wire + differential gates exercise it); MD-2 ask 2's disposition stands.

M4 is a composition milestone, not release admission. Durable forked operator state (M5), the
unified process and doors (M6), fleet behavior (M7), and external evidence (M8) remain open, and
every component's release quarantine in `components.lock.json` is unchanged by this phase.
