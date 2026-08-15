//! Reading a segment **one epoch at a time**, without holding the rest (C9).
//!
//! [`Log`](crate::Log) keeps every sealed batch resident (`sealed: Vec<Vec<Batch>>`), which is what makes
//! `Log::epoch` a borrow rather than a read. That is a fine trade for an append path and a poor one for a
//! *history*: a process whose log holds a gigabyte holds a gigabyte. This module is the other half —
//! a reader that walks the segment file, yields one epoch's records, and drops them before reading the
//! next, so its footprint is the largest epoch rather than the whole log.
//!
//! **What it is for, precisely.** A late registration's catch-up (`Memo::register_from_chunks`) needs the
//! history as a stream, and `testing/soak/tests/c9_memo_ceiling.rs` measures that it can do so under a
//! memory ceiling the history exceeds. What it does **not** do is make a running `schweepd` fit under such
//! a ceiling: the server also holds a `Log` for its append path, and that `Log` is still O(history)
//! resident. `docs/PROGRESS.md` records that limit against C10 rather than implying this module removed it.
//!
//! It reads only what the log wrote — the same frames, the same CRC rule, the same torn-tail treatment
//! (R6: stop at the first record that fails its CRC or is short). It is not a second log format.

use std::fs::File;
use std::io::{BufReader, Read};
use std::path::Path;

use crate::error::{LogError, Result};
use crate::log::Batch;
use crate::record::{crc32, Record};

/// One sealed epoch, read from disk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SealedEpoch {
    pub epoch: u64,
    pub batches: Vec<Batch>,
}

/// A segment, walked epoch by epoch.
///
/// The iterator ends at the torn tail or at end of file, and an epoch's worth of records is the most it
/// holds at once. Appends *after* it was created are not seen: it is a reader over what was there.
#[derive(Debug)]
pub struct Epochs {
    reader: BufReader<File>,
    /// Records appended and not yet closed by a seal — the pending set, as of this point in the file.
    pending: Vec<Batch>,
    done: bool,
}

impl Epochs {
    /// Open a segment file for streaming.
    ///
    /// Takes the *segment path* rather than a directory, because which segment is live is the pointer's
    /// business ([`crate::Log::segment_path`]) and duplicating that decision here would create a second
    /// answer to it.
    pub fn open(segment: impl AsRef<Path>) -> Result<Epochs> {
        let file = File::open(segment.as_ref()).map_err(|e| LogError::Io(e.to_string()))?;
        Ok(Epochs {
            // 64 KiB, matching the log's own write buffering: large enough that a frame rarely spans two
            // reads, small enough to be invisible next to any ceiling this runs under.
            reader: BufReader::with_capacity(64 * 1024, file),
            pending: Vec::new(),
            done: false,
        })
    }

    /// Read one framed record, or `None` at the torn tail or end of file (R6).
    fn next_record(&mut self) -> Result<Option<Record>> {
        let mut header = [0u8; 8];
        if !read_exactly(&mut self.reader, &mut header)? {
            return Ok(None);
        }
        let mut len_raw = [0u8; 4];
        let mut crc_raw = [0u8; 4];
        len_raw.copy_from_slice(header.get(0..4).ok_or(LogError::Corrupt("short frame"))?);
        crc_raw.copy_from_slice(header.get(4..8).ok_or(LogError::Corrupt("short frame"))?);
        let len = u32::from_be_bytes(len_raw) as usize;
        let expected = u32::from_be_bytes(crc_raw);

        let mut payload = vec![0u8; len];
        if !read_exactly(&mut self.reader, &mut payload)? {
            // The frame promised more bytes than the file holds: the torn tail.
            return Ok(None);
        }
        if crc32(&payload) != expected {
            return Ok(None);
        }
        Ok(Some(Record::decode(&payload)?))
    }
}

impl Iterator for Epochs {
    type Item = Result<SealedEpoch>;

    fn next(&mut self) -> Option<Result<SealedEpoch>> {
        if self.done {
            return None;
        }
        loop {
            match self.next_record() {
                Err(error) => {
                    self.done = true;
                    return Some(Err(error));
                }
                // End of file or torn tail. Whatever is pending was never sealed, so it is not an epoch
                // — the same rule the live path follows.
                Ok(None) => {
                    self.done = true;
                    return None;
                }
                Ok(Some(Record::Append {
                    source_id,
                    dedup_token,
                    table,
                    entries,
                })) => self.pending.push(Batch {
                    source_id,
                    dedup_token,
                    table,
                    entries,
                }),
                Ok(Some(Record::SealEpoch { epoch })) => {
                    return Some(Ok(SealedEpoch {
                        epoch,
                        batches: std::mem::take(&mut self.pending),
                    }));
                }
            }
        }
    }
}

/// Fill `buffer` completely, or report that the file ended first.
fn read_exactly(reader: &mut impl Read, buffer: &mut [u8]) -> Result<bool> {
    let mut filled = 0usize;
    while filled < buffer.len() {
        let Some(slice) = buffer.get_mut(filled..) else {
            return Ok(false);
        };
        match reader.read(slice) {
            Ok(0) => return Ok(false),
            Ok(read) => filled += read,
            Err(error) => return Err(LogError::Io(error.to_string())),
        }
    }
    Ok(true)
}
