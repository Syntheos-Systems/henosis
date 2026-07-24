use anyhow::Result;
use serde_json::Value;
use std::path::Path;
use std::sync::Arc;

use crate::ToolExecutionContext;

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

    /// Executes with authority retained before untrusted model output was accepted.
    async fn execute_with_context(
        &self,
        params: Value,
        context: &ToolExecutionContext,
    ) -> Result<ToolResult> {
        self.execute(params, context.cwd()).await
    }
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
/// `PermissiveGate` allows everything only when a host installs it explicitly.
/// Hosts can add confirmation or hook-executing gates without modifying tools.
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

/// Explicit gate that allows all tool executions for trusted embedding hosts.
pub struct PermissiveGate;

/// Implements `ToolGate` behavior for `PermissiveGate`.
#[async_trait::async_trait]
impl ToolGate for PermissiveGate {}

/// Explicit fail-closed gate used when an embedding host grants no tool authority.
pub struct DenyAllGate;

/// Denies every tool call until a host deliberately installs another gate.
#[async_trait::async_trait]
impl ToolGate for DenyAllGate {
    /// Rejects execution because no host authority was configured.
    async fn before_execute(&self, _name: &str, _params: &Value, _cwd: &Path) -> GateDecision {
        GateDecision::Deny("no explicit tool gate was configured".to_string())
    }
}

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

/// Exercises explicit allow and deny gate behavior.
#[cfg(test)]
mod tests {
    use super::*;

    /// Confirms the fail-closed gate rejects a tool without consulting ambient state.
    #[tokio::test]
    async fn deny_all_gate_rejects_every_call() {
        let decision = DenyAllGate
            .before_execute("bash", &Value::Null, Path::new("/tmp"))
            .await;

        assert!(matches!(decision, GateDecision::Deny(_)));
    }
}
