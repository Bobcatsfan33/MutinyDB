//! The dedup ledger: the acknowledged tokens, written down so they can survive a compaction.
//!
//! **Why this file exists at all.** R7 rebuilds the dedup index by scanning every `Append` record in
//! the retained log. Compaction (`docs/DURABILITY.md` §4) throws part of that log away. A token
//! acknowledged in the discarded prefix and re-offered afterwards would then look new, and the batch
//! would be applied a second time — I-4 broken by a *space optimisation*, silently, with no error and
//! no crash. So the ledger rides the snapshot, and R7 seeds from it.
//!
//! The format is deliberately dull and self-checking: a count, then `(token, content hash)` pairs,
//! then a CRC over the lot. It is the same shape as a log record's frame, for the same reason — a
//! ledger torn by a crash must be detectable rather than plausible.

use std::collections::BTreeMap;

use crate::error::{LogError, Result};
use crate::record::crc32;

/// Encode the ledger. Ordered by token, because a `BTreeMap` iterated twice must produce the same
/// bytes twice (I-2).
#[must_use]
pub fn encode(tokens: &BTreeMap<String, u64>) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&(tokens.len() as u64).to_be_bytes());
    for (token, hash) in tokens {
        body.extend_from_slice(&(token.len() as u64).to_be_bytes());
        body.extend_from_slice(token.as_bytes());
        body.extend_from_slice(&hash.to_be_bytes());
    }
    let mut out = crc32(&body).to_be_bytes().to_vec();
    out.extend_from_slice(&body);
    out
}

/// Decode a ledger, refusing one whose CRC does not match.
pub fn decode(bytes: &[u8]) -> Result<BTreeMap<String, u64>> {
    let stored = bytes
        .get(0..4)
        .ok_or(LogError::Corrupt("dedup ledger is short"))?;
    let mut raw = [0u8; 4];
    raw.copy_from_slice(stored);
    let expected = u32::from_be_bytes(raw);
    let body = bytes
        .get(4..)
        .ok_or(LogError::Corrupt("dedup ledger is short"))?;
    if crc32(body) != expected {
        return Err(LogError::Corrupt("dedup ledger failed its CRC"));
    }

    let mut at = 0usize;
    let count = take_u64(body, &mut at)?;
    let mut tokens = BTreeMap::new();
    for _ in 0..count {
        let len = take_u64(body, &mut at)? as usize;
        let raw = body
            .get(at..at + len)
            .ok_or(LogError::Corrupt("dedup ledger is short"))?;
        at += len;
        let token = String::from_utf8(raw.to_vec())
            .map_err(|_| LogError::Corrupt("dedup token is not UTF-8"))?;
        let hash = take_u64(body, &mut at)?;
        tokens.insert(token, hash);
    }
    Ok(tokens)
}

fn take_u64(bytes: &[u8], at: &mut usize) -> Result<u64> {
    let slice = bytes
        .get(*at..*at + 8)
        .ok_or(LogError::Corrupt("dedup ledger is short"))?;
    let mut raw = [0u8; 8];
    raw.copy_from_slice(slice);
    *at += 8;
    Ok(u64::from_be_bytes(raw))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]

    use super::*;

    fn ledger() -> BTreeMap<String, u64> {
        BTreeMap::from([
            ("epoch-1-t".to_owned(), 0xdead_beef),
            ("epoch-2-t".to_owned(), 7),
            ("odd token with spaces".to_owned(), u64::MAX),
        ])
    }

    #[test]
    fn a_ledger_round_trips() {
        assert_eq!(decode(&encode(&ledger())).unwrap(), ledger());
    }

    #[test]
    fn an_empty_ledger_round_trips() {
        let empty = BTreeMap::new();
        assert_eq!(decode(&encode(&empty)).unwrap(), empty);
    }

    #[test]
    fn encoding_is_byte_stable() {
        assert_eq!(encode(&ledger()), encode(&ledger()));
    }

    /// A torn or flipped ledger is detected rather than half-read. A dedup index built from half a
    /// ledger would silently forget tokens, which is exactly the I-4 hole compaction must not open.
    #[test]
    fn a_damaged_ledger_is_refused() {
        let good = encode(&ledger());
        for cut in 0..good.len() {
            assert!(
                decode(&good[..cut]).is_err(),
                "truncating to {cut} bytes must be refused"
            );
        }
        for index in 4..good.len() {
            let mut bad = good.clone();
            bad[index] ^= 0xFF;
            assert!(
                decode(&bad).is_err(),
                "flipping byte {index} must fail the CRC"
            );
        }
    }
}
