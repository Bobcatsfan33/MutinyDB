//! MutinyDB's one write boundary: a substrate commit plus a Loom envelope becomes one Schweep
//! epoch. The bridge owns the epoch clock and is deliberately the only product crate allowed to
//! append to the compute log.

use loom_core::{BranchId, SourceRef, TenantId, WriteEnvelope};
use schweep_log::{Ack, Batch, FaultInjector, Log};
use schweep_zset::{Row, Value};
use std::collections::{BTreeMap, BTreeSet};
use substrate_pager::{LogicalPageNo, Manifest, ManifestId};

/// The table that makes provenance an ordinary maintained relation.
pub const DERIVATION_TABLE: &str = "mutiny_derivation";

/// A stable reference to the canonical bytes of a Loom write envelope.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EnvelopeId([u8; 32]);

impl EnvelopeId {
    /// Derive the identifier from the exact bytes the envelope signs.
    #[must_use]
    pub fn of(envelope: &WriteEnvelope) -> Self {
        Self(*blake3::hash(&envelope.signing_bytes()).as_bytes())
    }

    /// Lowercase hexadecimal, suitable for a durable relation value.
    #[must_use]
    pub fn to_hex(self) -> String {
        hex(&self.0)
    }
}

/// The trust-plane admission hook. There is no bridge API that omits it.
///
/// Production implementations resolve the actor's registered verification key, verify the
/// signature, and prove the envelope is durably addressable by `id` before returning success.
pub trait EnvelopeAuthority {
    /// Admit this exact envelope or return a stable, operator-facing reason for refusal.
    fn admit(&self, id: EnvelopeId, envelope: &WriteEnvelope) -> Result<(), String>;
}

/// One logical row change captured by the writer before its substrate transaction commits.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CapturedChange {
    /// The compute-plane row.
    pub row: Row,
    /// Z-set weight. Updates are represented by a `-1` old row and `+1` new row.
    pub weight: i64,
    /// Canonical primary-key bytes, used by the derivation relation.
    pub primary_key: Vec<u8>,
    /// Physical pages this logical change explains.
    pub pages: BTreeSet<LogicalPageNo>,
}

/// All changes for one table in one storage commit.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CapturedTable {
    /// Deterministically sorted by the bridge before admission.
    pub changes: Vec<CapturedChange>,
}

/// The atomic handoff from the storage writer to the compute plane.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitCapture {
    pub tenant: TenantId,
    pub plane: String,
    pub commit: ManifestId,
    /// Dense within one tenant store. This becomes the Schweep epoch.
    pub commit_seq: u64,
    pub manifest: Manifest,
    pub branch: BranchId,
    /// Mandatory by type; the authority still verifies that the envelope is real and resolvable.
    pub envelope: WriteEnvelope,
    /// The actual keys in the substrate transaction's write set.
    pub physical_pages: BTreeSet<LogicalPageNo>,
    /// Logical capture keyed in canonical table order.
    pub tables: BTreeMap<String, CapturedTable>,
}

/// The exact wire record represented by one compute-log append.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BridgeDelta {
    pub tenant: TenantId,
    pub commit: ManifestId,
    pub commit_seq: u64,
    pub parent_commit: Option<ManifestId>,
    pub branch: BranchId,
    pub schema_version: u32,
    pub table: String,
    pub entries: Vec<(Row, i64)>,
    pub envelope: EnvelopeId,
    pub derived_from: Vec<SourceRef>,
    pub source_id: String,
    pub dedup_token: String,
}

/// Whether a call created the epoch or proved an already-sealed retry was identical.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApplyDisposition {
    Applied,
    Replayed,
}

/// Durable admission receipt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApplyReceipt {
    pub commit: ManifestId,
    pub epoch: u64,
    pub envelope: EnvelopeId,
    pub disposition: ApplyDisposition,
    pub appended_batches: usize,
    pub replayed_batches: usize,
}

#[derive(Debug, thiserror::Error)]
pub enum BridgeError {
    #[error("{field} must be non-empty and may not contain '/' or an ASCII control character")]
    InvalidIdentifier { field: &'static str },
    #[error("the Loom envelope is structurally incomplete")]
    InvalidEnvelope,
    #[error("the envelope names branch {envelope:?}, but the commit names {commit:?}")]
    BranchMismatch { envelope: String, commit: String },
    #[error("envelope {id} was refused by the trust plane: {reason}")]
    EnvelopeRefused { id: String, reason: String },
    #[error("the declared commit id does not equal the substrate manifest's content address")]
    ManifestIdMismatch,
    #[error("commit sequence must be non-zero")]
    ZeroSequence,
    #[error("commit {commit} captured no table changes")]
    EmptyCommit { commit: String },
    #[error("table {table:?} has no captured row changes")]
    EmptyTable { table: String },
    #[error("table {table:?} contains a zero-weight change")]
    ZeroWeight { table: String },
    #[error("physical/logical completeness audit failed; unexplained={unexplained:?}, phantom={phantom:?}")]
    AuditMismatch {
        unexplained: Vec<LogicalPageNo>,
        phantom: Vec<LogicalPageNo>,
    },
    #[error("commit sequence gap: the compute log expects {expected}, capture supplied {found}")]
    SequenceGap { expected: u64, found: u64 },
    #[error("epoch {epoch} was already sealed but its retained batches do not equal this replay")]
    ReplayMismatch { epoch: u64 },
    #[error("epoch {epoch} cannot be verified because its records were compacted")]
    ReplayCompacted { epoch: u64 },
    #[error("the compute log has a pending batch that does not belong to commit {commit}")]
    ForeignPendingBatch { commit: String },
    #[error(transparent)]
    Log(#[from] schweep_log::LogError),
}

/// Prepare and atomically advance the commit-to-epoch boundary.
///
/// A process failure can leave durable appends pending or a durable seal unacknowledged. Retrying
/// the same capture completes the former and proves the latter without producing a second epoch.
pub fn apply_commit(
    log: &mut Log,
    capture: &CommitCapture,
    authority: &impl EnvelopeAuthority,
    faults: &mut FaultInjector,
) -> Result<ApplyReceipt, BridgeError> {
    let prepared = prepare(capture, authority)?;
    let sealed = log.sealed_epoch();

    if capture.commit_seq <= sealed {
        if capture.commit_seq <= log.retained_from() {
            return Err(BridgeError::ReplayCompacted {
                epoch: capture.commit_seq,
            });
        }
        let existing = log.epoch(capture.commit_seq)?;
        if existing != prepared.batches {
            return Err(BridgeError::ReplayMismatch {
                epoch: capture.commit_seq,
            });
        }
        return Ok(ApplyReceipt {
            commit: capture.commit,
            epoch: capture.commit_seq,
            envelope: prepared.envelope,
            disposition: ApplyDisposition::Replayed,
            appended_batches: 0,
            replayed_batches: existing.len(),
        });
    }

    let expected = sealed + 1;
    if capture.commit_seq != expected {
        return Err(BridgeError::SequenceGap {
            expected,
            found: capture.commit_seq,
        });
    }

    // Validate all rows before the first durable write. Known tokens run first: if a crash left a
    // partial commit pending, a conflicting replay is rejected before any missing suffix is added.
    validate_against_catalog(log, &prepared.batches)?;
    if log
        .pending_batches()
        .iter()
        .any(|pending| !prepared.batches.iter().any(|expected| expected == pending))
    {
        return Err(BridgeError::ForeignPendingBatch {
            commit: capture.commit.to_hex(),
        });
    }
    let known: BTreeSet<&str> = log.tokens().collect();
    let mut ordered: Vec<&Batch> = prepared.batches.iter().collect();
    ordered.sort_by_key(|batch| (!known.contains(batch.dedup_token.as_str()), &batch.table));

    let mut appended = 0usize;
    let mut replayed = 0usize;
    for batch in ordered {
        match log.append(
            &batch.source_id,
            &batch.table,
            batch.entries.clone(),
            &batch.dedup_token,
            faults,
        )? {
            Ack::Appended => appended += 1,
            Ack::DroppedAsReplay => replayed += 1,
        }
    }

    let epoch = log.seal_epoch(faults)?;
    if epoch != capture.commit_seq {
        return Err(BridgeError::SequenceGap {
            expected: capture.commit_seq,
            found: epoch,
        });
    }

    Ok(ApplyReceipt {
        commit: capture.commit,
        epoch,
        envelope: prepared.envelope,
        disposition: ApplyDisposition::Applied,
        appended_batches: appended,
        replayed_batches: replayed,
    })
}

#[derive(Debug)]
struct Prepared {
    envelope: EnvelopeId,
    batches: Vec<Batch>,
}

fn prepare(
    capture: &CommitCapture,
    authority: &impl EnvelopeAuthority,
) -> Result<Prepared, BridgeError> {
    validate_identifier("tenant", capture.tenant.as_str())?;
    validate_identifier("plane", &capture.plane)?;
    validate_identifier("branch", capture.branch.as_str())?;
    if capture.commit_seq == 0 {
        return Err(BridgeError::ZeroSequence);
    }
    if !capture.envelope.is_valid() {
        return Err(BridgeError::InvalidEnvelope);
    }
    if capture.envelope.branch != capture.branch {
        return Err(BridgeError::BranchMismatch {
            envelope: capture.envelope.branch.as_str().to_owned(),
            commit: capture.branch.as_str().to_owned(),
        });
    }
    if capture
        .manifest
        .id()
        .map_err(|_| BridgeError::ManifestIdMismatch)?
        != capture.commit
    {
        return Err(BridgeError::ManifestIdMismatch);
    }
    if capture.tables.is_empty() {
        return Err(BridgeError::EmptyCommit {
            commit: capture.commit.to_hex(),
        });
    }

    let envelope = EnvelopeId::of(&capture.envelope);
    authority
        .admit(envelope, &capture.envelope)
        .map_err(|reason| BridgeError::EnvelopeRefused {
            id: envelope.to_hex(),
            reason,
        })?;

    let mut explained = BTreeSet::new();
    let mut sources = capture.envelope.derived_from.clone();
    sources.sort();
    sources.dedup();
    let commit_hex = capture.commit.to_hex();
    let envelope_hex = envelope.to_hex();
    let mut batches = BTreeMap::new();
    let mut derivation = Vec::new();

    for (table, captured) in &capture.tables {
        validate_identifier("table", table)?;
        if table == DERIVATION_TABLE {
            return Err(BridgeError::InvalidIdentifier { field: "table" });
        }
        if captured.changes.is_empty() {
            return Err(BridgeError::EmptyTable {
                table: table.clone(),
            });
        }
        let mut entries = Vec::with_capacity(captured.changes.len());
        for change in &captured.changes {
            if change.weight == 0 {
                return Err(BridgeError::ZeroWeight {
                    table: table.clone(),
                });
            }
            explained.extend(change.pages.iter().copied());
            entries.push((change.row.clone(), change.weight));
            for source in &sources {
                derivation.push((
                    Row::new(vec![
                        Value::Str(capture.tenant.as_str().to_owned()),
                        Value::Str(capture.branch.as_str().to_owned()),
                        Value::Str(table.clone()),
                        Value::Str(hex(&change.primary_key)),
                        Value::Str(source.system.clone()),
                        Value::Str(source.record_id.clone()),
                        Value::Str(envelope_hex.clone()),
                    ]),
                    change.weight,
                ));
            }
        }
        entries.sort();
        let token = format!("{commit_hex}/{table}");
        batches.insert(
            table.clone(),
            Batch {
                source_id: format!("{}/{}/{}", capture.tenant.as_str(), capture.plane, table),
                dedup_token: token,
                table: table.clone(),
                entries,
            },
        );
    }

    let unexplained = capture
        .physical_pages
        .difference(&explained)
        .copied()
        .collect::<Vec<_>>();
    let phantom = explained
        .difference(&capture.physical_pages)
        .copied()
        .collect::<Vec<_>>();
    if !unexplained.is_empty() || !phantom.is_empty() {
        return Err(BridgeError::AuditMismatch {
            unexplained,
            phantom,
        });
    }

    derivation.sort();
    batches.insert(
        DERIVATION_TABLE.to_owned(),
        Batch {
            source_id: format!("{}/trust/{DERIVATION_TABLE}", capture.tenant.as_str()),
            dedup_token: format!("{commit_hex}/{DERIVATION_TABLE}"),
            table: DERIVATION_TABLE.to_owned(),
            entries: derivation,
        },
    );

    Ok(Prepared {
        envelope,
        batches: batches.into_values().collect(),
    })
}

fn validate_against_catalog(log: &Log, batches: &[Batch]) -> Result<(), BridgeError> {
    for batch in batches {
        let schema = log
            .catalog()
            .get(&batch.table)
            .ok_or_else(|| schweep_log::LogError::UnknownTable(batch.table.clone()))?;
        for (row, _) in &batch.entries {
            if row.len() != schema.len() {
                return Err(
                    schweep_log::LogError::ZSet(schweep_zset::ZSetError::ArityMismatch {
                        expected: schema.len(),
                        found: row.len(),
                    })
                    .into(),
                );
            }
            for (index, value) in row.values().iter().enumerate() {
                schema
                    .check_value(index, value)
                    .map_err(schweep_log::LogError::ZSet)?;
            }
        }
    }
    Ok(())
}

fn validate_identifier(field: &'static str, value: &str) -> Result<(), BridgeError> {
    if value.trim().is_empty()
        || value.contains('/')
        || value.bytes().any(|byte| byte.is_ascii_control())
    {
        return Err(BridgeError::InvalidIdentifier { field });
    }
    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(output, "{byte:02x}");
    }
    output
}
