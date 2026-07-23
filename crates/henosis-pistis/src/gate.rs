//! The `pistis` gate: capability + trust authorization, fail-closed.
//!
//! Pistis is the first gate in the canonical dispatcher chain (`pistis ->
//! plutus -> eidolon -> human -> phylaxd`). It authorizes an invocation that
//! *declares a capability requirement* against the requesting principal's
//! admission and trust in the relevant room. An invocation that declares no
//! capability requirement is not Pistis's concern -- it is allowed for the rest
//! of the chain to decide, just as the broker gate only acts on its own tool.
//!
//! Convention: a capability-bearing invocation carries a string `capability`
//! arg (the requirement name) and a string `action_kind` arg (an [`ActionKind`]
//! token). The trusted invocation builder sets this requirement rather than the
//! principal. A malformed requirement -- unknown `action_kind` -- is DENIED.
//!
//! Fail-closed by construction. The only paths to `Allow` are: no capability
//! requirement declared, or an explicit admitted-and-trusted-and-capable
//! verdict from [`authorize_capabilities`]. A declared requirement with no room,
//! or a room with no materialized authority state, is DENIED -- Pistis cannot
//! verify, so it does not allow. There is NO advisory mode and NO self-approval.

use std::sync::Arc;

use async_trait::async_trait;
use syntheos_contracts::{Gate, GateDecision, GateError, GateRequest, ToolInvocation};
use time::OffsetDateTime;

use crate::authority::{authorize_capabilities, CapabilityCheckRequest, CapabilityRequirement};
use crate::model::ActionKind;
use crate::room::RoomState;

/// A source of materialized room state, keyed by room id. The live
/// implementation materializes the room's signed-event log inside Pistis; tests
/// supply an in-memory map. `None` means there is no Pistis authority state for
/// that room (the gate treats this as fail-closed: deny a declared requirement).
pub trait RoomStateSource: Send + Sync {
    /// Return the current materialized state for `room`, or `None` if unknown.
    fn room_state(&self, room: &str) -> Option<RoomState>;
}

/// An in-memory [`RoomStateSource`] backed by a room-id map.
///
/// The default empty source returns `None` for every room, so the gate denies every
/// capability-bearing request while allowing requests that declare no capability. Deployments
/// may provide materialized room state.
#[derive(Debug, Clone, Default)]
pub struct InMemoryRoomStateSource {
    /// Materialized state keyed by room id.
    rooms: std::collections::HashMap<String, RoomState>,
}

/// Implements construction and mutation for the in-memory state source.
impl InMemoryRoomStateSource {
    /// Construct an empty source (no rooms materialized).
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert or replace the materialized state for `room`.
    pub fn insert(&mut self, room: impl Into<String>, state: RoomState) {
        self.rooms.insert(room.into(), state);
    }
}

/// Supplies room state from the in-memory source.
impl RoomStateSource for InMemoryRoomStateSource {
    /// Look up the materialized state for `room`.
    fn room_state(&self, room: &str) -> Option<RoomState> {
        self.rooms.get(room).cloned()
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
    /// The clock that feeds the trust math.
    clock: Arc<dyn Clock>,
}

/// Implements Pistis gate construction and capability parsing.
impl PistisGate {
    /// Build the gate over a room-state source, using the system clock.
    pub fn new(source: Arc<dyn RoomStateSource>) -> Self {
        Self {
            source,
            clock: Arc::new(SystemClock),
        }
    }

    /// Build the gate with an explicit clock (for deterministic tests).
    pub fn with_clock(source: Arc<dyn RoomStateSource>, clock: Arc<dyn Clock>) -> Self {
        Self { source, clock }
    }

    /// Pull a required string field from the invocation args.
    fn arg_str<'a>(invocation: &'a ToolInvocation, key: &str) -> Option<&'a str> {
        invocation.args.get(key).and_then(|v| v.as_str())
    }

    /// Extract the declared capability requirement, if any.
    ///
    /// Returns `Ok(None)` when no `capability` arg is present (nothing to
    /// enforce), `Ok(Some(req))` for a well-formed requirement, and `Err(reason)`
    /// when a `capability` is declared but its `action_kind` is missing or
    /// unknown (a malformed requirement, which the caller denies).
    fn required_capability(
        invocation: &ToolInvocation,
    ) -> std::result::Result<Option<CapabilityRequirement>, String> {
        let Some(name) = Self::arg_str(invocation, "capability") else {
            return Ok(None);
        };
        let Some(kind_token) = Self::arg_str(invocation, "action_kind") else {
            return Err(format!(
                "capability '{name}' declared without an 'action_kind' arg"
            ));
        };
        let Some(action_kind) = ActionKind::parse(kind_token) else {
            return Err(format!("unknown action_kind '{kind_token}'"));
        };
        Ok(Some(CapabilityRequirement {
            name: name.to_owned(),
            action_kind,
        }))
    }
}

#[async_trait]
/// Applies Pistis capability and trust policy in the dispatcher gate chain.
impl Gate for PistisGate {
    /// The canonical authority name for this slot.
    fn name(&self) -> &str {
        "pistis"
    }

    /// Authorize a capability-bearing invocation; allow one that declares no
    /// requirement; deny a malformed or unverifiable one.
    async fn check(&self, req: &GateRequest) -> Result<GateDecision, GateError> {
        // What capability does this invocation require, if any?
        let requirement = match Self::required_capability(&req.invocation) {
            Ok(Some(r)) => r,
            // No capability requirement -- not Pistis's concern.
            Ok(None) => return Ok(GateDecision::Allow),
            Err(reason) => return Ok(GateDecision::Deny { reason }),
        };

        // A capability is required, so the request must be evaluable: it needs a
        // room, and that room needs materialized authority state. Either gap is
        // a fail-closed denial.
        let Some(room) = req.context.room.as_deref() else {
            return Ok(GateDecision::Deny {
                reason: format!(
                    "capability '{}' required but request carries no room",
                    requirement.name
                ),
            });
        };
        let Some(state) = self.source.room_state(room) else {
            return Ok(GateDecision::Deny {
                reason: format!("no pistis authority state for room {room}"),
            });
        };

        let decision = authorize_capabilities(
            &state,
            &CapabilityCheckRequest {
                principal: req.context.principal,
                required: vec![requirement],
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
    use crate::crypto::{PublicKey, SecretKey};
    use crate::model::{AdmittedPrincipal, Capability, RoomPolicy};
    use std::collections::{BTreeSet, HashMap};
    use syntheos_contracts::{PrincipalId, RequestContext, TenantId};

    /// A fixed clock for deterministic trust evaluation.
    struct FixedClock(OffsetDateTime);
    /// Reads the deterministic test instant.
    impl Clock for FixedClock {
        /// Returns the fixed test instant.
        fn now(&self) -> OffsetDateTime {
            self.0
        }
    }

    /// An in-memory room-state source.
    #[derive(Default)]
    struct MapSource(HashMap<String, RoomState>);
    /// Supplies cloned room state from the test map.
    impl RoomStateSource for MapSource {
        /// Returns the configured state for a room.
        fn room_state(&self, room: &str) -> Option<RoomState> {
            self.0.get(room).cloned()
        }
    }

    /// A fresh public key.
    fn pubkey() -> PublicKey {
        SecretKey::generate().0
    }

    /// A room admitting `principal` with a `deploy`/`Deploy` capability.
    fn room_with(principal: PrincipalId) -> RoomState {
        let cap = Capability {
            name: "deploy".into(),
            action_kinds: BTreeSet::from([ActionKind::Deploy]),
            granted_by: "operator".into(),
            expires_at: None,
        };
        RoomState::from_genesis(
            RoomPolicy::default(),
            [pubkey()].into_iter().collect(),
            vec![AdmittedPrincipal::new(principal, pubkey(), vec![cap])],
        )
    }

    /// Build a gate whose only known room "!r" admits `principal`.
    fn gate_for(principal: PrincipalId) -> PistisGate {
        let mut map = MapSource::default();
        map.0.insert("!r".to_string(), room_with(principal));
        PistisGate::with_clock(
            Arc::new(map),
            Arc::new(FixedClock(OffsetDateTime::now_utc())),
        )
    }

    /// Build a gate request.
    fn request(principal: PrincipalId, room: Option<&str>, args: serde_json::Value) -> GateRequest {
        GateRequest {
            context: RequestContext {
                tenant: TenantId::new(),
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
                action: "run".into(),
                args,
            },
        }
    }

    /// An invocation declaring no capability requirement is allowed.
    #[tokio::test]
    async fn no_requirement_allowed() {
        let p = PrincipalId::new();
        let gate = gate_for(p);
        let req = request(p, Some("!r"), serde_json::json!({}));
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
            serde_json::json!({"capability": "deploy", "action_kind": "deploy"}),
        );
        assert!(matches!(
            gate.check(&req).await.unwrap(),
            GateDecision::Deny { .. }
        ));
    }

    /// A malformed requirement (unknown action_kind) is denied, never allowed.
    #[tokio::test]
    async fn malformed_action_kind_denied() {
        let p = PrincipalId::new();
        let gate = gate_for(p);
        let req = request(
            p,
            Some("!r"),
            serde_json::json!({"capability": "deploy", "action_kind": "teleport"}),
        );
        assert!(matches!(
            gate.check(&req).await.unwrap(),
            GateDecision::Deny { .. }
        ));
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
            serde_json::json!({"capability": "deploy", "action_kind": "deploy"}),
        );
        assert!(matches!(
            gate.check(&req).await.unwrap(),
            GateDecision::Deny { .. }
        ));
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
    /// capability name, and (well-formed) action kind, a capability-bearing
    /// request evaluated against an *empty* room-state source -- the authority
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
            let gate = PistisGate::with_clock(
                Arc::new(InMemoryRoomStateSource::new()),
                Arc::new(FixedClock(OffsetDateTime::UNIX_EPOCH)),
            );
            let req = request(
                PrincipalId::new(),
                Some(&room),
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
