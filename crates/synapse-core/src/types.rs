use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;

use serde::Serialize;

use crate::compression::CompressionConfig;
use crate::hooks::HookConfig;
use crate::router::ModelRouter;
use synapse_session::SessionStore;
use synapse_tools::SharedGate;

/// Configuration for an agent loop invocation.
#[derive(Clone)]
pub struct AgentConfig {
    pub model: String,
    pub system_prompt: String,
    pub cwd: PathBuf,
    pub max_turns: usize,
    pub max_tokens: u32,
    /// Session store for persisting conversation turns.
    /// When set, the agent loop persists every turn to SQLite.
    pub session_store: Option<Arc<SessionStore>>,
    /// Active session ID within the session store.
    pub session_id: Option<i64>,
    /// Agent depth for delegation (0 = top-level, capped at MAX_DEPTH).
    pub depth: u8,
    /// Context compression configuration. When set, enables automatic
    /// compression when token count exceeds the threshold.
    pub compression: Option<CompressionConfig>,
    /// Multi-model router. When set, overrides `model` with per-turn
    /// model selection (primary for planning, fast for tool loops).
    pub router: Option<ModelRouter>,
    /// Maximum tokens allowed for a single tool result added to context.
    /// Results exceeding this limit are truncated (head + tail) before being
    /// added to the context window. The full result is still emitted as
    /// AgentEvent::ToolResult so the UI sees everything.
    /// Set to 0 to disable truncation.
    pub max_tool_result_tokens: usize,
    /// Optional tool gate. When set, wraps every tool execution with
    /// `before_execute` / `after_execute` callbacks (permission gates,
    /// PreToolUse/PostToolUse hooks, audit). When unset, the agent loop
    /// uses a permissive gate that allows everything.
    pub tool_gate: Option<SharedGate>,
    /// Optional hook configuration. Loaded from `~/.synapse/hooks.toml`
    /// or constructed programmatically. Drives SessionStart, Stop, and
    /// UserPromptSubmit phases; PreToolUse/PostToolUse hooks are fired by
    /// the `HookGate` wrapper around `tool_gate`.
    pub hooks: Option<Arc<HookConfig>>,
}

impl AgentConfig {
    /// Maximum delegation depth.
    pub const MAX_DEPTH: u8 = 2;
}

impl fmt::Debug for AgentConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AgentConfig")
            .field("model", &self.model)
            .field("cwd", &self.cwd)
            .field("max_turns", &self.max_turns)
            .field("max_tokens", &self.max_tokens)
            .field("session_id", &self.session_id)
            .field("depth", &self.depth)
            .field("has_session_store", &self.session_store.is_some())
            .field("has_compression", &self.compression.is_some())
            .field("router", &self.router)
            .field("max_tool_result_tokens", &self.max_tool_result_tokens)
            .field("has_tool_gate", &self.tool_gate.is_some())
            .field("has_hooks", &self.hooks.is_some())
            .finish()
    }
}

/// Events yielded by the agent loop stream.
///
/// Serializes with serde's default external tagging (variant name as the key)
/// so renderers can carry these over IPC or a wire protocol.
#[derive(Debug, Clone, Serialize)]
pub enum AgentEvent {
    TurnStart,
    Text(String),
    ToolStart {
        id: String,
        name: String,
    },
    ToolResult {
        id: String,
        content: String,
        is_error: bool,
    },
    Usage {
        input_tokens: u32,
        output_tokens: u32,
        cache_read_tokens: u32,
        cache_write_tokens: u32,
    },
    /// Cost telemetry emitted after Usage.
    Cost {
        model: String,
        input_tokens: u32,
        output_tokens: u32,
        cache_read_tokens: u32,
        cache_write_tokens: u32,
        turn_usd: f64,
        session_total_usd: f64,
    },
    /// Emitted when the model switches between turns (e.g. primary -> fast).
    ModelSwitch {
        from: String,
        to: String,
    },
    TurnEnd,
    Error(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_event_serializes_to_json() {
        let ev = AgentEvent::ToolStart {
            id: "t1".into(),
            name: "bash".into(),
        };
        let json = serde_json::to_string(&ev).expect("serialize");
        assert_eq!(json, r#"{"ToolStart":{"id":"t1","name":"bash"}}"#);
    }
}
