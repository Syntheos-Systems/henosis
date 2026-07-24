//! `PistisGate`: a capability-checking `ToolGate` with a pluggable authority.
//!
//! Pistis is the Synapse trust/capability system. This gate runs inside the
//! agent loop, before each tool executes, and asks an authority whether the
//! current principal may run that tool. The authority is pluggable so Synapse
//! works in two deployments without code changes at the call site:
//!
//! - **Standalone** (default): [`LocalAuthority`] checks a tool's required
//!   capabilities against a session-local `HashSet<Capability>` granted at
//!   construction. No Henosis dependency; Synapse runs on its own.
//! - **Under Henosis** (feature `henosis-pistis`): [`henosis::HenosisAuthority`]
//!   checks against in-process `henosis-pistis` room-state authority -- the same
//!   `authorize_capabilities` decision the dispatcher's pistis gate uses, with
//!   admission, trust threshold, and per-capability matching. Fail-closed: a
//!   room with no materialized state denies every restricted tool.
//!
//! ## Composition
//!
//! `PistisGate` wraps an inner `SharedGate` (typically `HookGate` wrapping
//! `PermissiveGate`). The capability check runs first; if it passes, the call is
//! delegated to the inner gate:
//!
//! ```text
//! PistisGate
//!   └── HookGate
//!         └── PermissiveGate
//! ```
//!
//! ## Static capability map
//!
//! Tool names are mapped to required capabilities via a static lookup shared by
//! both authorities. The map covers all built-in Synapse tools. Unknown tools
//! are denied so a missing policy entry cannot silently bypass authorization.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use serde_json::Value;

use crate::tool::{GateDecision, SharedGate, ToolGate, ToolResult};

// Re-export Capability at the crate level so callers don't need synapse-core.
// We define Capability here because synapse-tools is the gating layer, and
// synapse-core depends on synapse-tools (not the reverse).
pub use crate::capability::Capability;

// ---------------------------------------------------------------------------
// PistisAuthority -- the pluggable capability-decision backend
// ---------------------------------------------------------------------------

/// The outcome of a per-tool authorization decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthorizationOutcome {
    /// The tool is authorized to run.
    Allow,
    /// The tool is denied; the string is a human-readable reason.
    Deny(String),
}

/// A backend that decides whether a named tool may run for the current session.
///
/// `PistisGate` holds an `Arc<dyn PistisAuthority>` and consults it before each
/// tool. [`LocalAuthority`] implements the standalone path; the feature-gated
/// [`henosis::HenosisAuthority`] implements the in-process Henosis path.
#[async_trait::async_trait]
pub trait PistisAuthority: Send + Sync {
    /// Decide whether the tool named `name` may execute.
    async fn authorize_tool(&self, name: &str) -> AuthorizationOutcome;
}

// ---------------------------------------------------------------------------
// PistisClient (standalone capability set)
// ---------------------------------------------------------------------------

/// Session-local Pistis client holding the capabilities granted for this run.
///
/// In the standalone deployment this is the source of truth: a task is approved
/// with a concrete capability set, and the client answers membership queries
/// against it. Under Henosis the authority comes from room state instead and
/// this client is unused.
pub struct PistisClient {
    /// Capabilities currently granted for this session.
    granted: HashSet<Capability>,
}

/// Adds inherent behavior for `PistisClient`.
impl PistisClient {
    /// Construct a client with an explicit set of granted capabilities.
    pub fn new(granted: impl IntoIterator<Item = Capability>) -> Self {
        Self {
            granted: granted.into_iter().collect(),
        }
    }

    /// Construct a client that grants every capability.
    ///
    /// Useful in development and tests where Pistis enforcement is not yet needed.
    pub fn permissive() -> Self {
        Self {
            granted: [
                Capability::new(Capability::FS_READ),
                Capability::new(Capability::FS_WRITE),
                Capability::new(Capability::BASH),
                Capability::new(Capability::NETWORK),
            ]
            .into_iter()
            .collect(),
        }
    }

    /// Check whether all required capabilities are granted.
    ///
    /// Returns `Ok(())` if all are present, or `Err(missing)` naming the
    /// first missing capability. The lifetime `'a` ties the error reference
    /// to the `required` slice, not to `&self`.
    pub fn check<'a>(&self, required: &'a [Capability]) -> Result<(), &'a Capability> {
        for cap in required {
            if !self.granted.contains(cap) {
                return Err(cap);
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Static tool -> capability map
// ---------------------------------------------------------------------------

/// Build the static map from tool name to required capabilities.
///
/// Covers all built-in Synapse tools. Unknown tools are denied by both
/// authorities. Shared by both authorities.
pub(crate) fn capability_map() -> HashMap<&'static str, Vec<Capability>> {
    let fs_read = || Capability::new(Capability::FS_READ);
    let fs_write = || Capability::new(Capability::FS_WRITE);
    let bash = || Capability::new(Capability::BASH);
    let network = || Capability::new(Capability::NETWORK);

    [
        // Bash execution
        ("bash", vec![bash()]),
        // Filesystem reads
        ("read", vec![fs_read()]),
        ("grep", vec![fs_read()]),
        ("glob", vec![fs_read()]),
        ("ls", vec![fs_read()]),
        // Filesystem writes
        ("write", vec![fs_write()]),
        ("edit", vec![fs_write()]),
        // Filesystem read+write (delegate may read and write)
        ("delegate_task", vec![fs_read(), fs_write(), bash()]),
        // Network
        ("web_fetch", vec![network()]),
        ("web_search", vec![network()]),
        // Kleos tools -- require network (they call the Kleos API)
        ("kleos_search", vec![network()]),
        ("kleos_store", vec![network()]),
        ("kleos_delete", vec![network()]),
        ("kleos_list", vec![network()]),
        ("kleos_context", vec![network()]),
        ("kleos_recall", vec![network()]),
        ("kleos_faceted_search", vec![network()]),
        ("kleos_profile", vec![network()]),
        ("brain_query", vec![network()]),
        ("brain_absorb", vec![network()]),
        ("graph_search", vec![network()]),
        ("graph_neighborhood", vec![network()]),
        ("graph_create_entity", vec![network()]),
        ("intelligence_consolidate", vec![network()]),
        ("intelligence_contradictions", vec![network()]),
        ("intelligence_reflect", vec![network()]),
        ("intelligence_digest", vec![network()]),
        ("intelligence_sentiment", vec![network()]),
        ("intelligence_time_travel", vec![network()]),
        ("skill_search", vec![network()]),
        ("skill_get", vec![network()]),
        ("skill_execute", vec![network()]),
        ("skill_create", vec![network()]),
        ("skill_list", vec![network()]),
        ("skill_invoke", vec![network()]),
        ("handoff_store", vec![network()]),
        ("handoff_restore", vec![network()]),
        ("handoff_search", vec![network()]),
        ("activity_report", vec![network()]),
        ("task_create", vec![network()]),
        ("task_update", vec![network()]),
        ("task_list", vec![network()]),
        ("task_feed", vec![network()]),
        ("axon_publish", vec![network()]),
        ("axon_poll", vec![network()]),
        ("broca_log", vec![network()]),
        ("soma_register", vec![network()]),
        ("soma_heartbeat", vec![network()]),
        ("thymus_eval", vec![network()]),
        ("loom_create_workflow", vec![network()]),
        ("loom_create_run", vec![network()]),
        ("loom_complete_step", vec![network()]),
        ("conversation_create", vec![network()]),
        ("conversation_message", vec![network()]),
        ("conversation_search", vec![network()]),
        ("episode_create", vec![network()]),
        ("episode_finalize", vec![network()]),
        ("personality_profile", vec![network()]),
        ("personality_detect", vec![network()]),
        ("scratch_put", vec![network()]),
        ("scratch_list", vec![network()]),
        ("scratch_promote", vec![network()]),
        ("gate_check", vec![network()]),
        ("gate_respond", vec![network()]),
        ("growth_reflect", vec![network()]),
        ("growth_observations", vec![network()]),
        ("fsrs_recall_due", vec![network()]),
        ("fsrs_review", vec![network()]),
        ("prompt_generate", vec![network()]),
        ("prompt_header", vec![network()]),
        // Agent-forge structural tools execute outside the retained directory
        // capability, so they require the same ambient authority as a shell.
        ("repo_map", vec![fs_read(), bash()]),
        ("search_code", vec![fs_read(), bash()]),
        ("execute", vec![bash()]),
        ("verify", vec![bash()]),
        ("ast_search", vec![fs_read(), bash()]),
        ("log_hypothesis", vec![network()]),
        ("log_outcome", vec![network()]),
        ("recall_errors", vec![network()]),
        ("test_impact", vec![fs_read(), bash()]),
        ("session_diff", vec![fs_read(), bash(), network()]),
        ("prose_analyze", vec![network()]),
        ("prose_learn", vec![network()]),
        // LSP -- diagnostics launch language-specific local processes
        ("lsp_diagnostics", vec![fs_read(), bash()]),
        ("lsp_symbol_search", vec![fs_read(), bash()]),
        // Session search -- reads local SQLite
        ("session_search", vec![fs_read()]),
        ("session_list", vec![fs_read()]),
    ]
    .into_iter()
    .collect()
}

// ---------------------------------------------------------------------------
// LocalAuthority -- standalone capability set + static map
// ---------------------------------------------------------------------------

/// The standalone Pistis authority: a session-local granted capability set
/// checked against the static tool->capability map.
///
/// This is the default backend. It requires no Henosis dependency, so Synapse
/// runs on its own. An unmapped tool is denied; a mapped tool whose requirements
/// are not all granted is denied, naming the first missing capability.
pub struct LocalAuthority {
    /// The capabilities granted for this session.
    client: PistisClient,
    /// Static tool-to-capability map, computed once at construction.
    cap_map: HashMap<&'static str, Vec<Capability>>,
}

/// Adds inherent behavior for `LocalAuthority`.
impl LocalAuthority {
    /// Construct from a populated [`PistisClient`].
    pub fn new(client: PistisClient) -> Self {
        Self {
            client,
            cap_map: capability_map(),
        }
    }

    /// Construct from an explicit set of granted capabilities.
    pub fn from_granted_capabilities(granted: impl IntoIterator<Item = Capability>) -> Self {
        Self::new(PistisClient::new(granted))
    }

    /// Construct a permissive authority (all capabilities granted).
    pub fn permissive() -> Self {
        Self::new(PistisClient::permissive())
    }
}

/// Implements the standalone capability check.
#[async_trait::async_trait]
impl PistisAuthority for LocalAuthority {
    /// Deny unmapped tools; otherwise require every mapped capability to be
    /// granted, naming the first missing one on denial.
    async fn authorize_tool(&self, name: &str) -> AuthorizationOutcome {
        let Some(required) = self.cap_map.get(name) else {
            return AuthorizationOutcome::Deny(format!(
                "no capability policy registered for tool '{name}'"
            ));
        };
        match self.client.check(required) {
            Ok(()) => AuthorizationOutcome::Allow,
            Err(missing) => AuthorizationOutcome::Deny(format!(
                "missing capability: {missing} (required by tool '{name}')"
            )),
        }
    }
}

// ---------------------------------------------------------------------------
// PistisGate
// ---------------------------------------------------------------------------

/// `ToolGate` implementation that enforces Pistis capability grants via a
/// pluggable [`PistisAuthority`].
///
/// Before each tool execution, asks the authority whether the tool may run. On
/// `Deny`, returns `GateDecision::Deny`. On `Allow`, forwards to the inner gate.
pub struct PistisGate {
    /// The capability-decision backend (local or Henosis-backed).
    authority: Arc<dyn PistisAuthority>,
    /// Inner gate consulted after the capability check passes.
    inner: SharedGate,
}

/// Adds inherent behavior for `PistisGate`.
impl PistisGate {
    /// Construct a `PistisGate` over an explicit authority and inner gate.
    ///
    /// This is the general constructor. Use it to install the Henosis-backed
    /// authority (`with_authority(Arc::new(HenosisAuthority::new(...)), inner)`)
    /// or any custom backend.
    pub fn with_authority(authority: Arc<dyn PistisAuthority>, inner: SharedGate) -> Self {
        Self { authority, inner }
    }

    /// Construct a `PistisGate` from a standalone [`PistisClient`] and inner gate.
    pub fn new(client: PistisClient, inner: SharedGate) -> Self {
        Self::with_authority(Arc::new(LocalAuthority::new(client)), inner)
    }

    /// Construct a `PistisGate` from task-local granted capabilities and an inner gate.
    ///
    /// This is the standalone execution-mode constructor used by Synapse when a
    /// task has already been approved and carries a concrete capability set.
    pub fn from_granted_capabilities(
        granted: impl IntoIterator<Item = Capability>,
        inner: SharedGate,
    ) -> Self {
        Self::with_authority(
            Arc::new(LocalAuthority::from_granted_capabilities(granted)),
            inner,
        )
    }

    /// Convenience constructor for a permissive gate (all capabilities granted).
    ///
    /// Useful in development and tests.
    pub fn permissive(inner: SharedGate) -> Self {
        Self::with_authority(Arc::new(LocalAuthority::permissive()), inner)
    }
}

/// Implements `ToolGate` behavior for `PistisGate`.
#[async_trait::async_trait]
impl ToolGate for PistisGate {
    /// Ask the authority before the tool runs, then delegate to the inner gate.
    ///
    /// If the authority denies, returns `GateDecision::Deny` with its reason.
    /// Otherwise forwards to `inner.before_execute`.
    async fn before_execute(&self, name: &str, params: &Value, cwd: &Path) -> GateDecision {
        match self.authority.authorize_tool(name).await {
            AuthorizationOutcome::Allow => self.inner.before_execute(name, params, cwd).await,
            AuthorizationOutcome::Deny(reason) => GateDecision::Deny(reason),
        }
    }

    /// Delegate to the inner gate after execution. No capability logic needed here.
    async fn after_execute(&self, name: &str, params: &Value, result: &ToolResult, cwd: &Path) {
        self.inner.after_execute(name, params, result, cwd).await;
    }
}

// ---------------------------------------------------------------------------
// HenosisAuthority -- in-process henosis-pistis room-state backend
// ---------------------------------------------------------------------------

/// In-process Pistis authority backed by `henosis-pistis` room state.
///
/// Enabled by the `henosis-pistis` feature. Synapse running under Henosis
/// installs this authority so tool gating runs through the same
/// `authorize_capabilities` decision (admission + trust threshold + per-cap
/// match) as the dispatcher's pistis gate, fail-closed when room state is absent.
#[cfg(feature = "henosis-pistis")]
pub mod henosis {
    use std::collections::HashMap;
    use std::sync::Arc;

    use henosis_pistis::authority::{
        CapabilityCheckRequest, CapabilityRequirement, authorize_capabilities,
    };
    use henosis_pistis::model::{ActionKind, RoomScope};
    use henosis_pistis::{Clock, RoomStateSource, RoomTrustStore, SystemClock};
    use syntheos_contracts::{PrincipalId, TenantId};

    use super::{AuthorizationOutcome, Capability, PistisAuthority, capability_map};

    /// Capability authority backed by in-process henosis-pistis room state.
    ///
    /// Holds a room-state source, the principal the agent runs as, the room id,
    /// a clock, and the action kind synapse tool calls are arbitrated under
    /// (default [`ActionKind::Message`] -- a synapse tool call is an in-room
    /// agent action). The principal is supplied at construction; agent-name ->
    /// `PrincipalId` resolution is the caller's concern, not this gate's.
    pub struct HenosisAuthority {
        /// Where materialized room state is obtained.
        source: Arc<dyn RoomStateSource>,
        /// Gate-owned issuer pins and rollback floors.
        trust_store: Arc<RoomTrustStore>,
        /// Tenant whose exact room scope governs this session.
        tenant: TenantId,
        /// The principal whose admission/trust/capabilities are checked.
        principal: PrincipalId,
        /// The room whose authority state governs this session.
        room: String,
        /// Clock feeding the trust math (injected for deterministic tests).
        clock: Arc<dyn Clock>,
        /// The action kind synapse tool capabilities are required under.
        action_kind: ActionKind,
        /// Static tool-to-capability map (shared with the local authority).
        cap_map: HashMap<&'static str, Vec<Capability>>,
    }

    /// Adds inherent behavior for `HenosisAuthority`.
    impl HenosisAuthority {
        /// Construct over a room-state source, principal, and room id, using the
        /// system clock and [`ActionKind::Message`].
        pub fn new(
            source: Arc<dyn RoomStateSource>,
            trust_store: Arc<RoomTrustStore>,
            tenant: TenantId,
            principal: PrincipalId,
            room: impl Into<String>,
        ) -> Self {
            Self {
                source,
                trust_store,
                tenant,
                principal,
                room: room.into(),
                clock: Arc::new(SystemClock),
                action_kind: ActionKind::Message,
                cap_map: capability_map(),
            }
        }

        /// Override the clock (deterministic tests).
        pub fn with_clock(mut self, clock: Arc<dyn Clock>) -> Self {
            self.clock = clock;
            self
        }

        /// Override the action kind synapse tool capabilities are required under.
        pub fn with_action_kind(mut self, kind: ActionKind) -> Self {
            self.action_kind = kind;
            self
        }
    }

    /// Implements the in-process Henosis capability check, fail-closed.
    #[async_trait::async_trait]
    impl PistisAuthority for HenosisAuthority {
        /// Deny unmapped tools; otherwise authorize every mapped capability
        /// against the materialized room state. A room with no
        /// materialized authority state denies (fail-closed) -- Pistis cannot
        /// verify, so it does not allow.
        async fn authorize_tool(&self, name: &str) -> AuthorizationOutcome {
            let Some(required) = self.cap_map.get(name) else {
                return AuthorizationOutcome::Deny(format!(
                    "no capability policy registered for tool '{name}'"
                ));
            };

            let scope = RoomScope::new(self.tenant, &self.room);
            let Some(state) = self.source.room_state(&scope) else {
                return AuthorizationOutcome::Deny(format!(
                    "no pistis authority state for requested tenant and room {}",
                    self.room
                ));
            };
            let verified = match state.verify_for(&scope, &self.trust_store) {
                Ok(state) => state,
                Err(error) => {
                    return AuthorizationOutcome::Deny(format!(
                        "pistis authority state failed verification: {error}"
                    ));
                }
            };

            let request = CapabilityCheckRequest {
                principal: self.principal,
                required: required
                    .iter()
                    .map(|cap| CapabilityRequirement {
                        name: cap.as_str().to_owned(),
                        action_kind: self.action_kind,
                    })
                    .collect(),
            };

            let decision = authorize_capabilities(&verified, &request, self.clock.now());
            if decision.allowed {
                AuthorizationOutcome::Allow
            } else {
                AuthorizationOutcome::Deny(
                    decision
                        .reason
                        .unwrap_or_else(|| "capability denied".to_owned()),
                )
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::PermissiveGate;
    use std::path::Path;
    use std::sync::Arc;

    /// Build a `PistisGate` with only `fs_read` granted, wrapping `PermissiveGate`.
    fn read_only_gate() -> PistisGate {
        PistisGate::from_granted_capabilities(
            [Capability::new(Capability::FS_READ)],
            Arc::new(PermissiveGate),
        )
    }

    /// A tool that requires `fs_read` is allowed when that capability is granted.
    #[tokio::test]
    async fn allows_granted_capability() {
        let gate = read_only_gate();
        let decision = gate
            .before_execute("read", &Value::Null, Path::new("/tmp"))
            .await;
        assert!(matches!(decision, GateDecision::Allow));
    }

    /// A tool that requires `bash` is denied when only `fs_read` is granted.
    #[tokio::test]
    async fn denies_missing_capability() {
        let gate = read_only_gate();
        let decision = gate
            .before_execute("bash", &Value::Null, Path::new("/tmp"))
            .await;
        match decision {
            GateDecision::Deny(msg) => {
                assert!(
                    msg.contains("bash"),
                    "denial message should name the capability: {msg}"
                );
            }
            GateDecision::Allow => panic!("expected Deny but got Allow"),
        }
    }

    /// A tool that needs multiple mapped capabilities is denied unless all grants exist.
    #[tokio::test]
    async fn denies_delegate_when_grants_are_partial() {
        let gate = read_only_gate();
        let decision = gate
            .before_execute("delegate_task", &Value::Null, Path::new("/tmp"))
            .await;
        assert!(matches!(decision, GateDecision::Deny(_)));
    }

    /// Process and network tools cannot run with only filesystem-read access.
    #[tokio::test]
    async fn denies_sensitive_builtins_without_their_grants() {
        let gate = read_only_gate();
        for tool in &[
            "execute",
            "verify",
            "repo_map",
            "search_code",
            "ast_search",
            "test_impact",
            "session_diff",
            "skill_invoke",
            "lsp_diagnostics",
            "lsp_symbol_search",
        ] {
            let decision = gate
                .before_execute(tool, &Value::Null, Path::new("/tmp"))
                .await;
            assert!(
                matches!(decision, GateDecision::Deny(_)),
                "expected Deny for tool '{tool}'"
            );
        }
    }

    /// A permissive gate allows mapped tools because it holds every capability.
    #[tokio::test]
    async fn permissive_gate_allows_all() {
        let gate = PistisGate::permissive(Arc::new(PermissiveGate));
        for tool in &["bash", "read", "write", "web_fetch", "glob"] {
            let decision = gate
                .before_execute(tool, &Value::Null, Path::new("/tmp"))
                .await;
            assert!(
                matches!(decision, GateDecision::Allow),
                "expected Allow for tool '{tool}'"
            );
        }
    }

    /// Unknown tool names are denied because no capability policy covers them.
    #[tokio::test]
    async fn unknown_tool_denied() {
        let gate = read_only_gate();
        let decision = gate
            .before_execute("totally_unknown_tool", &Value::Null, Path::new("/tmp"))
            .await;
        assert!(matches!(decision, GateDecision::Deny(_)));
    }

    /// Every tool in the default registry has an explicit capability policy.
    #[test]
    fn default_registry_tools_have_capability_policies() {
        let map = capability_map();
        let registry = crate::default_tools();
        let missing: Vec<String> = registry
            .all_tool_schemas()
            .into_iter()
            .filter_map(|schema| schema["name"].as_str().map(str::to_owned))
            .filter(|name| !map.contains_key(name.as_str()))
            .collect();
        assert!(missing.is_empty(), "unmapped built-in tools: {missing:?}");
        assert!(map.contains_key("delegate_task"));
    }

    /// The `with_authority` constructor accepts any `PistisAuthority`; a custom
    /// always-deny authority denies every tool.
    #[tokio::test]
    async fn with_authority_uses_custom_backend() {
        /// Test authority that rejects every tool name.
        struct DenyAll;
        /// Implements the unconditional denial policy used by this test.
        #[async_trait::async_trait]
        impl PistisAuthority for DenyAll {
            /// Reject the requested tool with a deterministic policy reason.
            async fn authorize_tool(&self, name: &str) -> AuthorizationOutcome {
                AuthorizationOutcome::Deny(format!("policy: {name} forbidden"))
            }
        }
        let gate = PistisGate::with_authority(Arc::new(DenyAll), Arc::new(PermissiveGate));
        let decision = gate
            .before_execute("read", &Value::Null, Path::new("/tmp"))
            .await;
        assert!(matches!(decision, GateDecision::Deny(_)));
    }
}

// ---------------------------------------------------------------------------
// Henosis-backed authority tests (feature-gated)
// ---------------------------------------------------------------------------

#[cfg(all(test, feature = "henosis-pistis"))]
mod henosis_tests {
    use super::henosis::HenosisAuthority;
    use super::*;
    use crate::tool::PermissiveGate;
    use std::collections::BTreeSet;
    use std::path::Path;
    use std::sync::Arc;

    use henosis_pistis::crypto::SecretKey;
    use henosis_pistis::gate::{Clock, InMemoryRoomStateSource};
    use henosis_pistis::model::{
        ActionKind, AdmittedPrincipal, Capability as PCapability, RoomPolicy, RoomScope,
    };
    use henosis_pistis::room::{RoomState, RoomTrustStore};
    use syntheos_contracts::{PrincipalId, TenantId};
    use time::OffsetDateTime;

    /// A fixed clock so the trust math is deterministic.
    struct FixedClock(OffsetDateTime);
    /// Supplies the fixed timestamp used by Henosis authority tests.
    impl Clock for FixedClock {
        /// Return the clock's configured timestamp.
        fn now(&self) -> OffsetDateTime {
            self.0
        }
    }

    /// A room admitting `principal` holding the synapse `fs_read` and `bash`
    /// capabilities under `ActionKind::Message`.
    fn room_with(tenant: TenantId, principal: PrincipalId) -> (RoomState, RoomTrustStore) {
        let scope = RoomScope::new(tenant, "!r");
        let (_, issuer_key) = SecretKey::generate();
        let (_, root_key) = SecretKey::generate();
        let (_, principal_key) = SecretKey::generate();
        let mk = |name: &str| PCapability {
            name: name.to_owned(),
            action_kinds: BTreeSet::from([ActionKind::Message]),
            granted_by: "operator".to_owned(),
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
                vec![mk(Capability::FS_READ), mk(Capability::BASH)],
            )],
        )
        .unwrap();
        let mut trust = RoomTrustStore::new();
        trust.pin(scope, issuer_key.public_key(), 1).unwrap();
        (state, trust)
    }

    /// Build a Henosis-backed gate whose room "!r" admits `principal`.
    fn gate_for(principal: PrincipalId) -> PistisGate {
        let tenant = TenantId::new();
        let (state, trust) = room_with(tenant, principal);
        let mut source = InMemoryRoomStateSource::new();
        source.insert(state);
        let authority =
            HenosisAuthority::new(Arc::new(source), Arc::new(trust), tenant, principal, "!r")
                .with_clock(Arc::new(FixedClock(OffsetDateTime::now_utc())));
        PistisGate::with_authority(Arc::new(authority), Arc::new(PermissiveGate))
    }

    /// A held capability (`read` -> `fs_read`) is allowed.
    #[tokio::test]
    async fn henosis_allows_held_capability() {
        let p = PrincipalId::new();
        let gate = gate_for(p);
        let decision = gate
            .before_execute("read", &Value::Null, Path::new("/tmp"))
            .await;
        assert!(matches!(decision, GateDecision::Allow));
    }

    /// A tool needing an unheld capability (`write` -> `fs_write`) is denied.
    #[tokio::test]
    async fn henosis_denies_unheld_capability() {
        let p = PrincipalId::new();
        let gate = gate_for(p);
        let decision = gate
            .before_execute("write", &Value::Null, Path::new("/tmp"))
            .await;
        assert!(matches!(decision, GateDecision::Deny(_)));
    }

    /// A tool with no mapped policy is denied regardless of room state.
    #[tokio::test]
    async fn henosis_denies_unmapped_tool() {
        let p = PrincipalId::new();
        let gate = gate_for(p);
        let decision = gate
            .before_execute("totally_unknown_tool", &Value::Null, Path::new("/tmp"))
            .await;
        assert!(matches!(decision, GateDecision::Deny(_)));
    }

    /// Fail-closed: an empty room-state source denies every restricted tool.
    #[tokio::test]
    async fn henosis_empty_room_state_denies() {
        let p = PrincipalId::new();
        let authority = HenosisAuthority::new(
            Arc::new(InMemoryRoomStateSource::new()),
            Arc::new(RoomTrustStore::new()),
            TenantId::new(),
            p,
            "!r",
        )
        .with_clock(Arc::new(FixedClock(OffsetDateTime::UNIX_EPOCH)));
        let gate = PistisGate::with_authority(Arc::new(authority), Arc::new(PermissiveGate));
        let decision = gate
            .before_execute("read", &Value::Null, Path::new("/tmp"))
            .await;
        assert!(
            matches!(decision, GateDecision::Deny(_)),
            "empty authority must deny a restricted tool (fail-closed)"
        );
    }

    /// An unadmitted principal is denied even for a tool whose capability the
    /// room grants to someone else.
    #[tokio::test]
    async fn henosis_unadmitted_principal_denied() {
        let admitted = PrincipalId::new();
        let tenant = TenantId::new();
        let (state, trust) = room_with(tenant, admitted);
        let mut source = InMemoryRoomStateSource::new();
        source.insert(state);
        let intruder = PrincipalId::new();
        let authority =
            HenosisAuthority::new(Arc::new(source), Arc::new(trust), tenant, intruder, "!r")
                .with_clock(Arc::new(FixedClock(OffsetDateTime::now_utc())));
        let gate = PistisGate::with_authority(Arc::new(authority), Arc::new(PermissiveGate));
        let decision = gate
            .before_execute("read", &Value::Null, Path::new("/tmp"))
            .await;
        assert!(matches!(decision, GateDecision::Deny(_)));
    }

    /// The Synapse authority rejects a source snapshot under an unpinned issuer.
    #[tokio::test]
    async fn henosis_rejects_source_generated_issuer() {
        let principal = PrincipalId::new();
        let tenant = TenantId::new();
        let (state, _matching_trust) = room_with(tenant, principal);
        let mut source = InMemoryRoomStateSource::new();
        source.insert(state);
        let (_, pinned_issuer) = SecretKey::generate();
        let mut trust = RoomTrustStore::new();
        trust
            .pin(RoomScope::new(tenant, "!r"), pinned_issuer.public_key(), 1)
            .unwrap();
        let authority =
            HenosisAuthority::new(Arc::new(source), Arc::new(trust), tenant, principal, "!r")
                .with_clock(Arc::new(FixedClock(OffsetDateTime::now_utc())));
        let gate = PistisGate::with_authority(Arc::new(authority), Arc::new(PermissiveGate));
        let decision = gate
            .before_execute("read", &Value::Null, Path::new("/tmp"))
            .await;
        assert!(matches!(decision, GateDecision::Deny(_)));
    }
}
