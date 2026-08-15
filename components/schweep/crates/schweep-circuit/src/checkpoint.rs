//! The checkpoint protocol (`docs/DURABILITY.md` §3, §4; `ARCHITECTURE.md` §5.5).
//!
//! The ordering §6 C4 names, and every step is numbered to match the document:
//!
//! ```text
//! C1 state flush → C2 fsync files → C3 manifest → C4 publish (rename) → C5 CURRENT → C6 trim → C7 clean
//! ```
//!
//! **Publish-then-swap, never in-place.** A checkpoint becomes visible only at C4, by renaming
//! `ckpt-<n>.partial` to `ckpt-<n>`. A crash before that leaves a `.partial` directory that recovery
//! ignores and deletes, which is what makes "a torn checkpoint is detected" true by construction: a
//! torn checkpoint is one that never got renamed.
//!
//! **The trim is last, at C6, and reversing that is a data-loss bug.** The log is the source of truth;
//! trimming before the checkpoint is current would open a window in which neither holds the history.
//!
//! **The manifest carries a checksum** over every state file. A rename is atomic with respect to the
//! directory entry, not with respect to bytes written earlier, so the checksum is what turns "torn
//! checkpoint detected" from an assumption into a fact. Skipping it is one of C4's two canonical
//! mutations, and the crash harness catches it.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use schweep_log::{FaultInjector, Seam, SyncPolicy};

use crate::circuit::Circuit;
use crate::error::{CircuitError, Result};

/// The file naming the live checkpoint. Written by temp-and-rename, never edited in place.
const CURRENT: &str = "CURRENT";
/// The checksum file inside a checkpoint. Its presence *and* its agreement are both required.
const MANIFEST: &str = "MANIFEST";
/// The single state file inside a checkpoint.
const STATE: &str = "state.bin";

fn io(e: std::io::Error) -> CircuitError {
    CircuitError::Snapshot(e.to_string())
}

/// CRC-32 over the state file, recorded in the manifest.
fn crc32(bytes: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            let mask = if crc & 1 != 0 { 0xEDB8_8320 } else { 0 };
            crc = (crc >> 1) ^ mask;
        }
    }
    !crc
}

fn fsync_dir(path: &Path, sync: SyncPolicy) -> Result<()> {
    // Renames and creations are only durable once the *directory* is synced. Skipping this is the
    // classic "the file is there but the directory entry is not" bug after a power loss.
    //
    // `SyncPolicy::Deferred` skips it for the in-process crash gate, where it changes nothing
    // observable — see `schweep_log::SyncPolicy` for why, and for what that does not test.
    if sync == SyncPolicy::Full {
        let dir = fs::File::open(path).map_err(io)?;
        dir.sync_all().map_err(io)?;
    }
    Ok(())
}

/// Take a checkpoint of `circuit` at its current epoch, into `root`.
pub fn take(
    root: impl AsRef<Path>,
    circuit: &Circuit,
    faults: &mut FaultInjector,
    sync: SyncPolicy,
) -> Result<()> {
    let root = root.as_ref();
    fs::create_dir_all(root).map_err(io)?;
    let epoch = circuit.epoch();
    let partial = root.join(format!("ckpt-{epoch:010}.partial"));
    let published = root.join(format!("ckpt-{epoch:010}"));

    if faults.reached(Seam::CheckpointBeforeStateFlush) {
        return Err(CircuitError::InjectedFault(
            Seam::CheckpointBeforeStateFlush.name(),
        ));
    }

    // C1 · state flush, into a NEW directory. Nothing existing is touched.
    if partial.exists() {
        fs::remove_dir_all(&partial).map_err(io)?;
    }
    fs::create_dir_all(&partial).map_err(io)?;
    let state = circuit.snapshot()?;
    let state_path = partial.join(STATE);
    {
        let mut file = fs::File::create(&state_path).map_err(io)?;
        file.write_all(&state).map_err(io)?;

        if faults.reached(Seam::CheckpointAfterStateFlushBeforeFsync) {
            // A `.partial` with possibly-torn files. Ignored and deleted by recovery.
            return Err(CircuitError::InjectedFault(
                Seam::CheckpointAfterStateFlushBeforeFsync.name(),
            ));
        }

        // C2 · fsync the file, then the directory holding it.
        if sync == SyncPolicy::Full {
            file.sync_all().map_err(io)?;
        }
    }
    fsync_dir(&partial, sync)?;

    if faults.reached(Seam::CheckpointAfterFsyncBeforeManifest) {
        // Good files, no manifest. No manifest, no checkpoint.
        return Err(CircuitError::InjectedFault(
            Seam::CheckpointAfterFsyncBeforeManifest.name(),
        ));
    }

    // C3 · manifest: the epoch and a checksum over the state file.
    let manifest = format!("epoch {epoch}\nstate {} {}\n", state.len(), crc32(&state));
    {
        let mut file = fs::File::create(partial.join(MANIFEST)).map_err(io)?;
        file.write_all(manifest.as_bytes()).map_err(io)?;
        if sync == SyncPolicy::Full {
            file.sync_all().map_err(io)?;
        }
    }
    fsync_dir(&partial, sync)?;

    if faults.reached(Seam::CheckpointAfterManifestBeforePublish) {
        // A complete `.partial` that was never renamed. Still ignored: publication is the commit
        // point, and a checkpoint that was not published never happened.
        return Err(CircuitError::InjectedFault(
            Seam::CheckpointAfterManifestBeforePublish.name(),
        ));
    }

    // C4 · publish, by rename. The checkpoint exists from here.
    if published.exists() {
        fs::remove_dir_all(&published).map_err(io)?;
    }
    fs::rename(&partial, &published).map_err(io)?;
    fsync_dir(root, sync)?;

    if faults.reached(Seam::CheckpointAfterPublishBeforeCurrent) {
        // `ckpt-<n>` exists but CURRENT still names the older one. The older one is used and the log
        // suffix covers the gap: correct, and slower, which is the right trade.
        return Err(CircuitError::InjectedFault(
            Seam::CheckpointAfterPublishBeforeCurrent.name(),
        ));
    }

    // C5 · CURRENT, by temp-and-rename.
    let temp = root.join("CURRENT.tmp");
    {
        let mut file = fs::File::create(&temp).map_err(io)?;
        file.write_all(format!("ckpt-{epoch:010}\n").as_bytes())
            .map_err(io)?;
        if sync == SyncPolicy::Full {
            file.sync_all().map_err(io)?;
        }
    }
    fs::rename(&temp, root.join(CURRENT)).map_err(io)?;
    fsync_dir(root, sync)?;

    if faults.reached(Seam::CheckpointAfterCurrentBeforeTrim) {
        // The checkpoint is current and the log is longer than it needs to be. Replaying an
        // already-checkpointed prefix must be harmless, and it is: recovery replays only the suffix.
        return Err(CircuitError::InjectedFault(
            Seam::CheckpointAfterCurrentBeforeTrim.name(),
        ));
    }

    // C6 · trim. Nothing to trim in v1: the log keeps one segment and recovery replays only the
    // suffix after the checkpoint's epoch, so a trim would save disk and change no behaviour.
    // Segment rotation and trimming are C7's compaction work; the seam is here so the ordering is
    // exercised now and the step has somewhere to go.

    if faults.reached(Seam::CheckpointAfterTrimBeforeCleanup) {
        return Err(CircuitError::InjectedFault(
            Seam::CheckpointAfterTrimBeforeCleanup.name(),
        ));
    }

    // C7 · clean up superseded checkpoints and abandoned partials.
    cleanup(root, epoch)?;
    Ok(())
}

fn cleanup(root: &Path, keep: u64) -> Result<()> {
    let keep_name = format!("ckpt-{keep:010}");
    for entry in fs::read_dir(root).map_err(io)? {
        let entry = entry.map_err(io)?;
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.starts_with("ckpt-") {
            continue;
        }
        if name == keep_name {
            continue;
        }
        // Deleting an already-deleted directory is a no-op, which is what makes cleanup — and
        // therefore recovery — idempotent.
        let _ = fs::remove_dir_all(entry.path());
    }
    Ok(())
}

/// A checkpoint that verified, ready to load.
#[derive(Debug, Clone)]
pub struct Loaded {
    pub epoch: u64,
    pub state: Vec<u8>,
}

/// Find and verify the newest usable checkpoint (`docs/DURABILITY.md` R1, R2, R4).
///
/// Returns `None` when there is none, which means "start from epoch 0 and replay the whole log".
pub fn load(root: impl AsRef<Path>) -> Result<Option<Loaded>> {
    let root = root.as_ref();
    if !root.exists() {
        return Ok(None);
    }

    // R4 · delete abandoned partials. Idempotent, and done before anything is chosen so that a
    // partial can never be selected.
    for entry in fs::read_dir(root).map_err(io)? {
        let entry = entry.map_err(io)?;
        let name = entry.file_name().to_string_lossy().to_string();
        if name.ends_with(".partial") || name == "CURRENT.tmp" {
            let _ = fs::remove_dir_all(entry.path());
            let _ = fs::remove_file(entry.path());
        }
    }

    // Candidates, newest first: CURRENT's choice, then every published checkpoint by descending
    // epoch. R1 falls back down this list and R2 discards any whose manifest fails.
    let mut candidates: Vec<String> = Vec::new();
    if let Ok(text) = fs::read_to_string(root.join(CURRENT)) {
        let named = text.trim().to_owned();
        if !named.is_empty() {
            candidates.push(named);
        }
    }
    let mut published: Vec<String> = Vec::new();
    for entry in fs::read_dir(root).map_err(io)? {
        let entry = entry.map_err(io)?;
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with("ckpt-") && !name.ends_with(".partial") && entry.path().is_dir() {
            published.push(name);
        }
    }
    published.sort_unstable();
    published.reverse();
    for name in published {
        if !candidates.contains(&name) {
            candidates.push(name);
        }
    }

    for name in candidates {
        let dir = root.join(&name);
        if !dir.is_dir() {
            continue;
        }
        match verify(&dir) {
            Ok(loaded) => return Ok(Some(loaded)),
            // R2 · a checkpoint that does not verify is discarded and the next one tried. This is
            // the torn-checkpoint path, and it must never be an error the caller sees: falling back
            // is the correct behaviour, not a failure.
            Err(_) => continue,
        }
    }
    Ok(None)
}

/// Verify a published checkpoint: manifest present, epoch parsed, checksum agreeing.
///
/// **This is the torn-checkpoint detection.** A rename is atomic with respect to the directory entry,
/// not with respect to bytes written earlier, so the checksum is what makes "detected" a fact rather
/// than an assumption. Removing this check is C4's second canonical mutation, and the crash harness
/// catches it.
fn verify(dir: &Path) -> Result<Loaded> {
    let state = fs::read(dir.join(STATE)).map_err(io)?;
    let manifest = fs::read_to_string(dir.join(MANIFEST)).map_err(io)?;
    let mut epoch: Option<u64> = None;
    let mut expected: Option<(usize, u32)> = None;
    for line in manifest.lines() {
        let mut parts = line.split_whitespace();
        match parts.next() {
            Some("epoch") => epoch = parts.next().and_then(|v| v.parse().ok()),
            Some("state") => {
                let len = parts.next().and_then(|v| v.parse::<usize>().ok());
                let crc = parts.next().and_then(|v| v.parse::<u32>().ok());
                if let (Some(len), Some(crc)) = (len, crc) {
                    expected = Some((len, crc));
                }
            }
            _ => {}
        }
    }
    let epoch = epoch.ok_or(CircuitError::CorruptSnapshot)?;
    let (len, crc) = expected.ok_or(CircuitError::CorruptSnapshot)?;
    if state.len() != len || crc32(&state) != crc {
        return Err(CircuitError::CorruptSnapshot);
    }
    Ok(Loaded { epoch, state })
}

fn epoch_from_name(dir: &Path) -> Result<u64> {
    dir.file_name()
        .and_then(|n| n.to_str())
        .and_then(|n| n.strip_prefix("ckpt-"))
        .and_then(|n| n.parse().ok())
        .ok_or(CircuitError::CorruptSnapshot)
}

/// The published checkpoints in `root`, newest first. For tests and for reporting.
pub fn published_epochs(root: impl AsRef<Path>) -> Result<Vec<u64>> {
    let root = root.as_ref();
    if !root.exists() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in fs::read_dir(root).map_err(io)? {
        let entry = entry.map_err(io)?;
        let path = entry.path();
        if path.is_dir() {
            if let Ok(epoch) = epoch_from_name(&path) {
                out.push(epoch);
            }
        }
    }
    out.sort_unstable();
    out.reverse();
    Ok(out)
}

/// Corrupt a checkpoint's state file at a byte offset — a *byte-boundary fault* (§5).
///
/// Truncation and bit-flips are the faults no seam enumeration can predict, and they are what
/// exercises R2. Test-only, and it takes a path rather than a live checkpoint so that it can only be
/// aimed at something already on disk.
pub fn corrupt_for_test(
    root: impl AsRef<Path>,
    epoch: u64,
    offset: usize,
    truncate: bool,
) -> Result<bool> {
    let path = root.as_ref().join(format!("ckpt-{epoch:010}")).join(STATE);
    let Ok(mut bytes) = fs::read(&path) else {
        return Ok(false);
    };
    if bytes.is_empty() {
        return Ok(false);
    }
    let at = offset % bytes.len();
    if truncate {
        bytes.truncate(at);
    } else if let Some(byte) = bytes.get_mut(at) {
        *byte ^= 0xFF;
    }
    fs::write(&path, &bytes).map_err(io)?;
    Ok(true)
}

/// The path a checkpoint root uses for its state file, for tests that need to look.
#[must_use]
pub fn state_path(root: impl AsRef<Path>, epoch: u64) -> PathBuf {
    root.as_ref().join(format!("ckpt-{epoch:010}")).join(STATE)
}
