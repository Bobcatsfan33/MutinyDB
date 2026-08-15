//! Order-preserving byte encoding for values (D-15).
//!
//! D-15 put the *interface* in terms of `Vec<Value>` keys rather than bytes, on the grounds that an
//! order-preserving byte encoding is a storage concern and belongs inside the backend that needs
//! one. This is that backend's concern, arriving on schedule: `RocksBackend` stores bytes, and its
//! scans must come back in the order `docs/SEMANTICS.md` S-7 defines.
//!
//! ## The property
//!
//! For any two keys `a` and `b`:
//!
//! ```text
//! encode(a).cmp(&encode(b))  ==  a.cmp(&b)
//! ```
//!
//! Byte-lexicographic order of encodings equals the total order on value tuples. That is not a
//! nice-to-have: `StateBackend::scan_prefix` promises ordered results, the aggregate reads MIN as
//! the first entry of a scan and MAX as the last (S-30, §5.3), and a backend whose order differed
//! from `MemBackend`'s would give a different answer for the same data. It is checked by a property
//! test over random values, not argued from the code.
//!
//! ## How each part achieves it
//!
//! - **A tag byte first**, in the same rank order `Value::cmp` uses: `Null` < `Bool` < `Int` <
//!   `Float` < `Str`. Cross-type comparison does not arise in practice (S-2) but the order must be
//!   total, so it is defined here too.
//! - **`Int64`**: big-endian with the sign bit flipped. Two's-complement bytes do not sort correctly
//!   — `-1` is `0xFF…` and would outrank `1` — and flipping the top bit maps the signed range onto
//!   the unsigned range monotonically.
//! - **`Float64`**: the IEEE-754 total order, as bits. Negatives are bit-inverted and non-negatives
//!   get their sign bit set, which is exactly the transformation that makes `f64::total_cmp` agree
//!   with unsigned integer comparison of the result.
//! - **`Utf8`**: escaped and terminated, so that one encoded value can never be a prefix of another.
//!   A bare byte string would break multi-component keys: `["a", "b"]` and `["ab"]` would encode
//!   identically. `0x00` becomes `0x00 0xFF`, and `0x00 0x00` terminates.
//!
//! Every encoding is **self-delimiting**, which is what makes a prefix of components encode to a
//! prefix of bytes — the property `scan_prefix` is built on.

use schweep_zset::Value;

use crate::error::{Result, StateError};

const TAG_NULL: u8 = 0;
const TAG_BOOL: u8 = 1;
const TAG_INT: u8 = 2;
const TAG_FLOAT: u8 = 3;
const TAG_STR: u8 = 4;

/// Encode a key so that byte order equals value order (S-7).
#[must_use]
pub fn encode_key(key: &[Value]) -> Vec<u8> {
    let mut out = Vec::with_capacity(key.len() * 9);
    for value in key {
        encode_value(value, &mut out);
    }
    out
}

fn encode_value(value: &Value, out: &mut Vec<u8>) {
    match value {
        Value::Null => out.push(TAG_NULL),
        Value::Bool(b) => {
            out.push(TAG_BOOL);
            out.push(u8::from(*b));
        }
        Value::Int(i) => {
            out.push(TAG_INT);
            // Flip the sign bit: maps i64's order onto u64's order monotonically.
            out.extend_from_slice(&((*i as u64) ^ (1 << 63)).to_be_bytes());
        }
        Value::Float(x) => {
            out.push(TAG_FLOAT);
            let bits = x.to_bits();
            // The transformation that makes unsigned comparison agree with `f64::total_cmp`.
            let ordered = if bits & (1 << 63) != 0 {
                !bits
            } else {
                bits | (1 << 63)
            };
            out.extend_from_slice(&ordered.to_be_bytes());
        }
        Value::Str(s) => {
            out.push(TAG_STR);
            for byte in s.as_bytes() {
                out.push(*byte);
                if *byte == 0x00 {
                    // Escape, so a literal NUL cannot be mistaken for the terminator.
                    out.push(0xFF);
                }
            }
            out.extend_from_slice(&[0x00, 0x00]);
        }
    }
}

/// Decode a key encoded by [`encode_key`].
pub fn decode_key(bytes: &[u8]) -> Result<Vec<Value>> {
    let mut out = Vec::new();
    let mut at = 0usize;
    while at < bytes.len() {
        let (value, next) = decode_value(bytes, at)?;
        out.push(value);
        at = next;
    }
    Ok(out)
}

fn corrupt(what: &'static str) -> StateError {
    StateError::Backend(format!("corrupt encoded key: {what}"))
}

fn decode_value(bytes: &[u8], at: usize) -> Result<(Value, usize)> {
    let tag = *bytes.get(at).ok_or_else(|| corrupt("missing tag"))?;
    let body = at + 1;
    match tag {
        TAG_NULL => Ok((Value::Null, body)),
        TAG_BOOL => {
            let b = *bytes.get(body).ok_or_else(|| corrupt("short bool"))?;
            Ok((Value::Bool(b != 0), body + 1))
        }
        TAG_INT => {
            let raw = eight(bytes, body)?;
            Ok((
                Value::Int((u64::from_be_bytes(raw) ^ (1 << 63)) as i64),
                body + 8,
            ))
        }
        TAG_FLOAT => {
            let raw = eight(bytes, body)?;
            let ordered = u64::from_be_bytes(raw);
            let bits = if ordered & (1 << 63) != 0 {
                ordered & !(1 << 63)
            } else {
                !ordered
            };
            Ok((Value::Float(f64::from_bits(bits)), body + 8))
        }
        TAG_STR => {
            let mut text = Vec::new();
            let mut at = body;
            loop {
                let byte = *bytes
                    .get(at)
                    .ok_or_else(|| corrupt("unterminated string"))?;
                if byte == 0x00 {
                    let next = *bytes.get(at + 1).ok_or_else(|| corrupt("short escape"))?;
                    if next == 0x00 {
                        at += 2;
                        break;
                    }
                    if next == 0xFF {
                        text.push(0x00);
                        at += 2;
                        continue;
                    }
                    return Err(corrupt("bad escape"));
                }
                text.push(byte);
                at += 1;
            }
            let s = String::from_utf8(text).map_err(|_| corrupt("string is not UTF-8"))?;
            Ok((Value::Str(s), at))
        }
        other => Err(StateError::Backend(format!(
            "corrupt encoded key: unknown tag {other}"
        ))),
    }
}

fn eight(bytes: &[u8], at: usize) -> Result<[u8; 8]> {
    let slice = bytes
        .get(at..at + 8)
        .ok_or_else(|| corrupt("short fixed-width value"))?;
    let mut raw = [0u8; 8];
    raw.copy_from_slice(slice);
    Ok(raw)
}

/// Serialise `(key, weight)` entries for a snapshot (`docs/DURABILITY.md` C1).
///
/// Length-prefixed so the reader knows where each key ends; the key encoding itself is the
/// order-preserving one, so a snapshot restores in the same order it was taken.
#[must_use]
pub fn encode_entries(entries: &[(Vec<Value>, i64)]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&(entries.len() as u64).to_be_bytes());
    for (key, weight) in entries {
        let encoded = encode_key(key);
        out.extend_from_slice(&(encoded.len() as u32).to_be_bytes());
        out.extend_from_slice(&encoded);
        out.extend_from_slice(&weight.to_be_bytes());
    }
    out
}

/// Read entries written by [`encode_entries`].
pub fn decode_entries(bytes: &[u8]) -> Result<Vec<(Vec<Value>, i64)>> {
    let short = || corrupt("snapshot is short");
    let mut count_raw = [0u8; 8];
    count_raw.copy_from_slice(bytes.get(0..8).ok_or_else(short)?);
    let count = u64::from_be_bytes(count_raw) as usize;

    let mut at = 8usize;
    let mut out = Vec::with_capacity(count.min(1 << 16));
    for _ in 0..count {
        let mut len_raw = [0u8; 4];
        len_raw.copy_from_slice(bytes.get(at..at + 4).ok_or_else(short)?);
        let len = u32::from_be_bytes(len_raw) as usize;
        at += 4;
        let key = decode_key(bytes.get(at..at + len).ok_or_else(short)?)?;
        at += len;
        let mut weight_raw = [0u8; 8];
        weight_raw.copy_from_slice(bytes.get(at..at + 8).ok_or_else(short)?);
        at += 8;
        out.push((key, i64::from_be_bytes(weight_raw)));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]

    use super::*;

    fn round_trip(key: &[Value]) {
        let encoded = encode_key(key);
        let decoded = decode_key(&encoded).unwrap();
        assert_eq!(decoded, key, "round trip failed for {key:?}");
    }

    #[test]
    fn every_value_kind_round_trips() {
        round_trip(&[Value::Null]);
        round_trip(&[Value::Bool(false), Value::Bool(true)]);
        round_trip(&[
            Value::Int(0),
            Value::Int(-1),
            Value::Int(i64::MIN),
            Value::Int(i64::MAX),
        ]);
        round_trip(&[
            Value::Float(0.0),
            Value::Float(-0.0),
            Value::Float(f64::MAX),
        ]);
        round_trip(&[Value::Str(String::new()), Value::Str("hello".into())]);
        round_trip(&[
            Value::Int(7),
            Value::Str("mixed".into()),
            Value::Null,
            Value::Bool(true),
        ]);
    }

    /// A string containing a NUL byte must survive, and must not be confused with a terminator.
    #[test]
    fn a_string_containing_a_nul_byte_round_trips() {
        let key = vec![Value::Str("a\0b".into()), Value::Int(1)];
        round_trip(&key);
    }

    /// The property the whole file exists for: byte order equals value order (S-7).
    #[test]
    fn byte_order_equals_value_order() {
        let values = vec![
            Value::Null,
            Value::Bool(false),
            Value::Bool(true),
            Value::Int(i64::MIN),
            Value::Int(-2),
            Value::Int(-1),
            Value::Int(0),
            Value::Int(1),
            Value::Int(i64::MAX),
            Value::Float(f64::NEG_INFINITY),
            Value::Float(-1.5),
            Value::Float(-0.0),
            Value::Float(0.0),
            Value::Float(1.5),
            Value::Float(f64::INFINITY),
            Value::Str(String::new()),
            Value::Str("\0".into()),
            Value::Str("a".into()),
            Value::Str("ab".into()),
            Value::Str("b".into()),
        ];
        for a in &values {
            for b in &values {
                let by_value = a.cmp(b);
                let by_bytes =
                    encode_key(std::slice::from_ref(a)).cmp(&encode_key(std::slice::from_ref(b)));
                assert_eq!(
                    by_value, by_bytes,
                    "order disagrees for {a:?} vs {b:?}: value says {by_value:?}, bytes say \
                     {by_bytes:?}"
                );
            }
        }
    }

    /// Multi-component keys order lexicographically, and no encoding is a prefix of another.
    ///
    /// This is the case a bare byte string breaks: `["a", "b"]` and `["ab"]` would encode
    /// identically without the terminator, and `scan_prefix` would return rows from the wrong group.
    #[test]
    fn multi_component_keys_order_correctly_and_are_prefix_free() {
        let keys = vec![
            vec![Value::Str("a".into()), Value::Str("b".into())],
            vec![Value::Str("ab".into())],
            vec![Value::Str("a".into())],
            vec![Value::Str("a".into()), Value::Str("a".into())],
            vec![Value::Int(1), Value::Int(2)],
            vec![Value::Int(1)],
        ];
        for a in &keys {
            for b in &keys {
                assert_eq!(
                    a.cmp(b),
                    encode_key(a).cmp(&encode_key(b)),
                    "order disagrees for {a:?} vs {b:?}"
                );
            }
        }
        assert_ne!(
            encode_key(&[Value::Str("a".into()), Value::Str("b".into())]),
            encode_key(&[Value::Str("ab".into())])
        );
    }

    /// A prefix of components encodes to a prefix of bytes — what `scan_prefix` rests on.
    #[test]
    fn a_component_prefix_is_a_byte_prefix() {
        let full = vec![Value::Str("g".into()), Value::Int(3), Value::Bool(true)];
        for take in 0..full.len() {
            let prefix = &full[..take];
            assert!(
                encode_key(&full).starts_with(&encode_key(prefix)),
                "encoding of {prefix:?} is not a byte prefix of {full:?}"
            );
        }
    }

    #[test]
    fn entries_round_trip_through_a_snapshot() {
        let entries = vec![
            (vec![Value::Null, Value::Int(1)], 3),
            (vec![Value::Str("g".into()), Value::Float(1.5)], -2),
            (vec![], 7),
        ];
        let bytes = encode_entries(&entries);
        assert_eq!(decode_entries(&bytes).unwrap(), entries);
    }

    #[test]
    fn a_truncated_snapshot_is_refused_rather_than_half_read() {
        let bytes = encode_entries(&[(vec![Value::Int(1)], 1)]);
        for cut in 0..bytes.len() {
            assert!(
                decode_entries(&bytes[..cut]).is_err(),
                "truncating to {cut} must be refused"
            );
        }
    }

    /// A seeded sweep, so the order property is checked on values nobody chose.
    #[test]
    fn a_seeded_sweep_agrees_on_order() {
        let mut state: u64 = 0x1234_5678_9ABC_DEF0;
        let mut next = || {
            state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = state;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        };
        let value = |r: u64| -> Value {
            match r % 5 {
                0 => Value::Null,
                1 => Value::Bool(r & 8 != 0),
                2 => Value::Int(r as i64),
                3 => {
                    let x = f64::from_bits(r);
                    if x.is_nan() {
                        Value::Float(0.0)
                    } else {
                        Value::Float(x)
                    }
                }
                _ => Value::Str(format!("s{}", r % 97)),
            }
        };

        let mut keys = Vec::new();
        for _ in 0..600 {
            let len = (next() % 3 + 1) as usize;
            keys.push((0..len).map(|_| value(next())).collect::<Vec<_>>());
        }
        for a in &keys {
            for b in keys.iter().take(60) {
                assert_eq!(
                    a.cmp(b),
                    encode_key(a).cmp(&encode_key(b)),
                    "order disagrees for {a:?} vs {b:?}"
                );
            }
            decode_key(&encode_key(a)).unwrap();
        }
    }
}
