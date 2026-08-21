//! Cross-circuit taint: taint-as-retraction, composed across every plane (K-2, M4).
//!
//! `taint(S)` resolves what is downstream of a poisoned source through the `mutiny_derivation`
//! standing relation — a query, never an in-memory DAG walk — journals the resolved set to the
//! append-only `mutiny_taint_ledger`, heals the volatile branch-scoped semantic plane through the
//! host's [`SemanticHealer`], and then retracts the contamination from every payload channel with
//! Schweep's C11 `retract_source(source, table, predicate)` through the ordinary delta path. The
//! derivation channel is retracted last, because the edges are the resolution witness a resumed
//! taint needs. The [`RecallReport`] keeps Loom's two-section law unmodified: irreversible external
//! actions first, with receipts and registered compensations; the reversible section says
//! *already healed*, because the engine did it. `docs/M4-TAINT.md` is the contract.

use loom_core::{ExecutedAction, IrreversibleItem, SourceRef};
use mutiny_bridge::DERIVATION_TABLE;
use schweep_log::record::{read_framed, Record};
use schweep_log::{Ack, Epoch};
use schweep_memo::Admission;
use schweep_server::{Engine, RetractionReceipt, ServerError};
use schweep_zset::{DataType, Field, Row, Schema, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

pub mod archive;
pub use archive::{ArchiveStats, ArchivedRow, LedgerArchive};

/// The append-only taint journal: an ordinary relation on the ordinary epoch clock. It is what
/// keeps the report regenerable after the heal has deleted its own evidence from the derivation
/// relation, and it is never retracted.
pub const LEDGER_TABLE: &str = "mutiny_taint_ledger";

/// The reserved internal source system: a row derived from another MutinyDB row cites
/// `SourceRef { system: "mutiny", record_id: "<table>/<row_key_hex>" }`, and resolution follows
/// the chain with one more query per hop.
pub const INTERNAL_SOURCE_SYSTEM: &str = "mutiny";

/// Transitive resolution bound. A derivation chain this deep is a cycle or a bug, and chasing it
/// is a denial of service against ourselves — Loom's `MAX_DERIVATION_DEPTH`, for the same reason.
pub const MAX_TAINT_ROUNDS: usize = 64;

/// The most equality disjuncts one generated predicate or filter may carry. A bound with a name,
/// not a truncation: exceeding it is a refusal, never a silently partial heal.
pub const MAX_PREDICATE_DISJUNCTS: usize = 512;

/// How many internal-source citations one transitive resolution query names at a time.
const RESOLUTION_CHUNK: usize = 32;

/// The interruptible boundaries of the taint path, in execution order. The M4 gate kills the
/// process at every one of them and proves the resumed taint equals a never-crashed twin — the
/// same first-class fault modeling as schweep-log's `FaultInjector`, at this seam's altitude.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TaintSeam {
    AfterResolve,
    AfterJournal,
    AfterVolatileHeal,
    /// After the first payload channel's retraction sealed, before the next channel.
    BetweenPayloadChannels,
    BeforeDerivationRetraction,
    BeforeReport,
    /// Archival only (docs/M4-TAINT.md § "The archive tier"): after the segment and manifest are
    /// durable, before the hot retraction — the widest window a crash can leave both tiers
    /// holding the same rows.
    AfterArchiveAppend,
}

/// A planned interruption. Inert by default; a planned seam fires exactly once.
#[derive(Debug, Default)]
pub struct TaintFaults {
    plan: Option<TaintSeam>,
    fired: Option<TaintSeam>,
}

impl TaintFaults {
    #[must_use]
    pub fn inert() -> TaintFaults {
        TaintFaults::default()
    }

    #[must_use]
    pub fn planned(seam: TaintSeam) -> TaintFaults {
        TaintFaults {
            plan: Some(seam),
            fired: None,
        }
    }

    #[must_use]
    pub fn fired(&self) -> Option<TaintSeam> {
        self.fired
    }

    fn hit(&mut self, seam: TaintSeam) -> Result<(), TaintError> {
        if self.plan == Some(seam) {
            self.plan = None;
            self.fired = Some(seam);
            return Err(TaintError::Interrupted { seam });
        }
        Ok(())
    }
}

/// How a payload table's canonical `row_key` bytes decode back into its key column's literal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyType {
    /// The key bytes are the UTF-8 of the key column's string value.
    Utf8,
    /// The key bytes are the big-endian `i64` of the key column's integer value.
    Int64,
}

/// What taint must know about one payload table to name its rows in a predicate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaintTableSpec {
    /// The plane segment of the table's ingest channel (`<tenant>/<plane>/<table>`, MD-2 R6).
    pub plane: String,
    /// The column holding the canonical row key.
    pub key_column: String,
    /// The column holding the branch tag (MD-2 R7).
    pub branch_column: String,
    /// How `mutiny_derivation.row_key` decodes into the key column's literal.
    pub key_type: KeyType,
}

/// The taint core's view of one tenant's compute plane.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaintConfig {
    pub tenant: String,
    /// Every taint-healable payload table, by name. A contaminated row in a table absent here is
    /// a loud failure, not a skip.
    pub tables: BTreeMap<String, TaintTableSpec>,
    /// The ledger's cold tier (docs/M4-TAINT.md § "The archive tier"). `None` keeps the whole
    /// ledger hot — the pre-M8 posture, still valid for dev hosts.
    pub archive_dir: Option<std::path::PathBuf>,
}

impl TaintConfig {
    /// The fixed ledger schema. Creation and every reader use this one function, for the same
    /// reason the bridge fixes `derivation_schema`.
    pub fn ledger_schema() -> Result<Schema, TaintError> {
        Schema::new_table(vec![
            Field::not_null("source_system", DataType::Utf8),
            Field::not_null("source_record", DataType::Utf8),
            Field::not_null("branch", DataType::Utf8),
            Field::not_null("table_name", DataType::Utf8),
            Field::not_null("row_key", DataType::Utf8),
            Field::not_null("envelope", DataType::Utf8),
        ])
        .map_err(|error| TaintError::Schema {
            reason: error.to_string(),
        })
    }

    #[must_use]
    pub fn ledger_channel(&self) -> String {
        format!("{}/trust/{LEDGER_TABLE}", self.tenant)
    }

    #[must_use]
    pub fn derivation_channel(&self) -> String {
        format!("{}/trust/{DERIVATION_TABLE}", self.tenant)
    }

    #[must_use]
    pub fn payload_channel(&self, table: &str, spec: &TaintTableSpec) -> String {
        format!("{}/{}/{table}", self.tenant, spec.plane)
    }
}

/// One contaminated row, as the derivation relation names it.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ContaminatedRow {
    pub branch: String,
    pub table: String,
    pub row_key_hex: String,
    pub envelope_hex: String,
}

/// The volatile-plane heal seam (MD-1 R2's trait inversion): the host that mounts the trust plane
/// implements this over `OperatorTrustPlane::heal_semantic`. Healing an already-absent key must be
/// a skip, because a resumed taint heals the same set twice.
pub trait SemanticHealer {
    /// Retract the named row keys (decoded key-column values) on exactly this branch.
    /// Returns how many (operator, row) retractions were performed.
    fn heal(&mut self, branch: &str, table: &str, keys: &[String]) -> Result<usize, String>;
}

/// One channel's retraction outcome, straight from Schweep's C11 receipt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChannelReceipt {
    pub channel: String,
    pub table: String,
    pub receipt: RetractionReceipt,
}

/// One write the engine already corrected.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct HealedWrite {
    pub branch: String,
    pub table: String,
    pub row_key_hex: String,
    pub envelope_hex: String,
}

/// The two-section report. Loom's law, unmodified: the section we cannot fix is first, in the
/// struct and on the page; the reversible section is *done*, not proposed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecallReport {
    pub source: SourceRef,
    /// The actions that already happened in the world. First. Always.
    pub irreversible: Vec<IrreversibleItem>,
    /// The writes the engine already healed, citing the envelopes that admitted them.
    pub healed: Vec<HealedWrite>,
}

impl RecallReport {
    #[must_use]
    pub fn is_fully_reversible(&self) -> bool {
        self.irreversible.is_empty()
    }

    #[must_use]
    pub fn needs_human(&self) -> bool {
        self.irreversible
            .iter()
            .any(|item| item.compensating_action.is_none())
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.irreversible.is_empty() && self.healed.is_empty()
    }
}

impl fmt::Display for RecallReport {
    /// Written for a human having a bad day. The irreversible section is printed first and
    /// loudly, because the single worst outcome of this feature is a reader who skims a list of
    /// healed writes and never notices the account that is still suspended.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "RECALL REPORT — tainted source: {}", self.source)?;
        writeln!(
            f,
            "The reversible writes below are DONE, not proposed: the engine already healed them.\n"
        )?;

        if self.is_empty() {
            return writeln!(
                f,
                "Nothing is downstream of this source. No action is needed."
            );
        }

        if !self.irreversible.is_empty() {
            writeln!(
                f,
                "⚠  {} ACTION(S) ALREADY HAPPENED IN THE WORLD AND CANNOT BE UNDONE BY THIS DATABASE.",
                self.irreversible.len()
            )?;
            writeln!(
                f,
                "   Retraction heals writes. It does not un-suspend an account.\n"
            )?;
            for item in &self.irreversible {
                writeln!(f, "   • {} → {}", item.action_type, item.target)?;
                writeln!(f, "       requested by:  {}", item.actor)?;
                if let Some(receipt) = &item.receipt {
                    writeln!(f, "       receipt:       {receipt}")?;
                }
                match &item.compensating_action {
                    Some(action) => writeln!(f, "       CAN BE COMPENSATED: {action}")?,
                    None => writeln!(
                        f,
                        "       NO COMPENSATING ACTION EXISTS. A human must decide."
                    )?,
                }
                writeln!(f, "       → {}\n", item.escalation)?;
            }
        }

        if !self.healed.is_empty() {
            writeln!(
                f,
                "{} write(s) downstream of this source were ALREADY HEALED by the engine:\n",
                self.healed.len()
            )?;
            for item in &self.healed {
                writeln!(
                    f,
                    "   • {}/{} [{}]\n       admitted by envelope {}",
                    item.table, item.row_key_hex, item.branch, item.envelope_hex
                )?;
            }
            writeln!(f)?;
        }

        writeln!(
            f,
            "Every standing answer has already corrected itself through the ordinary delta path. \
             The irreversible section, if any, is yours."
        )
    }
}

/// What one `taint` call did.
#[derive(Clone, Debug)]
pub struct TaintOutcome {
    pub report: RecallReport,
    /// Resolution rounds run (1 = no transitive chains).
    pub resolution_rounds: usize,
    /// Rows resolved as contaminated by this call (excludes rows already healed by prior taints).
    pub resolved: usize,
    /// (operator, row) retractions the volatile semantic plane performed.
    pub semantic_healed: usize,
    /// The epoch the taint journal sealed, if this call journaled new contamination.
    pub ledger_epoch: Option<Epoch>,
    /// Per-channel C11 retraction receipts, payload channels first, derivation channel last.
    pub receipts: Vec<ChannelReceipt>,
}

#[derive(Debug, thiserror::Error)]
pub enum TaintError {
    #[error("source {value:?} may not be empty or contain quotes or control bytes")]
    InvalidSource { value: String },
    #[error("contaminated row names table {table:?}, which has no taint specification")]
    UnknownTable { table: String },
    #[error("resolution exceeded {MAX_TAINT_ROUNDS} rounds; a derivation chain this deep is a cycle or a bug")]
    RoundsExceeded,
    #[error("a generated predicate would carry {found} disjuncts, over the {MAX_PREDICATE_DISJUNCTS} bound")]
    DisjunctsExceeded { found: usize },
    #[error("row key {row_key_hex:?} in table {table:?} does not decode as its declared key type")]
    KeyDecode { table: String, row_key_hex: String },
    #[error("decoded key {value:?} cannot be named in a predicate literal")]
    KeyLiteral { value: String },
    #[error("the resolution answer frame is malformed: {reason}")]
    Frame { reason: String },
    #[error("the taint ledger schema is invalid: {reason}")]
    Schema { reason: String },
    #[error("the semantic heal failed on branch {branch}: {reason}")]
    Healer { branch: String, reason: String },
    #[error("the taint was interrupted at {seam:?}; calling taint again resumes it")]
    Interrupted { seam: TaintSeam },
    #[error("the ledger archive failed: {reason}")]
    Archive { reason: String },
    #[error(transparent)]
    Engine(#[from] ServerError),
}

/// **Taint, composed.** Resolve → journal → heal the volatile plane → retract the payload
/// channels → retract the derivation channel → report. The ordering is normative and is the
/// crash-consistency argument; `docs/M4-TAINT.md` states why. Interrupt it anywhere and calling
/// `taint` again resumes: already-journaled sets deduplicate, already-healed keys skip,
/// already-retracted contributions net to zero, and the derivation edges survive until every
/// payload heal that might need re-resolution has landed.
pub fn taint(
    engine: &mut Engine,
    config: &TaintConfig,
    source: &SourceRef,
    actions: &[ExecutedAction],
    healer: &mut dyn SemanticHealer,
) -> Result<TaintOutcome, TaintError> {
    taint_with_faults(
        engine,
        config,
        source,
        actions,
        healer,
        &mut TaintFaults::inert(),
    )
}

/// [`taint`], with planned interruptions at its named seams — the M4 crash gate's entry point.
pub fn taint_with_faults(
    engine: &mut Engine,
    config: &TaintConfig,
    source: &SourceRef,
    actions: &[ExecutedAction],
    healer: &mut dyn SemanticHealer,
    faults: &mut TaintFaults,
) -> Result<TaintOutcome, TaintError> {
    validate_literal(&source.system)?;
    validate_literal(&source.record_id)?;

    // 1. Resolve, through the standing relation.
    let (resolved, resolution_rounds) = resolve(engine, source)?;
    faults.hit(TaintSeam::AfterResolve)?;
    for row in &resolved {
        if !config.tables.contains_key(&row.table) {
            return Err(TaintError::UnknownTable {
                table: row.table.clone(),
            });
        }
    }

    // 2. Journal — before anything is healed, so the report survives its own success.
    let mut prior = ledger_rows(engine, source)?;
    if let Some(dir) = &config.archive_dir {
        // The union law: the ledger's memory is hot ∪ archive, and both tiers feed the report.
        prior.extend(LedgerArchive::new(dir).rows_for(&source.system, &source.record_id)?);
    }
    let ledger_epoch = journal(engine, config, source, &resolved)?;
    faults.hit(TaintSeam::AfterJournal)?;

    // 3. Heal the volatile plane, branch by branch, table by table.
    let mut semantic_healed = 0;
    let mut by_branch_table: BTreeMap<(&str, &str), Vec<String>> = BTreeMap::new();
    for row in &resolved {
        let spec = config
            .tables
            .get(&row.table)
            .ok_or_else(|| TaintError::UnknownTable {
                table: row.table.clone(),
            })?;
        by_branch_table
            .entry((row.branch.as_str(), row.table.as_str()))
            .or_default()
            .push(decode_key(&row.table, &row.row_key_hex, spec.key_type)?);
    }
    for ((branch, table), keys) in &by_branch_table {
        semantic_healed +=
            healer
                .heal(branch, table, keys)
                .map_err(|reason| TaintError::Healer {
                    branch: (*branch).to_owned(),
                    reason,
                })?;
    }
    faults.hit(TaintSeam::AfterVolatileHeal)?;

    // 4. Retract every payload channel, in canonical table order.
    let mut receipts = Vec::new();
    for (table, spec) in &config.tables {
        let rows: Vec<&ContaminatedRow> =
            resolved.iter().filter(|row| &row.table == table).collect();
        if rows.is_empty() {
            continue;
        }
        let predicate = payload_predicate(table, spec, &rows)?;
        let channel = config.payload_channel(table, spec);
        let receipt = engine.retract_source(&channel, Some(table), Some(&predicate))?;
        receipts.push(ChannelReceipt {
            channel,
            table: table.clone(),
            receipt,
        });
        faults.hit(TaintSeam::BetweenPayloadChannels)?;
    }
    faults.hit(TaintSeam::BeforeDerivationRetraction)?;

    // 5. Retract the derivation edges last — they are the resolution witness.
    if !resolved.is_empty() {
        let predicate = derivation_predicate(&resolved)?;
        let channel = config.derivation_channel();
        let receipt = engine.retract_source(&channel, Some(DERIVATION_TABLE), Some(&predicate))?;
        receipts.push(ChannelReceipt {
            channel,
            table: DERIVATION_TABLE.to_owned(),
            receipt,
        });
    }

    faults.hit(TaintSeam::BeforeReport)?;

    // 6. Report, from the union of this call's work and the ledger's memory of prior calls.
    let mut report_rows: BTreeSet<ContaminatedRow> = resolved.clone();
    report_rows.extend(prior);
    let report = build_report(source, actions, &report_rows);

    Ok(TaintOutcome {
        report,
        resolution_rounds,
        resolved: resolved.len(),
        semantic_healed,
        ledger_epoch,
        receipts,
    })
}

/// Everything currently downstream of `source`, by iterated queries over `mutiny_derivation`.
fn resolve(
    engine: &mut Engine,
    source: &SourceRef,
) -> Result<(BTreeSet<ContaminatedRow>, usize), TaintError> {
    let mut all: BTreeSet<ContaminatedRow> = BTreeSet::new();
    let mut frontier = derivation_rows(
        engine,
        &format!(
            "{DERIVATION_TABLE}.source_system = '{}' AND {DERIVATION_TABLE}.source_record = '{}'",
            source.system, source.record_id
        ),
    )?;
    let mut rounds = 0;
    while !frontier.is_empty() {
        rounds += 1;
        if rounds > MAX_TAINT_ROUNDS {
            return Err(TaintError::RoundsExceeded);
        }
        let fresh: Vec<ContaminatedRow> = frontier
            .into_iter()
            .filter(|row| all.insert(row.clone()))
            .collect();
        if fresh.is_empty() {
            break;
        }
        // The next hop: rows citing any freshly-contaminated row through the internal convention.
        let citations: BTreeSet<String> = fresh
            .iter()
            .map(|row| format!("{}/{}", row.table, row.row_key_hex))
            .collect();
        let mut next = Vec::new();
        let citations: Vec<String> = citations.into_iter().collect();
        for chunk in citations.chunks(RESOLUTION_CHUNK) {
            let disjuncts = chunk
                .iter()
                .map(|record| {
                    validate_literal(record)?;
                    Ok(format!("{DERIVATION_TABLE}.source_record = '{record}'"))
                })
                .collect::<Result<Vec<_>, TaintError>>()?
                .join(" OR ");
            next.extend(derivation_rows(
                engine,
                &format!(
                    "{DERIVATION_TABLE}.source_system = '{INTERNAL_SOURCE_SYSTEM}' AND ({disjuncts})"
                ),
            )?);
        }
        frontier = next;
    }
    Ok((all, rounds.max(1)))
}

/// One resolution query: register over the standing relation, read the typed answer through the
/// frame door, deregister. No rendered-text parsing — a value parser could hide a divergence.
fn derivation_rows(
    engine: &mut Engine,
    where_clause: &str,
) -> Result<Vec<ContaminatedRow>, TaintError> {
    let sql = format!(
        "SELECT {d}.branch AS branch, {d}.table_name AS table_name, {d}.row_key AS row_key, \
         {d}.envelope AS envelope FROM {d} WHERE {where_clause}",
        d = DERIVATION_TABLE
    );
    let rows = answer_rows(engine, &sql)?;
    rows.into_iter()
        .map(|row| {
            Ok(ContaminatedRow {
                branch: string_column(&row, 0)?,
                table: string_column(&row, 1)?,
                row_key_hex: string_column(&row, 2)?,
                envelope_hex: string_column(&row, 3)?,
            })
        })
        .collect()
}

/// Prior taints of this source, from the append-only journal.
fn ledger_rows(
    engine: &mut Engine,
    source: &SourceRef,
) -> Result<BTreeSet<ContaminatedRow>, TaintError> {
    let sql = format!(
        "SELECT {t}.branch AS branch, {t}.table_name AS table_name, {t}.row_key AS row_key, \
         {t}.envelope AS envelope FROM {t} WHERE {t}.source_system = '{}' AND \
         {t}.source_record = '{}'",
        source.system,
        source.record_id,
        t = LEDGER_TABLE
    );
    let rows = answer_rows(engine, &sql)?;
    rows.into_iter()
        .map(|row| {
            Ok(ContaminatedRow {
                branch: string_column(&row, 0)?,
                table: string_column(&row, 1)?,
                row_key_hex: string_column(&row, 2)?,
                envelope_hex: string_column(&row, 3)?,
            })
        })
        .collect()
}

/// Register, read frames, deregister — the query lifecycle every resolution shares. The handle is
/// deregistered on every path; a crash between register and deregister leaves a harmless rebuilt
/// standing query and no other state.
fn answer_rows(engine: &mut Engine, sql: &str) -> Result<Vec<Row>, TaintError> {
    let handle = engine.register(sql, Admission::bounded())?;
    let frames = engine.read_frames(handle);
    let deregistered = engine.deregister(handle);
    let bytes = frames?;
    deregistered?;
    let (record, _) = read_framed(&bytes, 0)
        .map_err(|error| TaintError::Frame {
            reason: error.to_string(),
        })?
        .ok_or_else(|| TaintError::Frame {
            reason: "the answer frame is torn or absent".to_owned(),
        })?;
    let Record::Append { entries, .. } = record else {
        return Err(TaintError::Frame {
            reason: "the answer frame is not an append record".to_owned(),
        });
    };
    Ok(entries
        .into_iter()
        .filter(|(_, weight)| *weight > 0)
        .map(|(row, _)| row)
        .collect())
}

fn string_column(row: &Row, index: usize) -> Result<String, TaintError> {
    match row.get(index) {
        Some(Value::Str(value)) => Ok(value.clone()),
        other => Err(TaintError::Frame {
            reason: format!("resolution column {index} is {other:?}, expected a string"),
        }),
    }
}

/// Journal the resolved set under a content-addressed token: a resumed taint's re-write is a
/// replay, not a duplicate, and a later taint that finds new contamination appends a new entry.
fn journal(
    engine: &mut Engine,
    config: &TaintConfig,
    source: &SourceRef,
    resolved: &BTreeSet<ContaminatedRow>,
) -> Result<Option<Epoch>, TaintError> {
    if resolved.is_empty() {
        return Ok(None);
    }
    let mut hasher = blake3::Hasher::new();
    let mut entries = Vec::with_capacity(resolved.len());
    for row in resolved {
        for field in [
            &source.system,
            &source.record_id,
            &row.branch,
            &row.table,
            &row.row_key_hex,
            &row.envelope_hex,
        ] {
            hasher.update(&(field.len() as u64).to_be_bytes());
            hasher.update(field.as_bytes());
        }
        entries.push((
            Row::new(vec![
                Value::Str(source.system.clone()),
                Value::Str(source.record_id.clone()),
                Value::Str(row.branch.clone()),
                Value::Str(row.table.clone()),
                Value::Str(row.row_key_hex.clone()),
                Value::Str(row.envelope_hex.clone()),
            ]),
            1,
        ));
    }
    let token = format!(
        "taint:{}:{}:{}",
        source.system,
        source.record_id,
        hasher.finalize().to_hex()
    );
    let ack = engine.ingest(&config.ledger_channel(), LEDGER_TABLE, &token, entries)?;
    match ack {
        Ack::Appended => Ok(Some(engine.seal()?)),
        // A previous run journaled this exact set. If its seal also landed there is nothing
        // pending and nothing to do; if the process died between append and seal, the append is
        // still pending and the seal below completes that run's epoch.
        Ack::DroppedAsReplay => {
            if engine.pending() > 0 {
                Ok(Some(engine.seal()?))
            } else {
                Ok(None)
            }
        }
    }
}

/// **Archival** (docs/M4-TAINT.md § "The archive tier"; runs only at a maintenance drain
/// point, never inside a taint): move every resolved recall in the hot relation to the cold
/// tier, then retract the moved rows through the ordinary ingest path under a content-addressed
/// `taint-archive:` token — a replayed retraction is `DroppedAsReplay`, exactly like a replayed
/// journal. Requires `config.archive_dir`; with the whole ledger hot this is a no-op.
pub fn archive_resolved(
    engine: &mut Engine,
    config: &TaintConfig,
) -> Result<ArchiveStats, TaintError> {
    archive_resolved_with_faults(engine, config, &mut TaintFaults::inert())
}

/// [`archive_resolved`] with a planned interruption, for the archival crash gate.
pub fn archive_resolved_with_faults(
    engine: &mut Engine,
    config: &TaintConfig,
    faults: &mut TaintFaults,
) -> Result<ArchiveStats, TaintError> {
    let Some(dir) = &config.archive_dir else {
        return Ok(ArchiveStats::default());
    };
    let sql = format!(
        "SELECT {t}.source_system AS source_system, {t}.source_record AS source_record, \
         {t}.branch AS branch, {t}.table_name AS table_name, {t}.row_key AS row_key, \
         {t}.envelope AS envelope FROM {t}",
        t = LEDGER_TABLE
    );
    let hot = answer_rows(engine, &sql)?;
    if hot.is_empty() {
        return Ok(ArchiveStats::default());
    }
    let mut rows = BTreeSet::new();
    let mut entries = Vec::with_capacity(hot.len());
    for row in &hot {
        rows.insert(ArchivedRow {
            source_system: string_column(row, 0)?,
            source_record: string_column(row, 1)?,
            branch: string_column(row, 2)?,
            table: string_column(row, 3)?,
            row_key_hex: string_column(row, 4)?,
            envelope_hex: string_column(row, 5)?,
        });
        entries.push((row.clone(), -1));
    }

    // Segment first, manifest second (inside append), hot retraction last — the crash order the
    // M4 doc states. The retraction token is the segment's own content address.
    let archive = LedgerArchive::new(dir);
    let segment = archive.append(&rows)?;
    faults.hit(TaintSeam::AfterArchiveAppend)?;
    let Some(segment_name) = segment else {
        return Ok(ArchiveStats::default());
    };
    let token = format!("taint-archive:{segment_name}");
    let ack = engine.ingest(&config.ledger_channel(), LEDGER_TABLE, &token, entries)?;
    match ack {
        Ack::Appended => {
            engine.seal()?;
        }
        Ack::DroppedAsReplay => {
            if engine.pending() > 0 {
                engine.seal()?;
            }
        }
    }
    Ok(ArchiveStats {
        archived_rows: rows.len() as u64,
        segment: Some(segment_name),
    })
}

fn payload_predicate(
    table: &str,
    spec: &TaintTableSpec,
    rows: &[&ContaminatedRow],
) -> Result<String, TaintError> {
    let mut by_branch: BTreeMap<&str, Vec<String>> = BTreeMap::new();
    let mut disjuncts = 0;
    for row in rows {
        let key = decode_key(table, &row.row_key_hex, spec.key_type)?;
        disjuncts += 1;
        by_branch.entry(row.branch.as_str()).or_default().push(key);
    }
    if disjuncts > MAX_PREDICATE_DISJUNCTS {
        return Err(TaintError::DisjunctsExceeded { found: disjuncts });
    }
    let branches = by_branch
        .into_iter()
        .map(|(branch, keys)| {
            validate_literal(branch)?;
            let keys = keys
                .iter()
                .map(|key| match spec.key_type {
                    KeyType::Utf8 => {
                        validate_literal(key)?;
                        Ok(format!("{table}.{} = '{key}'", spec.key_column))
                    }
                    KeyType::Int64 => Ok(format!("{table}.{} = {key}", spec.key_column)),
                })
                .collect::<Result<Vec<_>, TaintError>>()?
                .join(" OR ");
            Ok(format!(
                "({table}.{} = '{branch}' AND ({keys}))",
                spec.branch_column
            ))
        })
        .collect::<Result<Vec<_>, TaintError>>()?;
    Ok(branches.join(" OR "))
}

fn derivation_predicate(resolved: &BTreeSet<ContaminatedRow>) -> Result<String, TaintError> {
    if resolved.len() > MAX_PREDICATE_DISJUNCTS {
        return Err(TaintError::DisjunctsExceeded {
            found: resolved.len(),
        });
    }
    let disjuncts = resolved
        .iter()
        .map(|row| {
            validate_literal(&row.branch)?;
            validate_literal(&row.table)?;
            validate_literal(&row.row_key_hex)?;
            Ok(format!(
                "({d}.branch = '{}' AND {d}.table_name = '{}' AND {d}.row_key = '{}')",
                row.branch,
                row.table,
                row.row_key_hex,
                d = DERIVATION_TABLE
            ))
        })
        .collect::<Result<Vec<_>, TaintError>>()?;
    Ok(disjuncts.join(" OR "))
}

fn build_report(
    source: &SourceRef,
    actions: &[ExecutedAction],
    rows: &BTreeSet<ContaminatedRow>,
) -> RecallReport {
    let contaminated_keys: BTreeSet<Vec<u8>> = rows
        .iter()
        .filter_map(|row| hex_decode(&row.row_key_hex))
        .collect();

    let mut irreversible = Vec::new();
    for action in actions {
        let downstream = action
            .justified_by
            .iter()
            .any(|key| contaminated_keys.contains(key));
        if !downstream {
            continue;
        }
        let escalation = match &action.compensating_action {
            Some(comp) => format!(
                "run the registered compensating action ({comp}) for {} on {}, then confirm with \
                 the affected party.",
                action.action_type, action.target
            ),
            None => format!(
                "there is NO registered compensating action for {} on {}. A human must decide how \
                 to make this right — retraction will not un-{} it.",
                action.action_type,
                action.target,
                action
                    .action_type
                    .rsplit('.')
                    .next()
                    .unwrap_or(&action.action_type),
            ),
        };
        irreversible.push(IrreversibleItem {
            action_id: action.action_id.clone(),
            action_type: action.action_type.clone(),
            target: action.target.clone(),
            actor: action.actor.clone(),
            receipt: action.receipt.clone(),
            justified_by: Vec::new(),
            derived_via: Vec::new(),
            compensating_action: action.compensating_action.clone(),
            escalation,
        });
    }
    // Deterministic order, exactly as Loom sorts it — an incident responder diffs these.
    irreversible.sort_by(|a, b| {
        (&a.action_type, &a.target, &a.action_id).cmp(&(&b.action_type, &b.target, &b.action_id))
    });

    let healed = rows
        .iter()
        .map(|row| HealedWrite {
            branch: row.branch.clone(),
            table: row.table.clone(),
            row_key_hex: row.row_key_hex.clone(),
            envelope_hex: row.envelope_hex.clone(),
        })
        .collect();

    RecallReport {
        source: source.clone(),
        irreversible,
        healed,
    }
}

fn decode_key(table: &str, row_key_hex: &str, key_type: KeyType) -> Result<String, TaintError> {
    let bytes = hex_decode(row_key_hex).ok_or_else(|| TaintError::KeyDecode {
        table: table.to_owned(),
        row_key_hex: row_key_hex.to_owned(),
    })?;
    match key_type {
        KeyType::Utf8 => String::from_utf8(bytes).map_err(|_| TaintError::KeyDecode {
            table: table.to_owned(),
            row_key_hex: row_key_hex.to_owned(),
        }),
        KeyType::Int64 => {
            let array: [u8; 8] = bytes.try_into().map_err(|_| TaintError::KeyDecode {
                table: table.to_owned(),
                row_key_hex: row_key_hex.to_owned(),
            })?;
            Ok(i64::from_be_bytes(array).to_string())
        }
    }
}

fn hex_decode(hex: &str) -> Option<Vec<u8>> {
    if !hex.len().is_multiple_of(2) {
        return None;
    }
    (0..hex.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(hex.get(at..at + 2)?, 16).ok())
        .collect()
}

/// Refuse, rather than escape, anything that cannot sit inside a single-quoted SQL literal. The
/// same posture as the bridge's identifier rule: a value that needs escaping at an admission
/// boundary is a value the boundary refuses by name.
fn validate_literal(value: &str) -> Result<(), TaintError> {
    if value.is_empty()
        || value.contains('\'')
        || value.contains('"')
        || value.bytes().any(|byte| byte.is_ascii_control())
    {
        return Err(TaintError::InvalidSource {
            value: value.to_owned(),
        });
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use loom_core::ActorId;

    fn row(branch: &str, table: &str, key: &str) -> ContaminatedRow {
        ContaminatedRow {
            branch: branch.to_owned(),
            table: table.to_owned(),
            row_key_hex: key.bytes().fold(String::new(), |mut out, byte| {
                use std::fmt::Write as _;
                let _ = write!(out, "{byte:02x}");
                out
            }),
            envelope_hex: "ab".repeat(32),
        }
    }

    #[test]
    fn the_report_leads_with_what_it_cannot_undo() {
        let rows: BTreeSet<ContaminatedRow> = [row("hyp-a", "claims", "clm-2")].into();
        let action = ExecutedAction {
            action_id: "act_1".to_owned(),
            action_type: "identity.suspend_account".to_owned(),
            target: "user-4471".to_owned(),
            actor: ActorId::new("agent-responder"),
            justified_by: vec![b"clm-2".to_vec()],
            receipt: Some("okta:suspend:88213".to_owned()),
            compensating_action: None,
        };
        let report = build_report(&SourceRef::new("web", "scraped-page-77"), &[action], &rows);
        let text = report.to_string();
        let cannot = text.find("CANNOT BE UNDONE").expect("irreversible section");
        let healed = text.find("ALREADY HEALED").expect("healed section");
        assert!(cannot < healed, "the account must come before the writes");
        assert!(text.contains("NO COMPENSATING ACTION EXISTS"));
        assert!(report.needs_human());
    }

    #[test]
    fn an_action_justified_only_by_clean_claims_is_not_listed() {
        let rows: BTreeSet<ContaminatedRow> = [row("hyp-a", "claims", "clm-2")].into();
        let action = ExecutedAction {
            action_id: "act_2".to_owned(),
            action_type: "identity.suspend_account".to_owned(),
            target: "user-9".to_owned(),
            actor: ActorId::new("agent"),
            justified_by: vec![b"clm-clean".to_vec()],
            receipt: Some("r".to_owned()),
            compensating_action: None,
        };
        let report = build_report(&SourceRef::new("web", "scraped-page-77"), &[action], &rows);
        assert!(report.irreversible.is_empty());
        assert!(report.is_fully_reversible());
    }

    #[test]
    fn predicates_are_branch_scoped_and_refuse_unescapable_values() {
        let spec = TaintTableSpec {
            plane: "memory".to_owned(),
            key_column: "claim_id".to_owned(),
            branch_column: "branch".to_owned(),
            key_type: KeyType::Utf8,
        };
        let a = row("sess-a", "claims", "clm-1");
        let b = row("hyp-a", "claims", "clm-2");
        let predicate = payload_predicate("claims", &spec, &[&a, &b]).expect("predicate");
        assert_eq!(
            predicate,
            "(claims.branch = 'hyp-a' AND (claims.claim_id = 'clm-2')) OR \
             (claims.branch = 'sess-a' AND (claims.claim_id = 'clm-1'))"
        );
        assert!(validate_literal("it's").is_err());
        assert!(validate_literal("").is_err());
        assert!(validate_literal("ok-value").is_ok());
    }

    #[test]
    fn int64_keys_decode_into_unquoted_literals() {
        let spec = TaintTableSpec {
            plane: "events".to_owned(),
            key_column: "id".to_owned(),
            branch_column: "branch".to_owned(),
            key_type: KeyType::Int64,
        };
        let contaminated = ContaminatedRow {
            branch: "sess-a".to_owned(),
            table: "events".to_owned(),
            row_key_hex: {
                use std::fmt::Write as _;
                42i64
                    .to_be_bytes()
                    .iter()
                    .fold(String::new(), |mut out, byte| {
                        let _ = write!(out, "{byte:02x}");
                        out
                    })
            },
            envelope_hex: "cd".repeat(32),
        };
        let predicate = payload_predicate("events", &spec, &[&contaminated]).expect("predicate");
        assert_eq!(predicate, "(events.branch = 'sess-a' AND (events.id = 42))");
    }
}
