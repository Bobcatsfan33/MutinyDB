# M1 bridge operations and recovery

`mutiny-bridge` is MutinyDB's only storage-to-compute admission boundary. It uses the real
`substrate-wal`, `loom-core`, and `schweep-log` APIs from the exact component trees recorded in
`components.lock.json`.

## Write and recovery sequence

1. The writer stages application pages in a substrate transaction and constructs a `CommitDraft`
   from the same logical changes.
2. `commit_with_capture` verifies the envelope through the required `EnvelopeAuthority`, checks
   that every staged application page is explained by at least one logical change and that no
   logical change claims an unstaged page, and writes the bounded capture to reserved logical page
   `u64::MAX`.
3. The function consumes the transaction immediately. Application code cannot add a write after
   the audit. Substrate commits the application pages and capture page in one WAL transaction.
4. `apply_commit` emits payload and `mutiny_derivation` batches in canonical table/row order and
   seals exactly the dense epoch named by the storage commit sequence.
5. The compute log's sealed epoch is the consumer offset. After a crash,
   `recover_pending_captures` walks the immutable substrate manifest history from the current head,
   returns every newer capture oldest-first, and the caller re-applies them. Content-addressed
   tokens make retries idempotent.

There is no mutable second checkpoint between storage and compute. The manifest history is the
durable queue; the sealed epoch is the offset.

## Fail-closed boundaries

- The capture page is reserved and cannot already be present in the caller's transaction.
- Commit sequences must start at one and be dense relative to the parent manifest's capture.
- Sequence one is accepted only on an empty substrate root. A non-empty store with no capture page
  requires an explicit, separately gated bootstrap migration; pre-Mutiny data is never silently
  omitted from the compute plane.
- Tenant, plane, branch, and table identifiers are non-empty and cannot contain path separators or
  control bytes.
- A structurally incomplete envelope, branch mismatch, refused authority decision, forged manifest
  id, zero-weight row, empty table, physical/logical audit mismatch, sequence gap, altered replay,
  or foreign pending log batch stops admission before a new epoch is sealed.
- An old replay whose records have been compacted is refused rather than acknowledged without
  content verification.

## Bounded data and operator responsibilities

The capture codec rejects inputs above 1 MiB, and substrate additionally enforces the tenant
store's configured page size. A transaction whose logical capture does not fit one page must be
split before commit; the bridge never truncates or spills to an untracked side file. This is a
deliberate admission bound for M1. Multi-page capture is a future format change and must retain one
atomic manifest reference.

`EnvelopeAuthority` is mandatory on both storage commit and compute admission. A production trust
plane implementation must resolve the actor's registered key, verify the Loom Ed25519 signature,
and durably register the exact `EnvelopeId` before returning success. The M1 bridge owns the
requirement and call ordering; M3 supplies the enterprise authority implementation.

The reserved capture page contains provenance and row values. It receives the same encryption,
keyed page identity, backup, retention, and tenant-isolation controls as every other substrate page.

## Evidence

The `m1_gate` suite proves:

- 1,024 deterministic randomized commits equal an independently translated direct-ingest control
  batch-for-batch at every epoch;
- retries do not create a second epoch and altered retries are refused;
- all seven append/seal fault seams recover to the never-crashed twin;
- a crash after substrate commit and before the compute log opens recovers from the same manifest;
- multiple unapplied storage commits are recovered from manifest history in dense order; and
- all negative admission and bypass cases above fail closed.

These are development proofs. Schweep remains release-quarantined until its `current-v0.1`
scheduled-night contract passes, and MutinyDB remains unapproved for production until the later
M2–M8 composed and external gates are complete.
