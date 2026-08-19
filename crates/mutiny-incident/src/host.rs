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

use crate::corpus::{self, Corpus, CorpusAction, CorpusCommit, CLAIMS, TELEMETRY, TENANT};
use loom_action::{ActionRecord, Connector, ConnectorOutcome};
use loom_branch::{CapabilityToken, Loom, MAIN};
use loom_core::{
    ActorId, BranchId, Claim, ClaimId, ClaimStatus, Confidence, ExecutedAction, Interval, Method,
    SessionId, SourceRef, TenantId, Timestamp, TrustClass, WriteEnvelope,
};
use loom_policy::{Effect, Match, PolicyRule, PolicySet, PURPOSE_AUTHORIZE};
use mutiny_bridge::{
    commit_with_capture, prepared_batches, recover_pending_captures, CapturedChange, CapturedTable,
    CommitCapture, CommitDraft, EnvelopeAuthority, EnvelopeId,
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

fn policy() -> PolicySet {
    PolicySet::new(
        "m4-policy-v1",
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
    tainted: bool,
    records: Vec<ActionRecord>,
    branches: Vec<String>,
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
            tainted: false,
            records: Vec::new(),
            branches,
        })
    }

    /// Build the full corpus world: every commit through the real front door, then the action.
    pub fn build(paths: &HostPaths, corpus: &Corpus) -> Result<Host, HostError> {
        let mut host = Host::open(paths, corpus)?;
        for commit in &corpus.commits {
            host.ingest_commit(commit)?;
        }
        for action in &corpus.actions {
            host.execute_action(action)?;
        }
        Ok(host)
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

        // Storage commits the compute plane has not sealed yet: manifest history is the queue.
        let head = host.store.head();
        let sealed = host.engine.epoch();
        // Re-register every corpus envelope: recovery re-admits the same envelopes it admitted.
        for commit in &corpus.commits {
            let envelope = WriteEnvelope::new(
                ActorId::new(commit.actor.as_str()),
                SessionId::new(commit.session.as_str()),
                BranchId::new(commit.branch.as_str()),
                format!("record {} {}", commit.table, commit.key),
            )
            .derived_from(commit.sources.iter().cloned());
            host.authority.register(EnvelopeId::of(&envelope));
        }
        let pending = recover_pending_captures(&host.store, head, sealed)?;
        for capture in &pending {
            host.apply_capture(capture)?;
        }
        host.commit_seq = match mutiny_bridge::recover_capture(&host.store, head) {
            Ok(capture) => capture.commit_seq,
            Err(_) => 0,
        };

        // Rebuild the volatile plane from the compute plane's current telemetry, branch by branch.
        let telemetry = host.standing_rows("telemetry_current")?;
        let mut by_branch: BTreeMap<String, Vec<SemanticDelta>> = BTreeMap::new();
        for row in telemetry {
            let branch = match row.get(1) {
                Some(Value::Str(branch)) => branch.clone(),
                other => {
                    return Err(HostError::Composition(format!(
                        "telemetry branch column is {other:?}"
                    )))
                }
            };
            let delta = host.telemetry_delta(&row, 1)?;
            by_branch.entry(branch).or_default().push(delta);
        }
        for (branch, deltas) in by_branch {
            host.apply_semantic(&branch, deltas)?;
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
        let (record, _) = read_framed(&bytes, 0)
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
pub struct TrustHealer<'a> {
    pub operator: &'a OperatorTrustPlane,
}

impl SemanticHealer for TrustHealer<'_> {
    fn heal(&mut self, branch: &str, table: &str, keys: &[String]) -> Result<usize, String> {
        if table != TELEMETRY && table != CLAIMS {
            return Err(format!("unexpected taint table {table:?}"));
        }
        if table != TELEMETRY {
            return Ok(0);
        }
        self.operator
            .heal_semantic(&BranchId::new(branch), keys)
            .map_err(|error| error.to_string())
    }
}
