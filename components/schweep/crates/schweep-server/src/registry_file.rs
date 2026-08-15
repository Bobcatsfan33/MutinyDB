//! The persisted registry: what a registration means across a restart (D-22).
//!
//! ```text
//!   REGISTRY
//!     schweep registry v1
//!     handle=0 admission=bounded sql=SELECT t.n AS n FROM t WHERE t.k > 1
//!     handle=1 admission=unbounded:k is a user-supplied key space sql=SELECT ...
//!     crc=1a2b3c4d
//! ```
//!
//! **The SQL text is what is persisted, not the plan.** A plan is a compiled artefact whose structure
//! this project deliberately reserves the right to change — the incrementalizer is C5's code and C6's
//! canonicalization rewrites it — so persisting a plan would make every registration a compatibility
//! obligation on the plan format. The text is what the client said, it is what a person can read in the
//! file, and recompiling it is cheap next to rebuilding the circuit it names.
//!
//! **What that costs, stated:** if the *dialect* changes so that an old registration no longer binds, the
//! registration cannot be rebuilt. D-22 says what happens then — it is quarantined, not dropped, and the
//! handle reports the error until a client deregisters it. A registration that silently disappeared
//! would be the worst of the options, because the server would come back healthy and answer nothing.
//!
//! Written by write-to-temp + rename + fsync, the same publish-then-swap discipline every other pointer
//! in this system uses (`docs/DURABILITY.md` §4): a crash mid-write leaves the previous registry, never
//! half of two.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use schweep_memo::Admission;

use crate::error::{ServerError, ServerResult};

/// The file inside the data directory.
pub const REGISTRY: &str = "REGISTRY";

/// One persisted registration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub handle: u64,
    pub sql: String,
    pub admission: Admission,
}

/// Everything the registry file holds.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Registry {
    /// Handle → entry. Ordered, so the file's bytes are a function of its contents (I-2).
    pub entries: BTreeMap<u64, Entry>,
    /// The next handle to issue. Persisted so a handle is never reused after a restart — a reused
    /// handle would make a stale client's cursor point at somebody else's query, which is the one
    /// failure a handle namespace must not have.
    pub next_handle: u64,
}

impl Registry {
    #[must_use]
    pub fn path(dir: &Path) -> PathBuf {
        dir.join(REGISTRY)
    }

    /// Encode. The admission's reason travels with it, because D-22 makes the admission part of what a
    /// registration *is*: rebuilding it without the admission would rebuild a different query.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut body = format!("schweep registry v1\nnext_handle={}\n", self.next_handle);
        for entry in self.entries.values() {
            let admission = match entry.admission.unbounded_reason() {
                None => "bounded".to_owned(),
                Some(reason) => format!("unbounded:{}", escape(reason)),
            };
            // The SQL is last on the line and may contain anything but a newline, which the binder
            // never needs: a query is one statement (D-23's `/register` takes one).
            body.push_str(&format!(
                "handle={} admission={admission} sql={}\n",
                entry.handle,
                entry.sql.replace('\n', " ")
            ));
        }
        format!(
            "{body}crc={:08x}\n",
            schweep_log::record::crc32(body.as_bytes())
        )
        .into_bytes()
    }

    pub fn decode(bytes: &[u8]) -> ServerResult<Registry> {
        let text =
            std::str::from_utf8(bytes).map_err(|_| ServerError::CorruptRegistry("not UTF-8"))?;
        // The terminator is part of the format, so that *every* truncation is detectable. Accepting a
        // registry that lost only its final newline would leave one byte of truncation indistinguishable
        // from a whole file, and "one byte" is exactly what a crash mid-write leaves.
        if !text.ends_with('\n') {
            return Err(ServerError::CorruptRegistry("truncated: no final newline"));
        }
        let (body, checksum) = text
            .rsplit_once("crc=")
            .ok_or(ServerError::CorruptRegistry("no checksum"))?;
        let expected = u32::from_str_radix(checksum.trim(), 16)
            .map_err(|_| ServerError::CorruptRegistry("checksum is not hex"))?;
        if schweep_log::record::crc32(body.as_bytes()) != expected {
            return Err(ServerError::CorruptRegistry("failed its checksum"));
        }

        let mut registry = Registry::default();
        for line in body.lines() {
            if let Some(value) = line.strip_prefix("next_handle=") {
                registry.next_handle = value.trim().parse().unwrap_or(0);
                continue;
            }
            if !line.starts_with("handle=") {
                continue;
            }
            let handle = field(line, "handle=")
                .and_then(|raw| raw.parse::<u64>().ok())
                .ok_or(ServerError::CorruptRegistry("a handle is not a number"))?;
            let admission_text = field(line, "admission=")
                .ok_or(ServerError::CorruptRegistry("an entry has no admission"))?;
            let admission = match admission_text.strip_prefix("unbounded:") {
                // The reason is escaped on the way out, so it survives as one field however many spaces
                // it contains. An I-9 reason is a sentence; rebuilding the query under a *truncated*
                // reason would look like it worked and mean something else (D-22).
                Some(reason) => Admission::with_unbounded_state(unescape(reason)),
                None => Admission::bounded(),
            };
            let sql = line
                .find(" sql=")
                .and_then(|at| line.get(at + " sql=".len()..))
                .ok_or(ServerError::CorruptRegistry("an entry has no sql"))?
                .to_owned();
            registry.entries.insert(
                handle,
                Entry {
                    handle,
                    sql,
                    admission,
                },
            );
        }
        // A registry whose next handle is behind an entry it holds would reissue that handle.
        let highest = registry.entries.keys().copied().max().map_or(0, |h| h + 1);
        registry.next_handle = registry.next_handle.max(highest);
        Ok(registry)
    }

    /// Read the registry, or an empty one if there is none.
    ///
    /// A registry that fails its checksum is an **error**, not an absence: an empty registry would mean
    /// "no standing queries", and answering that when the truth is "the file is damaged" would silently
    /// discard every client's work. D-22 chose durability; this is where that choice has teeth.
    pub fn load(dir: &Path) -> ServerResult<Registry> {
        match fs::read(Registry::path(dir)) {
            Ok(bytes) => Registry::decode(&bytes),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Registry::default()),
            Err(e) => Err(ServerError::Io(e.to_string())),
        }
    }

    /// Publish-then-swap: temp, rename, fsync the parent.
    pub fn store(&self, dir: &Path) -> ServerResult<()> {
        fs::create_dir_all(dir).map_err(|e| ServerError::Io(e.to_string()))?;
        let temp = dir.join("REGISTRY.partial");
        fs::write(&temp, self.encode()).map_err(|e| ServerError::Io(e.to_string()))?;
        fs::File::open(&temp)
            .and_then(|file| file.sync_all())
            .map_err(|e| ServerError::Io(e.to_string()))?;
        fs::rename(&temp, Registry::path(dir)).map_err(|e| ServerError::Io(e.to_string()))?;
        fs::File::open(dir)
            .and_then(|file| file.sync_all())
            .map_err(|e| ServerError::Io(e.to_string()))?;
        Ok(())
    }
}

/// Escape a reason so it is one space-free field. `%` first, or unescaping would invent one.
fn escape(reason: &str) -> String {
    reason
        .replace('%', "%25")
        .replace(' ', "%20")
        .replace('\n', "%0A")
        .replace('\r', "%0D")
}

fn unescape(escaped: &str) -> String {
    escaped
        .replace("%20", " ")
        .replace("%0A", "\n")
        .replace("%0D", "\r")
        .replace("%25", "%")
}

fn field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let at = line.find(key)? + key.len();
    let rest = &line[at..];
    Some(rest.split(' ').next().unwrap_or(rest))
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing
    )]

    use super::*;

    fn registry() -> Registry {
        let mut entries = BTreeMap::new();
        entries.insert(
            0,
            Entry {
                handle: 0,
                sql: "SELECT t.n AS n FROM t WHERE t.k > 1".to_owned(),
                admission: Admission::bounded(),
            },
        );
        entries.insert(
            1,
            Entry {
                handle: 1,
                sql: "SELECT t.k AS k, COUNT(*) AS c FROM t GROUP BY t.k".to_owned(),
                admission: Admission::with_unbounded_state("k is a user-supplied key space"),
            },
        );
        Registry {
            entries,
            next_handle: 2,
        }
    }

    #[test]
    fn a_registry_round_trips_with_its_admissions() {
        let decoded = Registry::decode(&registry().encode()).unwrap();
        assert_eq!(decoded, registry());
        assert_eq!(
            decoded.entries[&1].admission.unbounded_reason(),
            Some("k is a user-supplied key space"),
            "rebuilding without the admission would rebuild a different query (D-22, I-9)"
        );
    }

    #[test]
    fn encoding_is_byte_stable() {
        assert_eq!(registry().encode(), registry().encode());
    }

    #[test]
    fn a_damaged_registry_is_an_error_and_never_an_empty_one() {
        let good = registry().encode();
        for cut in 0..good.len() {
            assert!(
                Registry::decode(&good[..cut]).is_err(),
                "truncating to {cut} bytes must be refused, not read as 'no standing queries'"
            );
        }
        let mut flipped = good.clone();
        flipped[20] ^= 0xFF;
        assert!(Registry::decode(&flipped).is_err());
    }

    /// The admission's reason is a sentence, and it may say anything — including the file's own
    /// delimiters. A reason that came back truncated would rebuild the query under a *different*
    /// admission (I-9) while the server reported success.
    #[test]
    fn an_admission_reason_survives_spaces_percents_and_the_files_own_delimiters() {
        for reason in [
            "k is a user-supplied key space",
            "100% unbounded",
            "a reason that says sql= in it",
            "a reason with a\nnewline",
            "%20 already looks escaped",
        ] {
            let mut entries = BTreeMap::new();
            entries.insert(
                0,
                Entry {
                    handle: 0,
                    sql: "SELECT t.k AS k, COUNT(*) AS c FROM t GROUP BY t.k".to_owned(),
                    admission: Admission::with_unbounded_state(reason),
                },
            );
            let original = Registry {
                entries,
                next_handle: 1,
            };
            let decoded = Registry::decode(&original.encode()).unwrap();
            assert_eq!(
                decoded.entries[&0].admission.unbounded_reason(),
                Some(reason),
                "the reason {reason:?} did not survive the registry file"
            );
        }
    }

    /// A handle must never be reissued, even if the counter in the file is behind.
    #[test]
    fn the_next_handle_is_never_behind_an_entry() {
        let mut damaged = registry();
        damaged.next_handle = 0;
        let decoded = Registry::decode(&damaged.encode()).unwrap();
        assert_eq!(
            decoded.next_handle, 2,
            "a reused handle would point a stale client at somebody else's query"
        );
    }

    #[test]
    fn a_missing_registry_reads_as_empty_and_a_stored_one_reads_back() {
        let dir = std::env::temp_dir().join(format!("schweep-registry-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        assert_eq!(Registry::load(&dir).unwrap(), Registry::default());
        registry().store(&dir).unwrap();
        assert_eq!(Registry::load(&dir).unwrap(), registry());
        assert!(
            !dir.join("REGISTRY.partial").exists(),
            "the temp file is renamed, not left behind"
        );
        let _ = fs::remove_dir_all(&dir);
    }
}
