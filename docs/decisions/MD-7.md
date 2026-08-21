# MD-7 · KMS custody — deferred to first enterprise adoption

**Status:** Accepted
**Phase:** M8 hardening · **Governs:** PrismDB's custody blocker (`EXT-KMS`), the release-vs-
production admission split, `components.lock.json`'s admission levels, and the closure path for
live key custody
**Relates to:** MD-6 (quarantined imports), PrismDB's own `EXT-KMS` / `DATA-01` / `EXT-SCALE`
records (read from the imported snapshot at `components/prismdb/docs/`, per MD-1 R4 never
amended from here)

## Context

PrismDB's per-tenant envelope encryption is implemented and gated: an AWS KMS `KeyProvider`
exists, the encryption gate suite covers wrap/unwrap on both paths, the full
expand/activate/rewrap/retire rotation, restore through a retired-but-authorized key, and the
disaster drill with encryption enabled. **Every one of those gate runs to date used the software
keystore.** That proves the code path. It does not prove custody: no run has produced the named
failure modes — unreachable, denied, throttled, revoked — from a real key service in a real
account, and PrismDB's own `EXT-KMS` gate says exactly that and blocks its production approval
on it. This repository's lock has, until this record, carried the same fact as one flat blocker
line ("a live AWS KMS rotation/rollback receipt has not been produced"), which conflates two
different questions.

## The split, and its precedent

The two questions are:

1. **Is PrismDB admissible for the `mutinydb-v0.1` OSS release?** A release ships source and
   gates, states its custody posture precisely, and claims nothing about any live deployment.
2. **Is custody proven for a production-approval claim?** That requires the real key service.

PrismDB's own governance already splits exactly this way, twice:

- **`DATA-01`** holds status *partial* with its remaining gaps named — partial protection ships,
  precisely described, while the remainder blocks approval, not release.
- **`EXT-SCALE`** records that the multi-host measurement is *"deliberately NOT funded before a
  design partner needs it"* — an honest open gate whose closure is triggered by the first party
  who actually needs the number, with the CI-runner shortcut explicitly forbidden because it
  measures the harness, not the claim.

This record applies the same two moves to custody.

## Decision

1. **Release admissibility.** PrismDB is admissible for the `mutinydb-v0.1` OSS release with
   custody stated precisely: the KMS provider is implemented and gated; every gate run to date
   used the software keystore; **live-service custody is unproven**. Release admission still
   requires everything MD-6 requires — PrismDB's release tag, the composed and release gates —
   none of which this record waives.
2. **`EXT-KMS` remains open and blocking** — for any production-approval or proven-custody
   claim, not for the release. The lock now expresses this as two levels per blocker rather
   than one flat list: `blockers` (release-blocking) and `productionBlockers` (open past
   release admission). The verifier refuses a production-approval marking while
   `productionBlockers` is non-empty, refuses it without a custody receipt, and continues to
   refuse *admission entirely* while PrismDB's release tag does not exist.
3. **Closure path, owner, trigger.** Owner: Security Engineering and SRE (PrismDB's own
   `EXT-KMS` owner role). Trigger: **the first enterprise adopter** runs the encryption gate
   suite against **their** key service in **their** account, and the receipts name that
   backend — the `EXT-SCALE` partner-trigger discipline applied to custody, and strictly
   stronger evidence than a vendor-account run because it proves custody in the deployment
   shape that matters.
4. **Fallback.** If a serious evaluation cannot or will not run the suite, the direct AWS close
   (three keys, scoped IAM, ~30 minutes of operator time) happens **before any
   production-approval claim** is made — the claim waits for the receipt, never the reverse.
5. **Simulated or emulated closure remains forbidden**, per the gate's own acceptance criteria:
   the named failure modes must be produced by the real key service, not injected by the
   software keystore's fault surface. Nothing in this record weakens that sentence.

## The recorded ask for PrismDB's own track (MD-1 R4: recorded here, executed there)

This repository does not amend sibling or component documents. The ask, recorded for PrismDB's
next session on its own track: PrismDB's `EXT-KMS` text in `enterprise-readiness.json` /
`procurement-readiness.md` gains the **partner-closure path and the direct-AWS fallback** as its
stated closure route, mirroring the `EXT-SCALE` partner-trigger language it already uses. Until
PrismDB records that on its own track, this MD is the only place the closure path is normative.

## Consequences

- `components.lock.json` gains `productionBlockers` (and a `productionApproved` field the
  verifier refuses to accept while they stand or while the custody receipt is absent);
  `verify_component_lock.py` enforces both directions, and a counter-check test proves the
  refusals actually fire (a gate that cannot fail is not a gate).
- The README quarantine table states PrismDB's custody posture in one sentence and keeps
  `EXT-KMS` visible — the open gate is a credibility asset, not a liability to bury.
- The engine's row carries issue #18 (the snapshot `DEDUP` index, O(commits-ever)), so the
  deliberately red nightly soak is explained where a stranger reads.
