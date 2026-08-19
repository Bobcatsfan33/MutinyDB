//! The dev-only composed host: substrate storage → M1 bridge translation → C9 compute engine →
//! branch-scoped semantic plane behind the M3 trust mount → Loom's real action gateway. It exists
//! for the frozen incident corpus and its gate; it is **not** the supported `mutinyd` (M6).
//!
//! Every write takes the real front door: a substrate transaction with the bounded logical
//! capture (`commit_with_capture`), the bridge's exact prepared batches, and the engine's
//! append-then-seal admission — the arrangement MD-2 ask 3 already ruled sound because this host
//! is the store's only writer. One deliberate M4 bound, named in `docs/M4-TAINT.md`: retraction
//! and ledger epochs are engine-native, so the strict epoch=commit bijection holds for the ingest
//! phase and this host refuses storage commits after a taint rather than mislabeling them.

use crate::corpus::{
    self, Corpus, CorpusAction, CorpusCommit, CorpusOp, CLAIMS, TELEMETRY, TENANT,
};
use loom_action::{ActionRecord, Connector, ConnectorOutcome};
use loom_branch::{CapabilityToken, Loom, MAIN};
use loom_core::{
    ActorId, BranchId, Claim, ClaimId, ClaimStatus, Confidence, ExecutedAction, Interval, Method,
    SessionId, SourceRef, TenantId, Timestamp, TrustClass, WriteEnvelope,
};
use loom_policy::{Effect, Match, PolicyRule, PolicySet, Request, PURPOSE_AUTHORIZE};
use mutiny_bridge::{
    commit_with_capture, prepared_batches, recover_pending_captures, CapturedChange, CapturedTable,
    CommitCapture, CommitDraft, EnvelopeAuthority, EnvelopeId,
};
use mutiny_forks::{
    lineage_source, merge_marker_source, ForkEvent, ForkKind, Lineage, FORKS_TABLE,
};
use mutiny_semantic::{
    ScalarColumns, ScalarPredicate, SemanticDelta, SemanticGroupPlan, SemanticGroups,
    SemanticQuery, SemanticRecord, SemanticTopK,
};
use mutiny_taint::{SemanticHealer, TaintOutcome};
use mutiny_trust::{mount, AgentTrustPlane, OperatorTrustPlane};
use prism_types::{Embedder, HashEmbedder};
use schweep_log::record::{read_framed, Record};
use schweep_log::{Ack, SyncPolicy};
use schweep_memo::Admission;
use schweep_server::{Engine, Policy};
use schweep_zset::{Row, Value};
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::Arc;

pub const TOPK_QUERY: &str = "incident-similar";
pub const GROUPS_QUERY: &str = "incident-groups";
pub const EMBED_DIM: usize = 16;
pub const EMBED_VERSION: &str = "m4-v1";
pub const TOPK_TEXT: &str = "urgent credential compromise investigation";

/// The engine standing queries, in their fixed registration order. Handles are dense from the
/// registry's initial counter, so a reopened host recomputes them; the constructor asserts it.
pub const STANDING: [(&str, &str); 5] = [
    (
        "claims_current",
        "SELECT claims.claim_id AS claim_id, claims.branch AS branch, claims.subject AS subject, \
         claims.asserts AS asserts, claims.confidence_bp AS confidence_bp FROM claims",
    ),
    (
        "telemetry_current",
        "SELECT telemetry.event_id AS event_id, telemetry.branch AS branch, telemetry.body AS \
         body, telemetry.cost_micros AS cost_micros, telemetry.error AS error, \
         telemetry.event_time AS event_time FROM telemetry",
    ),
    (
        "derivation_current",
        "SELECT mutiny_derivation.tenant AS tenant, mutiny_derivation.branch AS branch, \
         mutiny_derivation.table_name AS table_name, mutiny_derivation.row_key AS row_key, \
         mutiny_derivation.source_system AS source_system, mutiny_derivation.source_record AS \
         source_record, mutiny_derivation.envelope AS envelope FROM mutiny_derivation",
    ),
    (
        "cost_by_branch",
        "SELECT telemetry.branch AS branch, SUM(telemetry.cost_micros) AS total_cost, COUNT(*) \
         AS events FROM telemetry GROUP BY telemetry.branch",
    ),
    (
        "claims_by_subject",
        "SELECT claims.subject AS subject, COUNT(*) AS supporting FROM claims GROUP BY \
         claims.subject",
    ),
];

#[derive(Debug, thiserror::Error)]
pub enum HostError {
    #[error("storage: {0}")]
    Storage(String),
    #[error(transparent)]
    Bridge(#[from] mutiny_bridge::BridgeError),
    #[error(transparent)]
    Engine(#[from] schweep_server::ServerError),
    #[error(transparent)]
    Taint(#[from] mutiny_taint::TaintError),
    #[error(transparent)]
    Trust(#[from] mutiny_trust::TrustError),
    #[error("semantic: {0}")]
    Semantic(String),
    #[error("corpus: {0}")]
    Corpus(String),
    #[error(transparent)]
    Fork(#[from] mutiny_forks::ForkError),
    #[error("the merge was refused, all-or-nothing, before any write: {0}")]
    MergeRefused(String),
    #[error("the composed host is inconsistent: {0}")]
    Composition(String),
}

/// The trust-plane admission set: envelopes registered at commit time, admitted by exact id.
#[derive(Debug, Default)]
pub struct SetAuthority {
    known: RefCell<BTreeSet<EnvelopeId>>,
}

impl SetAuthority {
    fn register(&self, id: EnvelopeId) {
        self.known.borrow_mut().insert(id);
    }
}

impl EnvelopeAuthority for SetAuthority {
    fn admit(&self, id: EnvelopeId, _envelope: &WriteEnvelope) -> Result<(), String> {
        if self.known.borrow().contains(&id) {
            Ok(())
        } else {
            Err("envelope is not in the trust-plane registry".to_owned())
        }
    }
}

/// The deterministic incident connector: a real Loom connector with a registered compensation.
struct SuspendConnector;

impl Connector for SuspendConnector {
    fn action_type(&self) -> &str {
        "identity.suspend_account"
    }

    fn compensating_action(&self) -> Option<String> {
        Some("identity.reinstate_account".to_owned())
    }

    fn execute(&self, target: &str, _idempotency_key: &str) -> ConnectorOutcome {
        ConnectorOutcome::Succeeded {
            receipt: format!("okta:suspend:{target}"),
        }
    }
}

/// One candidate row for a merge: (epoch it landed at, its key, and its table/row/sources).
type MergeCandidate = (u64, String, (String, Row, Vec<SourceRef>));

/// The action every merged standing write is re-evaluated as, at merge time (Loom AT-016,
/// composed): a merge is a new write on the target, judged against policy as it is *now*.
pub const MERGE_ACTION: &str = "standing.merge";

fn policy() -> PolicySet {
    PolicySet::new(
        "m5-policy-v1",
        vec![
            PolicyRule {
                actor: Match::Any,
                label: Match::Is(TrustClass::Untrusted),
                purpose: Match::Is(PURPOSE_AUTHORIZE.to_owned()),
                action: Match::Is("identity.suspend_account".to_owned()),
                effect: Effect::Deny,
            },
            PolicyRule {
                actor: Match::Any,
                label: Match::Is(TrustClass::Untrusted),
                purpose: Match::Is(PURPOSE_AUTHORIZE.to_owned()),
                action: Match::Is(MERGE_ACTION.to_owned()),
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

/// Where one host's two durable planes live.
#[derive(Clone, Debug)]
pub struct HostPaths {
    pub storage: PathBuf,
    pub compute: PathBuf,
}

/// The composed world.
pub struct Host {
    pub store: substrate_wal::DurableStore,
    pub engine: Engine,
    pub agent: AgentTrustPlane,
    pub operator: OperatorTrustPlane,
    pub authority: SetAuthority,
    pub embedder: HashEmbedder,
    /// Branch name → the capability covering it.
    pub tokens: BTreeMap<String, CapabilityToken>,
    /// Standing query name → engine handle, in `STANDING` order.
    pub standing: BTreeMap<&'static str, u64>,
    /// The storage commit clock (dense, per MD-2 R1). Frozen once a taint has run.
    pub commit_seq: u64,
    /// (parent state bytes hydrated, nanoseconds) per durable fork — the O(state) evidence.
    pub fork_samples: Vec<(usize, u128)>,
    tainted: bool,
    records: Vec<ActionRecord>,
    branches: Vec<String>,
    /// Live mirror of the durable `mutiny_forks` relation, in commit order. Rebuilt by replay.
    lineage_events: Vec<ForkEvent>,
}

/// The embedding space every M4 semantic operator is pinned to.
#[must_use]
pub fn space() -> String {
    let embedder = HashEmbedder::with_version(EMBED_DIM, EMBED_VERSION);
    format!("{}:{}", embedder.model_id(), embedder.model_version())
}

impl Host {
    /// Open every plane and stand up sessions, forks, standing queries, and semantic operators —
    /// with no data yet. `ingest_commit` and `execute_action` drive the corpus through it.
    pub fn open(paths: &HostPaths, corpus: &Corpus) -> Result<Host, HostError> {
        let store = substrate_wal::DurableStore::open(
            substrate_pager::std_vfs(),
            &paths.storage,
            substrate_pager::StoreConfig::default(),
        )
        .map_err(|error| HostError::Storage(error.to_string()))?;
        store
            .recover()
            .map_err(|error| HostError::Storage(error.to_string()))?;

        let catalog = corpus::catalog().map_err(HostError::Corpus)?;
        let mut engine = Engine::open(
            &paths.compute,
            catalog,
            Policy::default(),
            SyncPolicy::Full,
            0,
        )?;

        let fresh = engine.epoch() == 0;
        let mut standing = BTreeMap::new();
        if fresh {
            for (name, sql) in STANDING {
                let handle = engine.register(sql, Admission::bounded())?;
                standing.insert(name, handle);
            }
        } else {
            // The registry file rebuilt them; the handles are dense from zero in registration
            // order because `open` registered them first, before any taint resolution queries.
            for (index, (name, _)) in STANDING.iter().enumerate() {
                standing.insert(*name, index as u64);
            }
            let probe = standing[&STANDING[0].0];
            engine.read(probe)?;
        }

        let embedder = HashEmbedder::with_version(EMBED_DIM, EMBED_VERSION);
        let db = Arc::new(
            Loom::in_memory(TenantId::new(TENANT)).map_err(mutiny_trust::TrustError::from)?,
        );
        let (agent, operator) = mount(
            db,
            TENANT,
            policy(),
            vec![Box::new(SuspendConnector) as Box<dyn Connector>],
        );

        let topk = SemanticTopK::new(
            SemanticQuery::new(
                TOPK_QUERY,
                space(),
                embedder
                    .embed(TOPK_TEXT)
                    .map_err(|error| HostError::Semantic(error.to_string()))?,
                3,
                ScalarPredicate::default(),
            )
            .map_err(|error| HostError::Semantic(error.to_string()))?,
        );
        let groups = SemanticGroups::new(
            SemanticGroupPlan::new(
                space(),
                vec![
                    embedder
                        .embed("urgent security incident")
                        .map_err(|error| HostError::Semantic(error.to_string()))?,
                    embedder
                        .embed("routine operations")
                        .map_err(|error| HostError::Semantic(error.to_string()))?,
                ],
                ScalarPredicate::default(),
            )
            .map_err(|error| HostError::Semantic(error.to_string()))?,
        );
        operator.install_standing(&BranchId::new(MAIN), topk)?;
        operator.install_groups(&BranchId::new(MAIN), GROUPS_QUERY, groups)?;

        let mut tokens = BTreeMap::new();
        let mut branches = Vec::new();
        for session in &corpus.sessions {
            let (handle, token) = agent.open_session_named(SessionId::new(session.as_str()))?;
            if handle.branch.as_str() != session {
                return Err(HostError::Composition(format!(
                    "session {session:?} opened branch {:?}",
                    handle.branch.as_str()
                )));
            }
            tokens.insert(session.clone(), token);
            branches.push(session.clone());
        }
        for (from, name) in &corpus.forks {
            let from_token = tokens.get(from).ok_or_else(|| {
                HostError::Composition(format!("fork {name:?} from unknown branch {from:?}"))
            })?;
            let (branch, token) = agent.branch(from_token, &BranchId::new(from.as_str()), name)?;
            if branch.as_str() != name {
                return Err(HostError::Composition(format!(
                    "fork {name:?} created branch {:?}",
                    branch.as_str()
                )));
            }
            tokens.insert(name.clone(), token);
            branches.push(name.clone());
        }
        branches.sort();

        Ok(Host {
            store,
            engine,
            agent,
            operator,
            authority: SetAuthority::default(),
            embedder,
            tokens,
            standing,
            commit_seq: 0,
            fork_samples: Vec::new(),
            tainted: false,
            records: Vec::new(),
            branches,
            lineage_events: Vec::new(),
        })
    }

    /// Build the full corpus world: every ordered operation through the real front door — commits,
    /// durable forks, merges, rewinds — then the actions.
    pub fn build(paths: &HostPaths, corpus: &Corpus) -> Result<Host, HostError> {
        let mut host = Host::open(paths, corpus)?;
        for op in &corpus.ops {
            host.apply_op(op)?;
        }
        for action in &corpus.actions {
            host.execute_action(action)?;
        }
        Ok(host)
    }

    /// Apply one ordered corpus operation. Lifecycle operations use the session's name as the
    /// acting identity on their bookkeeping envelopes.
    pub fn apply_op(&mut self, op: &CorpusOp) -> Result<(), HostError> {
        match op {
            CorpusOp::Commit(commit) => self.ingest_commit(commit),
            CorpusOp::Fork {
                session,
                from,
                child,
            } => self.fork_durable(session, session, from, child),
            CorpusOp::Rewind { session, child } => {
                self.rewind_durable(session, session, child).map(|_| ())
            }
            CorpusOp::Merge {
                session,
                child,
                into,
            } => self
                .merge_durable(
                    session,
                    session,
                    child,
                    into,
                    TrustClass::VerifiedSystem,
                    None,
                )
                .map(|_| ()),
        }
    }

    /// The live lineage, folded from the mirror of the durable relation.
    pub fn lineage(&self) -> Result<Lineage, HostError> {
        Lineage::from_events(self.lineage_events.iter().cloned()).map_err(HostError::from)
    }

    /// **Durable fork** (M5, MD-5 Option B): one ordinary commit on the parent's timeline records
    /// the fork, then the child's standing operators are hydrated from the parent's live state —
    /// O(state), measured into `fork_samples`, never claimed O(1).
    pub fn fork_durable(
        &mut self,
        session: &str,
        actor: &str,
        from: &str,
        child: &str,
    ) -> Result<(), HostError> {
        let lineage = self.lineage()?;
        if lineage.fork_of(child).is_some() {
            if self.tokens.contains_key(child) {
                return Ok(());
            }
            return Err(HostError::Composition(format!(
                "fork record for {child:?} exists without live standing state; reopen the host \
                 so replay rebuilds it"
            )));
        }
        let event = ForkEvent {
            child: child.to_owned(),
            parent: from.to_owned(),
            at_epoch: self.commit_seq + 1,
            kind: ForkKind::Fork,
        };
        let commit = CorpusCommit {
            session: session.to_owned(),
            branch: from.to_owned(),
            actor: actor.to_owned(),
            table: FORKS_TABLE.to_owned(),
            sources: vec![lineage_source(from)],
            key: child.to_owned(),
            row: event.to_row(),
        };
        self.ingest_commit(&commit)?;
        self.hydrate_fork(&event)
    }

    /// The hydration half of a fork: Loom branch + standing-state clone, measured. Shared by the
    /// live path and replay so a recovered fork is built by exactly the code that built it live.
    fn hydrate_fork(&mut self, event: &ForkEvent) -> Result<(), HostError> {
        let from_token = self.tokens.get(&event.parent).ok_or_else(|| {
            HostError::Composition(format!("fork from unknown branch {:?}", event.parent))
        })?;
        let parent_bytes = self
            .operator
            .branch_state_bytes(&BranchId::new(event.parent.as_str()))?;
        let started = std::time::Instant::now();
        let (branch, token) = self.agent.branch(
            from_token,
            &BranchId::new(event.parent.as_str()),
            &event.child,
        )?;
        self.fork_samples
            .push((parent_bytes, started.elapsed().as_nanos()));
        if branch.as_str() != event.child {
            return Err(HostError::Composition(format!(
                "fork of {:?} created branch {:?}",
                event.child,
                branch.as_str()
            )));
        }
        self.tokens.insert(event.child.clone(), token);
        self.branches.push(event.child.clone());
        self.branches.sort();
        self.lineage_events.push(event.clone());
        Ok(())
    }

    /// **Durable rewind**: recorded first, then the branch's standing state is torn down and its
    /// bytes return to the mount's baseline. Loom's branch and every committed row remain —
    /// auditable, never destroyed. Returns the standing-state bytes freed.
    pub fn rewind_durable(
        &mut self,
        session: &str,
        actor: &str,
        child: &str,
    ) -> Result<usize, HostError> {
        let lineage = self.lineage()?;
        let Some((parent, _)) = lineage.fork_of(child) else {
            return Err(HostError::Fork(mutiny_forks::ForkError::UnknownBranch {
                branch: child.to_owned(),
            }));
        };
        let parent = parent.to_owned();
        if lineage.rewound_at(child).is_none() {
            let event = ForkEvent {
                child: child.to_owned(),
                parent: parent.clone(),
                at_epoch: self.commit_seq + 1,
                kind: ForkKind::Rewind,
            };
            let commit = CorpusCommit {
                session: session.to_owned(),
                branch: child.to_owned(),
                actor: actor.to_owned(),
                table: FORKS_TABLE.to_owned(),
                sources: vec![lineage_source(&parent)],
                key: format!("{child}:rewind"),
                row: event.to_row(),
            };
            self.ingest_commit(&commit)?;
            self.lineage_events.push(event);
        }
        let freed = self.operator.rewind_branch(&BranchId::new(child))?;
        self.tokens.remove(child);
        self.branches.retain(|branch| branch != child);
        Ok(freed)
    }

    /// **Durable merge of the child's post-fork divergence into `into`, per Loom's law**: new
    /// commits on the target through the front door; every merged write re-evaluated against
    /// policy *now*, all-or-nothing; each merged row keeps its **own** original sources (Loom
    /// I-2) plus the durable merge marker that makes a repeated or crash-resumed merge a no-op —
    /// the +6-not-+3 double-count class dies on that marker. `limit` is the crash-injection hook:
    /// `Some(k)` commits at most k rows and then fails as an interrupted process would.
    pub fn merge_durable(
        &mut self,
        session: &str,
        actor: &str,
        child: &str,
        into: &str,
        label: TrustClass,
        limit: Option<usize>,
    ) -> Result<usize, HostError> {
        let lineage = self.lineage()?;
        // Candidate rows: the child's commits, straight from the durable manifest history.
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
                    let key = String::from_utf8(change.primary_key.clone()).map_err(|_| {
                        HostError::Composition(format!(
                            "merge candidate in {table:?} has a non-utf8 key"
                        ))
                    })?;
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

        // What a previous (possibly crashed) merge already landed: the durable marker.
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

        // Policy, re-run at merge time, all-or-nothing: if any write is forbidden, nothing lands.
        for (_, key, (table, _, _)) in &plan {
            let decision = self.agent.decide(&Request {
                actor: actor.to_owned(),
                label,
                purpose: PURPOSE_AUTHORIZE.to_owned(),
                action: MERGE_ACTION.to_owned(),
            });
            if decision.decision != loom_policy::Decision::Allow {
                return Err(HostError::MergeRefused(format!(
                    "policy denies {MERGE_ACTION} for {table}/{key} under label {label:?}; \
                     no write was made"
                )));
            }
        }

        let mut merged = 0usize;
        for (index, (_, key, (table, row, sources))) in plan.iter().enumerate() {
            if let Some(bound) = limit {
                if index >= bound {
                    return Err(HostError::Composition(
                        "merge interrupted by the injected crash hook".to_owned(),
                    ));
                }
            }
            let mut values = row.values().to_vec();
            let branch_column = 1;
            values[branch_column] = Value::Str(into.to_owned());
            let mut merged_sources = sources.clone();
            merged_sources.push(marker.clone());
            let commit = CorpusCommit {
                session: session.to_owned(),
                branch: into.to_owned(),
                actor: actor.to_owned(),
                table: table.clone(),
                sources: merged_sources,
                key: key.clone(),
                row: Row::new(values),
            };
            self.ingest_commit(&commit)?;
            merged += 1;
        }
        Ok(merged)
    }

    /// Ad-hoc typed read of one query's current answer, through the frame door (the taint core's
    /// register/read/deregister lifecycle).
    fn table_rows(&mut self, sql: &str) -> Result<Vec<Row>, HostError> {
        let handle = self.engine.register(sql, Admission::bounded())?;
        let frames = self.engine.read_frames(handle);
        let deregistered = self.engine.deregister(handle);
        let bytes = frames?;
        deregistered?;
        rows_from_frames(&bytes)
    }

    /// One corpus write: one substrate commit with its bounded capture, one bridge translation,
    /// one compute epoch, and the branch's semantic operators fed the same delta.
    pub fn ingest_commit(&mut self, commit: &CorpusCommit) -> Result<(), HostError> {
        if self.tainted {
            return Err(HostError::Composition(
                "this M4 host refuses storage commits after a taint: retraction epochs are \
                 engine-native and would break the epoch=commit bijection (docs/M4-TAINT.md)"
                    .to_owned(),
            ));
        }
        let seq = self.commit_seq + 1;
        let page: substrate_pager::LogicalPageNo = seq;
        let mut txn = self
            .store
            .begin()
            .map_err(|error| HostError::Storage(error.to_string()))?;
        self.store
            .write(
                &mut txn,
                page,
                format!("{}/{}/{seq}", commit.table, commit.key).into_bytes(),
            )
            .map_err(|error| HostError::Storage(error.to_string()))?;

        let envelope = WriteEnvelope::new(
            ActorId::new(commit.actor.as_str()),
            SessionId::new(commit.session.as_str()),
            BranchId::new(commit.branch.as_str()),
            format!("record {} {}", commit.table, commit.key),
        )
        .derived_from(commit.sources.iter().cloned());
        self.authority.register(EnvelopeId::of(&envelope));

        let draft = CommitDraft {
            tenant: TenantId::new(TENANT),
            plane: corpus::plane_of(&commit.table).to_owned(),
            commit_seq: seq,
            branch: BranchId::new(commit.branch.as_str()),
            envelope,
            tables: BTreeMap::from([(
                commit.table.clone(),
                CapturedTable {
                    changes: vec![CapturedChange {
                        row: commit.row.clone(),
                        weight: 1,
                        primary_key: commit.key.as_bytes().to_vec(),
                        pages: BTreeSet::from([page]),
                    }],
                },
            )]),
        };
        let capture = commit_with_capture(
            &self.store,
            txn,
            &draft,
            self.engine.catalog(),
            &self.authority,
        )?;
        self.apply_capture(&capture)?;
        self.commit_seq = seq;

        if commit.table == TELEMETRY {
            let delta = self.telemetry_delta(&commit.row, 1)?;
            self.apply_semantic(&commit.branch, vec![delta])?;
        }
        Ok(())
    }

    /// Drive one bridge-prepared capture through the engine's admission door and seal exactly the
    /// storage commit's epoch. Replays (recovery) are acknowledged and not resealed.
    pub fn apply_capture(&mut self, capture: &CommitCapture) -> Result<(), HostError> {
        let (_, batches) = prepared_batches(capture, &self.authority)?;
        let sealed = self.engine.epoch();
        if capture.commit_seq <= sealed {
            for batch in &batches {
                let ack = self.engine.ingest(
                    &batch.source_id,
                    &batch.table,
                    &batch.dedup_token,
                    batch.entries.clone(),
                )?;
                if ack != Ack::DroppedAsReplay {
                    return Err(HostError::Composition(format!(
                        "commit {} claims epoch {} but its batches were not replays",
                        capture.commit.to_hex(),
                        capture.commit_seq
                    )));
                }
            }
            return Ok(());
        }
        if capture.commit_seq != sealed + 1 {
            return Err(HostError::Composition(format!(
                "commit sequence gap: engine sealed {sealed}, capture supplies {}",
                capture.commit_seq
            )));
        }
        for batch in &batches {
            self.engine.ingest(
                &batch.source_id,
                &batch.table,
                &batch.dedup_token,
                batch.entries.clone(),
            )?;
        }
        let epoch = self.engine.seal()?;
        if epoch != capture.commit_seq {
            return Err(HostError::Composition(format!(
                "epoch {epoch} sealed for storage commit {}",
                capture.commit_seq
            )));
        }
        Ok(())
    }

    /// One executed external action through Loom's real propose-then-execute separation.
    pub fn execute_action(&mut self, action: &CorpusAction) -> Result<ActionRecord, HostError> {
        let claims: Vec<Claim> = action
            .justified_by
            .iter()
            .map(|key| Claim {
                id: ClaimId::of(key.as_bytes()),
                predicate: "requires_suspension".to_owned(),
                subject: action.target.clone(),
                object: loom_core::Value::Bool(true),
                valid: Interval::from(Timestamp::from_ms(1)),
                known: Interval::from(Timestamp::from_ms(1)),
                confidence: Confidence::new(0.99, Method::Rule, "v1"),
                evidence: vec![SourceRef::new("mutiny", "claims")],
                status: ClaimStatus::Asserted,
                policy: None,
                actor: ActorId::new(action.actor.as_str()),
            })
            .collect();
        let justified: Vec<Vec<u8>> = action
            .justified_by
            .iter()
            .map(|key| key.as_bytes().to_vec())
            .collect();
        let proposal = self
            .agent
            .agent(
                ActorId::new(action.actor.as_str()),
                &BranchId::new(action.branch.as_str()),
                false,
            )
            .propose(
                &action.action_type,
                &action.target,
                &action.idempotency_key,
                claims,
                justified,
                TrustClass::VerifiedSystem,
            );
        let record = self.operator.execute(&proposal);
        if !record.status.is_success() {
            return Err(HostError::Composition(format!(
                "the corpus action did not execute: {:?}",
                record.status
            )));
        }
        self.records.push(record.clone());
        Ok(record)
    }

    /// Every terminal-success action, in the form taint consumes.
    #[must_use]
    pub fn executed_actions(&self) -> Vec<ExecutedAction> {
        self.records
            .iter()
            .filter_map(ActionRecord::to_executed)
            .collect()
    }

    /// **The one taint call.** Every plane heals from it; `docs/M4-TAINT.md` is the contract.
    pub fn taint(&mut self, source: &SourceRef) -> Result<TaintOutcome, HostError> {
        self.taint_with_faults(source, &mut mutiny_taint::TaintFaults::inert())
    }

    /// [`Self::taint`] with a planned interruption, for the M4 crash gate.
    pub fn taint_with_faults(
        &mut self,
        source: &SourceRef,
        faults: &mut mutiny_taint::TaintFaults,
    ) -> Result<TaintOutcome, HostError> {
        let actions = self.executed_actions();
        let config = corpus::taint_config();
        let mut healer = TrustHealer {
            operator: &self.operator,
            lineage: self.lineage()?,
        };
        let outcome = mutiny_taint::taint_with_faults(
            &mut self.engine,
            &config,
            source,
            &actions,
            &mut healer,
            faults,
        );
        // Even an interrupted taint may have advanced the engine clock; the ingest-phase
        // epoch=commit bijection is over either way.
        self.tainted = true;
        outcome.map_err(HostError::from)
    }

    /// Reopen after a crash: storage recovers, the engine replays its log and registry, pending
    /// storage commits are re-applied, the volatile semantic plane is rebuilt from the engine's
    /// current standing state, and the action ledger is re-driven through the idempotent gateway.
    pub fn reopen(paths: &HostPaths, corpus: &Corpus) -> Result<Host, HostError> {
        let mut host = Host::open(paths, corpus)?;

        // The manifest capture history is the durable log; replay it oldest-first through the fork
        // lineage. A fork record clones the parent's replayed state at exactly the point the fork
        // happened — inheritance falls out of the replay order — and a rewind record tears the
        // branch's state down again. Storage commits the compute plane has not sealed complete
        // through the same admission path they would have taken live.
        let head = host.store.head();
        let captures = recover_pending_captures(&host.store, head, 0)?;
        for capture in &captures {
            // The capture itself carries the envelope that was durably admitted at commit time;
            // recovery re-admits exactly that record.
            host.authority.register(EnvelopeId::of(&capture.envelope));
            host.apply_capture(capture)?;
            for (table, captured) in &capture.tables {
                if table == FORKS_TABLE {
                    for change in &captured.changes {
                        let event = ForkEvent::from_row(&change.row)?;
                        match event.kind {
                            ForkKind::Fork => host.hydrate_fork(&event)?,
                            ForkKind::Rewind => {
                                host.operator
                                    .rewind_branch(&BranchId::new(event.child.as_str()))?;
                                host.tokens.remove(&event.child);
                                host.branches.retain(|branch| branch != &event.child);
                                host.lineage_events.push(event);
                            }
                        }
                    }
                } else if table == TELEMETRY {
                    let branch = capture.branch.as_str().to_owned();
                    for change in &captured.changes {
                        let delta = host.telemetry_delta(&change.row, change.weight)?;
                        host.apply_semantic(&branch, vec![delta])?;
                    }
                }
            }
        }
        host.commit_seq = match mutiny_bridge::recover_capture(&host.store, head) {
            Ok(capture) => capture.commit_seq,
            Err(_) => 0,
        };

        // Then the heals the taint ledger recorded: engine-native epochs that never appear in the
        // manifest history. Each cascades through the lineage exactly as the live heal does.
        let ledger_sql = format!(
            "SELECT {t}.branch AS branch, {t}.table_name AS table_name, {t}.row_key AS row_key \
             FROM {t}",
            t = mutiny_taint::LEDGER_TABLE
        );
        let healed_rows = host.table_rows(&ledger_sql)?;
        if !healed_rows.is_empty() {
            host.tainted = true;
        }
        let lineage = host.lineage()?;
        let mut healer = TrustHealer {
            operator: &host.operator,
            lineage,
        };
        for row in healed_rows {
            let (Some(Value::Str(branch)), Some(Value::Str(table)), Some(Value::Str(row_key))) =
                (row.get(0), row.get(1), row.get(2))
            else {
                return Err(HostError::Composition(
                    "malformed taint ledger row".to_owned(),
                ));
            };
            let Some(key) = decode_hex_utf8(row_key) else {
                return Err(HostError::Composition(format!(
                    "taint ledger row key {row_key:?} does not decode"
                )));
            };
            healer
                .heal(branch, table, &[key])
                .map_err(HostError::Composition)?;
        }

        // The world's actions happened; the rebuilt gateway re-derives the same records.
        for action in &corpus.actions {
            host.execute_action(action)?;
        }
        Ok(host)
    }

    /// The typed rows of one engine standing answer, through the frame door.
    pub fn standing_rows(&self, name: &str) -> Result<Vec<Row>, HostError> {
        let handle = *self
            .standing
            .get(name)
            .ok_or_else(|| HostError::Composition(format!("unknown standing query {name:?}")))?;
        let bytes = self.engine.read_frames(handle)?;
        rows_from_frames(&bytes)
    }

    fn telemetry_delta(&self, row: &Row, weight: i64) -> Result<SemanticDelta, HostError> {
        let column = |index: usize| -> Result<&Value, HostError> {
            row.get(index).ok_or_else(|| {
                HostError::Composition(format!("telemetry row is missing column {index}"))
            })
        };
        let (
            Value::Str(key),
            Value::Str(body),
            Value::Int(cost),
            Value::Bool(error),
            Value::Int(time),
        ) = (column(0)?, column(2)?, column(3)?, column(4)?, column(5)?)
        else {
            return Err(HostError::Composition(
                "telemetry row has unexpected column types".to_owned(),
            ));
        };
        let vector = self
            .embedder
            .embed(body)
            .map_err(|error| HostError::Semantic(error.to_string()))?;
        let record = SemanticRecord::new(
            key.clone(),
            space(),
            vector,
            ScalarColumns {
                tenant: TENANT.to_owned(),
                event_time: *time,
                cost: *cost as f64 / 1_000_000.0,
                error: *error,
            },
        )
        .map_err(|error| HostError::Semantic(error.to_string()))?;
        Ok(SemanticDelta { record, weight })
    }

    fn apply_semantic(&self, branch: &str, deltas: Vec<SemanticDelta>) -> Result<(), HostError> {
        if deltas.is_empty() {
            return Ok(());
        }
        let token = self.tokens.get(branch).ok_or_else(|| {
            HostError::Composition(format!("no capability for branch {branch:?}"))
        })?;
        let branch = BranchId::new(branch);
        self.agent
            .apply_semantic_epoch(token, &branch, TOPK_QUERY, deltas.clone())?;
        self.agent
            .apply_group_epoch(token, &branch, GROUPS_QUERY, deltas)?;
        Ok(())
    }

    /// Every standing answer in the composed world, rendered canonically: the engine's own
    /// rendering for the compute plane, and a fixed deterministic layout for the branch-scoped
    /// semantic plane. This string is what the gate freezes and what the oracle must equal.
    pub fn standing_answers(&self) -> Result<String, HostError> {
        let mut out = String::new();
        for (name, _) in STANDING {
            let handle = self.standing[&name];
            let (_, answer) = self.engine.read(handle)?;
            let _ = writeln!(out, "== {name} ==");
            out.push_str(&answer);
            out.push('\n');
        }
        for branch in &self.branches {
            let token = &self.tokens[branch];
            let id = BranchId::new(branch.as_str());
            let hits = self.agent.answer(token, &id, TOPK_QUERY)?;
            let _ = writeln!(out, "== semantic:{branch}:{TOPK_QUERY} ==");
            for hit in hits {
                let _ = writeln!(out, "{}. {} score={:.6}", hit.rank, hit.key, hit.score);
            }
            out.push('\n');
            let summaries = self.agent.group_summaries(token, &id, GROUPS_QUERY)?;
            let _ = writeln!(out, "== groups:{branch}:{GROUPS_QUERY} ==");
            for group in summaries {
                let _ = writeln!(
                    out,
                    "group {}: count={} avg_cost={:.6} error_rate={:.6} exemplar={} members={}",
                    group.group_id,
                    group.count,
                    group.avg_cost,
                    group.error_rate,
                    group.exemplar_key,
                    group.member_keys.join(",")
                );
            }
            out.push('\n');
        }
        Ok(out)
    }

    /// The corpus branches, sorted — the order `standing_answers` renders them in.
    #[must_use]
    pub fn branches(&self) -> &[String] {
        &self.branches
    }
}

/// MD-1 R2's trait inversion: the taint core names the heal it needs, this host implements it
/// over the operator's mounted semantic plane. Only telemetry feeds that plane in this corpus.
///
/// M5 extends the heal through the fork lineage: a descendant's standing state was hydrated from
/// an ancestor, so healing the writing branch also heals every **active** descendant.
/// Retract-by-key skips branches that never inherited the row or already diverged away from it,
/// which is what keeps the cascade idempotent. With no recorded lineage (the M4 corpus) this is
/// exactly the M4 behavior.
pub struct TrustHealer<'a> {
    pub operator: &'a OperatorTrustPlane,
    pub lineage: Lineage,
}

impl SemanticHealer for TrustHealer<'_> {
    fn heal(&mut self, branch: &str, table: &str, keys: &[String]) -> Result<usize, String> {
        if table != TELEMETRY && table != CLAIMS {
            return Err(format!("unexpected taint table {table:?}"));
        }
        if table != TELEMETRY {
            return Ok(0);
        }
        let mut healed = self
            .operator
            .heal_semantic(&BranchId::new(branch), keys)
            .map_err(|error| error.to_string())?;
        for descendant in self.lineage.active_descendants(branch) {
            healed += self
                .operator
                .heal_semantic(&BranchId::new(descendant.as_str()), keys)
                .map_err(|error| error.to_string())?;
        }
        Ok(healed)
    }
}

/// Decode one answer frame into its positive rows.
fn rows_from_frames(bytes: &[u8]) -> Result<Vec<Row>, HostError> {
    let (record, _) = read_framed(bytes, 0)
        .map_err(|error| HostError::Composition(error.to_string()))?
        .ok_or_else(|| HostError::Composition("torn answer frame".to_owned()))?;
    let Record::Append { entries, .. } = record else {
        return Err(HostError::Composition(
            "the answer frame is not an append record".to_owned(),
        ));
    };
    Ok(entries
        .into_iter()
        .filter(|(_, weight)| *weight > 0)
        .map(|(row, _)| row)
        .collect())
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
