use anyhow::Result;
use serde_json::Value;
use std::path::Path;
use std::sync::Arc;

/// Result returned by a tool execution.
#[derive(Debug, Clone)]
pub struct ToolResult {
    pub content: String,
    pub is_error: bool,
}

/// Trait for all agent tools. Every tool takes an explicit `cwd`
/// so sub-agents in worktrees work correctly (no global CWD).
#[async_trait::async_trait]
pub trait AgentTool: Send + Sync {
    /// Returns this component's stable registry name.
    fn name(&self) -> &str;
    /// Returns this component's user-facing description.
    fn description(&self) -> &str;
    /// Returns this component's JSON parameter schema.
    fn schema(&self) -> Value;
    /// Executes this component with the provided JSON parameters.
    async fn execute(&self, params: Value, cwd: &Path) -> Result<ToolResult>;
}

/// Decision returned by a ToolGate before a tool runs.
#[derive(Debug, Clone)]
pub enum GateDecision {
    /// Run the tool normally.
    Allow,
    /// Skip the tool. The denial reason becomes the error ToolResult content.
    Deny(String),
}

/// Interceptor invoked around every tool execution. Lets the host (CLI, TUI,
/// Tauri app) impose confirmation prompts, hook scripts, or auditing without
/// modifying individual tools.
///
/// The default `PermissiveGate` allows everything. Phase 3 (Ratatui TUI) will
/// add a confirmation gate; Phase 0 (hooks) adds a hook-executing gate.
#[async_trait::async_trait]
pub trait ToolGate: Send + Sync {
    /// Called before a tool runs. Return `Deny` to short-circuit with an
    /// error result. Default: allow.
    async fn before_execute(&self, name: &str, params: &Value, cwd: &Path) -> GateDecision {
        let _ = (name, params, cwd);
        GateDecision::Allow
    }

    /// Called after a tool runs (whether it succeeded or failed). Useful for
    /// PostToolUse hooks, audit trails, and skill-capture heuristics.
    /// Default: no-op.
    async fn after_execute(&self, name: &str, params: &Value, result: &ToolResult, cwd: &Path) {
        let _ = (name, params, result, cwd);
    }
}

/// Default gate that allows all tool executions. Used when no host-provided
/// gate is installed; preserves the pre-gate behavior.
pub struct PermissiveGate;

/// Implements `ToolGate` behavior for `PermissiveGate`.
#[async_trait::async_trait]
impl ToolGate for PermissiveGate {}

/// Convenience type for callers that want to pass a gate around.
pub type SharedGate = Arc<dyn ToolGate>;

/// Registry holding all registered tools. Look up by name, get all schemas.
pub struct ToolRegistry {
    tools: Vec<Box<dyn AgentTool>>,
}

/// Adds inherent behavior for `ToolRegistry`.
impl ToolRegistry {
    /// Handles `new` behavior.
    pub fn new() -> Self {
        Self { tools: Vec::new() }
    }

    /// Handles `register` behavior.
    pub fn register(&mut self, tool: Box<dyn AgentTool>) {
        self.tools.push(tool);
    }

    /// Handles `get` behavior.
    pub fn get(&self, name: &str) -> Option<&dyn AgentTool> {
        self.tools
            .iter()
            .find(|t| t.name() == name)
            .map(|t| t.as_ref())
    }

    /// Returns JSON schema objects for all registered tools, suitable for
    /// sending to an LLM as the tools list.
    pub fn all_tool_schemas(&self) -> Vec<Value> {
        self.tools
            .iter()
            .map(|t| {
                serde_json::json!({
                    "name": t.name(),
                    "description": t.description(),
                    "input_schema": t.schema(),
                })
            })
            .collect()
    }
}

/// Implements `Default` behavior for `ToolRegistry`.
impl Default for ToolRegistry {
    /// Handles `default` behavior.
    fn default() -> Self {
        Self::new()
    }
}
