//! `schweep-log` v1: a directory of files (`ARCHITECTURE.md` §5.4; `docs/DURABILITY.md` §1, §2, §4).
//!
//! > The write path and the only place time enters.
//!
//! The log is the **source of truth**. Operator state is a cache of it, which is why recovery can be
//! "load a checkpoint and replay the suffix" and why a crash after a seal record is durable loses
//! nothing: everything downstream of a sealed epoch is a deterministic function of the log (I-2, D-6).
//!
//! Every ordering in this file is the one `docs/DURABILITY.md` numbers, and every fault hook is a
//! seam that document names. Read them together.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use schweep_zset::{Row, Schema};

use crate::error::{LogError, Result};
use crate::fault::{FaultInjector, Seam};
use crate::record::{frame, Record};

/// Epochs are dense integers starting at 1 (S-6).
pub type Epoch = u64;

/// Whether the log actually calls `fsync`.
///
/// **Why this is a choice and not a bug.** `fsync` is what makes an ack a promise against *power
/// loss*. The crash harness is in-process (`docs/DURABILITY.md` §5): it aborts at a named seam and
/// drops every in-memory object, then re-reads the file. The bytes are in the file either way,
/// because `write_all` already put them there and the page cache survives the simulated crash — so
/// `fsync` contributes **nothing** to what the 10,000-cycle gate measures, while costing a
/// millisecond or more per call on macOS and turning that gate into hours.
///
/// So the equivalence gate runs [`SyncPolicy::Deferred`] and says so, and the orderings — which are
/// what the seams test — are unchanged either way. [`SyncPolicy::Full`] is the default, is what
/// production uses, and is what the log's own durability tests use.
///
/// What this means honestly: **nothing here tests power loss.** Doing so needs a filesystem-level
/// fault injector or a VM that can be cut off mid-write, and that is named as remaining work in
/// `docs/PROGRESS.md` rather than implied by a green gate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SyncPolicy {
    /// `fsync` every append and every seal. Production.
    Full,
    /// Skip `fsync`. For in-process crash simulation, where it changes nothing observable.
    Deferred,
}

/// The outcome of an append (`docs/DURABILITY.md` §1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ack {
    /// Durable, and this is the first time this token was seen.
    Appended,
    /// A replay: the same token with the same content. Acknowledged and dropped, so the batch is
    /// applied exactly once however many times it is offered (I-4, A3).
    DroppedAsReplay,
}

/// One batch as it will be replayed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Batch {
    /// Where the data came from. Carried from birth, and the hook taint-as-retraction and Loom's
    /// envelopes attach to later (§5.4, **[MutinyDB seam]**).
    pub source_id: String,
    /// The token this batch was acknowledged under (I-4).
    ///
    /// Carried because compaction *rewrites* the retained records rather than copying bytes, and a
    /// rewritten record must carry the token the original did — otherwise reopening a compacted log
    /// would rebuild a dedup index full of invented tokens, and the real ones would be known only from
    /// the snapshot's ledger. That would still refuse a replay, by luck, while the index drifted from
    /// the records that produced it. A batch that knows its own token cannot drift.
    pub dedup_token: String,
    pub table: String,
    pub entries: Vec<(Row, i64)>,
}

/// The append-only input log.
#[derive(Debug)]
pub struct Log {
    dir: PathBuf,
    sync: SyncPolicy,
    /// Table name → schema. Appends are validated against it (A1) before anything is written.
    catalog: BTreeMap<String, Schema>,
    /// `dedup_token` → content hash. Rebuilt from the log at open, never from memory (R6), which is
    /// what closes the window in which the log knows something the caller does not.
    dedup: BTreeMap<String, u64>,
    /// Batches appended but not yet sealed into an epoch.
    pending: Vec<Batch>,
    /// **Where each sealed epoch's records are, not what they are** (C10).
    ///
    /// Until C10 this held the batches themselves, and a server's resident memory was therefore
    /// O(retained log): C9's soak measured 1,589 bytes an epoch with nothing else running, and its
    /// memo-ceiling gate peaked at 342 MB streaming 269 MB of history *through a `Log`* while the
    /// consumer's own footprint was a fraction of that. What lives here now is a byte range per epoch —
    /// sixteen bytes — and [`Log::epoch`] reads the records from the segment when somebody asks.
    ///
    /// Epoch `n` is at index `n - retained_from - 1`.
    sealed: Vec<EpochSpan>,
    /// The segment's length in bytes, tracked as records are written so that a span can be closed
    /// without asking the filesystem. Rebuilt by the open scan.
    segment_len: u64,
    /// Where the epoch currently being appended to begins.
    epoch_start: u64,
    /// The live segment file.
    segment: PathBuf,
    /// The last epoch whose records compaction discarded; 0 before any compaction.
    ///
    /// Epochs at or below this are gone from the log and live only in the snapshot. `sealed` holds
    /// epochs `retained_from + 1 ..= sealed_epoch()`, which is why every index arithmetic in this file
    /// goes through [`Log::epoch`] rather than subtracting one by hand.
    retained_from: Epoch,
    /// The live snapshot directory, if a compaction has published one.
    snapshot: Option<PathBuf>,
}

/// How much of the segment the open scan and [`Log::epoch`] buffer at a time.
///
/// 64 KiB, matching `stream::Epochs` and the log's own write buffering. It bounds what a scan holds
/// independently of how large the segment is, which is the point of C10's residency change.
pub const SCAN_BUFFER_BYTES: usize = 64 * 1024;

/// Read one framed record from a stream, returning it and the bytes it consumed.
///
/// `Ok(None)` at end of file or at a torn tail — the same rule as [`crate::record::read_framed`], which
/// reads from a slice. Two readers of one format is the arrangement that drifts, so
/// `crates/schweep-log/tests/residency.rs` holds them to each other over every truncation of a real
/// segment.
fn read_one(reader: &mut impl Read) -> Result<Option<(Record, u64)>> {
    let mut header = [0u8; 8];
    if !fill(reader, &mut header)? {
        return Ok(None);
    }
    let mut len_raw = [0u8; 4];
    let mut crc_raw = [0u8; 4];
    len_raw.copy_from_slice(header.get(0..4).ok_or(LogError::Corrupt("short frame"))?);
    crc_raw.copy_from_slice(header.get(4..8).ok_or(LogError::Corrupt("short frame"))?);
    let len = u32::from_be_bytes(len_raw) as usize;
    let expected = u32::from_be_bytes(crc_raw);

    let mut payload = vec![0u8; len];
    if !fill(reader, &mut payload)? {
        return Ok(None);
    }
    if crate::record::crc32(&payload) != expected {
        return Ok(None);
    }
    Ok(Some((Record::decode(&payload)?, 8 + len as u64)))
}

/// Fill `buffer` completely, or report that the stream ended first.
fn fill(reader: &mut impl Read, buffer: &mut [u8]) -> Result<bool> {
    let mut filled = 0usize;
    while filled < buffer.len() {
        let Some(slice) = buffer.get_mut(filled..) else {
            return Ok(false);
        };
        match reader.read(slice) {
            Ok(0) => return Ok(false),
            Ok(read) => filled += read,
            Err(e) => return Err(e.into()),
        }
    }
    Ok(true)
}

/// The byte range one sealed epoch's records occupy in the segment.
///
/// Sixteen bytes an epoch, against the batches themselves — which is the whole of C10's residency change.
/// The range covers the epoch's `Append` records and stops before its `SealEpoch` record: the seal is the
/// boundary, not content, and including it would make every read decode a record it discards.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EpochSpan {
    pub start: u64,
    pub end: u64,
}

impl EpochSpan {
    #[must_use]
    pub fn len_bytes(&self) -> u64 {
        self.end.saturating_sub(self.start)
    }
}

/// Where the log's authority lives: the segment, the snapshot, and the epoch they meet at.
///
/// Written by compaction's P7 as a single file, so that moving authority from one consistent pair to
/// another is one rename (`docs/DURABILITY.md` §4).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pointer {
    pub segment: String,
    pub snapshot: Option<String>,
    pub retained_from: Epoch,
}

impl Pointer {
    /// The pointer's on-disk form: readable text, with a CRC over the body.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let body = format!(
            "schweep-log pointer v1\nsegment={}\nsnapshot={}\nretained_from={}\n",
            self.segment,
            self.snapshot.as_deref().unwrap_or("-"),
            self.retained_from
        );
        format!("{body}crc={:08x}\n", crate::record::crc32(body.as_bytes())).into_bytes()
    }

    pub fn decode(bytes: &[u8]) -> Result<Pointer> {
        let text =
            std::str::from_utf8(bytes).map_err(|_| LogError::Corrupt("pointer is not UTF-8"))?;
        let (body, crc_line) = text
            .rsplit_once("crc=")
            .ok_or(LogError::Corrupt("pointer has no crc"))?;
        let expected = u32::from_str_radix(crc_line.trim(), 16)
            .map_err(|_| LogError::Corrupt("pointer crc is not hex"))?;
        if crate::record::crc32(body.as_bytes()) != expected {
            return Err(LogError::Corrupt("pointer failed its CRC"));
        }
        let mut segment = None;
        let mut snapshot = None;
        let mut retained_from = 0u64;
        for line in body.lines() {
            match line.split_once('=') {
                Some(("segment", value)) => segment = Some(value.to_owned()),
                Some(("snapshot", "-")) => snapshot = None,
                Some(("snapshot", value)) => snapshot = Some(value.to_owned()),
                Some(("retained_from", value)) => {
                    retained_from = value
                        .parse()
                        .map_err(|_| LogError::Corrupt("pointer epoch is not a number"))?;
                }
                _ => {}
            }
        }
        Ok(Pointer {
            segment: segment.ok_or(LogError::Corrupt("pointer names no segment"))?,
            snapshot,
            retained_from,
        })
    }
}

/// The segment a log with no pointer uses.
const DEFAULT_SEGMENT: &str = "segment-00000001.log";
/// The pointer compaction's P7 swaps.
const POINTER: &str = "LOG";
/// The dedup ledger inside a snapshot directory (P2, R7).
pub const DEDUP_LEDGER: &str = "DEDUP";

fn read_pointer(dir: &Path) -> Result<Option<Pointer>> {
    match fs::read(dir.join(POINTER)) {
        Ok(bytes) => match Pointer::decode(&bytes) {
            Ok(pointer) => Ok(Some(pointer)),
            // A pointer that does not verify is treated as absent: the default segment is
            // authoritative, which is the same outcome as a crash before P7 (§4).
            Err(_) => Ok(None),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Write the pointer by write-to-temp + rename + fsync parent — P7, and the only commit point.
fn write_pointer(dir: &Path, pointer: &Pointer, sync: SyncPolicy) -> Result<()> {
    let temp = dir.join("LOG.partial");
    {
        let mut file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&temp)?;
        file.write_all(&pointer.encode())?;
        if sync == SyncPolicy::Full {
            file.sync_all()?;
        }
    }
    fs::rename(&temp, dir.join(POINTER))?;
    sync_dir(dir, sync)
}

fn sync_dir(dir: &Path, sync: SyncPolicy) -> Result<()> {
    if sync == SyncPolicy::Full {
        // A rename is only durable once the directory holding it is synced.
        File::open(dir)?.sync_all()?;
    }
    Ok(())
}

impl Log {
    /// Open, or create, a log in `dir`, recovering whatever is there (R5, R6).
    pub fn open(
        dir: impl AsRef<Path>,
        catalog: BTreeMap<String, Schema>,
        faults: &mut FaultInjector,
        sync: SyncPolicy,
    ) -> Result<Log> {
        let dir = dir.as_ref().to_path_buf();
        if dir.exists() && !dir.is_dir() {
            return Err(LogError::NotADirectory(dir.display().to_string()));
        }
        fs::create_dir_all(&dir)?;

        // R5 · read `LOG`. A pointer that is absent, unreadable, or names artefacts that are not
        // there leaves the default segment authoritative — which is exactly what every kill point
        // before compaction's P7 must produce (§4).
        let pointer = read_pointer(&dir)?;
        let (segment, snapshot, retained_from) = match pointer {
            Some(pointer) => {
                let segment = dir.join(&pointer.segment);
                let snapshot = pointer.snapshot.as_ref().map(|name| dir.join(name));
                let usable = segment.exists()
                    && snapshot
                        .as_ref()
                        .is_none_or(|path| path.join("MANIFEST").exists());
                if usable {
                    (segment, snapshot, pointer.retained_from)
                } else {
                    (dir.join(DEFAULT_SEGMENT), None, 0)
                }
            }
            None => (dir.join(DEFAULT_SEGMENT), None, 0),
        };

        let mut log = Log {
            dir,
            sync,
            catalog,
            dedup: BTreeMap::new(),
            pending: Vec::new(),
            sealed: Vec::new(),
            segment_len: 0,
            epoch_start: 0,
            segment,
            retained_from,
            snapshot: snapshot.clone(),
        };

        // R7 · seed the dedup index from the snapshot's ledger *before* scanning the segment. This is
        // what carries I-4 across a compaction: the tokens acknowledged in the discarded prefix are
        // remembered here and nowhere else.
        if let Some(snapshot) = &snapshot {
            let ledger = snapshot.join(DEDUP_LEDGER);
            match fs::read(&ledger) {
                Ok(bytes) => log.dedup = crate::dedup::decode(&bytes)?,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    // A published snapshot with no ledger cannot be trusted to keep I-4, so it is a
                    // corruption rather than an absence.
                    return Err(LogError::Corrupt("snapshot has no dedup ledger"));
                }
                Err(e) => return Err(e.into()),
            }
        }

        log.replay_from_disk(faults)?;
        Ok(log)
    }

    /// The epoch whose records the log no longer holds; 0 before any compaction.
    #[must_use]
    pub fn retained_from(&self) -> Epoch {
        self.retained_from
    }

    /// The live snapshot directory, if a compaction has published one.
    #[must_use]
    pub fn snapshot(&self) -> Option<&Path> {
        self.snapshot.as_deref()
    }

    /// The schema catalog used to validate every append.
    #[must_use]
    pub fn catalog(&self) -> &BTreeMap<String, Schema> {
        &self.catalog
    }

    /// The dedup ledger this log would write into a snapshot (compaction's P2).
    #[must_use]
    pub fn dedup_ledger(&self) -> Vec<u8> {
        crate::dedup::encode(&self.dedup)
    }

    /// R5 and R6: scan the segment, stop at the torn tail, rebuild the dedup index **and the span
    /// index** — without ever holding more than one record.
    ///
    /// Before C10 this read the whole segment with `read_to_end` and kept every batch. Both halves of that
    /// were O(retained log): the read and the keep. It now streams, and what it retains per epoch is a
    /// [`EpochSpan`] — two integers — plus the appends after the last seal, which are pending and bounded
    /// by admission rather than by history.
    ///
    /// **What still grows with history is the dedup index**, one entry per acknowledged token, and that is
    /// I-4's price rather than an oversight: a token forgotten is a batch that can be applied twice.
    /// `Log::dedup_len` reports it so a gate can measure it, and `docs/PROGRESS.md` records the figure.
    fn replay_from_disk(&mut self, faults: &mut FaultInjector) -> Result<()> {
        let mut reader = match File::open(&self.segment) {
            Ok(file) => BufReader::with_capacity(SCAN_BUFFER_BYTES, file),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                self.segment_len = 0;
                self.epoch_start = 0;
                return Ok(());
            }
            Err(e) => return Err(e.into()),
        };

        let mut at = 0u64;
        let mut epoch_start = 0u64;
        let mut pending: Vec<Batch> = Vec::new();
        let mut records_replayed = 0u32;
        // R5: stop at the first record that fails its CRC or is short. Everything after it is
        // discarded — a torn tail is expected, not exceptional.
        while let Some((record, consumed)) = read_one(&mut reader)? {
            records_replayed += 1;
            if records_replayed % 4 == 0 && faults.reached(Seam::RecoveryMidReplay) {
                return Err(LogError::InjectedFault(Seam::RecoveryMidReplay.name()));
            }
            match record {
                Record::Append {
                    source_id,
                    dedup_token,
                    table,
                    entries,
                } => {
                    let replayed = Record::Append {
                        source_id: source_id.clone(),
                        dedup_token: dedup_token.clone(),
                        table: table.clone(),
                        entries: entries.clone(),
                    };
                    self.dedup
                        .insert(dedup_token.clone(), replayed.content_hash());
                    pending.push(Batch {
                        source_id,
                        dedup_token,
                        table,
                        entries,
                    });
                }
                Record::SealEpoch { .. } => {
                    self.sealed.push(EpochSpan {
                        start: epoch_start,
                        end: at,
                    });
                    // The seal record itself is the boundary; the next epoch starts after it.
                    epoch_start = at + consumed;
                    pending.clear();
                }
            }
            at += consumed;
        }
        // Appends after the last seal record are durable but not yet visible: they are pending, and
        // whatever seals next will include them (S-6).
        self.pending = pending;
        self.segment_len = at;
        self.epoch_start = epoch_start;
        Ok(())
    }

    #[must_use]
    pub fn sealed_epoch(&self) -> Epoch {
        self.retained_from + self.sealed.len() as Epoch
    }

    #[must_use]
    pub fn pending_batches(&self) -> &[Batch] {
        &self.pending
    }

    /// The batches of one sealed epoch, **read from the segment** (C10).
    ///
    /// Owned rather than borrowed, and that is the residency change at its API: the log holds a byte range
    /// per epoch, not the batches, so there is nothing to lend. What a caller pays is one read of one
    /// epoch's bytes; what the process no longer pays is the whole history, resident, for the lifetime of
    /// the server.
    pub fn epoch(&self, epoch: Epoch) -> Result<Vec<Batch>> {
        if epoch != 0 && epoch <= self.retained_from {
            return Err(LogError::EpochCompacted {
                requested: epoch,
                retained_from: self.retained_from,
            });
        }
        if epoch == 0 || epoch > self.sealed_epoch() {
            return Err(LogError::EpochOutOfRange {
                requested: epoch,
                sealed: self.sealed_epoch(),
            });
        }
        let index = (epoch - self.retained_from - 1) as usize;
        let span = *self.sealed.get(index).ok_or(LogError::EpochOutOfRange {
            requested: epoch,
            sealed: self.sealed_epoch(),
        })?;
        self.read_span(span)
    }

    /// Read every `Append` record inside a span.
    fn read_span(&self, span: EpochSpan) -> Result<Vec<Batch>> {
        if span.len_bytes() == 0 {
            return Ok(Vec::new());
        }
        let mut file = File::open(&self.segment)?;
        file.seek(SeekFrom::Start(span.start))?;
        let mut reader = BufReader::with_capacity(SCAN_BUFFER_BYTES, file.take(span.len_bytes()));
        let mut batches = Vec::new();
        while let Some((record, _)) = read_one(&mut reader)? {
            match record {
                Record::Append {
                    source_id,
                    dedup_token,
                    table,
                    entries,
                } => batches.push(Batch {
                    source_id,
                    dedup_token,
                    table,
                    entries,
                }),
                // A span stops before its seal record, so one inside it means the index and the file
                // disagree — which is corruption, not a case to skip past.
                Record::SealEpoch { .. } => {
                    return Err(LogError::Corrupt("a seal record inside an epoch's span"));
                }
            }
        }
        Ok(batches)
    }

    /// How many acknowledged tokens the dedup index holds.
    ///
    /// **The one structure that still grows with history**, at one entry per acknowledged batch, and it is
    /// exposed so a gate can measure it rather than a document assert it. I-4 is why it exists: a token
    /// forgotten is a batch that can be applied twice. Bounding it needs a retention policy, which is a
    /// decision and not an optimisation.
    #[must_use]
    pub fn dedup_len(&self) -> usize {
        self.dedup.len()
    }

    /// Bytes of segment the span index accounts for, and what the index itself costs.
    #[must_use]
    pub fn index_bytes(&self) -> u64 {
        (self.sealed.len() * std::mem::size_of::<EpochSpan>()) as u64
    }

    /// Append a batch (`docs/DURABILITY.md` §1, steps A1–A8).
    pub fn append(
        &mut self,
        source_id: &str,
        table: &str,
        entries: Vec<(Row, i64)>,
        dedup_token: &str,
        faults: &mut FaultInjector,
    ) -> Result<Ack> {
        if faults.reached(Seam::AckBeforeValidate) {
            return Err(LogError::InjectedFault(Seam::AckBeforeValidate.name()));
        }

        // A1 · validate. A malformed batch is refused and nothing is written.
        let schema = self
            .catalog
            .get(table)
            .ok_or_else(|| LogError::UnknownTable(table.to_owned()))?;
        for (row, _) in &entries {
            if row.len() != schema.len() {
                return Err(LogError::ZSet(schweep_zset::ZSetError::ArityMismatch {
                    expected: schema.len(),
                    found: row.len(),
                }));
            }
            for (index, value) in row.values().iter().enumerate() {
                schema.check_value(index, value)?;
            }
        }

        let record = Record::Append {
            source_id: source_id.to_owned(),
            dedup_token: dedup_token.to_owned(),
            table: table.to_owned(),
            entries: entries.clone(),
        };
        let hash = record.content_hash();

        // A2–A4 · dedup. Same token, same content is a replay; same token, different content is a
        // caller bug and is refused loudly (I-4).
        if let Some(known) = self.dedup.get(dedup_token) {
            if *known == hash {
                return Ok(Ack::DroppedAsReplay);
            }
            return Err(LogError::TokenReused {
                source_id: source_id.to_owned(),
                token: dedup_token.to_owned(),
            });
        }

        if faults.reached(Seam::AckBeforeAppend) {
            return Err(LogError::InjectedFault(Seam::AckBeforeAppend.name()));
        }

        // A5 · append.
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.segment)?;
        file.write_all(&frame(&record.encode()))?;

        if faults.reached(Seam::AckAfterAppendBeforeFsync) {
            // The record may or may not be on disk, and either is correct: no ack was given. If a
            // partial record landed it is a torn tail and R5 discards it.
            return Err(LogError::InjectedFault(
                Seam::AckAfterAppendBeforeFsync.name(),
            ));
        }

        // A6 · fsync. Nothing above this line is a promise; everything below it is.
        if self.sync == SyncPolicy::Full {
            file.sync_all()?;
        }

        if faults.reached(Seam::AckAfterFsyncBeforeIndex) {
            // Durable, and the caller has no ack. A retry with the same token will be dropped as a
            // replay after the index is rebuilt from the log — which is why A3 is load-bearing.
            return Err(LogError::InjectedFault(
                Seam::AckAfterFsyncBeforeIndex.name(),
            ));
        }

        // A7 · index. The frame's length is added to the segment's tracked length here rather than by
        // asking the filesystem: the write above is the only thing that extends it.
        self.segment_len += frame(&record.encode()).len() as u64;
        self.dedup.insert(dedup_token.to_owned(), hash);
        self.pending.push(Batch {
            source_id: source_id.to_owned(),
            dedup_token: dedup_token.to_owned(),
            table: table.to_owned(),
            entries,
        });

        if faults.reached(Seam::AckAfterFsyncBeforeAck) {
            return Err(LogError::InjectedFault(Seam::AckAfterFsyncBeforeAck.name()));
        }

        // A8 · ack.
        Ok(Ack::Appended)
    }

    /// Seal an epoch (`docs/DURABILITY.md` §2, steps S1–S2).
    ///
    /// The circuit step (S3) and the counter (S4) belong to the caller, which is why they are not
    /// here: the log's job ends when the seal record is durable, and that is the commit point.
    pub fn seal_epoch(&mut self, faults: &mut FaultInjector) -> Result<Epoch> {
        if faults.reached(Seam::SealBeforeRecord) {
            return Err(LogError::InjectedFault(Seam::SealBeforeRecord.name()));
        }
        let epoch = self.sealed_epoch() + 1;

        // S1 · record.
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.segment)?;
        file.write_all(&frame(&Record::SealEpoch { epoch }.encode()))?;

        if faults.reached(Seam::SealAfterRecordBeforeFsync) {
            return Err(LogError::InjectedFault(
                Seam::SealAfterRecordBeforeFsync.name(),
            ));
        }

        // S2 · fsync. The epoch is sealed here and nowhere else.
        if self.sync == SyncPolicy::Full {
            file.sync_all()?;
        }

        let seal_bytes = frame(&Record::SealEpoch { epoch }.encode()).len() as u64;
        self.sealed.push(EpochSpan {
            start: self.epoch_start,
            end: self.segment_len,
        });
        self.segment_len += seal_bytes;
        self.epoch_start = self.segment_len;
        self.pending.clear();
        Ok(epoch)
    }

    /// Compaction's log side — **P6, P7, P8** of `docs/DURABILITY.md` §4.
    ///
    /// The snapshot has already been written and published by the caller (P2–P5); this writes the
    /// retained suffix to a new segment, swaps authority with one rename, and only then deletes the
    /// superseded segment.
    ///
    /// `anchor` is the epoch the snapshot covers. Records for epochs after it, and the appends not yet
    /// sealed into any epoch, are what the new segment holds.
    pub fn compact(
        &mut self,
        anchor: Epoch,
        snapshot: &Path,
        faults: &mut FaultInjector,
    ) -> Result<()> {
        if anchor <= self.retained_from {
            return Err(LogError::NothingToCompact {
                anchor,
                retained_from: self.retained_from,
            });
        }
        if anchor > self.sealed_epoch() {
            return Err(LogError::EpochOutOfRange {
                requested: anchor,
                sealed: self.sealed_epoch(),
            });
        }
        let snapshot_name = snapshot
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or(LogError::Corrupt("snapshot path has no name"))?
            .to_owned();

        // P6 · write the retained suffix to a *new* segment, **streaming, one epoch at a time**. The old
        // one is untouched and stays authoritative until P7.
        //
        // Streamed rather than assembled in a `Vec<u8>`, for C10's reason: a compaction that materialises
        // its whole output holds the suffix resident, and the suffix is unbounded. The new segment's span
        // index is built here as the bytes are written — exactly, from the frame lengths — rather than by
        // rescanning the file afterwards.
        let next = self.next_segment_name();
        let partial = self.dir.join(format!("{next}.partial"));
        let mut rebuilt: Vec<EpochSpan> = Vec::new();
        let mut written = 0u64;
        // Where a future epoch's records will start in the new segment: after the last seal, which is
        // also where the pending appends were written.
        let mut rebuilt_epoch_start = 0u64;
        {
            let file = OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&partial)?;
            let mut out = std::io::BufWriter::with_capacity(SCAN_BUFFER_BYTES, file);
            for epoch in (anchor + 1)..=self.sealed_epoch() {
                for batch in self.epoch(epoch)? {
                    let framed = frame(
                        &Record::Append {
                            source_id: batch.source_id.clone(),
                            dedup_token: batch.dedup_token.clone(),
                            table: batch.table.clone(),
                            entries: batch.entries.clone(),
                        }
                        .encode(),
                    );
                    out.write_all(&framed)?;
                    written += framed.len() as u64;
                }
                rebuilt.push(EpochSpan {
                    start: rebuilt_epoch_start,
                    end: written,
                });
                let seal = frame(&Record::SealEpoch { epoch }.encode());
                out.write_all(&seal)?;
                written += seal.len() as u64;
                rebuilt_epoch_start = written;
            }
            for batch in &self.pending {
                let framed = frame(
                    &Record::Append {
                        source_id: batch.source_id.clone(),
                        dedup_token: batch.dedup_token.clone(),
                        table: batch.table.clone(),
                        entries: batch.entries.clone(),
                    }
                    .encode(),
                );
                out.write_all(&framed)?;
                written += framed.len() as u64;
            }
            out.flush()?;
            let file = out
                .into_inner()
                .map_err(|_| LogError::Io("flushing the compacted segment".to_owned()))?;
            if self.sync == SyncPolicy::Full {
                file.sync_all()?;
            }
        }
        fs::rename(&partial, self.dir.join(&next))?;
        sync_dir(&self.dir, self.sync)?;

        if faults.reached(Seam::CompactAfterSegmentBeforePointer) {
            // Both a whole log and a complete snapshot+suffix are on disk, and `LOG` still names the
            // old pair. The old log is authoritative; the new artefacts are orphans.
            return Err(LogError::InjectedFault(
                Seam::CompactAfterSegmentBeforePointer.name(),
            ));
        }

        // P7 · THE SWAP. One rename moves authority from one consistent pair to another.
        let pointer = Pointer {
            segment: next.clone(),
            snapshot: Some(snapshot_name),
            retained_from: anchor,
        };
        write_pointer(&self.dir, &pointer, self.sync)?;

        if faults.reached(Seam::CompactAfterPointerBeforeTrim) {
            // The swap happened. The superseded segment is still on disk and nothing reads it, because
            // `LOG` does not name it.
            return Err(LogError::InjectedFault(
                Seam::CompactAfterPointerBeforeTrim.name(),
            ));
        }

        // P8 · delete the superseded segment. Only now: before P7 it was the authoritative one.
        let superseded = std::mem::replace(&mut self.segment, self.dir.join(&next));
        if superseded != self.segment {
            let _ = fs::remove_file(&superseded);
        }

        // The in-memory view follows the on-disk one. **The span index is replaced, not drained**: every
        // span was a byte offset into the segment that no longer exists, so dropping the compacted prefix
        // would leave the survivors pointing at the wrong file. `rebuilt` was computed from the frame
        // lengths as the new segment was written, so it needs no rescan.
        self.sealed = rebuilt;
        self.segment_len = written;
        self.epoch_start = rebuilt_epoch_start;
        self.retained_from = anchor;
        self.snapshot = Some(snapshot.to_path_buf());
        Ok(())
    }

    /// The segment file a compaction would write next.
    fn next_segment_name(&self) -> String {
        let current = self
            .segment
            .file_stem()
            .and_then(|stem| stem.to_str())
            .and_then(|stem| stem.rsplit('-').next())
            .and_then(|digits| digits.parse::<u64>().ok())
            .unwrap_or(1);
        format!("segment-{:08}.log", current + 1)
    }

    #[must_use]
    pub fn directory(&self) -> &Path {
        &self.dir
    }

    #[must_use]
    pub fn segment_path(&self) -> &Path {
        &self.segment
    }

    /// Tokens the log knows about — for tests, and for reporting.
    #[must_use]
    pub fn known_tokens(&self) -> usize {
        self.dedup.len()
    }

    /// Every acknowledged token, in order.
    ///
    /// Named rather than counted, because I-4 is a statement about *which* batches were applied. A
    /// count agrees with itself while the identities drift.
    pub fn tokens(&self) -> impl Iterator<Item = &str> {
        self.dedup.keys().map(String::as_str)
    }

    /// A deterministic rendering of the whole log, for crash comparisons (I-2, I-7).
    pub fn render(&self) -> String {
        let mut out = format!(
            "log @ epoch {} · retained from {} · {} pending · {} token(s)\n",
            self.sealed_epoch(),
            self.retained_from,
            self.pending.len(),
            self.dedup.len()
        );
        // Reads each epoch back from the segment rather than printing what is held, because after C10
        // nothing is held. A rendering that quietly omitted the rows would be a rendering that stopped
        // being usable for the crash harness's comparisons, which is what it is for.
        for index in 0..self.sealed.len() {
            let epoch = self.retained_from + index as Epoch + 1;
            out.push_str(&format!("epoch {epoch}\n"));
            let batches = match self.epoch(epoch) {
                Ok(batches) => batches,
                // A render is a diagnostic, and a diagnostic that panics is worse than one that says it
                // could not read.
                Err(error) => {
                    out.push_str(&format!("  unreadable: {error}\n"));
                    continue;
                }
            };
            for batch in &batches {
                for (row, weight) in &batch.entries {
                    out.push_str(&format!(
                        "  {}/{}: {row} => {weight}\n",
                        batch.source_id, batch.table
                    ));
                }
            }
        }
        out
    }
}
