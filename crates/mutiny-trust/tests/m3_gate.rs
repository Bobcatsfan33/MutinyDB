#![allow(clippy::panic, clippy::unwrap_used)]

use loom_action::{ActionStatus, Connector, ConnectorOutcome};
use loom_branch::{Loom, MAIN};
use loom_core::{
    ActorId, BranchId, Claim, ClaimId, ClaimStatus, Confidence, Interval, Method, SessionId,
    SourceRef, TenantId, Timestamp, TrustClass, Value,
};
use loom_policy::{Decision, Effect, Match, PolicyRule, PolicySet, Request, PURPOSE_AUTHORIZE};
use mutiny_semantic::{
    ScalarColumns, ScalarPredicate, SemanticDelta, SemanticQuery, SemanticRecord, SemanticTopK,
};
use mutiny_trust::{mount, TrustError};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

struct ReceiptConnector(Arc<AtomicUsize>);

impl Connector for ReceiptConnector {
    fn action_type(&self) -> &str {
        "identity.suspend_account"
    }

    fn compensating_action(&self) -> Option<String> {
        Some("identity.reinstate_account".to_owned())
    }

    fn execute(&self, target: &str, _idempotency_key: &str) -> ConnectorOutcome {
        self.0.fetch_add(1, Ordering::SeqCst);
        ConnectorOutcome::Succeeded {
            receipt: format!("receipt-{target}"),
        }
    }
}

fn policy() -> PolicySet {
    PolicySet::new(
        "m3-policy-v1",
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

fn standing() -> SemanticTopK {
    SemanticTopK::new(
        SemanticQuery::new(
            "risk",
            "model:v1",
            vec![1.0, 0.0],
            3,
            ScalarPredicate::default(),
        )
        .unwrap(),
    )
}

fn delta(key: &str, vector: Vec<f32>) -> SemanticDelta {
    SemanticDelta {
        record: SemanticRecord::new(
            key,
            "model:v1",
            vector,
            ScalarColumns {
                tenant: "acme".to_owned(),
                event_time: 1,
                cost: 1.0,
                error: false,
            },
        )
        .unwrap(),
        weight: 1,
    }
}

#[test]
fn branch_scoped_answers_use_looms_capability_verifier_and_fork_isolation() {
    let db = Arc::new(Loom::in_memory(TenantId::new("acme")).unwrap());
    let (agent, operator) = mount(db, "acme", policy(), Vec::new());
    operator
        .install_standing(&BranchId::new(MAIN), standing())
        .unwrap();

    let (first, first_token) = agent.open_session_named(SessionId::new("first")).unwrap();
    let (second, second_token) = agent.open_session_named(SessionId::new("second")).unwrap();
    let (hypothesis, hypothesis_token) = agent
        .branch(&first_token, &first.branch, "hypothesis")
        .unwrap();

    agent
        .apply_semantic_epoch(
            &hypothesis_token,
            &hypothesis,
            "risk",
            [delta("hypothesis-only", vec![1.0, 0.0])],
        )
        .unwrap();
    assert_eq!(
        agent
            .answer(&hypothesis_token, &hypothesis, "risk")
            .unwrap()[0]
            .key,
        "hypothesis-only"
    );
    assert!(agent
        .answer(&first_token, &first.branch, "risk")
        .unwrap()
        .is_empty());
    assert!(agent
        .answer(&second_token, &second.branch, "risk")
        .unwrap()
        .is_empty());
    assert!(matches!(
        agent.answer(&second_token, &hypothesis, "risk"),
        Err(TrustError::Loom(_))
    ));
}

fn eligible_claim() -> Claim {
    Claim {
        id: ClaimId::of(b"claim/risk"),
        predicate: "is_risky".to_owned(),
        subject: "user-42".to_owned(),
        object: Value::Bool(true),
        valid: Interval::from(Timestamp::from_ms(1)),
        known: Interval::from(Timestamp::from_ms(1)),
        confidence: Confidence::new(0.99, Method::Rule, "v1"),
        evidence: vec![SourceRef::new("erp", "fraud-1")],
        status: ClaimStatus::Asserted,
        policy: None,
        actor: ActorId::new("agent"),
    }
}

#[test]
fn policy_fails_closed_and_only_the_operator_can_execute_with_a_receipt() {
    let calls = Arc::new(AtomicUsize::new(0));
    let db = Arc::new(Loom::in_memory(TenantId::new("acme")).unwrap());
    let (agent, operator) = mount(
        db,
        "acme",
        policy(),
        vec![Box::new(ReceiptConnector(Arc::clone(&calls))) as Box<dyn Connector>],
    );
    let (session, _) = agent.open_session().unwrap();

    let denied = agent.decide(&Request {
        actor: "agent".to_owned(),
        label: TrustClass::Untrusted,
        purpose: PURPOSE_AUTHORIZE.to_owned(),
        action: "identity.suspend_account".to_owned(),
    });
    assert_eq!(denied.decision, Decision::Deny);

    let proposal = agent
        .agent(ActorId::new("agent"), &session.branch, false)
        .propose(
            "identity.suspend_account",
            "user-42",
            "suspend-user-42",
            vec![eligible_claim()],
            vec![b"claim/risk".to_vec()],
            TrustClass::VerifiedSystem,
        );
    let first = operator.execute(&proposal);
    let replay = operator.execute(&proposal);
    assert_eq!(first, replay);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(matches!(first.status, ActionStatus::Succeeded { .. }));

    operator.disable_actions().unwrap();
    let disabled = agent
        .agent(ActorId::new("agent"), &session.branch, false)
        .propose(
            "identity.suspend_account",
            "user-43",
            "suspend-user-43",
            vec![eligible_claim()],
            vec![b"claim/risk".to_vec()],
            TrustClass::VerifiedSystem,
        );
    assert!(matches!(
        operator.execute(&disabled).status,
        ActionStatus::Refused { .. }
    ));
}
