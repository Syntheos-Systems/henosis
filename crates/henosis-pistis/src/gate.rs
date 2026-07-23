//! The `pistis` gate: capability + trust authorization, fail-closed.
//!
//! Pistis is the first gate in the canonical dispatcher chain (`pistis ->
//! plutus -> eidolon -> human -> phylaxd`). It authorizes an invocation against
//! the requesting principal's admission, trust, and capabilities in the relevant
//! room.
//!
//! Requirements come only from a trusted [`ToolActionPolicy`] keyed by the
//! invocation's tool and action. Invocation arguments are caller-controlled and
//! never select or suppress the capability check. An unknown tool/action pair is
//! denied.
//!
//! Fail-closed by construction. The only path to `Allow` is an explicit
//! admitted-and-trusted-and-capable verdict from [`authorize_capabilities`]. An
//! unknown action, a request with no room, or a room with no materialized
//! authority state is denied. There is no advisory mode and no self-approval.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use syntheos_contracts::{Gate, GateDecision, GateError, GateRequest};
use time::OffsetDateTime;

use crate::authority::{CapabilityCheckRequest, CapabilityRequirement, authorize_capabilities};
use crate::model::{ActionKind, RoomScope};
use crate::room::{RoomState, RoomTrustStore};

/// A source of materialized room state, keyed by exact tenant and room. The live
/// implementation materializes the room's signed-event log inside Pistis; tests
/// supply an in-memory map. `None` means there is no Pistis authority state for
/// that room (the gate treats this as fail-closed: deny a declared requirement).
pub trait RoomStateSource: Send + Sync {
    /// Return a shared current raw snapshot for `scope`, or `None` if unknown.
    fn room_state(&self, scope: &RoomScope) -> Option<Arc<RoomState>>;
}

/// An in-memory [`RoomStateSource`] backed by an exact-scope map.
///
/// The default empty source returns `None` for every room, so the gate denies
/// every canonical action. Deployments may provide materialized room state.
#[derive(Debug, Clone, Default)]
pub struct InMemoryRoomStateSource {
    /// Materialized state keyed by exact tenant and room.
    rooms: HashMap<RoomScope, Arc<RoomState>>,
}

/// Implements construction and mutation for the in-memory state source.
impl InMemoryRoomStateSource {
    /// Construct an empty source (no rooms materialized).
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert or replace a raw snapshot under the scope its manifest claims.
    pub fn insert(&mut self, state: RoomState) {
        self.rooms.insert(state.scope().clone(), Arc::new(state));
    }
}

/// Supplies room state from the in-memory source.
impl RoomStateSource for InMemoryRoomStateSource {
    /// Look up the materialized state for an exact tenant and room.
    fn room_state(&self, scope: &RoomScope) -> Option<Arc<RoomState>> {
        self.rooms.get(scope).map(Arc::clone)
    }
}

/// Trusted capability requirements keyed by an invocation's tool and action.
///
/// Policy entries are host-controlled configuration. Invocation arguments never
/// alter this registry. Missing entries deny by default.
#[derive(Debug, Clone, Default)]
pub struct ToolActionPolicy {
    /// Capability requirements nested under exact tool and action identifiers.
    requirements: HashMap<String, HashMap<String, Vec<CapabilityRequirement>>>,
}

/// Builds and queries trusted tool/action capability policy.
impl ToolActionPolicy {
    /// Construct an empty policy that denies every action.
    pub fn new() -> Self {
        Self::default()
    }

    /// Construct the canonical policy for the production Hermes adapter catalog.
    pub fn canonical() -> Self {
        let mut policy = Self::new();

        policy.register("henosis", "probe", "henosis", ActionKind::Message);

        policy.register("gmail", "send", "gmail", ActionKind::Message);
        policy.register("gmail", "read", "gmail", ActionKind::Message);
        policy.register("gmail", "search", "gmail", ActionKind::Message);
        policy.register("gmail", "list_labels", "gmail", ActionKind::Message);

        policy.register("gdrive", "list", "gdrive", ActionKind::Message);
        policy.register("gdrive", "upload", "gdrive", ActionKind::Commit);
        policy.register("gdrive", "download", "gdrive", ActionKind::Message);
        policy.register("gdrive", "get_metadata", "gdrive", ActionKind::Message);

        policy.register("gcal", "list_events", "gcal", ActionKind::Message);
        policy.register("gcal", "create_event", "gcal", ActionKind::Commit);
        policy.register("gcal", "update_event", "gcal", ActionKind::Commit);
        policy.register("gcal", "delete_event", "gcal", ActionKind::Delete);

        policy.register("github", "create_issue", "github", ActionKind::Commit);
        policy.register("github", "list_issues", "github", ActionKind::Message);
        policy.register("github", "get_issue", "github", ActionKind::Message);
        policy.register("github", "create_pr", "github", ActionKind::Commit);
        policy.register("github", "list_prs", "github", ActionKind::Message);
        policy.register("github", "merge_pr", "github", ActionKind::Merge);
        policy.register("github", "search_code", "github", ActionKind::Message);
        policy.register("github", "list_repos", "github", ActionKind::Message);
        policy.register("github", "create_webhook", "github", ActionKind::Commit);

        policy.register("slack", "send_message", "slack", ActionKind::Message);

        policy.register("linear", "create_issue", "linear", ActionKind::Commit);
        policy.register("linear", "list_issues", "linear", ActionKind::Message);
        policy.register("linear", "update_issue", "linear", ActionKind::Commit);
        policy.register("linear", "search", "linear", ActionKind::Message);
        policy.register("linear", "create_webhook", "linear", ActionKind::Commit);

        policy.register("notion", "search", "notion", ActionKind::Message);
        policy.register("notion", "get_page", "notion", ActionKind::Message);
        policy.register("notion", "create_page", "notion", ActionKind::Commit);
        policy.register("notion", "append_blocks", "notion", ActionKind::Commit);

        policy
    }

    /// Register one capability requirement for an exact tool/action pair.
    pub fn register(
        &mut self,
        tool: impl Into<String>,
        action: impl Into<String>,
        capability: impl Into<String>,
        action_kind: ActionKind,
    ) {
        self.requirements.entry(tool.into()).or_default().insert(
            action.into(),
            vec![CapabilityRequirement {
                name: capability.into(),
                action_kind,
            }],
        );
    }

    /// Return trusted requirements for an exact tool/action pair.
    pub fn requirements(&self, tool: &str, action: &str) -> Option<&[CapabilityRequirement]> {
        self.requirements
            .get(tool)
            .and_then(|actions| actions.get(action))
            .map(Vec::as_slice)
    }
}

/// A wall-clock source, injected so the trust math stays testable.
pub trait Clock: Send + Sync {
    /// The current UTC instant.
    fn now(&self) -> OffsetDateTime;
}

/// The system wall clock.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

/// Reads current instants from the system clock.
impl Clock for SystemClock {
    /// Read the OS clock in UTC.
    fn now(&self) -> OffsetDateTime {
        OffsetDateTime::now_utc()
    }
}

/// The fail-closed capability/trust gate for the dispatcher's pistis slot.
pub struct PistisGate {
    /// Where the gate obtains materialized room state.
    source: Arc<dyn RoomStateSource>,
    /// Gate-owned issuer pins and rollback floors.
    trust_store: Arc<RoomTrustStore>,
    /// Trusted mapping from tool/action pairs to capability requirements.
    policy: Arc<ToolActionPolicy>,
    /// The clock that feeds the trust math.
    clock: Arc<dyn Clock>,
}

/// Implements Pistis gate construction.
impl PistisGate {
    /// Build the gate over raw room state and independent issuer pins.
    pub fn new(source: Arc<dyn RoomStateSource>, trust_store: Arc<RoomTrustStore>) -> Self {
        Self {
            source,
            trust_store,
            policy: Arc::new(ToolActionPolicy::canonical()),
            clock: Arc::new(SystemClock),
        }
    }

    /// Build the gate with independent issuer pins and an explicit clock.
    pub fn with_clock(
        source: Arc<dyn RoomStateSource>,
        trust_store: Arc<RoomTrustStore>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            source,
            trust_store,
            policy: Arc::new(ToolActionPolicy::canonical()),
            clock,
        }
    }

    /// Build the gate with explicit trusted policy and clock.
    pub fn with_policy_and_clock(
        source: Arc<dyn RoomStateSource>,
        trust_store: Arc<RoomTrustStore>,
        policy: Arc<ToolActionPolicy>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            source,
            trust_store,
            policy,
            clock,
        }
    }
}

#[async_trait]
/// Applies Pistis capability and trust policy in the dispatcher gate chain.
impl Gate for PistisGate {
    /// The canonical authority name for this slot.
    fn name(&self) -> &str {
        "pistis"
    }

    /// Authorize requirements derived from trusted tool policy.
    async fn check(&self, req: &GateRequest) -> Result<GateDecision, GateError> {
        let Some(requirements) = self
            .policy
            .requirements(&req.invocation.tool, &req.invocation.action)
        else {
            return Ok(GateDecision::Deny {
                reason: format!(
                    "no pistis capability policy for {}.{}",
                    req.invocation.tool, req.invocation.action
                ),
            });
        };

        let Some(room) = req.context.room.as_deref() else {
            return Ok(GateDecision::Deny {
                reason: format!(
                    "pistis policy for {}.{} requires a room",
                    req.invocation.tool, req.invocation.action
                ),
            });
        };
        let scope = RoomScope::new(req.context.tenant, room);
        let Some(state) = self.source.room_state(&scope) else {
            return Ok(GateDecision::Deny {
                reason: format!("no pistis authority state for requested tenant and room {room}"),
            });
        };
        let verified = match state.verify_for(&scope, &self.trust_store) {
            Ok(state) => state,
            Err(error) => {
                return Ok(GateDecision::Deny {
                    reason: format!("pistis authority state failed verification: {error}"),
                });
            }
        };

        let decision = authorize_capabilities(
            &verified,
            &CapabilityCheckRequest {
                principal: req.context.principal,
                required: requirements.to_vec(),
            },
            self.clock.now(),
        );

        if decision.allowed {
            Ok(GateDecision::Allow)
        } else {
            Ok(GateDecision::Deny {
                reason: decision
                    .reason
                    .unwrap_or_else(|| "capability denied".to_owned()),
            })
        }
    }
}

/// Tests Pistis capability authorization behavior.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::SecretKey;
    use crate::model::{AdmittedPrincipal, Capability, RoomPolicy, RoomScope};
    use std::collections::BTreeSet;
    use syntheos_contracts::{PrincipalId, RequestContext, TenantId, ToolInvocation};

    /// A fixed clock for deterministic trust evaluation.
    struct FixedClock(OffsetDateTime);
    /// Reads the deterministic test instant.
    impl Clock for FixedClock {
        /// Returns the fixed test instant.
        fn now(&self) -> OffsetDateTime {
            self.0
        }
    }

    /// Return the stable tenant shared by gate fixtures and requests.
    fn tenant() -> TenantId {
        "00000000-0000-8000-8000-000000000001".parse().unwrap()
    }

    /// Build a raw room and independent trust pin for one admitted principal.
    fn room_with(principal: PrincipalId) -> (RoomState, RoomTrustStore) {
        let scope = RoomScope::new(tenant(), "!r");
        let (_, issuer_key) = SecretKey::generate();
        let (_, root_key) = SecretKey::generate();
        let (_, principal_key) = SecretKey::generate();
        let cap = Capability {
            name: "deploy".into(),
            action_kinds: BTreeSet::from([ActionKind::Deploy]),
            granted_by: "operator".into(),
            expires_at: None,
        };
        let state = RoomState::from_genesis(
            scope.clone(),
            1,
            RoomPolicy::default(),
            BTreeSet::from([root_key.public_key()]),
            &issuer_key,
            vec![AdmittedPrincipal::new(
                scope.clone(),
                principal,
                principal_key.public_key(),
                &root_key,
                vec![cap],
            )],
        )
        .unwrap();
        let mut trust = RoomTrustStore::new();
        trust.pin(scope, issuer_key.public_key(), 1).unwrap();
        (state, trust)
    }

    /// Build a gate whose only known room "!r" admits `principal`.
    fn gate_for(principal: PrincipalId) -> PistisGate {
        let (state, trust) = room_with(principal);
        let mut source = InMemoryRoomStateSource::new();
        source.insert(state);
        let mut policy = ToolActionPolicy::new();
        policy.register("synapse", "run", "deploy", ActionKind::Deploy);
        policy.register("synapse", "delete", "delete", ActionKind::Delete);
        PistisGate::with_policy_and_clock(
            Arc::new(source),
            Arc::new(trust),
            Arc::new(policy),
            Arc::new(FixedClock(OffsetDateTime::now_utc())),
        )
    }

    /// Build a gate request.
    fn request(
        principal: PrincipalId,
        room: Option<&str>,
        action: &str,
        args: serde_json::Value,
    ) -> GateRequest {
        GateRequest {
            context: RequestContext {
                tenant: tenant(),
                principal,
                persona: None,
                session: None,
                room: room.map(str::to_owned),
                task: None,
                workflow: None,
                authority: None,
            },
            invocation: ToolInvocation {
                tool: "synapse".into(),
                action: action.into(),
                args,
            },
        }
    }

    /// Omitted caller metadata cannot suppress the trusted policy requirement.
    #[tokio::test]
    async fn omitted_metadata_still_authorizes_from_policy() {
        let p = PrincipalId::new();
        let gate = gate_for(p);
        let req = request(p, Some("!r"), "run", serde_json::json!({}));
        assert_eq!(gate.check(&req).await.unwrap(), GateDecision::Allow);
    }

    /// A held, valid capability in a known room is allowed.
    #[tokio::test]
    async fn held_capability_allowed() {
        let p = PrincipalId::new();
        let gate = gate_for(p);
        let req = request(
            p,
            Some("!r"),
            "run",
            serde_json::json!({"capability": "deploy", "action_kind": "deploy"}),
        );
        assert_eq!(gate.check(&req).await.unwrap(), GateDecision::Allow);
    }

    /// A capability the principal does not hold is denied.
    #[tokio::test]
    async fn unheld_capability_denied() {
        let p = PrincipalId::new();
        let gate = gate_for(p);
        let req = request(
            p,
            Some("!r"),
            "delete",
            serde_json::json!({"capability": "delete", "action_kind": "delete"}),
        );
        assert!(matches!(
            gate.check(&req).await.unwrap(),
            GateDecision::Deny { .. }
        ));
    }

    /// A capability requirement with no room is denied (unevaluable).
    #[tokio::test]
    async fn requirement_without_room_denied() {
        let p = PrincipalId::new();
        let gate = gate_for(p);
        let req = request(
            p,
            None,
            "run",
            serde_json::json!({"capability": "deploy", "action_kind": "deploy"}),
        );
        assert!(matches!(
            gate.check(&req).await.unwrap(),
            GateDecision::Deny { .. }
        ));
    }

    /// A requirement for a room with no materialized authority state is denied
    /// (fail-closed).
    #[tokio::test]
    async fn unknown_room_denied() {
        let p = PrincipalId::new();
        let gate = gate_for(p);
        let req = request(
            p,
            Some("!unknown"),
            "run",
            serde_json::json!({"capability": "deploy", "action_kind": "deploy"}),
        );
        assert!(matches!(
            gate.check(&req).await.unwrap(),
            GateDecision::Deny { .. }
        ));
    }

    /// A room snapshot from one tenant cannot authorize the same room id in another tenant.
    #[tokio::test]
    async fn cross_tenant_room_lookup_is_denied() {
        let principal = PrincipalId::new();
        let gate = gate_for(principal);
        let mut req = request(principal, Some("!r"), "run", serde_json::json!({}));
        req.context.tenant = TenantId::new();
        assert!(matches!(
            gate.check(&req).await.unwrap(),
            GateDecision::Deny { .. }
        ));
    }

    /// Caller-provided metadata cannot change the trusted policy requirement.
    #[tokio::test]
    async fn caller_metadata_cannot_override_policy() {
        let p = PrincipalId::new();
        let gate = gate_for(p);
        let req = request(
            p,
            Some("!r"),
            "run",
            serde_json::json!({"capability": "delete", "action_kind": "teleport"}),
        );
        assert_eq!(gate.check(&req).await.unwrap(), GateDecision::Allow);
    }

    /// An action missing from trusted policy is denied before room lookup.
    #[tokio::test]
    async fn unknown_action_policy_denied() {
        let p = PrincipalId::new();
        let gate = gate_for(p);
        let req = request(p, Some("!r"), "unknown", serde_json::json!({}));
        let decision = gate.check(&req).await.unwrap();
        assert!(matches!(decision, GateDecision::Deny { .. }));
    }

    /// The canonical production catalog assigns high-risk actions explicitly.
    #[test]
    fn canonical_policy_maps_production_actions() {
        let policy = ToolActionPolicy::canonical();
        let merge = [CapabilityRequirement {
            name: "github".to_owned(),
            action_kind: ActionKind::Merge,
        }];
        let delete = [CapabilityRequirement {
            name: "gcal".to_owned(),
            action_kind: ActionKind::Delete,
        }];
        assert_eq!(
            policy.requirements("github", "merge_pr"),
            Some(merge.as_slice())
        );
        assert_eq!(
            policy.requirements("gcal", "delete_event"),
            Some(delete.as_slice())
        );
        assert!(policy.requirements("github", "unknown").is_none());
    }

    /// An unadmitted principal is denied even with a well-formed requirement.
    #[tokio::test]
    async fn unadmitted_principal_denied() {
        let admitted = PrincipalId::new();
        let gate = gate_for(admitted);
        let intruder = PrincipalId::new();
        let req = request(
            intruder,
            Some("!r"),
            "run",
            serde_json::json!({"capability": "deploy", "action_kind": "deploy"}),
        );
        assert!(matches!(
            gate.check(&req).await.unwrap(),
            GateDecision::Deny { .. }
        ));
    }

    /// A source-controlled issuer cannot replace the independent gate trust pin.
    #[tokio::test]
    async fn source_generated_issuer_is_denied() {
        let principal = PrincipalId::new();
        let scope = RoomScope::new(tenant(), "!r");
        let (_, pinned_issuer) = SecretKey::generate();
        let (_, source_issuer) = SecretKey::generate();
        let (_, root_key) = SecretKey::generate();
        let (_, principal_key) = SecretKey::generate();
        let capability = Capability {
            name: "deploy".into(),
            action_kinds: BTreeSet::from([ActionKind::Deploy]),
            granted_by: "operator".into(),
            expires_at: None,
        };
        let state = RoomState::from_genesis(
            scope.clone(),
            1,
            RoomPolicy::default(),
            BTreeSet::from([root_key.public_key()]),
            &source_issuer,
            vec![AdmittedPrincipal::new(
                scope.clone(),
                principal,
                principal_key.public_key(),
                &root_key,
                vec![capability],
            )],
        )
        .unwrap();
        let mut source = InMemoryRoomStateSource::new();
        source.insert(state);
        let mut trust = RoomTrustStore::new();
        trust.pin(scope, pinned_issuer.public_key(), 1).unwrap();
        let mut policy = ToolActionPolicy::new();
        policy.register("synapse", "run", "deploy", ActionKind::Deploy);
        let gate = PistisGate::with_policy_and_clock(
            Arc::new(source),
            Arc::new(trust),
            Arc::new(policy),
            Arc::new(FixedClock(OffsetDateTime::now_utc())),
        );
        let decision = gate
            .check(&request(
                principal,
                Some("!r"),
                "run",
                serde_json::json!({}),
            ))
            .await
            .unwrap();
        assert!(matches!(decision, GateDecision::Deny { .. }));
    }

    /// The real gate, in the pistis slot of the canonical chain, denies a
    /// capability the principal lacks at the pistis slot specifically, and lets
    /// a held one through to the executor.
    #[tokio::test]
    async fn dispatcher_denies_at_pistis_slot() {
        use syntheos_axon::AxonBus;
        use syntheos_dispatch::stubs::{EchoExecutor, StubGate};
        use syntheos_dispatch::{DispatchOutcome, Dispatcher};

        let p = PrincipalId::new();
        let bus = Arc::new(AxonBus::new());
        let gates: Vec<Box<dyn Gate>> = vec![
            Box::new(gate_for(p)),
            Box::new(StubGate::new("plutus")),
            Box::new(StubGate::new("eidolon")),
            Box::new(StubGate::new("human")),
            Box::new(StubGate::new("phylaxd")),
        ];
        let dispatcher =
            Dispatcher::new(gates, Box::new(EchoExecutor), bus).expect("canonical chain");

        // A capability the principal lacks -> denied at pistis.
        let denied = dispatcher
            .dispatch(request(
                p,
                Some("!r"),
                "delete",
                serde_json::json!({"capability": "delete", "action_kind": "delete"}),
            ))
            .await
            .expect("dispatch");
        match denied {
            DispatchOutcome::Denied { gate, .. } => assert_eq!(gate, "pistis"),
            other => panic!("expected Denied at pistis, got {other:?}"),
        }

        // A held capability -> traverses every stub and reaches the executor.
        let allowed = dispatcher
            .dispatch(request(
                p,
                Some("!r"),
                "run",
                serde_json::json!({"capability": "deploy", "action_kind": "deploy"}),
            ))
            .await
            .expect("dispatch");
        assert!(
            matches!(allowed, DispatchOutcome::Executed { .. }),
            "held capability must reach the executor, got {allowed:?}"
        );
    }

    /// Security invariant: an unavailable backing authority must
    /// produce a `Deny`, never an `Allow`. Property: for any principal, room,
    /// arbitrary caller metadata, a request evaluated against an *empty*
    /// room-state source -- the authority
    /// has no state to verify against -- never returns `Allow`.
    use proptest::prelude::*;

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(128))]
        /// Proves an empty authority never allows a capability request.
        #[test]
        fn empty_authority_never_allows_capability_request(
            name in "[a-z_]{1,16}",
            kind in proptest::sample::select(vec![
                "message", "capability_claim", "outcome", "task_accept", "task_complete",
                "commit", "commit_protected", "merge", "deploy", "delete", "credential_rotate",
                "ledger_modify", "review", "endorse",
            ]),
            room in "![a-z]{1,10}",
        ) {
            let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
            let mut policy = ToolActionPolicy::new();
            policy.register("synapse", "run", "deploy", ActionKind::Deploy);
            let gate = PistisGate::with_policy_and_clock(
                Arc::new(InMemoryRoomStateSource::new()),
                Arc::new(RoomTrustStore::new()),
                Arc::new(policy),
                Arc::new(FixedClock(OffsetDateTime::UNIX_EPOCH)),
            );
            let req = request(
                PrincipalId::new(),
                Some(&room),
                "run",
                serde_json::json!({"capability": name, "action_kind": kind}),
            );
            let decision = rt.block_on(gate.check(&req)).expect("gate check is total");
            prop_assert!(
                !matches!(decision, GateDecision::Allow),
                "empty authority must never Allow a capability request, got {decision:?}"
            );
        }
    }
}
