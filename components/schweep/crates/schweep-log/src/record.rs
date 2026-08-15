//! The on-disk record format (`docs/DURABILITY.md` §1, §2, §4).
//!
//! A segment file is a sequence of records, each framed as:
//!
//! ```text
//! [ len: u32 BE ][ crc: u32 BE ][ payload: len bytes ]
//! ```
//!
//! **Length *and* CRC, both.** A length alone cannot tell a short write from a valid record whose
//! payload happens to begin with plausible bytes — and a crash between A5 and A6 of the ack sequence
//! leaves exactly that. The reader stops at the first record whose frame is short or whose CRC does
//! not match, and discards everything after it (R5). Torn tails are expected, not exceptional.
//!
//! The payload encoding is hand-rolled and deliberately dull. It has one job — round-trip a record —
//! and no requirement to be compact, fast, or ordered, so it does not use the order-preserving key
//! codec (`schweep_state::codec`) that `RocksBackend` needs. Two encodings with two different jobs
//! is less risk than one encoding serving two masters.

use schweep_zset::{Row, Value};

use crate::error::{LogError, Result};

/// One record in a segment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Record {
    /// A batch appended by a source, with the identity a replay is checked against (I-4).
    Append {
        source_id: String,
        dedup_token: String,
        table: String,
        entries: Vec<(Row, i64)>,
    },
    /// The commit point of an epoch (S-6). Everything appended before it belongs to that epoch.
    SealEpoch { epoch: u64 },
}

// ---- primitive writers -----------------------------------------------------------------------

fn put_u64(out: &mut Vec<u8>, v: u64) {
    out.extend_from_slice(&v.to_be_bytes());
}

fn put_i64(out: &mut Vec<u8>, v: i64) {
    put_u64(out, v as u64);
}

fn put_str(out: &mut Vec<u8>, s: &str) {
    put_u64(out, s.len() as u64);
    out.extend_from_slice(s.as_bytes());
}

fn put_value(out: &mut Vec<u8>, value: &Value) {
    match value {
        Value::Null => out.push(0),
        Value::Bool(b) => {
            out.push(1);
            out.push(u8::from(*b));
        }
        Value::Int(i) => {
            out.push(2);
            put_i64(out, *i);
        }
        Value::Float(x) => {
            out.push(3);
            put_u64(out, x.to_bits());
        }
        Value::Str(s) => {
            out.push(4);
            put_str(out, s);
        }
    }
}

// ---- primitive readers -----------------------------------------------------------------------

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Reader<'a> {
        Reader { bytes, at: 0 }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let slice = self
            .bytes
            .get(self.at..self.at + n)
            .ok_or(LogError::Corrupt("record payload is short"))?;
        self.at += n;
        Ok(slice)
    }

    fn u8(&mut self) -> Result<u8> {
        Ok(*self
            .take(1)?
            .first()
            .ok_or(LogError::Corrupt("record payload is short"))?)
    }

    fn u64(&mut self) -> Result<u64> {
        let slice = self.take(8)?;
        let mut raw = [0u8; 8];
        raw.copy_from_slice(slice);
        Ok(u64::from_be_bytes(raw))
    }

    fn i64(&mut self) -> Result<i64> {
        Ok(self.u64()? as i64)
    }

    fn string(&mut self) -> Result<String> {
        let len = self.u64()? as usize;
        let slice = self.take(len)?;
        String::from_utf8(slice.to_vec()).map_err(|_| LogError::Corrupt("string is not UTF-8"))
    }

    fn value(&mut self) -> Result<Value> {
        match self.u8()? {
            0 => Ok(Value::Null),
            1 => Ok(Value::Bool(self.u8()? != 0)),
            2 => Ok(Value::Int(self.i64()?)),
            3 => Ok(Value::Float(f64::from_bits(self.u64()?))),
            4 => Ok(Value::Str(self.string()?)),
            _ => Err(LogError::Corrupt("unknown value tag")),
        }
    }
}

impl Record {
    /// The record's payload bytes, without the frame.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        match self {
            Record::Append {
                source_id,
                dedup_token,
                table,
                entries,
            } => {
                out.push(1);
                put_str(&mut out, source_id);
                put_str(&mut out, dedup_token);
                put_str(&mut out, table);
                put_u64(&mut out, entries.len() as u64);
                for (row, weight) in entries {
                    put_u64(&mut out, row.len() as u64);
                    for value in row.values() {
                        put_value(&mut out, value);
                    }
                    put_i64(&mut out, *weight);
                }
            }
            Record::SealEpoch { epoch } => {
                out.push(2);
                put_u64(&mut out, *epoch);
            }
        }
        out
    }

    pub fn decode(payload: &[u8]) -> Result<Record> {
        let mut r = Reader::new(payload);
        match r.u8()? {
            1 => {
                let source_id = r.string()?;
                let dedup_token = r.string()?;
                let table = r.string()?;
                let count = r.u64()? as usize;
                let mut entries = Vec::with_capacity(count.min(4096));
                for _ in 0..count {
                    let width = r.u64()? as usize;
                    let mut values = Vec::with_capacity(width.min(4096));
                    for _ in 0..width {
                        values.push(r.value()?);
                    }
                    let weight = r.i64()?;
                    entries.push((Row::new(values), weight));
                }
                Ok(Record::Append {
                    source_id,
                    dedup_token,
                    table,
                    entries,
                })
            }
            2 => Ok(Record::SealEpoch { epoch: r.u64()? }),
            _ => Err(LogError::Corrupt("unknown record tag")),
        }
    }

    /// A stable hash of the batch's *content*, used to tell a replay from a token collision (I-4).
    ///
    /// The token names an identity; this names what was sent under it. A1–A4 of the ack sequence
    /// compares both: same token and same content is a replay to be dropped; same token and
    /// different content is a caller bug to be refused loudly.
    #[must_use]
    pub fn content_hash(&self) -> u64 {
        // FNV-1a over the encoded payload. Not cryptographic and not required to be: it
        // distinguishes two batches a caller sent under one token, which is a mistake to catch, not
        // an attack to withstand. A collision would make a refusal look like a replay, which is why
        // the token is compared too.
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for byte in self.encode() {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01B3);
        }
        hash
    }
}

/// Frame a payload for the wire: length, CRC, payload.
#[must_use]
pub fn frame(payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 8);
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(&crc32(payload).to_be_bytes());
    out.extend_from_slice(payload);
    out
}

/// Read one framed record, returning it and the bytes consumed.
///
/// `Ok(None)` means "the tail is torn or absent" — the caller stops there and discards the rest
/// (R5). It is not an error: it is the expected state of a log whose process died mid-write.
pub fn read_framed(bytes: &[u8], at: usize) -> Result<Option<(Record, usize)>> {
    let Some(header) = bytes.get(at..at + 8) else {
        return Ok(None);
    };
    let mut len_raw = [0u8; 4];
    let mut crc_raw = [0u8; 4];
    len_raw.copy_from_slice(header.get(0..4).ok_or(LogError::Corrupt("short frame"))?);
    crc_raw.copy_from_slice(header.get(4..8).ok_or(LogError::Corrupt("short frame"))?);
    let len = u32::from_be_bytes(len_raw) as usize;
    let expected = u32::from_be_bytes(crc_raw);

    let Some(payload) = bytes.get(at + 8..at + 8 + len) else {
        // The frame promised more bytes than the file holds: a torn write.
        return Ok(None);
    };
    if crc32(payload) != expected {
        // The bytes are there but wrong — a torn or flipped write. Same treatment.
        return Ok(None);
    }
    let record = Record::decode(payload)?;
    Ok(Some((record, at + 8 + len)))
}

/// Incremental CRC-32 (IEEE).
///
/// The stateful form lets callers verify files with a bounded buffer. Reading a snapshot into one
/// `Vec` merely to checksum it makes integrity verification the largest allocation in bootstrap,
/// which defeats C10's streaming hydration even though the Parquet reader itself is bounded.
#[derive(Clone, Copy, Debug)]
pub struct Crc32(u32);

impl Crc32 {
    #[must_use]
    pub const fn new() -> Crc32 {
        Crc32(0xFFFF_FFFF)
    }

    pub fn update(&mut self, bytes: &[u8]) {
        let mut crc = self.0;
        for byte in bytes {
            crc ^= u32::from(*byte);
            for _ in 0..8 {
                let mask = if crc & 1 != 0 { 0xEDB8_8320 } else { 0 };
                crc = (crc >> 1) ^ mask;
            }
        }
        self.0 = crc;
    }

    #[must_use]
    pub const fn finish(self) -> u32 {
        !self.0
    }
}

impl Default for Crc32 {
    fn default() -> Self {
        Self::new()
    }
}

/// CRC-32 (IEEE), computed directly so the log has no dependency for a table-free implementation.
#[must_use]
pub fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = Crc32::new();
    crc.update(bytes);
    crc.finish()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]

    use super::*;

    fn append() -> Record {
        Record::Append {
            source_id: "src-1".into(),
            dedup_token: "tok-1".into(),
            table: "t".into(),
            entries: vec![
                (Row::new(vec![Value::Int(1), Value::Null]), 2),
                (
                    Row::new(vec![Value::Str("a\0b".into()), Value::Bool(true)]),
                    -1,
                ),
                (Row::new(vec![Value::Float(1.5)]), 1),
            ],
        }
    }

    #[test]
    fn records_round_trip() {
        for record in [append(), Record::SealEpoch { epoch: 42 }] {
            let payload = record.encode();
            assert_eq!(Record::decode(&payload).unwrap(), record);
        }
    }

    #[test]
    fn a_framed_record_reads_back() {
        let record = append();
        let bytes = frame(&record.encode());
        let (read, next) = read_framed(&bytes, 0).unwrap().unwrap();
        assert_eq!(read, record);
        assert_eq!(next, bytes.len());
        assert!(
            read_framed(&bytes, next).unwrap().is_none(),
            "no more records"
        );
    }

    /// A truncated frame is a torn tail, not an error (R5).
    #[test]
    fn a_truncated_frame_reads_as_a_torn_tail() {
        let bytes = frame(&append().encode());
        for cut in 0..bytes.len() {
            assert!(
                read_framed(&bytes[..cut], 0).unwrap().is_none(),
                "truncating to {cut} bytes should read as a torn tail"
            );
        }
    }

    /// A flipped byte in the payload fails the CRC and reads as a torn tail.
    #[test]
    fn a_flipped_byte_fails_the_crc() {
        let good = frame(&append().encode());
        for index in 8..good.len() {
            let mut bad = good.clone();
            bad[index] ^= 0xFF;
            assert!(
                read_framed(&bad, 0).unwrap().is_none(),
                "flipping byte {index} must fail the CRC"
            );
        }
    }

    /// Two records in sequence, with the second torn: the first survives, the second does not.
    #[test]
    fn a_good_record_followed_by_a_torn_one_keeps_the_first() {
        let mut bytes = frame(&append().encode());
        let first_len = bytes.len();
        let second = frame(&Record::SealEpoch { epoch: 1 }.encode());
        bytes.extend_from_slice(&second[..second.len() - 2]);

        let (record, next) = read_framed(&bytes, 0).unwrap().unwrap();
        assert!(matches!(record, Record::Append { .. }));
        assert_eq!(next, first_len);
        assert!(read_framed(&bytes, next).unwrap().is_none());
    }

    #[test]
    fn the_content_hash_distinguishes_different_batches() {
        let a = append();
        let mut b_entries = match &a {
            Record::Append { entries, .. } => entries.clone(),
            _ => panic!(),
        };
        b_entries.push((Row::new(vec![Value::Int(9)]), 1));
        let b = Record::Append {
            source_id: "src-1".into(),
            dedup_token: "tok-1".into(),
            table: "t".into(),
            entries: b_entries,
        };
        assert_ne!(
            a.content_hash(),
            b.content_hash(),
            "the same token with different content must hash differently (I-4)"
        );
        assert_eq!(a.content_hash(), append().content_hash(), "and be stable");
    }

    #[test]
    fn crc32_matches_known_values() {
        // The standard check value for CRC-32/IEEE over "123456789".
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b""), 0);
    }

    #[test]
    fn incremental_crc_is_independent_of_chunk_boundaries() {
        let bytes = b"a snapshot checksum must not require the snapshot in memory";
        let mut incremental = Crc32::new();
        for chunk in bytes.chunks(3) {
            incremental.update(chunk);
        }
        assert_eq!(incremental.finish(), crc32(bytes));
    }
}
