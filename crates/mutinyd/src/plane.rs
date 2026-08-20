//! One tenant's composed plane: substrate storage → M1 bridge translation → C9 compute engine →
//! branch-scoped semantic operators behind the M3 trust mount → Loom's action gateway — the same
//! seams the M4/M5 gates froze, generalized from corpus-driven to config-driven, serving behind
//! `mutinyd`'s admission boundary. `docs/M6-SURFACE.md` is the wire contract this implements.
//!
//! One deliberate contract evolution over the frozen dev host, documented in M6-SURFACE §"the
//! epoch clock, composed": engine-native taint epochs run the engine clock ahead of the storage
//! commit sequence, so this plane **accepts writes after a taint** and recovers by offering the
//! full capture history idempotently instead of comparing the two clocks.

use crate::config::{EmbeddingConfig, TableConfig, TenantConfig};
use crate::metrics::{trace, Metrics};
use loom_action::{ActionRecord, Connector, ConnectorOutcome, Proposal};
use loom_branch::{CapabilityToken, Loom, MAIN};
use loom_core::{
    ActorId, BranchId, Claim, ClaimId, ClaimStatus, Confidence, ExecutedAction, Interval, Method,
    SessionId, SourceRef, TenantId, Timestamp, TrustClass, WriteEnvelope,
};
use loom_policy::{Decision, Effect, Match, PolicyRule, PolicySet, Request, PURPOSE_AUTHORIZE};
use mutiny_bridge::{
    commit_with_capture, prepared_batches, recover_pending_captures, CapturedChange, CapturedTable,
    CommitCapture, CommitDraft, EnvelopeAuthority, EnvelopeId,
};
use mutiny_forks::{
    lineage_source, merge_marker_source, ForkEvent, ForkKind, Lineage, FORKS_TABLE,
};
use mutiny_semantic::{
    ScalarColumns, ScalarPredicate, SemanticDelta, SemanticGroupPlan, SemanticGroups, SemanticHit,
    SemanticQuery, SemanticRecord, SemanticTopK,
};
use mutiny_taint::{SemanticHealer, TaintConfig, TaintOutcome, TaintTableSpec};
use mutiny_trust::{mount, AgentTrustPlane, OperatorTrustPlane};
use prism_types::{Embedder, HashEmbedder};
use schweep_log::record::{read_framed, Record};
use schweep_log::SyncPolicy;
use schweep_memo::Admission;
use schweep_server::wire::ErrorKind;
use schweep_server::{Engine, Policy, ServerError};
use schweep_zset::{DataType, Field, Row, Schema, Value};
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

/// One merge candidate: (epoch, key, (table, row, sources)).
type MergeCandidate = (u64, String, (String, Row, Vec<SourceRef>));
use std::sync::Arc;

/// Rows per write commit; the capture page is bounded (M1), so the bound has a name.
pub const MAX_ROWS_PER_WRITE: usize = 256;

/// Page allocation: `commit_seq * PAGE_STRIDE + row_index`, collision-free and recoverable.
const PAGE_STRIDE: u64 = 1024;

/// The MD-3 extension constructs the SQL door refuses by name until the MutinyDB binder lands.
const UNSHIPPED_SQL: [(&str, &str); 7] = [
    ("≈≈", "the semantic ranking operator ≈≈ is served by the typed/MCP semantic operators (M2); its SQL form lands with the MutinyDB binder"),
    ("~~", "the semantic ranking alias ~~ is served by the typed/MCP semantic operators (M2); its SQL form lands with the MutinyDB binder"),
    (" AS OF ", "AS OF is served by branch-scoped standing operators (M3/M5); its SQL form lands with the MutinyDB binder"),
    ("TAINTED BY", "TAINTED BY is served by the derivation relation and taint operations (M4); its SQL form lands with the MutinyDB binder"),
    ("SEMANTIC_CLUSTER", "semantic_cluster is served by the typed/MCP grouping operators (M2); its SQL form lands with the MutinyDB binder"),
    ("NOVELTY", "NOVELTY is an M2 semantic construct; its SQL form lands with the MutinyDB binder"),
    ("SEMANTIC_DIFF", "SEMANTIC_DIFF is an M2 semantic construct; its SQL form lands with the MutinyDB binder"),
];

#[derive(Debug, thiserror::Error)]
pub enum PlaneError {
    #[error("{0}")]
    Refused(String),
    #[error("{0}")]
    NotFound(String),
    #[error("{0}")]
    Rejected(String),
    #[error(transparent)]
    Engine(#[from] ServerError),
    #[error("storage: {0}")]
    Storage(String),
    #[error(transparent)]
    Bridge(#[from] mutiny_bridge::BridgeError),
    #[error(transparent)]
    Taint(#[from] mutiny_taint::TaintError),
    #[error(transparent)]
    Trust(#[from] mutiny_trust::TrustError),
    #[error(transparent)]
    Fork(#[from] mutiny_forks::ForkError),
    #[error("semantic: {0}")]
    Semantic(String),
    #[error("internal: {0}")]
    Internal(String),
}

impl PlaneError {
    /// Map into the wire taxonomy (D-23, inherited whole; docs/M6-SURFACE.md).
    #[must_use]
    pub fn kind(&self) -> ErrorKind {
        match self {
            PlaneError::Refused(_) => ErrorKind::Refused,
            PlaneError::NotFound(_) => ErrorKind::NotFound,
            PlaneError::Rejected(_) | PlaneError::Fork(_) => ErrorKind::Rejected,
            PlaneError::Engine(error) => error.kind(),
            PlaneError::Bridge(_) | PlaneError::Taint(_) | PlaneError::Trust(_) => {
                ErrorKind::Rejected
            }
            PlaneError::Semantic(_) | PlaneError::Storage(_) | PlaneError::Internal(_) => {
                ErrorKind::Internal
            }
        }
    }
}

/// The trust-plane admission set: envelopes registered at commit time, admitted by exact id.
/// v0 posture per docs/M6-SURFACE.md: structural validation + durable registration; Ed25519
/// key verification is the configured-off enterprise hardening M8's ledger carries.
#[derive(Debug, Default)]
struct RegistryAuthority {
    known: RefCell<BTreeSet<EnvelopeId>>,
}

impl RegistryAuthority {
    fn register(&self, id: EnvelopeId) {
        self.known.borrow_mut().insert(id);
    }
}

impl EnvelopeAuthority for RegistryAuthority {
    fn admit(&self, id: EnvelopeId, _envelope: &WriteEnvelope) -> Result<(), String> {
        if self.known.borrow().contains(&id) {
            Ok(())
        } else {
            Err("envelope is not in the trust-plane registry".to_owned())
        }
    }
}

/// The v0 connector: deterministic echo receipts (`<prefix>:<target>`), per config.
struct EchoConnector {
    action_type: String,
    compensating: Option<String>,
    receipt_prefix: String,
}

impl Connector for EchoConnector {
    fn action_type(&self) -> &str {
        &self.action_type
    }

    fn compensating_action(&self) -> Option<String> {
        self.compensating.clone()
    }

    fn execute(&self, target: &str, _idempotency_key: &str) -> ConnectorOutcome {
        ConnectorOutcome::Succeeded {
            receipt: format!("{}:{target}", self.receipt_prefix),
        }
    }
}

fn policy() -> PolicySet {
    PolicySet::new(
        "mutinyd-v0",
        vec![
            PolicyRule {
                actor: Match::Any,
                label: Match::Is(TrustClass::Untrusted),
                purpose: Match::Is(PURPOSE_AUTHORIZE.to_owned()),
                action: Match::Any,
                effect: Effect::Deny,
            },
            PolicyRule {
                actor: Match::Any,
                label: Match::Any,
                purpose: Match::Any,
                action: Match::Any,
                effect: Effect::Allow,
            },
        ],
    )
}

/// A typed write through the front door: the envelope's fields are the request's fields, and
/// there is no form of this request without them (MD-2 R2).
#[derive(Clone, Debug)]
pub struct WriteRequest {
    pub actor: String,
    pub session: String,
    pub branch: String,
    pub intent: String,
    pub sources: Vec<(String, String)>,
    pub table: String,
    pub rows: Vec<Vec<serde_json::Value>>,
}

#[derive(Clone, Debug)]
pub struct WriteReceipt {
    pub commit_seq: u64,
    pub epoch: Option<u64>,
    pub rows: usize,
}

/// One tenant's composed plane. Single-writer: exactly one worker thread owns it (the admission
/// boundary's round-robin discipline), which is what keeps everything past ingest deterministic.
pub struct TenantPlane {
    pub name: String,
    config: TenantConfig,
    embedding: EmbeddingConfig,
    store: substrate_wal::DurableStore,
    engine: Engine,
    agent: AgentTrustPlane,
    operator: OperatorTrustPlane,
    authority: RegistryAuthority,
    embedder: HashEmbedder,
    tokens: BTreeMap<String, CapabilityToken>,
    commit_seq: u64,
    lineage_events: Vec<ForkEvent>,
    records: Vec<ActionRecord>,
    proposals: BTreeMap<String, Proposal>,
    metrics: Arc<Metrics>,
    /// The standing-state membership mirror: branch -> the (table, key) rows its semantic
    /// operators hold. Every entry corresponds to a live engine row (the M4/M5 invariant), which
    /// is what makes the plane checkpoint a membership list rather than serialized state.
    membership: BTreeMap<String, BTreeSet<(String, String)>>,
    tenant_dir: std::path::PathBuf,
    compute_dir: std::path::PathBuf,
    /// The `commit_seq` as of the last maintenance pass (or open), for the worker's policy
    /// trigger. Deliberately not persisted: after a restart the counter restarts, and the next
    /// maintenance is at most `maintenance_every` commits away.
    last_maintained: u64,
}

/// The crash-injection instrument for the maintenance seams (docs/M8-MAINTENANCE.md): when
/// `MUTINYD_MAINT_ABORT_AT` names the seam just completed, the process dies **right here**, the
/// way a SIGKILL would. The kill-matrix gate drives every seam through this deterministically;
/// production never sets the variable, and a set variable makes the process abort — loudly —
/// so it cannot be mistaken for a tuning knob.
fn maintenance_seam(seam: &str) {
    if std::env::var("MUTINYD_MAINT_ABORT_AT").is_ok_and(|at| at == seam) {
        std::process::abort();
    }
}

/// What `plane-checkpoint.json` holds (docs/M7-FLEET.md): enough to wake in
/// O(checkpoint + suffix) — never serialized operator state that could drift from the log.
#[derive(serde::Serialize, serde::Deserialize)]
struct PlaneCheckpoint {
    commit_seq: u64,
    lineage: Vec<(String, String, u64, String)>,
    membership: BTreeMap<String, Vec<(String, String)>>,
}

impl TenantPlane {
    /// Open the plane and recover it — **checkpoint-aware since M8** (docs/M8-MAINTENANCE.md):
    /// a tenant that has ever been maintained or slept holds a plane checkpoint, and recovery
    /// takes the bounded path (checkpoint + suffix, self-healing when stale). Full capture
    /// replay remains exactly what it was for a tenant that has neither — and **fails closed by
    /// name** against a collapsed store, because a consumed queue cannot serve full replay and
    /// must never be silently partially rebuilt.
    pub fn open(
        data_dir: &Path,
        config: &TenantConfig,
        embedding: &EmbeddingConfig,
        checkpoint_every: u64,
        metrics: Arc<Metrics>,
    ) -> Result<TenantPlane, PlaneError> {
        let mut plane = Self::open_shell(data_dir, config, embedding, checkpoint_every, metrics)?;
        if plane.checkpoint_path().exists() {
            plane.wake_from_checkpoint()?;
        } else {
            let head = plane.store.head();
            if let Some(floor) = mutiny_bridge::collapsed_floor(&plane.store, head)? {
                return Err(PlaneError::Rejected(format!(
                    "tenant {:?}: refusing full replay of a collapsed store (history consumed \
                     through commit {floor}, and plane-checkpoint.json is missing) — a collapsed \
                     store without its checkpoint is corruption, not an inconvenience \
                     (docs/M8-MAINTENANCE.md)",
                    plane.name
                )));
            }
            plane.recover()?;
        }
        plane.last_maintained = plane.commit_seq;
        Ok(plane)
    }

    /// **Bounded wake** (docs/M7-FLEET.md): O(checkpoint + suffix), never O(history). Refuses by
    /// name if the promised checkpoint is missing — a slept tenant without one is corruption,
    /// not an inconvenience; the full-replay path stays reserved for crashed-while-awake tenants.
    pub fn wake(
        data_dir: &Path,
        config: &TenantConfig,
        embedding: &EmbeddingConfig,
        checkpoint_every: u64,
        metrics: Arc<Metrics>,
    ) -> Result<TenantPlane, PlaneError> {
        let mut plane = Self::open_shell(data_dir, config, embedding, checkpoint_every, metrics)?;
        plane.wake_from_checkpoint()?;
        plane.last_maintained = plane.commit_seq;
        Ok(plane)
    }

    fn open_shell(
        data_dir: &Path,
        config: &TenantConfig,
        embedding: &EmbeddingConfig,
        checkpoint_every: u64,
        metrics: Arc<Metrics>,
    ) -> Result<TenantPlane, PlaneError> {
        let tenant_dir = data_dir.join(&config.name);
        let storage_dir = tenant_dir.join("storage");
        let compute_dir = tenant_dir.join("compute");
        std::fs::create_dir_all(&storage_dir).map_err(|e| PlaneError::Storage(e.to_string()))?;

        let store = substrate_wal::DurableStore::open(
            substrate_pager::std_vfs(),
            &storage_dir,
            substrate_pager::StoreConfig::default(),
        )
        .map_err(|e| PlaneError::Storage(e.to_string()))?;
        store
            .recover()
            .map_err(|e| PlaneError::Storage(e.to_string()))?;

        let catalog = build_catalog(config)?;
        let engine = Engine::open(
            &compute_dir,
            catalog,
            Policy::default(),
            SyncPolicy::Full,
            checkpoint_every,
        )?;

        let embedder = HashEmbedder::with_version(embedding.dim, &embedding.version);
        let db = Arc::new(
            Loom::in_memory(TenantId::new(config.name.as_str()))
                .map_err(mutiny_trust::TrustError::from)?,
        );
        let connectors: Vec<Box<dyn Connector>> = config
            .connectors
            .iter()
            .map(|c| {
                Box::new(EchoConnector {
                    action_type: c.action_type.clone(),
                    compensating: c.compensating_action.clone(),
                    receipt_prefix: c.receipt_prefix.clone(),
                }) as Box<dyn Connector>
            })
            .collect();
        let (agent, operator) = mount(db, config.name.as_str(), policy(), connectors);

        for topk in &config.semantic_standing.topk {
            let query = SemanticQuery::new(
                topk.id.as_str(),
                space(&embedder),
                embedder
                    .embed(&topk.text)
                    .map_err(|e| PlaneError::Semantic(e.to_string()))?,
                topk.k,
                ScalarPredicate::default(),
            )
            .map_err(|e| PlaneError::Semantic(e.to_string()))?;
            operator.install_standing(&BranchId::new(MAIN), SemanticTopK::new(query))?;
        }
        for groups in &config.semantic_standing.groups {
            let anchors = groups
                .anchors
                .iter()
                .map(|anchor| {
                    embedder
                        .embed(anchor)
                        .map_err(|e| PlaneError::Semantic(e.to_string()))
                })
                .collect::<Result<Vec<_>, _>>()?;
            let plan =
                SemanticGroupPlan::new(space(&embedder), anchors, ScalarPredicate::default())
                    .map_err(|e| PlaneError::Semantic(e.to_string()))?;
            operator.install_groups(
                &BranchId::new(MAIN),
                groups.id.as_str(),
                SemanticGroups::new(plan),
            )?;
        }

        let plane = TenantPlane {
            name: config.name.clone(),
            config: config.clone(),
            embedding: embedding.clone(),
            store,
            engine,
            agent,
            operator,
            authority: RegistryAuthority::default(),
            embedder,
            tokens: BTreeMap::new(),
            commit_seq: 0,
            lineage_events: Vec::new(),
            records: Vec::new(),
            proposals: BTreeMap::new(),
            metrics,
            membership: BTreeMap::new(),
            tenant_dir,
            compute_dir,
            last_maintained: 0,
        };
        Ok(plane)
    }

    fn recover(&mut self) -> Result<(), PlaneError> {
        let head = self.store.head();
        let captures = recover_pending_captures(&self.store, head, 0)?;
        for capture in &captures {
            self.authority.register(EnvelopeId::of(&capture.envelope));
            self.apply_capture(capture)?;
            let branch = capture.branch.as_str().to_owned();
            if !self.tokens.contains_key(&branch) {
                // A branch first seen as a writer with no fork record is a session root.
                self.session_open(&branch)?;
            }
            for (table, captured) in &capture.tables {
                if table == FORKS_TABLE {
                    for change in &captured.changes {
                        let event = ForkEvent::from_row(&change.row)?;
                        match event.kind {
                            ForkKind::Fork => self.hydrate_fork(&event)?,
                            ForkKind::Rewind => {
                                self.operator
                                    .rewind_branch(&BranchId::new(event.child.as_str()))?;
                                self.tokens.remove(&event.child);
                                self.lineage_events.push(event);
                            }
                        }
                    }
                } else if let Some(spec) = self.semantic_spec(table) {
                    for change in &captured.changes {
                        let delta = self.semantic_delta(&spec, &change.row, change.weight)?;
                        let key = delta.record.key.clone();
                        self.apply_semantic(&branch, table, &key, delta)?;
                    }
                }
            }
            self.commit_seq = capture.commit_seq;
        }

        self.apply_ledger_heals()
    }

    /// Re-apply every heal the taint ledger records — engine-native epochs the capture history
    /// never saw. Idempotent by construction (retract-by-key, skip-absent), so both recovery
    /// paths apply them all.
    fn apply_ledger_heals(&mut self) -> Result<(), PlaneError> {
        let ledger_sql = format!(
            "SELECT {t}.branch AS branch, {t}.table_name AS table_name, {t}.row_key AS row_key \
             FROM {t}",
            t = mutiny_taint::LEDGER_TABLE
        );
        let healed = self.table_rows(&ledger_sql)?;
        let lineage = self.lineage()?;
        let semantic_tables = self.semantic_table_names();
        for row in healed {
            let (Some(Value::Str(branch)), Some(Value::Str(table)), Some(Value::Str(row_key))) =
                (row.get(0), row.get(1), row.get(2))
            else {
                return Err(PlaneError::Internal(
                    "malformed taint ledger row".to_owned(),
                ));
            };
            let Some(key) = decode_hex_utf8(row_key) else {
                continue;
            };
            let mut healer = LineageHealer {
                operator: &self.operator,
                lineage: lineage.clone(),
                semantic_tables: semantic_tables.clone(),
                membership: &mut self.membership,
            };
            healer
                .heal(branch, table, &[key])
                .map_err(PlaneError::Internal)?;
        }
        Ok(())
    }

    /// **Sleep** (docs/M7-FLEET.md, extended at M8): drain is structural (the worker is serial
    /// and this runs on it), then the full maintenance sequence — compact the engine, write the
    /// plane checkpoint, prune the consumed queue, collapse and sweep the storage — and drop
    /// everything. What remains is **bounded** bytes on the storage backend and the registry row
    /// the fleet keeps.
    pub fn sleep(mut self) -> Result<(), PlaneError> {
        self.maintain()?;
        Ok(())
    }

    /// **Awake maintenance** (docs/M8-MAINTENANCE.md, issue #12): the M7 sleep-side bounding
    /// machinery, runnable in place on the worker at a drain point. Steps S1–S6, in the order
    /// the durability doc proves crash-safe; `MUTINYD_MAINT_ABORT_AT` is the crash-injection
    /// instrument the kill matrix uses to land a death on every seam deterministically.
    pub fn maintain(&mut self) -> Result<mutiny_bridge::MaintenanceStats, PlaneError> {
        self.compact_engine_guarded()?; // S1
        maintenance_seam("S1");
        self.write_plane_checkpoint()?; // S2
        maintenance_seam("S2");
        let pruned_pages = mutiny_bridge::prune_consumed(&self.store, self.commit_seq)?; // S3
        maintenance_seam("S3");
        let collapsed = mutiny_bridge::install_collapsed_root(&self.store)?; // S4
        maintenance_seam("S4");
        mutiny_bridge::checkpoint_wal(&self.store)?; // S5
        maintenance_seam("S5");
        let (manifests_swept, pages_swept) = mutiny_bridge::sweep(&self.store)?; // S6
        maintenance_seam("S6");
        let stats = mutiny_bridge::MaintenanceStats {
            pruned_pages,
            collapsed,
            manifests_swept,
            pages_swept,
        };
        self.last_maintained = self.commit_seq;
        self.metrics.inc("mutiny_maintenance_total");
        Ok(stats)
    }

    /// How many commits have landed since the last maintenance pass (or open) — the worker's
    /// policy input.
    pub fn commits_since_maintenance(&self) -> u64 {
        self.commit_seq.saturating_sub(self.last_maintained)
    }

    /// The tenant's maintenance policy (docs/M8-MAINTENANCE.md); `0` disables.
    pub fn maintenance_every(&self) -> u64 {
        self.config.maintenance_every
    }

    /// Compaction has two benign refusals (nothing sealed yet; already compacted to the
    /// anchor). The preconditions the engine itself states are checked rather than matching
    /// error strings, and at least a fresh checkpoint is always left behind.
    fn compact_engine_guarded(&mut self) -> Result<(), PlaneError> {
        let epoch = self.engine.epoch();
        let retained_from = self
            .engine
            .health()
            .lines()
            .find_map(|line| {
                line.strip_prefix("retained_from ")?
                    .trim()
                    .parse::<u64>()
                    .ok()
            })
            .unwrap_or(0);
        if epoch > 0 && epoch > retained_from {
            self.engine.compact()?;
        } else {
            self.engine.checkpoint()?;
        }
        Ok(())
    }

    /// The M7 plane checkpoint, written atomically: `commit_seq`, fork lineage, and the
    /// standing-state membership — never serialized operator state.
    fn write_plane_checkpoint(&self) -> Result<(), PlaneError> {
        let checkpoint = PlaneCheckpoint {
            commit_seq: self.commit_seq,
            lineage: self
                .lineage_events
                .iter()
                .map(|event| {
                    (
                        event.child.clone(),
                        event.parent.clone(),
                        event.at_epoch,
                        event.kind.as_str().to_owned(),
                    )
                })
                .collect(),
            membership: self
                .membership
                .iter()
                .map(|(branch, keys)| (branch.clone(), keys.iter().cloned().collect()))
                .collect(),
        };
        let text = serde_json::to_string_pretty(&checkpoint)
            .map_err(|e| PlaneError::Internal(e.to_string()))?;
        let path = self.checkpoint_path();
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, text).map_err(|e| PlaneError::Storage(e.to_string()))?;
        std::fs::rename(&tmp, &path).map_err(|e| PlaneError::Storage(e.to_string()))?;
        Ok(())
    }

    fn checkpoint_path(&self) -> std::path::PathBuf {
        self.tenant_dir.join("plane-checkpoint.json")
    }

    /// The bounded half of [`TenantPlane::wake`]: checkpoint + current rows + suffix + heals.
    fn wake_from_checkpoint(&mut self) -> Result<(), PlaneError> {
        let path = self.checkpoint_path();
        let text = std::fs::read_to_string(&path).map_err(|_| {
            PlaneError::Rejected(format!(
                "tenant {:?} was slept with a checkpoint the contract promised, and {} is \
                 missing: refusing the wake rather than guessing (docs/M7-FLEET.md)",
                self.name,
                path.display()
            ))
        })?;
        let checkpoint: PlaneCheckpoint =
            serde_json::from_str(&text).map_err(|e| PlaneError::Internal(e.to_string()))?;

        self.commit_seq = checkpoint.commit_seq;
        self.lineage_events = checkpoint
            .lineage
            .iter()
            .map(|(child, parent, at_epoch, kind)| {
                Ok(ForkEvent {
                    child: child.clone(),
                    parent: parent.clone(),
                    at_epoch: *at_epoch,
                    kind: ForkKind::parse(kind).ok_or_else(|| {
                        PlaneError::Internal(format!("unknown lineage kind {kind:?}"))
                    })?,
                })
            })
            .collect::<Result<Vec<_>, PlaneError>>()?;

        // Current rows per semantic table, one bounded read each (the engine opened from its
        // snapshot + suffix, so this is O(current data)).
        let mut rows_by_table: BTreeMap<String, BTreeMap<String, Row>> = BTreeMap::new();
        for table in self.semantic_table_names() {
            let Some(spec) = self.semantic_spec(&table) else {
                continue;
            };
            let config = self
                .table_config(&table)
                .ok_or_else(|| PlaneError::Internal(format!("no config for {table:?}")))?
                .clone();
            let projection = config
                .columns
                .iter()
                .map(|(name, _)| format!("{table}.{name} AS {name}"))
                .collect::<Vec<_>>()
                .join(", ");
            let rows = self.table_rows(&format!("SELECT {projection} FROM {table}"))?;
            let mut by_key = BTreeMap::new();
            for row in rows {
                if let Some(Value::Str(key)) = row.get(spec.key) {
                    by_key.insert(key.clone(), row);
                }
            }
            rows_by_table.insert(table, by_key);
        }

        // Rebuild every branch's standing stores from membership x current rows.
        let membership: Vec<(String, Vec<(String, String)>)> = checkpoint
            .membership
            .iter()
            .map(|(branch, keys)| (branch.clone(), keys.clone()))
            .collect();
        for (branch, held) in membership {
            self.session_open(&branch)?;
            for (table, key) in held {
                let Some(spec) = self.semantic_spec(&table) else {
                    continue;
                };
                let row = rows_by_table
                    .get(&table)
                    .and_then(|by_key| by_key.get(&key))
                    .cloned()
                    .ok_or_else(|| {
                        PlaneError::Internal(format!(
                            "checkpoint names {branch}/{table}/{key}, but no live engine row \
                             backs it — the membership invariant broke"
                        ))
                    })?;
                let delta = self.semantic_delta(&spec, &row, 1)?;
                self.apply_semantic(&branch, &table, &key, delta)?;
            }
        }

        // The suffix: captures after the checkpoint (a fresh sleep leaves none; a stale
        // checkpoint self-heals from here).
        let head = self.store.head();
        let captures = recover_pending_captures(&self.store, head, checkpoint.commit_seq)?;
        for capture in &captures {
            self.authority.register(EnvelopeId::of(&capture.envelope));
            self.apply_capture(capture)?;
            let branch = capture.branch.as_str().to_owned();
            if !self.tokens.contains_key(&branch) {
                self.session_open(&branch)?;
            }
            for (table, captured) in &capture.tables {
                if table == FORKS_TABLE {
                    for change in &captured.changes {
                        let event = ForkEvent::from_row(&change.row)?;
                        match event.kind {
                            ForkKind::Fork => self.hydrate_fork(&event)?,
                            ForkKind::Rewind => {
                                self.operator
                                    .rewind_branch(&BranchId::new(event.child.as_str()))?;
                                self.tokens.remove(&event.child);
                                self.membership.remove(&event.child);
                                self.lineage_events.push(event);
                            }
                        }
                    }
                } else if let Some(spec) = self.semantic_spec(table) {
                    for change in &captured.changes {
                        let delta = self.semantic_delta(&spec, &change.row, change.weight)?;
                        let key = delta.record.key.clone();
                        self.apply_semantic(&branch, table, &key, delta)?;
                    }
                }
            }
            self.commit_seq = capture.commit_seq;
        }

        self.apply_ledger_heals()
    }

    /// **The delta->circuit mapping, observed from the compute plane** (MD-1 R2's anticipated
    /// inversion, recorded in docs/M7-FLEET.md): the engine's own persisted registration file is
    /// read, each SQL text is bound through the compute plane's public binder, and the bound
    /// source tree names the tables. Compute never calls the fleet; the fleet observes.
    pub fn circuit_mapping(&self) -> Result<BTreeMap<String, BTreeSet<String>>, PlaneError> {
        circuit_mapping_for(&self.compute_dir, &self.config)
    }

    // ---- sessions and branches -----------------------------------------------------------------

    /// Idempotent per name: capability tokens are process-lifetime at v0 (M6-SURFACE), so a
    /// restarted server re-mints on reopen.
    pub fn session_open(&mut self, session: &str) -> Result<CapabilityToken, PlaneError> {
        if let Some(token) = self.tokens.get(session) {
            return Ok(token.clone());
        }
        let (handle, token) = self.agent.open_session_named(SessionId::new(session))?;
        if handle.branch.as_str() != session {
            return Err(PlaneError::Internal(format!(
                "session {session:?} opened branch {:?}",
                handle.branch.as_str()
            )));
        }
        self.tokens.insert(session.to_owned(), token.clone());
        Ok(token)
    }

    pub fn fork(&mut self, session: &str, from: &str, child: &str) -> Result<(), PlaneError> {
        let lineage = self.lineage()?;
        if lineage.fork_of(child).is_some() {
            if self.tokens.contains_key(child) {
                return Ok(());
            }
            return Err(PlaneError::Rejected(format!(
                "fork record for {child:?} exists without live state; restart to rebuild"
            )));
        }
        let event = ForkEvent {
            child: child.to_owned(),
            parent: from.to_owned(),
            at_epoch: self.commit_seq + 1,
            kind: ForkKind::Fork,
        };
        self.lifecycle_commit(session, from, child, &event)?;
        self.hydrate_fork(&event)
    }

    pub fn rewind(&mut self, session: &str, child: &str) -> Result<usize, PlaneError> {
        let lineage = self.lineage()?;
        let Some((parent, _)) = lineage.fork_of(child) else {
            return Err(PlaneError::NotFound(format!(
                "branch {child:?} has no fork record"
            )));
        };
        let parent = parent.to_owned();
        if lineage.rewound_at(child).is_none() {
            let event = ForkEvent {
                child: child.to_owned(),
                parent: parent.clone(),
                at_epoch: self.commit_seq + 1,
                kind: ForkKind::Rewind,
            };
            self.lifecycle_commit(session, child, &format!("{child}:rewind"), &event)?;
            self.lineage_events.push(event);
        }
        let freed = self.operator.rewind_branch(&BranchId::new(child))?;
        self.tokens.remove(child);
        self.membership.remove(child);
        Ok(freed)
    }

    /// Merge per Loom's law, composed exactly as M5 proved it: policy re-run per write,
    /// all-or-nothing; per-key original sources plus the durable marker.
    pub fn merge(&mut self, session: &str, child: &str, into: &str) -> Result<usize, PlaneError> {
        let lineage = self.lineage()?;
        let head = self.store.head();
        let captures = recover_pending_captures(&self.store, head, 0)?;
        let mut candidates: Vec<MergeCandidate> = Vec::new();
        for capture in &captures {
            if capture.branch.as_str() != child {
                continue;
            }
            for (table, captured) in &capture.tables {
                if table == FORKS_TABLE {
                    continue;
                }
                for change in &captured.changes {
                    let key = String::from_utf8(change.primary_key.clone())
                        .map_err(|_| PlaneError::Internal("non-utf8 merge key".to_owned()))?;
                    candidates.push((
                        capture.commit_seq,
                        key,
                        (
                            table.clone(),
                            change.row.clone(),
                            capture.envelope.derived_from.clone(),
                        ),
                    ));
                }
            }
        }
        candidates.sort_by(|a, b| (a.0, &a.1).cmp(&(b.0, &b.1)));

        let marker = merge_marker_source(child);
        let sql = format!(
            "SELECT {d}.row_key AS row_key FROM {d} WHERE {d}.source_system = '{}' AND \
             {d}.source_record = '{}'",
            marker.system,
            marker.record_id,
            d = mutiny_bridge::DERIVATION_TABLE
        );
        let already: BTreeSet<String> = self
            .table_rows(&sql)?
            .into_iter()
            .filter_map(|row| match row.get(0) {
                Some(Value::Str(hex)) => decode_hex_utf8(hex),
                _ => None,
            })
            .collect();
        let plan = lineage.merge_divergence(child, &candidates, &already)?;

        for (_, key, (table, _, _)) in &plan {
            let decision = self.agent.decide(&Request {
                actor: session.to_owned(),
                label: TrustClass::VerifiedSystem,
                purpose: PURPOSE_AUTHORIZE.to_owned(),
                action: crate::MERGE_ACTION.to_owned(),
            });
            if decision.decision != Decision::Allow {
                return Err(PlaneError::Rejected(format!(
                    "policy denies {} for {table}/{key}; no write was made",
                    crate::MERGE_ACTION
                )));
            }
        }

        let mut merged = 0usize;
        let plan: Vec<MergeCandidate> = plan.into_iter().cloned().collect();
        for (_, key, (table, row, sources)) in plan {
            let spec = self
                .table_config(&table)
                .ok_or_else(|| PlaneError::NotFound(format!("unknown table {table:?}")))?
                .clone();
            let branch_index = column_index(&spec, &spec.branch_column)?;
            let mut values = row.values().to_vec();
            values[branch_index] = Value::Str(into.to_owned());
            let mut merged_sources: Vec<(String, String)> = sources
                .iter()
                .map(|s| (s.system.clone(), s.record_id.clone()))
                .collect();
            merged_sources.push((marker.system.clone(), marker.record_id.clone()));
            self.write_rows(
                session,
                session,
                into,
                &format!("merge {child} {table}/{key}"),
                &merged_sources,
                &table,
                vec![Row::new(values)],
            )?;
            merged += 1;
        }
        Ok(merged)
    }

    // ---- the write path ------------------------------------------------------------------------

    pub fn write(&mut self, request: &WriteRequest) -> Result<WriteReceipt, PlaneError> {
        let spec = self
            .table_config(&request.table)
            .ok_or_else(|| PlaneError::NotFound(format!("unknown table {:?}", request.table)))?
            .clone();
        if request.rows.is_empty() {
            return Err(PlaneError::Refused(
                "a write needs at least one row".to_owned(),
            ));
        }
        if request.rows.len() > MAX_ROWS_PER_WRITE {
            return Err(PlaneError::Refused(format!(
                "a write carries at most {MAX_ROWS_PER_WRITE} rows; split the batch"
            )));
        }
        self.session_open(&request.branch)?;
        let branch_index = column_index(&spec, &spec.branch_column)?;
        let mut rows = Vec::with_capacity(request.rows.len());
        for json_row in &request.rows {
            let row = decode_row(&spec, json_row)?;
            match row.get(branch_index) {
                Some(Value::Str(branch)) if branch == &request.branch => {}
                other => {
                    return Err(PlaneError::Refused(format!(
                        "row branch column is {other:?}, but the write names branch {:?}",
                        request.branch
                    )))
                }
            }
            rows.push(row);
        }
        self.write_rows(
            &request.actor,
            &request.session,
            &request.branch,
            &request.intent,
            &request.sources,
            &request.table,
            rows,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn write_rows(
        &mut self,
        actor: &str,
        session: &str,
        branch: &str,
        intent: &str,
        sources: &[(String, String)],
        table: &str,
        rows: Vec<Row>,
    ) -> Result<WriteReceipt, PlaneError> {
        let spec = self
            .table_config(table)
            .ok_or_else(|| PlaneError::NotFound(format!("unknown table {table:?}")))?
            .clone();
        if sources.is_empty() {
            return Err(PlaneError::Refused(
                "derived_from may not be empty: every write names what it derived from".to_owned(),
            ));
        }
        let key_index = column_index(&spec, &spec.key_column)?;
        let seq = self.commit_seq + 1;

        let mut txn = self
            .store
            .begin()
            .map_err(|e| PlaneError::Storage(e.to_string()))?;
        let mut changes = Vec::with_capacity(rows.len());
        for (index, row) in rows.iter().enumerate() {
            let key_bytes = match row.get(key_index) {
                Some(Value::Str(key)) => key.as_bytes().to_vec(),
                Some(Value::Int(key)) => key.to_be_bytes().to_vec(),
                other => {
                    return Err(PlaneError::Refused(format!(
                        "key column holds {other:?}; keys are utf8 or int64"
                    )))
                }
            };
            let page: substrate_pager::LogicalPageNo = seq * PAGE_STRIDE + index as u64;
            self.store
                .write(
                    &mut txn,
                    page,
                    format!("{table}/{seq}/{index}").into_bytes(),
                )
                .map_err(|e| PlaneError::Storage(e.to_string()))?;
            changes.push(CapturedChange {
                row: row.clone(),
                weight: 1,
                primary_key: key_bytes,
                pages: BTreeSet::from([page]),
            });
        }

        let envelope = WriteEnvelope::new(
            ActorId::new(actor),
            SessionId::new(session),
            BranchId::new(branch),
            intent,
        )
        .derived_from(
            sources
                .iter()
                .map(|(system, record)| SourceRef::new(system.clone(), record.clone())),
        );
        self.authority.register(EnvelopeId::of(&envelope));
        let draft = CommitDraft {
            tenant: TenantId::new(self.name.as_str()),
            plane: spec.plane.clone(),
            commit_seq: seq,
            branch: BranchId::new(branch),
            envelope,
            tables: BTreeMap::from([(table.to_owned(), CapturedTable { changes })]),
        };
        let catalog = build_catalog(&self.config)?;
        let capture = commit_with_capture(&self.store, txn, &draft, &catalog, &self.authority)?;
        let epoch = self.apply_capture(&capture)?;
        self.commit_seq = seq;
        self.metrics.inc(&format!(
            "mutiny_storage_commits_total{{tenant=\"{}\"}}",
            self.name
        ));
        if let Some(epoch) = epoch {
            self.metrics.inc(&format!(
                "mutiny_epochs_sealed_total{{tenant=\"{}\"}}",
                self.name
            ));
            trace(
                "epoch_sealed",
                &[
                    ("tenant", self.name.clone()),
                    ("epoch", epoch.to_string()),
                    ("commit_seq", seq.to_string()),
                ],
            );
        }

        if let Some(semantic) = self.semantic_spec(table) {
            for row in &rows {
                let delta = self.semantic_delta(&semantic, row, 1)?;
                let key = delta.record.key.clone();
                self.apply_semantic(branch, table, &key, delta)?;
            }
        }
        Ok(WriteReceipt {
            commit_seq: seq,
            epoch,
            rows: rows.len(),
        })
    }

    /// Offer a capture's batches; dedup drops what already landed; seal if anything is pending.
    /// This is the evolved clock rule M6-SURFACE documents: no comparison of the two clocks.
    fn apply_capture(&mut self, capture: &CommitCapture) -> Result<Option<u64>, PlaneError> {
        let (_, batches) = prepared_batches(capture, &self.authority)?;
        for batch in &batches {
            self.engine.ingest(
                &batch.source_id,
                &batch.table,
                &batch.dedup_token,
                batch.entries.clone(),
            )?;
        }
        if self.engine.pending() > 0 {
            Ok(Some(self.engine.seal()?))
        } else {
            Ok(None)
        }
    }

    fn lifecycle_commit(
        &mut self,
        session: &str,
        branch: &str,
        key: &str,
        event: &ForkEvent,
    ) -> Result<(), PlaneError> {
        self.session_open(branch)?;
        let seq = self.commit_seq + 1;
        let mut txn = self
            .store
            .begin()
            .map_err(|e| PlaneError::Storage(e.to_string()))?;
        let page: substrate_pager::LogicalPageNo = seq * PAGE_STRIDE;
        self.store
            .write(
                &mut txn,
                page,
                format!("{FORKS_TABLE}/{key}/{seq}").into_bytes(),
            )
            .map_err(|e| PlaneError::Storage(e.to_string()))?;
        let envelope = WriteEnvelope::new(
            ActorId::new(session),
            SessionId::new(session),
            BranchId::new(branch),
            format!("{} {key}", event.kind.as_str()),
        )
        .derived_from([lineage_source(&event.parent)]);
        self.authority.register(EnvelopeId::of(&envelope));
        let draft = CommitDraft {
            tenant: TenantId::new(self.name.as_str()),
            plane: "trust".to_owned(),
            commit_seq: seq,
            branch: BranchId::new(branch),
            envelope,
            tables: BTreeMap::from([(
                FORKS_TABLE.to_owned(),
                CapturedTable {
                    changes: vec![CapturedChange {
                        row: event.to_row(),
                        weight: 1,
                        primary_key: key.as_bytes().to_vec(),
                        pages: BTreeSet::from([page]),
                    }],
                },
            )]),
        };
        let catalog = build_catalog(&self.config)?;
        let capture = commit_with_capture(&self.store, txn, &draft, &catalog, &self.authority)?;
        self.apply_capture(&capture)?;
        self.commit_seq = seq;
        Ok(())
    }

    fn hydrate_fork(&mut self, event: &ForkEvent) -> Result<(), PlaneError> {
        let parent_token = match self.tokens.get(&event.parent) {
            Some(token) => token.clone(),
            None => self.session_open(&event.parent)?,
        };
        let (branch, token) = self.agent.branch(
            &parent_token,
            &BranchId::new(event.parent.as_str()),
            &event.child,
        )?;
        if branch.as_str() != event.child {
            return Err(PlaneError::Internal(format!(
                "fork of {:?} created branch {:?}",
                event.child,
                branch.as_str()
            )));
        }
        self.tokens.insert(event.child.clone(), token);
        let inherited = self
            .membership
            .get(&event.parent)
            .cloned()
            .unwrap_or_default();
        self.membership.insert(event.child.clone(), inherited);
        self.lineage_events.push(event.clone());
        Ok(())
    }

    // ---- queries -------------------------------------------------------------------------------

    /// Refuse MD-3's unshipped extension constructs by name, with the door that serves them.
    fn guard_sql(sql: &str) -> Result<(), PlaneError> {
        let upper = sql.to_uppercase();
        for (needle, message) in UNSHIPPED_SQL {
            let hit = if needle.chars().any(|c| c.is_ascii_alphabetic()) {
                upper.contains(needle)
            } else {
                sql.contains(needle)
            };
            if hit {
                return Err(PlaneError::Refused(format!("refused by name: {message}")));
            }
        }
        Ok(())
    }

    pub fn register(&mut self, sql: &str, unbounded: Option<&str>) -> Result<u64, PlaneError> {
        Self::guard_sql(sql)?;
        let admission = match unbounded {
            Some(reason) if !reason.is_empty() => Admission::with_unbounded_state(reason),
            _ => Admission::bounded(),
        };
        let handle = self.engine.register(sql, admission)?;
        self.metrics.gauge(
            &format!("mutiny_registrations{{tenant=\"{}\"}}", self.name),
            self.engine.health().lines().count() as i64,
        );
        Ok(handle)
    }

    pub fn deregister(&mut self, handle: u64) -> Result<(), PlaneError> {
        self.engine.deregister(handle).map_err(PlaneError::from)
    }

    pub fn read(&self, handle: u64) -> Result<(u64, String), PlaneError> {
        self.engine.read(handle).map_err(PlaneError::from)
    }

    pub fn read_frames(&self, handle: u64) -> Result<Vec<u8>, PlaneError> {
        self.engine.read_frames(handle).map_err(PlaneError::from)
    }

    pub fn oneshot(&self, sql: &str) -> Result<String, PlaneError> {
        Self::guard_sql(sql)?;
        self.engine.oneshot(sql).map_err(PlaneError::from)
    }

    pub fn subscribe(
        &self,
        handle: u64,
        from: u64,
    ) -> Result<(u64, Vec<schweep_server::EpochDelta>), PlaneError> {
        self.engine
            .subscribe(handle, from)
            .map_err(PlaneError::from)
    }

    pub fn plan_of(&self, handle: u64) -> Result<String, PlaneError> {
        self.engine.plan_of(handle).map_err(PlaneError::from)
    }

    pub fn counters(&self) -> String {
        self.engine.counters()
    }

    pub fn explain_state(&self) -> Result<String, PlaneError> {
        self.engine.explain_state().map_err(PlaneError::from)
    }

    pub fn explain_maintenance(&self) -> String {
        self.engine.explain_maintenance()
    }

    pub fn semantic_answer(
        &self,
        branch: &str,
        query: &str,
    ) -> Result<Vec<SemanticHit>, PlaneError> {
        let token = self
            .tokens
            .get(branch)
            .ok_or_else(|| PlaneError::NotFound(format!("unknown branch {branch:?}")))?;
        self.agent
            .answer(token, &BranchId::new(branch), query)
            .map_err(PlaneError::from)
    }

    pub fn semantic_groups(
        &self,
        branch: &str,
        group: &str,
    ) -> Result<Vec<mutiny_semantic::SemanticGroupSummary>, PlaneError> {
        let token = self
            .tokens
            .get(branch)
            .ok_or_else(|| PlaneError::NotFound(format!("unknown branch {branch:?}")))?;
        self.agent
            .group_summaries(token, &BranchId::new(branch), group)
            .map_err(PlaneError::from)
    }

    // ---- actions -------------------------------------------------------------------------------

    /// PROPOSE. The inert half; the agent doors reach nothing else (M3's law at the wire).
    pub fn propose(
        &mut self,
        actor: &str,
        branch: &str,
        action_type: &str,
        target: &str,
        idempotency_key: &str,
        justified_by: &[String],
    ) -> Result<String, PlaneError> {
        let claims: Vec<Claim> = justified_by
            .iter()
            .map(|key| Claim {
                id: ClaimId::of(key.as_bytes()),
                predicate: action_type.to_owned(),
                subject: target.to_owned(),
                object: loom_core::Value::Bool(true),
                valid: Interval::from(Timestamp::from_ms(1)),
                known: Interval::from(Timestamp::from_ms(1)),
                confidence: Confidence::new(0.99, Method::Rule, "v1"),
                evidence: vec![SourceRef::new("mutiny", "claims")],
                status: ClaimStatus::Asserted,
                policy: None,
                actor: ActorId::new(actor),
            })
            .collect();
        let justified: Vec<Vec<u8>> = justified_by.iter().map(|k| k.as_bytes().to_vec()).collect();
        let proposal = self
            .agent
            .agent(ActorId::new(actor), &BranchId::new(branch), false)
            .propose(
                action_type,
                target,
                idempotency_key,
                claims,
                justified,
                TrustClass::VerifiedSystem,
            );
        self.proposals.insert(idempotency_key.to_owned(), proposal);
        Ok(idempotency_key.to_owned())
    }

    /// EXECUTE — reachable only through the operator door; the MCP tool registry has no
    /// counterpart by construction.
    pub fn execute(&mut self, proposal_key: &str) -> Result<ActionRecord, PlaneError> {
        let proposal = self
            .proposals
            .get(proposal_key)
            .ok_or_else(|| PlaneError::NotFound(format!("unknown proposal {proposal_key:?}")))?
            .clone();
        let record = self.operator.execute(&proposal);
        self.records.push(record.clone());
        Ok(record)
    }

    // ---- taint ---------------------------------------------------------------------------------

    pub fn taint(&mut self, system: &str, record: &str) -> Result<TaintOutcome, PlaneError> {
        let source = SourceRef::new(system, record);
        let actions: Vec<ExecutedAction> = self
            .records
            .iter()
            .filter_map(ActionRecord::to_executed)
            .collect();
        let taint_config = self.taint_config();
        let lineage = self.lineage()?;
        let semantic_tables = self.semantic_table_names();
        let mut healer = LineageHealer {
            operator: &self.operator,
            lineage,
            semantic_tables,
            membership: &mut self.membership,
        };
        let outcome = mutiny_taint::taint(
            &mut self.engine,
            &taint_config,
            &source,
            &actions,
            &mut healer,
        )?;
        self.metrics.inc(&format!(
            "mutiny_taint_runs_total{{tenant=\"{}\"}}",
            self.name
        ));
        self.metrics.add(
            &format!(
                "mutiny_taint_rows_resolved_total{{tenant=\"{}\"}}",
                self.name
            ),
            outcome.resolved as u64,
        );
        self.metrics.add(
            &format!(
                "mutiny_taint_semantic_healed_total{{tenant=\"{}\"}}",
                self.name
            ),
            outcome.semantic_healed as u64,
        );
        trace(
            "taint",
            &[
                ("tenant", self.name.clone()),
                ("source", format!("{system}:{record}")),
                ("resolved", outcome.resolved.to_string()),
            ],
        );
        Ok(outcome)
    }

    // ---- ops -----------------------------------------------------------------------------------

    pub fn health(&self) -> String {
        format!(
            "surface {}\nquarantine {}\ntenant {}\ncommit_seq {}\nembedding {}:{}\n{}",
            crate::config::SURFACE_VERSION,
            crate::config::QUARANTINE_NOTICE,
            self.name,
            self.commit_seq,
            self.embedding.dim,
            self.embedding.version,
            self.engine.health()
        )
    }

    pub fn engine_epoch(&self) -> u64 {
        self.engine.epoch()
    }

    pub fn shutdown(&mut self) -> Result<schweep_server::Drained, PlaneError> {
        self.engine.shutdown().map_err(PlaneError::from)
    }

    // ---- internals -----------------------------------------------------------------------------

    fn lineage(&self) -> Result<Lineage, PlaneError> {
        Lineage::from_events(self.lineage_events.iter().cloned()).map_err(PlaneError::from)
    }

    fn table_config(&self, table: &str) -> Option<&TableConfig> {
        self.config.tables.iter().find(|t| t.name == table)
    }

    fn semantic_spec(&self, table: &str) -> Option<SemanticSpec> {
        let config = self.table_config(table)?;
        let semantic = config.semantic.as_ref()?;
        Some(SemanticSpec {
            key: column_index(config, &config.key_column).ok()?,
            body: column_index(config, &semantic.body_column).ok()?,
            event_time: column_index(config, &semantic.event_time_column).ok()?,
            cost_micros: column_index(config, &semantic.cost_micros_column).ok()?,
            error: column_index(config, &semantic.error_column).ok()?,
        })
    }

    fn semantic_table_names(&self) -> BTreeSet<String> {
        self.config
            .tables
            .iter()
            .filter(|t| t.semantic.is_some())
            .map(|t| t.name.clone())
            .collect()
    }

    fn semantic_delta(
        &self,
        spec: &SemanticSpec,
        row: &Row,
        weight: i64,
    ) -> Result<SemanticDelta, PlaneError> {
        let text = |index: usize| match row.get(index) {
            Some(Value::Str(value)) => Ok(value.clone()),
            other => Err(PlaneError::Internal(format!(
                "semantic column {index} is {other:?}"
            ))),
        };
        let int = |index: usize| match row.get(index) {
            Some(Value::Int(value)) => Ok(*value),
            other => Err(PlaneError::Internal(format!(
                "semantic column {index} is {other:?}"
            ))),
        };
        let boolean = |index: usize| match row.get(index) {
            Some(Value::Bool(value)) => Ok(*value),
            other => Err(PlaneError::Internal(format!(
                "semantic column {index} is {other:?}"
            ))),
        };
        let body = text(spec.body)?;
        let vector = self
            .embedder
            .embed(&body)
            .map_err(|e| PlaneError::Semantic(e.to_string()))?;
        let record = SemanticRecord::new(
            text(spec.key)?,
            space(&self.embedder),
            vector,
            ScalarColumns {
                tenant: self.name.clone(),
                event_time: int(spec.event_time)?,
                cost: int(spec.cost_micros)? as f64 / 1_000_000.0,
                error: boolean(spec.error)?,
            },
        )
        .map_err(|e| PlaneError::Semantic(e.to_string()))?;
        Ok(SemanticDelta { record, weight })
    }

    fn apply_semantic(
        &mut self,
        branch: &str,
        table: &str,
        key: &str,
        delta: SemanticDelta,
    ) -> Result<(), PlaneError> {
        self.membership
            .entry(branch.to_owned())
            .or_default()
            .insert((table.to_owned(), key.to_owned()));
        let token = self
            .tokens
            .get(branch)
            .ok_or_else(|| PlaneError::Internal(format!("no capability for branch {branch:?}")))?;
        let id = BranchId::new(branch);
        for topk in &self.config.semantic_standing.topk {
            self.agent
                .apply_semantic_epoch(token, &id, &topk.id, [delta.clone()])?;
        }
        for groups in &self.config.semantic_standing.groups {
            self.agent
                .apply_group_epoch(token, &id, &groups.id, [delta.clone()])?;
        }
        Ok(())
    }

    fn taint_config(&self) -> TaintConfig {
        TaintConfig {
            tenant: self.name.clone(),
            tables: self
                .config
                .tables
                .iter()
                .map(|table| {
                    (
                        table.name.clone(),
                        TaintTableSpec {
                            plane: table.plane.clone(),
                            key_column: table.key_column.clone(),
                            branch_column: table.branch_column.clone(),
                            key_type: mutiny_taint::KeyType::Utf8,
                        },
                    )
                })
                .collect(),
        }
    }

    fn table_rows(&mut self, sql: &str) -> Result<Vec<Row>, PlaneError> {
        let handle = self.engine.register(sql, Admission::bounded())?;
        let frames = self.engine.read_frames(handle);
        let deregistered = self.engine.deregister(handle);
        let bytes = frames?;
        deregistered?;
        let (record, _) = read_framed(&bytes, 0)
            .map_err(|e| PlaneError::Internal(e.to_string()))?
            .ok_or_else(|| PlaneError::Internal("torn answer frame".to_owned()))?;
        let Record::Append { entries, .. } = record else {
            return Err(PlaneError::Internal("not an append record".to_owned()));
        };
        Ok(entries
            .into_iter()
            .filter(|(_, weight)| *weight > 0)
            .map(|(row, _)| row)
            .collect())
    }
}

struct SemanticSpec {
    key: usize,
    body: usize,
    event_time: usize,
    cost_micros: usize,
    error: usize,
}

/// The lineage-cascading healer: heal the writing branch and every active descendant, for tables
/// that feed the semantic plane (the M5 law, config-driven).
struct LineageHealer<'a> {
    operator: &'a OperatorTrustPlane,
    lineage: Lineage,
    semantic_tables: BTreeSet<String>,
    /// The plane's membership mirror: a heal removes what it retracted, on every holding branch,
    /// so the sleep checkpoint never records a row the stores no longer hold.
    membership: &'a mut BTreeMap<String, BTreeSet<(String, String)>>,
}

impl SemanticHealer for LineageHealer<'_> {
    fn heal(&mut self, branch: &str, table: &str, keys: &[String]) -> Result<usize, String> {
        if !self.semantic_tables.contains(table) {
            return Ok(0);
        }
        let mut branches = vec![branch.to_owned()];
        branches.extend(self.lineage.active_descendants(branch));
        let mut healed = 0;
        for holder in &branches {
            healed += self
                .operator
                .heal_semantic(&BranchId::new(holder.as_str()), keys)
                .map_err(|e| e.to_string())?;
            if let Some(held) = self.membership.get_mut(holder) {
                for key in keys {
                    held.remove(&(table.to_owned(), key.clone()));
                }
            }
        }
        Ok(healed)
    }
}

fn space(embedder: &HashEmbedder) -> String {
    format!("{}:{}", embedder.model_id(), embedder.model_version())
}

fn build_catalog(config: &TenantConfig) -> Result<BTreeMap<String, Schema>, PlaneError> {
    let mut catalog = BTreeMap::new();
    for table in &config.tables {
        let fields = table
            .columns
            .iter()
            .map(|(name, kind)| {
                let data_type = match kind.as_str() {
                    "utf8" => DataType::Utf8,
                    "int64" => DataType::Int64,
                    "bool" => DataType::Boolean,
                    other => {
                        return Err(PlaneError::Refused(format!(
                            "unknown column type {other:?}"
                        )))
                    }
                };
                Ok(Field::not_null(name, data_type))
            })
            .collect::<Result<Vec<_>, PlaneError>>()?;
        let schema = Schema::new_table(fields).map_err(|e| PlaneError::Internal(e.to_string()))?;
        catalog.insert(table.name.clone(), schema);
    }
    catalog.insert(
        mutiny_bridge::DERIVATION_TABLE.to_owned(),
        mutiny_bridge::derivation_schema().map_err(PlaneError::from)?,
    );
    catalog.insert(
        mutiny_taint::LEDGER_TABLE.to_owned(),
        TaintConfig::ledger_schema().map_err(PlaneError::from)?,
    );
    catalog.insert(
        FORKS_TABLE.to_owned(),
        mutiny_forks::forks_schema().map_err(PlaneError::from)?,
    );
    Ok(catalog)
}

fn column_index(table: &TableConfig, column: &str) -> Result<usize, PlaneError> {
    table
        .columns
        .iter()
        .position(|(name, _)| name == column)
        .ok_or_else(|| {
            PlaneError::Internal(format!("column {column:?} missing from {:?}", table.name))
        })
}

fn decode_row(table: &TableConfig, values: &[serde_json::Value]) -> Result<Row, PlaneError> {
    if values.len() != table.columns.len() {
        return Err(PlaneError::Refused(format!(
            "table {:?} takes {} columns, the row carries {}",
            table.name,
            table.columns.len(),
            values.len()
        )));
    }
    let mut row = Vec::with_capacity(values.len());
    for ((name, kind), value) in table.columns.iter().zip(values) {
        let typed = match (kind.as_str(), value) {
            ("utf8", serde_json::Value::String(text)) => Value::Str(text.clone()),
            ("int64", serde_json::Value::Number(number)) => Value::Int(
                number
                    .as_i64()
                    .ok_or_else(|| PlaneError::Refused(format!("{name}: not an int64")))?,
            ),
            ("bool", serde_json::Value::Bool(flag)) => Value::Bool(*flag),
            (kind, other) => {
                return Err(PlaneError::Refused(format!(
                    "column {name:?} expects {kind}, the row carries {other}"
                )))
            }
        };
        row.push(typed);
    }
    Ok(Row::new(row))
}

/// The delta->circuit mapping, observed from the compute plane without waking anything: the
/// engine's persisted registration file plus the public binder (MD-1 R2, docs/M7-FLEET.md).
pub fn circuit_mapping_for(
    compute_dir: &Path,
    config: &TenantConfig,
) -> Result<BTreeMap<String, BTreeSet<String>>, PlaneError> {
    let mut mapping: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let registry = schweep_server::Registry::load(compute_dir)?;
    let catalog = build_catalog(config)?;
    for entry in registry.entries.values() {
        let bound = schweep_sql::bind_sql(&entry.sql, &catalog)
            .map_err(|e| PlaneError::Internal(format!("registered SQL failed to bind: {e}")))?;
        let mut tables = BTreeSet::new();
        collect_tables(&bound.query.source, &mut tables);
        mapping.insert(format!("handle-{}", entry.handle), tables);
    }
    let semantic_tables: BTreeSet<String> = config
        .tables
        .iter()
        .filter(|t| t.semantic.is_some())
        .map(|t| t.name.clone())
        .collect();
    for topk in &config.semantic_standing.topk {
        mapping.insert(format!("semantic-{}", topk.id), semantic_tables.clone());
    }
    for groups in &config.semantic_standing.groups {
        mapping.insert(format!("groups-{}", groups.id), semantic_tables.clone());
    }
    Ok(mapping)
}

fn collect_tables(source: &schweep_plan::Source, out: &mut BTreeSet<String>) {
    match source {
        schweep_plan::Source::Scan { table, .. } => {
            out.insert(table.clone());
        }
        schweep_plan::Source::Join { left, right, .. } => {
            collect_tables(left, out);
            collect_tables(right, out);
        }
    }
}

fn decode_hex_utf8(hex: &str) -> Option<String> {
    if !hex.len().is_multiple_of(2) {
        return None;
    }
    let bytes: Option<Vec<u8>> = (0..hex.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(hex.get(at..at + 2)?, 16).ok())
        .collect();
    String::from_utf8(bytes?).ok()
}
