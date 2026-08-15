# DURABILITY — the exact orderings, and every point a crash may land

**This document is written before the code.** §6 C4's pitfall says so in as many words:

> fsync discipline — write down the exact ordering (state flush → checkpoint record → log trim) in a
> doc comment before implementing, and have the crash harness kill between each pair.

Durability bugs are not found by reading code, because the code looks right at every line. They are
found by naming the instants between the lines and then landing on each one deliberately. So this
document numbers the instants. **The crash harness enumerates the kill points defined here**; a seam
that is not in this document is a seam nothing tests.

Rules referenced as `S-n` are in `docs/SEMANTICS.md`; invariants as `I-n` are in `ARCHITECTURE.md`
§4.

---

## 0 · What durability has to deliver

Two invariants, and everything below exists to serve them.

- **I-4 · Exactly-once ingest.** An acknowledged input batch is applied in exactly one epoch,
  survives crashes, and is never applied twice. Replays are detected and suppressed at the log.
- **I-7 · Crash equals replay.** Recovery = load last checkpoint + replay log suffix, and the
  recovered state is byte-identical to a process that never crashed — *provable* because of I-2.

The word **provable** is the load-bearing one. Because everything downstream of a sealed epoch is a
deterministic function of the log (I-2, D-6), "byte-identical to a twin that never crashed" is a
comparison a test can actually make: run a scenario twice, crash one of them, compare state
fingerprints and answers. Without determinism this would be untestable and the invariant would be a
hope.

## 1 · The ack sequence

What happens when a source appends a batch. **Nothing is acknowledged before it is durable**, and
the durable record is what a later replay is checked against.

| Step | Action | Durable after? |
| --- | --- | --- |
| **A1** | Validate the batch against the table's schema (S-2). A malformed batch is refused and *nothing is written*. | no |
| **A2** | Look the `dedup_token` up in the in-memory dedup index, which was rebuilt from the log at open. | no |
| **A3** | If the token is known **and the content hash matches** → acknowledge and drop. Idempotent by construction; no write. | no |
| **A4** | If the token is known and the content hash **differs** → refuse loudly (`TokenReused`). Never silently rewritten. | no |
| **A5** | Append the record — length, CRC, then payload — to the open segment file. | not yet |
| **A6** | `fsync` the segment file. | **yes, here** |
| **A7** | Insert the token into the in-memory dedup index. | yes |
| **A8** | Return the ack to the caller. | yes |

**Why A4 refuses rather than overwriting.** A reused token with different content is not a replay,
it is a *bug in the caller* — two different batches claiming the same identity. Accepting either one
silently would make "exactly once" a statement about counting rather than about identity, and the
wrong batch would be the one that survived. §5.4 says "refused loudly" and that is what it means.

**Why the fsync is at A6 and not later.** The ack at A8 is a promise that the batch survives a crash.
A promise made before the data is on disk is the classic acknowledged-then-lost bug, and it is
invisible in any test that does not actually crash — which is why it is the first of C4's two
canonical mutations (§6 below).

### Kill points in the ack sequence

| Kill point | Lands between | What recovery must show |
| --- | --- | --- |
| `AckBeforeValidate` | before A1 | the batch never existed; the caller has no ack, so it may retry with the same token |
| `AckBeforeAppend` | A4 → A5 | as above: no record, no ack |
| `AckAfterAppendBeforeFsync` | A5 → A6 | the record may or may not be on disk. **Either is correct** — no ack was given. If a partial record is on disk it is a torn tail and is discarded (§4) |
| `AckAfterFsyncBeforeIndex` | A6 → A7 | the record **is** durable and the caller got no ack. On replay the token is found in the log, so a retry with the same token is acknowledged-and-dropped (A3) and the batch is applied exactly once |
| `AckAfterFsyncBeforeAck` | A7 → A8 | same as above. This is the case that makes A3 load-bearing: without dedup, the caller's retry would double-apply |

`AckAfterFsyncBeforeIndex` and `AckAfterFsyncBeforeAck` are the two that matter. They are the states
in which the system knows something the caller does not, and the dedup index — rebuilt from the log,
never from memory — is what closes the gap.

## 2 · The seal sequence

Sealing an epoch makes its batches visible together (S-6, I-3).

| Step | Action | Durable after? |
| --- | --- | --- |
| **S1** | Append a `SealEpoch(n)` record to the segment. | not yet |
| **S2** | `fsync` the segment. | **yes, here** |
| **S3** | Step every resident circuit for epoch `n`. | in memory only |
| **S4** | Advance the in-memory sealed-epoch counter. | in memory only |

**The seal record is the commit point, not the circuit step.** A sealed epoch is a fact about the
*log*; the circuit's state for it is a deterministic function of that fact (I-2), so a crash after S2
and before S4 loses nothing — recovery replays epoch `n` from the log and arrives at the same state.
This is why the log is the source of truth and operator state is a cache of it.

### Kill points in the seal sequence

| Kill point | Lands between | What recovery must show |
| --- | --- | --- |
| `SealBeforeRecord` | before S1 | epoch `n` was never sealed; its batches are durable but not yet visible, and will be sealed by whatever the caller does next |
| `SealAfterRecordBeforeFsync` | S1 → S2 | torn tail: the seal record may be absent or partial. Absent or partial ⇒ not sealed (§4). Either outcome is consistent |
| `SealAfterFsyncBeforeStep` | S2 → S3 | epoch `n` **is** sealed. Recovery must replay it and reach the same state as a twin that stepped it normally |
| `SealAfterStepBeforeCounter` | S3 → S4 | same as above. The step is not durable and is redone; redoing it is safe *because* it is deterministic |

## 3 · The checkpoint sequence

The ordering §5.5 and §6 C4 name: **state flush → checkpoint record → log trim**.

| Step | Action | Durable after? |
| --- | --- | --- |
| **C1** | Serialise every operator's state and both circuit stores into a **new** checkpoint directory, `ckpt-<epoch>.partial`. | not yet |
| **C2** | `fsync` each state file, then the checkpoint directory. | the files, yes |
| **C3** | Write `MANIFEST` inside the checkpoint — the epoch number and a checksum over every state file — and `fsync` it. | the manifest, yes |
| **C4** | Atomically rename `ckpt-<epoch>.partial` → `ckpt-<epoch>`, then `fsync` the parent directory. | **the checkpoint exists, here** |
| **C5** | Update `CURRENT` to name `ckpt-<epoch>`, by write-to-temp + rename + `fsync` parent. | **the checkpoint is current, here** |
| **C6** | Trim log segments wholly before the checkpoint's epoch. | yes |
| **C7** | Delete superseded checkpoint directories. | yes |

**Publish-then-swap, never in-place.** A checkpoint becomes visible only at C4/C5, by rename. A
crash at any earlier point leaves a `.partial` directory that recovery ignores and deletes. This is
the same discipline C7 will need for compaction, and it is the reason a torn checkpoint cannot be
mistaken for a good one: a torn checkpoint is one that never got renamed.

**Why the trim is last, at C6.** The log is the source of truth. Trimming before the checkpoint is
current would create a window in which neither holds the history — the exact window in which a crash
loses committed data. The ordering is not a preference; reversing it is a data-loss bug.

**Why the manifest carries a checksum.** A renamed directory is atomic with respect to *its own
creation*, but the files inside it were written by an earlier step and could have been torn by a
crash between C1 and C2 on a filesystem that reorders. The checksum is what makes "torn checkpoint
detected" a fact rather than an assumption, and skipping it is the second of C4's canonical
mutations.

### Kill points in the checkpoint sequence

| Kill point | Lands between | What recovery must show |
| --- | --- | --- |
| `CheckpointBeforeStateFlush` | before C1 | no new checkpoint; the previous one plus the log suffix reconstructs the state |
| `CheckpointAfterStateFlushBeforeFsync` | C1 → C2 | a `.partial` directory with possibly-torn files. Ignored and deleted; previous checkpoint used |
| `CheckpointAfterFsyncBeforeManifest` | C2 → C3 | a `.partial` with good files but no manifest. Ignored — no manifest, no checkpoint |
| `CheckpointAfterManifestBeforePublish` | C3 → C4 | a complete `.partial` that was never renamed. **Still ignored**: publication is the commit point, and a checkpoint that was not published never happened |
| `CheckpointAfterPublishBeforeCurrent` | C4 → C5 | `ckpt-<n>` exists but `CURRENT` still names the older one. The older one is used and the log suffix covers the gap. Correct, and slower — which is the right trade |
| `CheckpointAfterCurrentBeforeTrim` | C5 → C6 | the new checkpoint is current and the log is longer than it needs to be. Replay of an already-checkpointed prefix must be **harmless**, which it is, because recovery replays only the suffix after the checkpoint's epoch |
| `CheckpointAfterTrimBeforeCleanup` | C6 → C7 | stale checkpoint directories left behind. Cleaned up on the next open; they are never selected because `CURRENT` names the live one |

## 4 · The compaction sequence (C7)

The log cannot grow forever, so a **snapshot** of the input integrals replaces a prefix of it: recovery
and bootstrap read *snapshot + suffix* instead of the whole history. §6 C7's pitfall is the whole design
constraint:

> compaction must be publish-then-swap, never in-place; a crash mid-compaction leaves the old log
> authoritative.

**The invariant compaction must preserve, at every instant:** either the whole log is authoritative, or
a *published* snapshot plus the retained suffix is. **Never neither.** Every step below is arranged so
that the only instant at which authority moves is a single rename.

**What compaction is anchored to.** The compaction epoch `E` is the epoch of the **oldest published
checkpoint still on disk** — `min(published_epochs)`, not the newest. Compaction with no published
checkpoint is refused: there is nothing for it to be consistent with.

*The newest would be wrong, and this is worth spelling out because it is the mistake this section
originally made.* Recovery does not always use the newest checkpoint: R1 and R2 **fall back** to an
older one when the newest fails to verify its manifest, which is exactly what a torn checkpoint
produces. A compaction anchored to the newest checkpoint would delete the records an older checkpoint
needs to replay, so the fallback would land on a checkpoint whose suffix is gone — and the recovered
state would be missing an epoch entirely, silently, because every remaining epoch would replay fine.

An earlier draft of this document anchored to the live checkpoint and argued that making `E` earlier
"would be pointless". The C4 crash gate disagreed within minutes of compaction being wired into it: a
torn-checkpoint cycle recovered to epoch 3 where its twin was at epoch 5. The rule is therefore: **the
anchor is bounded by the oldest checkpoint recovery may still choose**, and every checkpoint on disk is
one recovery may still choose.

**The arrangement this guarantees, and the one thing that must never happen.** After a compaction,
`retained_from = E ≤ every published checkpoint's epoch`. So a circuit restored from *any* checkpoint
finds every epoch it needs to replay. A checkpoint older than the snapshot is not a state to recover
from cleverly — it is a violated invariant, and recovery refuses loudly rather than skipping the epochs
it cannot find. (Bootstrap *could* rebuild that circuit from the snapshot, and it would give the same
answers by I-2; it would not give byte-identical operator *counters*, so it would break the I-7 twin
comparison. Refusing is honest; substituting a different-but-equal state is not.)

| Step | Action | Durable after? |
| --- | --- | --- |
| **P1** | Take `E` = the oldest published checkpoint's epoch; refuse if none is published, or if the log's retained prefix already starts at `E`. | nothing written |
| **P2** | Write the input integral of every table, as of `E`, into `snap-<E>.partial/` — one Parquet file per table — plus `DEDUP`, the ledger of acknowledged tokens. | not yet |
| **P3** | `fsync` each file, then the directory. | the files, yes |
| **P4** | Write `MANIFEST` inside the snapshot — `E`, a checksum and row count per Parquet file, and a checksum over `DEDUP` — and `fsync` it. | the manifest, yes |
| **P5** | Rename `snap-<E>.partial` → `snap-<E>`, `fsync` the parent. | **the snapshot exists, here** |
| **P6** | Write the records of every epoch **after** `E`, plus the unsealed pending appends, to a *new* segment file `segment-<k+1>.log.partial`; `fsync`; rename to `segment-<k+1>.log`; `fsync` the parent. | **the retained suffix exists, here — and the old segment is still the authoritative one** |
| **P7** | Write `LOG` — naming the live segment, the live snapshot, and `E` — by write-to-temp + rename + `fsync` parent. | **THE SWAP. Authority moves here, in one rename.** |
| **P8** | Delete the superseded segment file. | yes |
| **P9** | Delete superseded snapshot directories. | yes |

**Why the swap is one pointer and not two.** The snapshot and the retained segment are useless apart:
the snapshot without the suffix is stale, the suffix without the snapshot has lost its prefix. If they
were published by two separate commits there would be an instant between them at which neither pairing
was complete. `LOG` names **both**, so a single rename moves from one consistent pair to another.

**Why the old segment is deleted at P8 and not at P6.** Between P6 and P7 both a whole log and a
complete snapshot+suffix exist on disk; either could be read and both give the same answers. That
overlap is not waste, it is the safety margin: deleting the old segment before the pointer moved would
create the one window in which a crash finds neither pairing.

**The dedup ledger is not optional, and this is the edge that makes compaction dangerous.** R6 rebuilt
the dedup index by scanning *every* `Append` in the log. Compaction throws away part of that log. A
token acknowledged in the discarded prefix and re-offered afterwards would then look new, and the batch
would be applied a second time — I-4 broken, silently, by a *space optimisation*. So the ledger of
acknowledged tokens rides the snapshot (P2), and R6 is amended below to seed from it. **Compaction
without the ledger is not a smaller log, it is a lost exactly-once guarantee**, which is why one of
C7's canonical mutations is to omit it.

**What is in a snapshot and what is not.** The snapshot holds *input integrals* — the accumulated
contents of each table, consolidated, with zero-weight rows dropped (S-4, S-5) — and the dedup ledger.
It does **not** hold operator state or answers; that is a checkpoint's job (§3), and the two artifacts
answer different questions. A checkpoint restores *this* circuit; a snapshot lets *any* circuit be
built from the data, which is what bootstrap and C6's mid-history attach need.

### Kill points in the compaction sequence

| Kill point | Lands between | What recovery must show |
| --- | --- | --- |
| `CompactBeforeSnapshot` | before P2 | nothing written; the whole log is authoritative |
| `CompactAfterWriteBeforeFsync` | P2 → P3 | a `.partial` snapshot with possibly-torn Parquet. Ignored and deleted; the whole log is authoritative |
| `CompactAfterFsyncBeforeManifest` | P3 → P4 | a `.partial` with good files but no manifest. Ignored — no manifest, no snapshot |
| `CompactAfterManifestBeforePublish` | P4 → P5 | a complete `.partial` that was never renamed. **Still ignored**: publication is the commit point |
| `CompactAfterPublishBeforeSegment` | P5 → P6 | a published snapshot that nothing points at. Ignored — `LOG` is what makes a snapshot live — and the whole log is authoritative |
| `CompactAfterSegmentBeforePointer` | P6 → P7 | both a whole log and a complete snapshot+suffix on disk, with `LOG` still absent or naming the old pair. **The old log is authoritative**; the new artefacts are orphans, and are cleaned up |
| `CompactAfterPointerBeforeTrim` | P7 → P8 | the swap happened: snapshot+suffix is authoritative, and the superseded segment is still on disk taking space. Harmless — nothing reads it, because `LOG` does not name it |
| `CompactAfterTrimBeforeCleanup` | P8 → P9 | stale snapshot directories left behind. Cleaned up on the next open; never selected, because `LOG` names the live one |

Seven of the eight leave the old log authoritative. The eighth, `CompactAfterPointerBeforeTrim`, is the
first instant at which the snapshot is — and by then it is complete, published, and paired with a
suffix that was published before it.

## 5 · The recovery sequence

| Step | Action |
| --- | --- |
| **R1** | Read `CURRENT`. If it is missing or names a directory that is absent, fall back to the newest **published** checkpoint whose manifest verifies; if none, start from epoch 0. |
| **R2** | Verify the chosen checkpoint's manifest checksums. On mismatch, discard it and repeat R1 with the next-newest. |
| **R3** | Load operator state and both circuit stores from it; the circuit is now as of the checkpoint's epoch. |
| **R4** | Delete every `.partial` directory and every checkpoint not reachable from `CURRENT`. |
| **R5** | Read `LOG`. If it is present and both artefacts it names exist and verify, the retained log is its segment and the snapshot is its snapshot; otherwise fall back to the default segment with no snapshot, and delete any orphaned `.partial` or unreferenced artefacts (C7, P5–P9). |
| **R6** | Scan the retained segment from its beginning, verifying each record's CRC. **Stop at the first record that fails CRC or is short** — that is the torn tail, and everything after it is discarded. |
| **R7** | Rebuild the dedup index: **seed it from the snapshot's `DEDUP` ledger if there is a snapshot**, then add every `Append` record in the retained segment. Seeding is what carries I-4 across a compaction; without it a token acknowledged before the compaction and re-offered after it would be applied twice. |
| **R8** | Replay epochs **after** the checkpoint's epoch, sealing each one and stepping the circuit exactly as the live path does. |

**Torn tails are expected, not exceptional.** A crash between A5 and A6, or S1 and S2, leaves a
partial record. R5's rule — stop at the first bad record — is what makes that a non-event. It is
also why every record carries its length *and* a CRC: a length alone cannot distinguish a short
write from a valid record whose payload happens to look like a length.

**Recovery must be idempotent.** A crash *during* recovery must leave the next recovery able to
reach the same state. R1–R7 only read the log and the checkpoint, and write nothing except the R4
cleanup — which is itself idempotent, because deleting an already-deleted directory is a no-op. This
is a bug class that has bitten sibling systems, so the gate tests it explicitly rather than arguing
it from the code.

### Bootstrap — building a circuit that was never there (C7)

Recovery restores a circuit that existed. **Bootstrap** builds one that did not: a standing query
registered now, or a one-shot query, must answer for the *whole* history without replaying it.

| Step | Action |
| --- | --- |
| **B1** | Read `LOG`. If a snapshot is live, load each table's integral from its Parquet file and verify it against `MANIFEST`. |
| **B2** | Feed those integrals to the new circuit as **one delta**, bringing it to the snapshot's epoch `E`. |
| **B3** | Replay the retained segment's epochs after `E`, sealing each one. |

**Why one delta is not an approximation.** Every operator's state is a function of the *accumulated*
input rather than of how it was divided into epochs, and every answer is the integral of the sink's
output deltas — so `Δ₁ + … + Δₙ` applied at once integrates to what the pieces integrate to. That is
I-2 restated, and it is the same argument C6's mid-history attach rests on. Bootstrap and attach are
therefore the *same* mechanism with different sources: before a compaction the accumulated input comes
from the log, after one it comes from the snapshot plus the suffix, and nothing downstream can tell.

## 5a · What a durable state backend adds to this document: nothing (C8)

C8 replaced `MemBackend` with `RedbBackend` for operator state (D-19), which means every stateful
operator now performs a real transaction on a real file inside every step. The question the sprint had
to answer before writing any of it was whether that introduces **new write boundaries** — new instants
between which a crash must be named and tested. It does not, and the reason is worth stating because
"the backend writes to disk now" sounds like it must:

- **A backend's transaction is atomic and lands inside an existing seam pair.** An operator's writes
  happen during S3 (step the circuit), which sits between `SealAfterFsyncBeforeStep` and
  `SealAfterStepBeforeCounter`. A crash there leaves each operator's store either at its previous
  committed transaction or at its next one — never half-way, because redb is ACID — and in every case
  the epoch was *not* sealed in the circuit.
- **Recovery discards whatever the stores hold, wholesale.** R3 loads operator state from the
  checkpoint through the frozen trait's `restore`, which *replaces* rather than merges. So a partially
  updated store is not a state recovery has to reason about: it is overwritten before the first replay
  step.
- **Therefore the backend is a spill target, not a second durability mechanism.** State crosses a
  restart through `snapshot`/`restore` and the checkpoint protocol, exactly as it did on `MemBackend`.
  The spill directory is *cleared* when a circuit is built, and a run that inherited stale redb files
  would be reading state no checkpoint accounted for — a second, unaudited recovery path.

If a future backend's durability *were* load-bearing — if recovery read the store instead of the
checkpoint — then its commit points would need naming here and the crash harness would need to land on
them. That is the test to apply to the next backend, and this paragraph is what it should be compared
against.

## 6 · What the crash harness does with this document

The named kill points above are **deterministic seams in the code**, not timers. Each is a call to a
fault hook that, when the seed's fault plan selects it, aborts the operation and discards every
in-memory object — the same information loss as process death, at a point that can be named and
reproduced.

The harness runs two kinds of fault, chosen by seed:

1. **Seam faults** — the named points above, at a chosen occurrence (the *k*th time that seam is
   reached), so that a crash on the third checkpoint is as reachable as one on the first.
2. **Byte-boundary faults** — truncate a log segment or a checkpoint file at a random byte offset, or
   flip a byte. These are the faults no seam enumeration can predict, and they are what exercises R2
   and R5.

**The harness asserts the fault count it injected.** A crash suite that injects no faults passes
trivially and proves nothing; C3 learned that lesson from a mutation that silently failed to apply,
and the same discipline applies here. Every cycle reports which fault fired, and the gate fails if
the total is zero or if any named seam was never reached.

**Everything is seeded (I-2, D-6).** Which seam, which occurrence, which byte offset, and the
scenario itself all come from one seed. There is no timing in the harness, no sleep, no wall clock,
and no thread scheduling — a crash test that is flaky is worse than no crash test, because it teaches
people to re-run.

### What is simulated, and what is not

The 10,000-cycle gate uses in-process fault injection: the fault hook aborts and the harness drops
all in-memory state before recovering from disk. **It is not 10,000 process kills**, and saying so
matters:

- what it faithfully models: loss of everything not yet written to disk, at a named instant;
- what it does not model: kernel-level reordering of writes that never reached the filesystem, and
  anything the OS does to a dying process that our own code cannot observe.

**Correction (C7).** This section used to say, in the present tense:

> A separate, smaller test does use real `kill -9` on a child process, over the same scenarios and
> asserting the same invariants.

**It does not, and never has.** C4 planned that test, did not deliver it, and said so in
`docs/PROGRESS.md` — but this paragraph was left standing as though it described something real. A
document that claims a test which does not exist is worse than one that admits the gap, because the gap
is then invisible to anyone reading only the design. It is corrected here rather than quietly deleted.

What exists is in-process fault injection, and only that. The real-kill test's job — checking that the
in-process model is faithful — is **scheduled at C9**, whose exit gate is `kill -9` under load at 1,000
random points. Until then, no count in `docs/PROGRESS.md` should be read as a count of process kills,
and each is reported separately rather than added together.

### C9: the real kills exist — what they retire, and what they do not

`crates/schweep-server/tests/kill9.rs` spawns `schweepd` as a subprocess and `SIGKILL`s it at **1,000**
points under concurrent ingest, read and subscribe load, then restarts on the same directory. Per cycle it
asserts that every token the server acknowledged before the kill is applied in exactly one epoch (I-4), and
that the recovered state equals a never-crashed twin fed the same log — the **full** fingerprint, emission
counters included (I-7). Measured on the committed run: 1,000 kills, 24,219 acknowledged appends verified
exactly-once, 6,459 epochs recovered, and 968 of the 1,000 cycles killed between an acknowledgement and a
seal — the position that matters most — with 32 killed before any acknowledgement at all. A concurrent
subscriber was delivered 5,459 epochs across the run and none of them twice.

Three runs of the finished code produced those same counts — two on macOS and one on the Linux CI runner,
which took 28 seconds where macOS took 188. That is the seeded half working as intended: the workload and
the kill *point* (a count of acknowledged appends) are functions of the seed, so the set of batches the
server promised is reproducible across operating systems. The instruction the signal lands on is not, and
that is the property under test.

**What this retires.** The limit above — "it is not 10,000 process kills" — is retired for the *class of
failure a dying process produces*: a real `SIGKILL` at an arbitrary instruction, with the OS doing whatever
it does to a dying process, is now exercised a thousand times, and the in-process model's verdicts agree
with the real one's. The counts stay reported separately, as the correction above requires.

**What it does not retire, demonstrated rather than asserted.** `SIGKILL` kills a process; it does not
touch the page cache. A write this process issued is still visible to the next process even if no `fsync`
had returned — so the matrix **cannot see an acknowledgement sent before the durable write reaches the
disk.** That was measured, not reasoned about: running the same 60-cycle matrix with the log at
`SyncPolicy::Deferred` — which is exactly "acknowledge before the `fsync` returns" — passes, green, with
1,620 acknowledged appends "verified". The mutation is invisible to this harness by construction.

So the standing limit is narrower than it was, and it is now precisely statable:

| Failure | Modelled by | Status |
| --- | --- | --- |
| loss of in-memory state at a named instant | C4's 26 seams, 10,000 cycles | covered |
| a process dying at an arbitrary instruction, under load | C9's 1,000 real `SIGKILL`s | **covered** |
| an ack that precedes the `fsync` | nothing — `SIGKILL` preserves the page cache | **not covered**, and measured to be invisible |
| power loss, a lying disk cache, torn media | nothing | **not covered** |

The last two need a machine that loses power or a filesystem that lies, and neither is in this repository.
Until one is, `SyncPolicy::Full` is a claim resting on the operating system's contract rather than on a test
of ours, and this table is what a reader should see before they trust it.
