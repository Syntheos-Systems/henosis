//! `PistisGate`: capability-checking `ToolGate` backed by a placeholder Pistis client.
//!
//! Pistis is the Synapse trust/capability system (Rust port of "Blindfold" from
//! the Rift design doc). This module provides the gate-layer placeholder. The real
//! Pistis client -- with opaque capability handles, scoped grants, and an audit
//! trail -- lands in a future crate. Until then, `PistisClient` is a thin wrapper
//! around a `HashSet<Capability>` that the caller populates at construction time.
//!
//! ## Composition
//!
//! `PistisGate` wraps an inner `SharedGate` (typically `HookGate` wrapping
//! `PermissiveGate`) following the same pattern as `HookGate`. The capability
//! check runs first; if it passes, the call is delegated to the inner gate.
//! This lets callers compose:
//!
//! ```text
//! PistisGate
//!   └── HookGate
//!         └── PermissiveGate
//! ```
//!
//! ## Static capability map
//!
//! Tool names are mapped to required capabilities via a static lookup. The map
//! covers all built-in Synapse tools. Unknown tools require no capabilities (they
//! are not blocked by this gate; add them to the map to restrict them).

use std::collections::{HashMap, HashSet};
use std::path::Path;

use serde_json::Value;

use crate::tool::{GateDecision, SharedGate, ToolGate, ToolResult};

// Re-export Capability at the crate level so callers don't need synapse-core.
// We define Capability here because synapse-tools is the gating layer, and
// synapse-core depends on synapse-tools (not the reverse).
pub use crate::capability::Capability;

// ---------------------------------------------------------------------------
// PistisClient (placeholder)
// ---------------------------------------------------------------------------

/// Placeholder Pistis client.
///
/// Holds the set of capabilities granted for the current execution session.
/// The real client will replace this with opaque handles, scoped grants,
/// revocation support, and an audit trail.
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
        use crate::capability::Capability;
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
/// Covers all built-in Synapse tools. Unknown tools are not restricted
/// by this gate (their required slice is empty).
fn capability_map() -> HashMap<&'static str, Vec<Capability>> {
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
        ("delegate", vec![fs_read(), fs_write(), bash()]),
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
        // Agent-forge -- mix of network and local execution
        ("repo_map", vec![fs_read()]),
        ("search_code", vec![fs_read()]),
        ("forge_execute", vec![bash()]),
        ("forge_verify", vec![bash()]),
        ("ast_search", vec![fs_read()]),
        ("log_hypothesis", vec![network()]),
        ("log_outcome", vec![network()]),
        ("recall_errors", vec![network()]),
        ("test_impact", vec![fs_read()]),
        ("session_diff", vec![network()]),
        ("prose_analyze", vec![network()]),
        ("prose_learn", vec![network()]),
        // LSP -- reads diagnostics from a local server
        ("lsp_diagnostics", vec![fs_read()]),
        ("lsp_symbol_search", vec![fs_read()]),
        // Session search -- reads local SQLite
        ("session_search", vec![fs_read()]),
        ("session_list", vec![fs_read()]),
    ]
    .into_iter()
    .collect()
}

// ---------------------------------------------------------------------------
// PistisGate
// ---------------------------------------------------------------------------

/// `ToolGate` implementation that enforces Pistis capability grants.
///
/// Before each tool execution, looks up the capabilities required for the
/// tool name in the static map. If the `PistisClient`'s granted set is missing
/// any required capability, the tool is denied with a descriptive message.
/// When the check passes, the call is forwarded to the inner gate.
pub struct PistisGate {
    /// Placeholder Pistis client holding the granted capability set.
    client: PistisClient,
    /// Inner gate consulted after the capability check passes.
    inner: SharedGate,
    /// Static tool-to-capability map, computed once at construction.
    cap_map: HashMap<&'static str, Vec<Capability>>,
}

/// Adds inherent behavior for `PistisGate`.
impl PistisGate {
    /// Construct a `PistisGate` with explicit granted capabilities and an inner gate.
    ///
    /// `client` holds the set of capabilities granted for this session.
    /// `inner` is typically `HookGate(PermissiveGate)` or `PermissiveGate` directly.
    pub fn new(client: PistisClient, inner: SharedGate) -> Self {
        Self {
            client,
            inner,
            cap_map: capability_map(),
        }
    }

    /// Construct a `PistisGate` from task-local granted capabilities and an inner gate.
    ///
    /// This is the execution-mode constructor used by Synapse when a task has
    /// already been approved by Pistis and carries a concrete capability set.
    pub fn from_granted_capabilities(
        granted: impl IntoIterator<Item = Capability>,
        inner: SharedGate,
    ) -> Self {
        Self::new(PistisClient::new(granted), inner)
    }

    /// Convenience constructor for a permissive gate (all capabilities granted).
    ///
    /// Useful in development and tests.
    pub fn permissive(inner: SharedGate) -> Self {
        Self::new(PistisClient::permissive(), inner)
    }
}

/// Implements `ToolGate` behavior for `PistisGate`.
#[async_trait::async_trait]
impl ToolGate for PistisGate {
    /// Check capabilities before the tool runs, then delegate to the inner gate.
    ///
    /// If the client's granted set is missing any required capability, returns
    /// `GateDecision::Deny` with the name of the first missing capability.
    /// Otherwise forwards to `inner.before_execute`.
    async fn before_execute(&self, name: &str, params: &Value, cwd: &Path) -> GateDecision {
        let required: &[Capability] = self.cap_map.get(name).map(|v| v.as_slice()).unwrap_or(&[]);

        if let Err(missing) = self.client.check(required) {
            return GateDecision::Deny(format!(
                "missing capability: {missing} (required by tool '{name}')"
            ));
        }

        self.inner.before_execute(name, params, cwd).await
    }

    /// Delegate to the inner gate after execution. No capability logic needed here.
    async fn after_execute(&self, name: &str, params: &Value, result: &ToolResult, cwd: &Path) {
        self.inner.after_execute(name, params, result, cwd).await;
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
            .before_execute("delegate", &Value::Null, Path::new("/tmp"))
            .await;
        assert!(matches!(decision, GateDecision::Deny(_)));
    }

    /// A permissive gate allows everything.
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

    /// Unknown tool names (not in the cap map) are allowed through.
    #[tokio::test]
    async fn unknown_tool_allowed() {
        let gate = read_only_gate();
        let decision = gate
            .before_execute("totally_unknown_tool", &Value::Null, Path::new("/tmp"))
            .await;
        assert!(matches!(decision, GateDecision::Allow));
    }
}
