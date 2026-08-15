//! MutinyDB's one write boundary: a substrate commit plus a Loom envelope becomes one Schweep
//! epoch. The bridge owns the epoch clock and is deliberately the only product crate allowed to
//! append to the compute log.

use bincode::Options as _;
use loom_core::{
    ActorId, BranchId, PolicyDecisionId, SessionId, SourceRef, TenantId, WriteEnvelope,
};
use schweep_log::{Ack, Batch, FaultInjector, Log};
use schweep_zset::{DataType, Field, Row, Schema, Value};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use substrate_pager::{LogicalPageNo, Manifest, ManifestId, PageStore, PagerError, Txn};
use substrate_wal::{DurableStore, WalError};

/// The table that makes provenance an ordinary maintained relation.
pub const DERIVATION_TABLE: &str = "mutiny_derivation";

/// The substrate logical page reserved for the durable bridge outbox.
///
/// Every MutinyDB write goes through [`commit_with_capture`], so application schemas can never
/// allocate this page. Each manifest keeps its own content-addressed version, making the capture
/// recoverable as-of the exact commit even after later writes replace the head's version.
pub const CAPTURE_PAGE: LogicalPageNo = LogicalPageNo::MAX;

const CAPTURE_MAGIC: [u8; 8] = *b"MTNYCAP1";
const MAX_CAPTURE_BYTES: u64 = 1024 * 1024;

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

/// A logical write set before substrate assigns its content-addressed manifest id.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitDraft {
    pub tenant: TenantId,
    pub plane: String,
    pub commit_seq: u64,
    pub branch: BranchId,
    pub envelope: WriteEnvelope,
    pub tables: BTreeMap<String, CapturedTable>,
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
    #[error("logical page {page} is reserved for MutinyDB's durable capture outbox")]
    CapturePageReserved { page: LogicalPageNo },
    #[error("the durable capture could not be encoded: {reason}")]
    CaptureEncode { reason: String },
    #[error("the compute catalog is incompatible with M1 admission: {reason}")]
    CatalogMismatch { reason: String },
    #[error("the durable capture at commit {commit} is missing or malformed: {reason}")]
    CaptureDecode { commit: String, reason: String },
    #[error("substrate refused the captured commit: {reason}")]
    Storage { reason: String },
    #[error(
        "cannot start MutinyDB capture at non-empty parent {commit} ({page_count} pages); run an explicit bootstrap migration"
    )]
    UncapturedParent { commit: String, page_count: u64 },
    #[error(transparent)]
    Log(#[from] schweep_log::LogError),
}

/// Commit the application pages and their complete logical capture in one substrate transaction.
///
/// This function consumes `txn` immediately after writing the reserved outbox page. No caller can
/// add an unaccounted write between capture and commit. A crash after substrate's commit point but
/// before [`apply_commit`] is recovered with [`recover_capture`].
pub fn commit_with_capture(
    store: &DurableStore,
    mut txn: Txn,
    draft: &CommitDraft,
    catalog: &BTreeMap<String, Schema>,
    authority: &impl EnvelopeAuthority,
) -> Result<CommitCapture, BridgeError> {
    if txn.writes().contains_key(&CAPTURE_PAGE) {
        return Err(BridgeError::CapturePageReserved { page: CAPTURE_PAGE });
    }
    validate_draft(draft, authority)?;
    validate_draft_against_catalog(draft, catalog)?;
    validate_parent_sequence(store, txn.base(), draft.commit_seq)?;
    let explained = explained_pages(&draft.tables)?;
    let physical = txn.writes().keys().copied().collect::<BTreeSet<_>>();
    audit_pages(&physical, &explained)?;

    let bytes = encode_draft(draft)?;
    store
        .write(&mut txn, CAPTURE_PAGE, bytes)
        .map_err(|error| BridgeError::Storage {
            reason: error.to_string(),
        })?;
    let physical_pages = txn.writes().keys().copied().collect();
    let commit = store.commit(txn).map_err(|error| BridgeError::Storage {
        reason: error.to_string(),
    })?;
    materialize_capture(store, commit, draft.clone(), physical_pages)
}

/// The fixed M1 provenance-relation schema. Catalog creation and recovery use the same function so
/// a column reorder cannot make a storage commit permanently unbridgeable.
pub fn derivation_schema() -> Result<Schema, BridgeError> {
    Schema::new_table(vec![
        Field::not_null("tenant", DataType::Utf8),
        Field::not_null("branch", DataType::Utf8),
        Field::not_null("table_name", DataType::Utf8),
        Field::not_null("row_key", DataType::Utf8),
        Field::not_null("source_system", DataType::Utf8),
        Field::not_null("source_record", DataType::Utf8),
        Field::not_null("envelope", DataType::Utf8),
    ])
    .map_err(schweep_log::LogError::ZSet)
    .map_err(BridgeError::from)
}

/// Reconstruct the exact logical capture stored inside `commit`'s substrate manifest.
pub fn recover_capture(
    store: &DurableStore,
    commit: ManifestId,
) -> Result<CommitCapture, BridgeError> {
    let page = store
        .read(&commit, CAPTURE_PAGE)
        .map_err(|error| BridgeError::CaptureDecode {
            commit: commit.to_hex(),
            reason: error.to_string(),
        })?;
    let draft = decode_draft(page.as_bytes()).map_err(|reason| BridgeError::CaptureDecode {
        commit: commit.to_hex(),
        reason,
    })?;
    let mut physical_pages = explained_pages(&draft.tables)?;
    physical_pages.insert(CAPTURE_PAGE);
    materialize_capture(store, commit, draft, physical_pages)
}

/// Recover every storage commit newer than the compute plane's sealed epoch, oldest first.
///
/// The manifest history is the durable queue. There is no second mutable checkpoint that can
/// disagree with it: callers apply this returned vector in order and Schweep's own sealed epoch is
/// the consumer offset.
pub fn recover_pending_captures(
    store: &DurableStore,
    head: ManifestId,
    sealed_epoch: u64,
) -> Result<Vec<CommitCapture>, BridgeError> {
    let mut current = head;
    let mut pending = Vec::new();
    loop {
        let manifest = store
            .pager()
            .manifest(&current)
            .map_err(|error| BridgeError::Storage {
                reason: error.to_string(),
            })?;
        let Some(parent) = manifest.parent else {
            break;
        };
        let capture = recover_capture(store, current)?;
        if capture.commit_seq <= sealed_epoch {
            break;
        }
        pending.push(capture);
        current = parent;
    }
    pending.reverse();
    for (expected, capture) in (sealed_epoch + 1..).zip(&pending) {
        if capture.commit_seq != expected {
            return Err(BridgeError::SequenceGap {
                expected,
                found: capture.commit_seq,
            });
        }
    }
    Ok(pending)
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

    let mut application_pages = capture.physical_pages.clone();
    application_pages.remove(&CAPTURE_PAGE);
    let unexplained = application_pages
        .difference(&explained)
        .copied()
        .collect::<Vec<_>>();
    let phantom = explained
        .difference(&application_pages)
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

fn validate_draft(
    draft: &CommitDraft,
    authority: &impl EnvelopeAuthority,
) -> Result<(), BridgeError> {
    validate_identifier("tenant", draft.tenant.as_str())?;
    validate_identifier("plane", &draft.plane)?;
    validate_identifier("branch", draft.branch.as_str())?;
    if draft.commit_seq == 0 {
        return Err(BridgeError::ZeroSequence);
    }
    if !draft.envelope.is_valid() {
        return Err(BridgeError::InvalidEnvelope);
    }
    if draft.envelope.branch != draft.branch {
        return Err(BridgeError::BranchMismatch {
            envelope: draft.envelope.branch.as_str().to_owned(),
            commit: draft.branch.as_str().to_owned(),
        });
    }
    if draft.tables.is_empty() {
        return Err(BridgeError::EmptyCommit {
            commit: "not-yet-assigned".to_owned(),
        });
    }
    for (table, captured) in &draft.tables {
        validate_identifier("table", table)?;
        if table == DERIVATION_TABLE {
            return Err(BridgeError::InvalidIdentifier { field: "table" });
        }
        if captured.changes.is_empty() {
            return Err(BridgeError::EmptyTable {
                table: table.clone(),
            });
        }
        if captured.changes.iter().any(|change| change.weight == 0) {
            return Err(BridgeError::ZeroWeight {
                table: table.clone(),
            });
        }
    }
    let envelope = EnvelopeId::of(&draft.envelope);
    authority
        .admit(envelope, &draft.envelope)
        .map_err(|reason| BridgeError::EnvelopeRefused {
            id: envelope.to_hex(),
            reason,
        })
}

fn validate_parent_sequence(
    store: &DurableStore,
    parent: ManifestId,
    found: u64,
) -> Result<(), BridgeError> {
    match store.read(&parent, CAPTURE_PAGE) {
        Ok(page) => {
            let prior =
                decode_draft(page.as_bytes()).map_err(|reason| BridgeError::CaptureDecode {
                    commit: parent.to_hex(),
                    reason,
                })?;
            let expected = prior.commit_seq + 1;
            if found != expected {
                return Err(BridgeError::SequenceGap { expected, found });
            }
            Ok(())
        }
        Err(WalError::Pager(PagerError::PageNotFound { .. })) if found == 1 => {
            let manifest =
                store
                    .pager()
                    .manifest(&parent)
                    .map_err(|error| BridgeError::Storage {
                        reason: error.to_string(),
                    })?;
            if manifest.page_count == 0 {
                Ok(())
            } else {
                Err(BridgeError::UncapturedParent {
                    commit: parent.to_hex(),
                    page_count: manifest.page_count,
                })
            }
        }
        Err(WalError::Pager(PagerError::PageNotFound { .. })) => {
            Err(BridgeError::SequenceGap { expected: 1, found })
        }
        Err(error) => Err(BridgeError::Storage {
            reason: error.to_string(),
        }),
    }
}

fn validate_draft_against_catalog(
    draft: &CommitDraft,
    catalog: &BTreeMap<String, Schema>,
) -> Result<(), BridgeError> {
    let declared_derivation = catalog
        .get(DERIVATION_TABLE)
        .ok_or_else(|| schweep_log::LogError::UnknownTable(DERIVATION_TABLE.to_owned()))?;
    if declared_derivation != &derivation_schema()? {
        return Err(BridgeError::CatalogMismatch {
            reason: format!("catalog schema for {DERIVATION_TABLE:?} is not the fixed M1 schema"),
        });
    }
    for (table, captured) in &draft.tables {
        let schema = catalog
            .get(table)
            .ok_or_else(|| schweep_log::LogError::UnknownTable(table.clone()))?;
        for change in &captured.changes {
            if change.row.len() != schema.len() {
                return Err(
                    schweep_log::LogError::ZSet(schweep_zset::ZSetError::ArityMismatch {
                        expected: schema.len(),
                        found: change.row.len(),
                    })
                    .into(),
                );
            }
            for (index, value) in change.row.values().iter().enumerate() {
                schema
                    .check_value(index, value)
                    .map_err(schweep_log::LogError::ZSet)?;
            }
        }
    }
    Ok(())
}

fn explained_pages(
    tables: &BTreeMap<String, CapturedTable>,
) -> Result<BTreeSet<LogicalPageNo>, BridgeError> {
    let mut pages = BTreeSet::new();
    for (table, captured) in tables {
        if captured.changes.is_empty() {
            return Err(BridgeError::EmptyTable {
                table: table.clone(),
            });
        }
        for change in &captured.changes {
            pages.extend(change.pages.iter().copied());
        }
    }
    Ok(pages)
}

fn audit_pages(
    physical: &BTreeSet<LogicalPageNo>,
    explained: &BTreeSet<LogicalPageNo>,
) -> Result<(), BridgeError> {
    let unexplained = physical.difference(explained).copied().collect::<Vec<_>>();
    let phantom = explained.difference(physical).copied().collect::<Vec<_>>();
    if unexplained.is_empty() && phantom.is_empty() {
        Ok(())
    } else {
        Err(BridgeError::AuditMismatch {
            unexplained,
            phantom,
        })
    }
}

fn materialize_capture(
    store: &DurableStore,
    commit: ManifestId,
    draft: CommitDraft,
    physical_pages: BTreeSet<LogicalPageNo>,
) -> Result<CommitCapture, BridgeError> {
    let manifest = store
        .pager()
        .manifest(&commit)
        .map_err(|error| BridgeError::Storage {
            reason: error.to_string(),
        })?;
    Ok(CommitCapture {
        tenant: draft.tenant,
        plane: draft.plane,
        commit,
        commit_seq: draft.commit_seq,
        manifest,
        branch: draft.branch,
        envelope: draft.envelope,
        physical_pages,
        tables: draft.tables,
    })
}

#[derive(Debug, Serialize, Deserialize)]
struct CaptureWire {
    magic: [u8; 8],
    tenant: String,
    plane: String,
    commit_seq: u64,
    branch: String,
    envelope: EnvelopeWire,
    tables: BTreeMap<String, TableWire>,
}

#[derive(Debug, Serialize, Deserialize)]
struct EnvelopeWire {
    actor: String,
    session: String,
    branch: String,
    context_hash: [u8; 32],
    delegation: Vec<String>,
    derived_from: Vec<SourceWire>,
    intent: String,
    policy: Option<String>,
    signature: Vec<u8>,
}

#[derive(Debug, Serialize, Deserialize)]
struct SourceWire {
    system: String,
    record_id: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct TableWire {
    changes: Vec<ChangeWire>,
}

#[derive(Debug, Serialize, Deserialize)]
struct ChangeWire {
    row: Vec<ValueWire>,
    weight: i64,
    primary_key: Vec<u8>,
    pages: Vec<LogicalPageNo>,
}

#[derive(Debug, Serialize, Deserialize)]
enum ValueWire {
    Null,
    Int(i64),
    Str(String),
    Bool(bool),
    Float(u64),
}

fn encode_draft(draft: &CommitDraft) -> Result<Vec<u8>, BridgeError> {
    let wire = CaptureWire {
        magic: CAPTURE_MAGIC,
        tenant: draft.tenant.as_str().to_owned(),
        plane: draft.plane.clone(),
        commit_seq: draft.commit_seq,
        branch: draft.branch.as_str().to_owned(),
        envelope: EnvelopeWire {
            actor: draft.envelope.actor.as_str().to_owned(),
            session: draft.envelope.session.as_str().to_owned(),
            branch: draft.envelope.branch.as_str().to_owned(),
            context_hash: draft.envelope.context_hash,
            delegation: draft
                .envelope
                .delegation
                .iter()
                .map(|actor| actor.as_str().to_owned())
                .collect(),
            derived_from: draft
                .envelope
                .derived_from
                .iter()
                .map(|source| SourceWire {
                    system: source.system.clone(),
                    record_id: source.record_id.clone(),
                })
                .collect(),
            intent: draft.envelope.intent.clone(),
            policy: draft
                .envelope
                .policy
                .as_ref()
                .map(|policy| policy.as_str().to_owned()),
            signature: draft.envelope.signature.clone(),
        },
        tables: draft
            .tables
            .iter()
            .map(|(name, table)| {
                let wire = TableWire {
                    changes: table
                        .changes
                        .iter()
                        .map(|change| ChangeWire {
                            row: change.row.values().iter().map(value_to_wire).collect(),
                            weight: change.weight,
                            primary_key: change.primary_key.clone(),
                            pages: change.pages.iter().copied().collect(),
                        })
                        .collect(),
                };
                (name.clone(), wire)
            })
            .collect(),
    };
    capture_codec()
        .serialize(&wire)
        .map_err(|error| BridgeError::CaptureEncode {
            reason: error.to_string(),
        })
}

fn decode_draft(bytes: &[u8]) -> Result<CommitDraft, String> {
    let wire: CaptureWire = capture_codec()
        .deserialize(bytes)
        .map_err(|error| error.to_string())?;
    if wire.magic != CAPTURE_MAGIC {
        return Err("capture magic/version is not MTNYCAP1".to_owned());
    }
    let envelope = WriteEnvelope {
        actor: ActorId::new(wire.envelope.actor),
        session: SessionId::new(wire.envelope.session),
        branch: BranchId::new(wire.envelope.branch),
        context_hash: wire.envelope.context_hash,
        delegation: wire
            .envelope
            .delegation
            .into_iter()
            .map(ActorId::new)
            .collect(),
        derived_from: wire
            .envelope
            .derived_from
            .into_iter()
            .map(|source| SourceRef::new(source.system, source.record_id))
            .collect(),
        intent: wire.envelope.intent,
        policy: wire.envelope.policy.map(PolicyDecisionId::new),
        signature: wire.envelope.signature,
    };
    let tables = wire
        .tables
        .into_iter()
        .map(|(name, table)| {
            let changes = table
                .changes
                .into_iter()
                .map(|change| CapturedChange {
                    row: Row::new(change.row.into_iter().map(value_from_wire).collect()),
                    weight: change.weight,
                    primary_key: change.primary_key,
                    pages: change.pages.into_iter().collect(),
                })
                .collect();
            (name, CapturedTable { changes })
        })
        .collect();
    Ok(CommitDraft {
        tenant: TenantId::new(wire.tenant),
        plane: wire.plane,
        commit_seq: wire.commit_seq,
        branch: BranchId::new(wire.branch),
        envelope,
        tables,
    })
}

fn capture_codec() -> impl bincode::Options {
    bincode::DefaultOptions::new()
        .with_fixint_encoding()
        .with_limit(MAX_CAPTURE_BYTES)
        .reject_trailing_bytes()
}

fn value_to_wire(value: &Value) -> ValueWire {
    match value {
        Value::Null => ValueWire::Null,
        Value::Int(value) => ValueWire::Int(*value),
        Value::Str(value) => ValueWire::Str(value.clone()),
        Value::Bool(value) => ValueWire::Bool(*value),
        Value::Float(value) => ValueWire::Float(value.to_bits()),
    }
}

fn value_from_wire(value: ValueWire) -> Value {
    match value {
        ValueWire::Null => Value::Null,
        ValueWire::Int(value) => Value::Int(value),
        ValueWire::Str(value) => Value::Str(value),
        ValueWire::Bool(value) => Value::Bool(value),
        ValueWire::Float(value) => Value::Float(f64::from_bits(value)),
    }
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(output, "{byte:02x}");
    }
    output
}
