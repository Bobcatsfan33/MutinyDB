//! Loom's trust controls mounted over MutinyDB's branch-scoped standing result stores.
//!
//! The agent and operator handles are different types. An agent can open/fork branches, maintain
//! authorized branch-local results, ask policy, and create inert proposals. Only the operator handle
//! contains the action gateway and can turn a proposal into an external effect.

use loom_action::{ActionGateway, ActionRecord, AgentStore, Connector, Proposal};
use loom_branch::{CapabilityToken, Loom, SessionHandle, MAIN};
use loom_core::{ActorId, BranchId, SessionId};
use loom_policy::{Engine, PolicyDecision, PolicySet, Request};
use mutiny_semantic::{
    AnswerDelta, SemanticDelta, SemanticError, SemanticGroupSummary, SemanticGroups, SemanticHit,
    SemanticTopK,
};
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
    #[error("standing grouping {group:?} is not installed on branch {branch}")]
    GroupNotFound { branch: String, group: String },
    #[error("standing grouping {group:?} is already installed on branch {branch}")]
    GroupAlreadyExists { branch: String, group: String },
    #[error("trust-plane state lock was poisoned")]
    StatePoisoned,
    #[error("action gateway lock was poisoned")]
    GatewayPoisoned,
}

type BranchQueries = BTreeMap<String, SemanticTopK>;
type BranchGroups = BTreeMap<String, SemanticGroups>;

struct MountedState {
    db: Arc<Loom>,
    queries: RwLock<BTreeMap<BranchId, BranchQueries>>,
    // Lock discipline everywhere in this crate: `queries` before `groups`, never the reverse.
    groups: RwLock<BTreeMap<BranchId, BranchGroups>>,
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
    let mut groups = BTreeMap::new();
    groups.insert(BranchId::new(MAIN), BTreeMap::new());
    let state = Arc::new(MountedState {
        db,
        queries: RwLock::new(queries),
        groups: RwLock::new(groups),
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
        let mut groups = self
            .state
            .groups
            .write()
            .map_err(|_| TrustError::StatePoisoned)?;
        let (session, token) = self.state.db.open_session()?;
        let inherited = queries
            .get(&BranchId::new(MAIN))
            .cloned()
            .unwrap_or_default();
        queries.insert(session.branch.clone(), inherited);
        let inherited_groups = groups
            .get(&BranchId::new(MAIN))
            .cloned()
            .unwrap_or_default();
        groups.insert(session.branch.clone(), inherited_groups);
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
        let mut groups = self
            .state
            .groups
            .write()
            .map_err(|_| TrustError::StatePoisoned)?;
        let (session, token) = self.state.db.open_session_named(session_id)?;
        let inherited = queries
            .get(&BranchId::new(MAIN))
            .cloned()
            .unwrap_or_default();
        queries.insert(session.branch.clone(), inherited);
        let inherited_groups = groups
            .get(&BranchId::new(MAIN))
            .cloned()
            .unwrap_or_default();
        groups.insert(session.branch.clone(), inherited_groups);
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
        let mut groups = self
            .state
            .groups
            .write()
            .map_err(|_| TrustError::StatePoisoned)?;
        let inherited = queries.get(from).cloned().unwrap_or_default();
        let inherited_groups = groups.get(from).cloned().unwrap_or_default();
        let (branch, token) = self.state.db.branch(token, from, name)?;
        queries.insert(branch.clone(), inherited);
        groups.insert(branch.clone(), inherited_groups);
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

    /// Apply one grouping epoch only after Loom authorizes this exact branch capability.
    pub fn apply_group_epoch(
        &self,
        token: &CapabilityToken,
        branch: &BranchId,
        group: &str,
        deltas: impl IntoIterator<Item = SemanticDelta>,
    ) -> Result<(), TrustError> {
        self.state.db.authorize_read(token, branch)?;
        let mut branches = self
            .state
            .groups
            .write()
            .map_err(|_| TrustError::StatePoisoned)?;
        let state = branches
            .get_mut(branch)
            .and_then(|groups| groups.get_mut(group))
            .ok_or_else(|| TrustError::GroupNotFound {
                branch: branch.as_str().to_owned(),
                group: group.to_owned(),
            })?;
        state.apply_epoch(deltas).map_err(TrustError::from)
    }

    /// Read a branch's grouping summaries through Loom's exact capability verifier.
    pub fn group_summaries(
        &self,
        token: &CapabilityToken,
        branch: &BranchId,
        group: &str,
    ) -> Result<Vec<SemanticGroupSummary>, TrustError> {
        self.state.db.authorize_read(token, branch)?;
        let branches = self
            .state
            .groups
            .read()
            .map_err(|_| TrustError::StatePoisoned)?;
        branches
            .get(branch)
            .and_then(|groups| groups.get(group))
            .map(SemanticGroups::summaries)
            .ok_or_else(|| TrustError::GroupNotFound {
                branch: branch.as_str().to_owned(),
                group: group.to_owned(),
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

    /// Install a reviewed standing grouping on a branch. Sessions fork it from main thereafter.
    pub fn install_groups(
        &self,
        branch: &BranchId,
        group: impl Into<String>,
        standing: SemanticGroups,
    ) -> Result<(), TrustError> {
        let group = group.into();
        let mut branches = self
            .state
            .groups
            .write()
            .map_err(|_| TrustError::StatePoisoned)?;
        let groups = branches.entry(branch.clone()).or_default();
        match groups.entry(group.clone()) {
            std::collections::btree_map::Entry::Vacant(slot) => {
                slot.insert(standing);
                Ok(())
            }
            std::collections::btree_map::Entry::Occupied(_) => {
                Err(TrustError::GroupAlreadyExists {
                    branch: branch.as_str().to_owned(),
                    group,
                })
            }
        }
    }

    /// Retract the named record keys from every standing semantic operator on exactly this branch
    /// (M4 taint heal). An operator power beside [`Self::install_standing`], deliberately absent
    /// from the agent surface; sibling branches are untouched by construction, so M3's isolation
    /// gate extends rather than weakens. Keys an operator does not hold are skipped — a resumed
    /// taint heals idempotently. Returns the number of (operator, row) retractions performed.
    pub fn heal_semantic(&self, branch: &BranchId, keys: &[String]) -> Result<usize, TrustError> {
        let mut queries = self
            .state
            .queries
            .write()
            .map_err(|_| TrustError::StatePoisoned)?;
        let mut groups = self
            .state
            .groups
            .write()
            .map_err(|_| TrustError::StatePoisoned)?;
        let mut healed = 0;
        if let Some(branch_queries) = queries.get_mut(branch) {
            for standing in branch_queries.values_mut() {
                let (count, _) = standing.retract_keys(keys.iter().map(String::as_str))?;
                healed += count;
            }
        }
        if let Some(branch_groups) = groups.get_mut(branch) {
            for standing in branch_groups.values_mut() {
                healed += standing.retract_keys(keys.iter().map(String::as_str))?;
            }
        }
        Ok(healed)
    }

    /// One branch's mounted standing-state bytes (top-k and groupings together).
    pub fn branch_state_bytes(&self, branch: &BranchId) -> Result<usize, TrustError> {
        let queries = self
            .state
            .queries
            .read()
            .map_err(|_| TrustError::StatePoisoned)?;
        let groups = self
            .state
            .groups
            .read()
            .map_err(|_| TrustError::StatePoisoned)?;
        let query_bytes: usize = queries
            .get(branch)
            .map(|standing| standing.values().map(SemanticTopK::state_bytes).sum())
            .unwrap_or(0);
        let group_bytes: usize = groups
            .get(branch)
            .map(|standing| standing.values().map(SemanticGroups::state_bytes).sum())
            .unwrap_or(0);
        Ok(query_bytes + group_bytes)
    }

    /// Total mounted standing-state bytes across every branch — the accounting the M5 rewind
    /// gate requires to return to its pre-fork baseline.
    pub fn mounted_state_bytes(&self) -> Result<usize, TrustError> {
        let queries = self
            .state
            .queries
            .read()
            .map_err(|_| TrustError::StatePoisoned)?;
        let groups = self
            .state
            .groups
            .read()
            .map_err(|_| TrustError::StatePoisoned)?;
        let query_bytes: usize = queries
            .values()
            .flat_map(|standing| standing.values())
            .map(SemanticTopK::state_bytes)
            .sum();
        let group_bytes: usize = groups
            .values()
            .flat_map(|standing| standing.values())
            .map(SemanticGroups::state_bytes)
            .sum();
        Ok(query_bytes + group_bytes)
    }

    /// Discard a branch's standing operators (M5 rewind). Loom's branch and its committed history
    /// remain — auditable, never destroyed — but the branch's circuit state is torn down and its
    /// bytes return to the mount's baseline: the C6 teardown discipline, composed. Idempotent —
    /// rewinding an absent branch frees zero, because a resumed rewind must not be an error.
    /// Returns the bytes freed.
    pub fn rewind_branch(&self, branch: &BranchId) -> Result<usize, TrustError> {
        let mut queries = self
            .state
            .queries
            .write()
            .map_err(|_| TrustError::StatePoisoned)?;
        let mut groups = self
            .state
            .groups
            .write()
            .map_err(|_| TrustError::StatePoisoned)?;
        let query_bytes: usize = queries
            .remove(branch)
            .map(|standing| standing.values().map(SemanticTopK::state_bytes).sum())
            .unwrap_or(0);
        let group_bytes: usize = groups
            .remove(branch)
            .map(|standing| standing.values().map(SemanticGroups::state_bytes).sum())
            .unwrap_or(0);
        Ok(query_bytes + group_bytes)
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
