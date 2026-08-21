//! **The ledger's cold tier** (docs/M4-TAINT.md § "The archive tier"): content-addressed segment
//! files plus an append-only manifest, holding resolved recalls the hot relation no longer
//! carries. Every reader of the ledger's memory reads hot ∪ archive, so nothing about the
//! regenerate-forever promise changes — only where the bytes live.
//!
//! Integrity is structural: a segment's filename is the BLAKE3 of its content (tampering is
//! detected on read), and the `MANIFEST` names every segment that was ever retracted from the
//! hot relation (a deleted segment is refused by name, never a silently smaller union). The
//! crash order is append segment → append manifest line → retract hot; every step is idempotent
//! (same rows → same segment → same manifest line), so a replayed archival deduplicates.

use crate::{ContaminatedRow, TaintError};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::io::Write as _;
use std::path::{Path, PathBuf};

/// One archived ledger row: the full six-column relation row, source included.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ArchivedRow {
    pub source_system: String,
    pub source_record: String,
    pub branch: String,
    pub table: String,
    pub row_key_hex: String,
    pub envelope_hex: String,
}

/// What one archival pass did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ArchiveStats {
    /// Rows moved to the cold tier (and retracted from the hot relation).
    pub archived_rows: u64,
    /// The segment written, when any rows moved.
    pub segment: Option<String>,
}

/// The cold tier rooted at one directory (the tenant's own; it travels with the tenant).
pub struct LedgerArchive {
    dir: PathBuf,
}

impl LedgerArchive {
    #[must_use]
    pub fn new(dir: impl Into<PathBuf>) -> LedgerArchive {
        LedgerArchive { dir: dir.into() }
    }

    fn manifest_path(&self) -> PathBuf {
        self.dir.join("MANIFEST")
    }

    /// Append one content-addressed segment and record it in the manifest. Idempotent: the same
    /// rows are the same bytes, the same name, and a duplicate manifest line deduplicates on
    /// read. Returns the segment name, or `None` for an empty set.
    pub fn append(&self, rows: &BTreeSet<ArchivedRow>) -> Result<Option<String>, TaintError> {
        if rows.is_empty() {
            return Ok(None);
        }
        std::fs::create_dir_all(&self.dir).map_err(|error| TaintError::Archive {
            reason: format!("creating {}: {error}", self.dir.display()),
        })?;
        let content =
            serde_json::to_vec_pretty(&rows.iter().collect::<Vec<_>>()).map_err(|error| {
                TaintError::Archive {
                    reason: format!("encoding segment: {error}"),
                }
            })?;
        let name = format!("{}.json", blake3::hash(&content).to_hex());
        let path = self.dir.join(&name);
        if !path.exists() {
            let tmp = self.dir.join(format!("{name}.tmp"));
            std::fs::write(&tmp, &content).map_err(|error| TaintError::Archive {
                reason: format!("writing {}: {error}", tmp.display()),
            })?;
            std::fs::rename(&tmp, &path).map_err(|error| TaintError::Archive {
                reason: format!("publishing {}: {error}", path.display()),
            })?;
        }
        // The manifest line lands strictly after the segment is durable; the hot retraction
        // lands strictly after this returns. A crash between any two steps replays cleanly.
        let mut manifest = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.manifest_path())
            .map_err(|error| TaintError::Archive {
                reason: format!("opening manifest: {error}"),
            })?;
        writeln!(manifest, "{name}").map_err(|error| TaintError::Archive {
            reason: format!("appending manifest: {error}"),
        })?;
        manifest.sync_all().map_err(|error| TaintError::Archive {
            reason: format!("syncing manifest: {error}"),
        })?;
        Ok(Some(name))
    }

    /// Every archived row for one source. Reads the manifest, verifies every named segment
    /// exists and hashes to its own name, and refuses by name otherwise — a smaller union must
    /// never be served silently.
    pub fn rows_for(
        &self,
        system: &str,
        record: &str,
    ) -> Result<BTreeSet<ContaminatedRow>, TaintError> {
        Ok(self
            .all_rows()?
            .into_iter()
            .filter(|row| row.source_system == system && row.source_record == record)
            .map(|row| ContaminatedRow {
                branch: row.branch,
                table: row.table,
                row_key_hex: row.row_key_hex,
                envelope_hex: row.envelope_hex,
            })
            .collect())
    }

    /// Every archived row, integrity-checked against the manifest.
    pub fn all_rows(&self) -> Result<BTreeSet<ArchivedRow>, TaintError> {
        let manifest = match std::fs::read_to_string(self.manifest_path()) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(BTreeSet::new())
            }
            Err(error) => {
                return Err(TaintError::Archive {
                    reason: format!("reading manifest: {error}"),
                })
            }
        };
        let names: BTreeSet<&str> = manifest
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .collect();
        let mut rows = BTreeSet::new();
        for name in names {
            let path = self.dir.join(name);
            let content = std::fs::read(&path).map_err(|error| TaintError::Archive {
                reason: format!(
                    "archive segment {name} is named by the manifest and cannot be read \
                     ({error}): refusing to serve a smaller union than the ledger promised"
                ),
            })?;
            let hash = format!("{}.json", blake3::hash(&content).to_hex());
            if hash != name {
                return Err(TaintError::Archive {
                    reason: format!(
                        "archive segment {name} does not hash to its own name: the content is \
                         not what was archived"
                    ),
                });
            }
            let segment: Vec<ArchivedRow> =
                serde_json::from_slice(&content).map_err(|error| TaintError::Archive {
                    reason: format!("archive segment {name} does not decode: {error}"),
                })?;
            rows.extend(segment);
        }
        Ok(rows)
    }

    /// Rows grouped by source — the report generator's view of the cold tier.
    pub fn rows_by_source(
        &self,
    ) -> Result<BTreeMap<(String, String), BTreeSet<ContaminatedRow>>, TaintError> {
        let mut by_source: BTreeMap<(String, String), BTreeSet<ContaminatedRow>> = BTreeMap::new();
        for row in self.all_rows()? {
            by_source
                .entry((row.source_system.clone(), row.source_record.clone()))
                .or_default()
                .insert(ContaminatedRow {
                    branch: row.branch,
                    table: row.table,
                    row_key_hex: row.row_key_hex,
                    envelope_hex: row.envelope_hex,
                });
        }
        Ok(by_source)
    }
}

/// A quick existence probe used by hosts deciding whether the cold tier participates.
pub fn archive_of(dir: &Path) -> LedgerArchive {
    LedgerArchive::new(dir)
}
