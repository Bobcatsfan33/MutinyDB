//! Loom's trust controls mounted over MutinyDB's branch-scoped standing result stores.
//!
//! The agent and operator handles are different types. An agent can open/fork branches, maintain
//! authorized branch-local results, ask policy, and create inert proposals. Only the operator handle
//! contains the action gateway and can turn a proposal into an external effect.

use loom_action::{ActionGateway, ActionRecord, AgentStore, Connector, Proposal};
use loom_branch::{CapabilityToken, Loom, SessionHandle, MAIN};
use loom_core::{ActorId, BranchId, SessionId};
use loom_policy::{Engine, PolicyDecision, PolicySet, Request};
use mutiny_semantic::{AnswerDelta, SemanticDelta, SemanticError, SemanticHit, SemanticTopK};
use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

#[derive(Debug, thiserror::Error)]
pub enum TrustError {
    #[error(transparent)]
    Loom(#[from] loom_core::LoomError),
    #[error(transparent)]
    Semantic(#[from] SemanticError),
    #[error("standing query {query:?} is not installed on branch {branch}")]
    QueryNotFound { branch: String, query: String },
    #[error("standing query {query:?} is already installed on branch {branch}")]
    QueryAlreadyExists { branch: String, query: String },
    #[error("trust-plane state lock was poisoned")]
    StatePoisoned,
    #[error("action gateway lock was poisoned")]
    GatewayPoisoned,
}

type BranchQueries = BTreeMap<String, SemanticTopK>;

struct MountedState {
    db: Arc<Loom>,
    queries: RwLock<BTreeMap<BranchId, BranchQueries>>,
}

/// The capability-scoped surface safe to hand to an agent runtime. It has no execute method and
/// owns no action gateway.
#[derive(Clone)]
pub struct AgentTrustPlane {
    state: Arc<MountedState>,
    policy: Arc<Engine>,
}

/// The operator-only surface. Possession of this type is the authority to execute a proposal.
pub struct OperatorTrustPlane {
    state: Arc<MountedState>,
    gateway: ActionGateway,
}

/// Mount one Loom tenant and split it into structurally distinct agent/operator capabilities.
pub fn mount(
    db: Arc<Loom>,
    tenant: impl Into<String>,
    policy: PolicySet,
    connectors: impl IntoIterator<Item = Box<dyn Connector>>,
) -> (AgentTrustPlane, OperatorTrustPlane) {
    let policy_engine = Arc::new(Engine::new(&policy));
    let mut gateway = ActionGateway::new(tenant, Engine::new(&policy));
    for connector in connectors {
        gateway = gateway.with_connector(connector);
    }
    let mut queries = BTreeMap::new();
    queries.insert(BranchId::new(MAIN), BTreeMap::new());
    let state = Arc::new(MountedState {
        db,
        queries: RwLock::new(queries),
    });
    (
        AgentTrustPlane {
            state: Arc::clone(&state),
            policy: policy_engine,
        },
        OperatorTrustPlane { state, gateway },
    )
}

impl AgentTrustPlane {
    /// Open a Loom session and fork the main branch's standing answers in the same critical section.
    pub fn open_session(&self) -> Result<(SessionHandle, CapabilityToken), TrustError> {
        let mut queries = self
            .state
            .queries
            .write()
            .map_err(|_| TrustError::StatePoisoned)?;
        let (session, token) = self.state.db.open_session()?;
        let inherited = queries
            .get(&BranchId::new(MAIN))
            .cloned()
            .unwrap_or_default();
        queries.insert(session.branch.clone(), inherited);
        Ok((session, token))
    }

    /// Open a caller-identified session. Production front doors should prefer this form with their
    /// collision-resistant request id; Loom's millisecond convenience id is retained only for
    /// embedded callers that can guarantee serialized opens.
    pub fn open_session_named(
        &self,
        session_id: SessionId,
    ) -> Result<(SessionHandle, CapabilityToken), TrustError> {
        let mut queries = self
            .state
            .queries
            .write()
            .map_err(|_| TrustError::StatePoisoned)?;
        let (session, token) = self.state.db.open_session_named(session_id)?;
        let inherited = queries
            .get(&BranchId::new(MAIN))
            .cloned()
            .unwrap_or_default();
        queries.insert(session.branch.clone(), inherited);
        Ok((session, token))
    }

    /// Fork a Loom branch and its materialized standing state. The original token is not widened;
    /// the returned token is the only capability covering the child.
    pub fn branch(
        &self,
        token: &CapabilityToken,
        from: &BranchId,
        name: &str,
    ) -> Result<(BranchId, CapabilityToken), TrustError> {
        let mut queries = self
            .state
            .queries
            .write()
            .map_err(|_| TrustError::StatePoisoned)?;
        let inherited = queries.get(from).cloned().unwrap_or_default();
        let (branch, token) = self.state.db.branch(token, from, name)?;
        queries.insert(branch.clone(), inherited);
        Ok((branch, token))
    }

    /// Apply one semantic epoch only after Loom authorizes this exact branch capability.
    pub fn apply_semantic_epoch(
        &self,
        token: &CapabilityToken,
        branch: &BranchId,
        query: &str,
        deltas: impl IntoIterator<Item = SemanticDelta>,
    ) -> Result<Vec<AnswerDelta>, TrustError> {
        self.state.db.authorize_read(token, branch)?;
        let mut branches = self
            .state
            .queries
            .write()
            .map_err(|_| TrustError::StatePoisoned)?;
        let state = branches
            .get_mut(branch)
            .and_then(|queries| queries.get_mut(query))
            .ok_or_else(|| TrustError::QueryNotFound {
                branch: branch.as_str().to_owned(),
                query: query.to_owned(),
            })?;
        state.apply_epoch(deltas).map_err(TrustError::from)
    }

    /// Read a materialized answer through Loom's exact capability verifier.
    pub fn answer(
        &self,
        token: &CapabilityToken,
        branch: &BranchId,
        query: &str,
    ) -> Result<Vec<SemanticHit>, TrustError> {
        self.state.db.authorize_read(token, branch)?;
        let branches = self
            .state
            .queries
            .read()
            .map_err(|_| TrustError::StatePoisoned)?;
        branches
            .get(branch)
            .and_then(|queries| queries.get(query))
            .map(SemanticTopK::answer)
            .ok_or_else(|| TrustError::QueryNotFound {
                branch: branch.as_str().to_owned(),
                query: query.to_owned(),
            })
    }

    #[must_use]
    pub fn decide(&self, request: &Request) -> PolicyDecision {
        self.policy.decide(request)
    }

    /// Create the inert, propose-only handle for an agent on a branch.
    #[must_use]
    pub fn agent(&self, actor: ActorId, branch: &BranchId, simulation: bool) -> AgentStore {
        AgentStore::new(actor, branch.as_str(), simulation)
    }
}

impl OperatorTrustPlane {
    /// Install a reviewed standing query on a branch. Sessions fork it from main thereafter.
    pub fn install_standing(
        &self,
        branch: &BranchId,
        standing: SemanticTopK,
    ) -> Result<(), TrustError> {
        let query = standing.query().id.clone();
        let mut branches = self
            .state
            .queries
            .write()
            .map_err(|_| TrustError::StatePoisoned)?;
        let queries = branches.entry(branch.clone()).or_default();
        match queries.entry(query.clone()) {
            std::collections::btree_map::Entry::Vacant(slot) => {
                slot.insert(standing);
                Ok(())
            }
            std::collections::btree_map::Entry::Occupied(_) => {
                Err(TrustError::QueryAlreadyExists {
                    branch: branch.as_str().to_owned(),
                    query,
                })
            }
        }
    }

    /// Execute only through Loom's kill-switch, evidence, policy, simulation, idempotency, and
    /// receipt checks. The agent handle has no equivalent method.
    #[must_use]
    pub fn execute(&self, proposal: &Proposal) -> ActionRecord {
        self.gateway.execute(proposal)
    }

    #[must_use]
    pub fn action_records(&self) -> Vec<ActionRecord> {
        self.gateway.records()
    }

    pub fn disable_actions(&self) -> Result<(), TrustError> {
        self.gateway
            .kill_switch()
            .lock()
            .map_err(|_| TrustError::GatewayPoisoned)?
            .disable_all();
        Ok(())
    }
}
