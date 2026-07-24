//! Adapter that exposes a `ToolRegistry` as a
//! `synapse_provider::ToolExecutor`.
//!
//! The Claude Max provider runs an MCP bridge to the `claude` CLI
//! subprocess and needs (a) tool schemas in Anthropic format and (b)
//! a way to execute tool calls when the CLI requests them. Both
//! capabilities already exist on `ToolRegistry`; this adapter just
//! pins them behind the `ToolExecutor` trait so synapse-provider
//! does not have to know about synapse-tools' types.

use crate::ToolExecutionContext;
use crate::tool::{DenyAllGate, GateDecision, SharedGate, ToolRegistry, ToolResult};
use anyhow::Result;
use async_trait::async_trait;
use std::path::PathBuf;
use std::sync::Arc;
use synapse_provider::{ToolExecutionResult, ToolExecutor};

/// Wraps a registry with retained task-root authority and an explicit tool gate.
pub struct ToolRegistryExecutor {
    /// Shared registry. Cloned so the executor is `Send + Sync` and
    /// agent and provider can hold simultaneous references.
    registry: Arc<ToolRegistry>,
    /// Task-root authority retained before an enabled provider can emit tool calls.
    context: Option<ToolExecutionContext>,
    /// Explicit host authority evaluated around every provider-originated call.
    gate: SharedGate,
}

/// Adds constructors for enabled and fail-closed provider tool execution.
impl ToolRegistryExecutor {
    /// Build an executor with a retained task root and explicit host gate.
    pub fn new(registry: Arc<ToolRegistry>, cwd: PathBuf, gate: SharedGate) -> Result<Self> {
        Ok(Self {
            registry,
            context: Some(ToolExecutionContext::new(cwd)?),
            gate,
        })
    }

    /// Build a fail-closed executor that exposes no MCP tools.
    ///
    /// Shared providers cannot safely reuse a task-scoped gate or working
    /// directory. Callers use this constructor until they can create one
    /// provider instance per authorized session.
    pub fn disabled() -> Self {
        Self {
            registry: Arc::new(ToolRegistry::new()),
            context: None,
            gate: Arc::new(DenyAllGate),
        }
    }
}

#[async_trait]
/// Exposes registry schemas and execution through the provider-facing contract.
impl ToolExecutor for ToolRegistryExecutor {
    /// Return schemas for every tool visible to the provider.
    fn tool_schemas(&self) -> Vec<serde_json::Value> {
        self.registry.all_tool_schemas()
    }

    /// Execute one provider-originated tool call against the bound registry and cwd.
    async fn execute_tool(
        &self,
        name: &str,
        input: serde_json::Value,
    ) -> Result<ToolExecutionResult> {
        let Some(context) = self.context.as_ref() else {
            return Ok(ToolExecutionResult {
                output: format!("tool execution is disabled: {name}"),
                is_error: true,
            });
        };
        let result = match self.gate.before_execute(name, &input, context.cwd()).await {
            GateDecision::Deny(reason) => ToolResult {
                content: format!("tool gate denied {name}: {reason}"),
                is_error: true,
            },
            GateDecision::Allow => match self.registry.get(name) {
                Some(tool) => match tool.execute_with_context(input.clone(), context).await {
                    Ok(result) => result,
                    Err(error) => ToolResult {
                        content: format!("tool {name} failed: {error}"),
                        is_error: true,
                    },
                },
                None => ToolResult {
                    content: format!("unknown tool: {name}"),
                    is_error: true,
                },
            },
        };
        self.gate
            .after_execute(name, &input, &result, context.cwd())
            .await;

        Ok(ToolExecutionResult {
            output: result.content,
            is_error: result.is_error,
        })
    }
}

/// Verifies the fail-closed executor advertises no callable tools.
#[cfg(test)]
mod tests {
    use super::*;

    /// Confirms shared providers cannot discover tools through the disabled executor.
    #[test]
    fn disabled_executor_has_no_tool_schemas() {
        let executor = ToolRegistryExecutor::disabled();

        assert!(executor.tool_schemas().is_empty());
    }

    /// Confirms a disabled executor rejects calls without opening a filesystem path.
    #[tokio::test]
    async fn disabled_executor_rejects_tool_calls() {
        let executor = ToolRegistryExecutor::disabled();
        let result = executor
            .execute_tool("read", serde_json::json!({"file_path": "secret.txt"}))
            .await
            .expect("disabled provider result");

        assert!(result.is_error);
        assert!(result.output.contains("disabled"));
    }

    /// Confirms provider-originated tool calls preserve task-root confinement errors.
    #[tokio::test]
    async fn provider_executor_rejects_filesystem_escape() {
        let workspace = tempfile::tempdir().expect("workspace");
        let root = workspace.path().join("task");
        std::fs::create_dir(&root).expect("task root");
        let outside = workspace.path().join("outside.txt");
        let mut registry = ToolRegistry::new();
        registry.register(Box::new(crate::write::WriteTool));
        let executor = ToolRegistryExecutor::new(
            Arc::new(registry),
            root,
            Arc::new(crate::tool::PermissiveGate),
        )
        .expect("provider executor");

        let result = executor
            .execute_tool(
                "write",
                serde_json::json!({
                    "file_path": "../outside.txt",
                    "content": "escaped"
                }),
            )
            .await
            .expect("provider result");

        assert!(result.is_error);
        assert!(!outside.exists());
    }

    /// Confirms provider execution cannot bypass an explicit host denial.
    #[tokio::test]
    async fn provider_executor_honors_explicit_gate() {
        let root = tempfile::tempdir().expect("task root");
        let mut registry = ToolRegistry::new();
        registry.register(Box::new(crate::write::WriteTool));
        let executor = ToolRegistryExecutor::new(
            Arc::new(registry),
            root.path().to_path_buf(),
            Arc::new(DenyAllGate),
        )
        .expect("provider executor");

        let result = executor
            .execute_tool(
                "write",
                serde_json::json!({
                    "file_path": "denied.txt",
                    "content": "must not exist"
                }),
            )
            .await
            .expect("provider result");

        assert!(result.is_error);
        assert!(!root.path().join("denied.txt").exists());
    }

    /// Confirms provider tools keep the original root after its ambient path is replaced.
    #[cfg(unix)]
    #[tokio::test]
    async fn provider_executor_retains_original_task_root() {
        let workspace = tempfile::tempdir().expect("workspace");
        let root = workspace.path().join("task");
        let retained = workspace.path().join("retained");
        let outside = workspace.path().join("outside");
        std::fs::create_dir(&root).expect("task root");
        std::fs::create_dir(&outside).expect("outside root");
        let mut registry = ToolRegistry::new();
        registry.register(Box::new(crate::write::WriteTool));
        let executor = ToolRegistryExecutor::new(
            Arc::new(registry),
            root.clone(),
            Arc::new(crate::tool::PermissiveGate),
        )
        .expect("provider executor");

        std::fs::rename(&root, &retained).expect("move original root");
        std::os::unix::fs::symlink(&outside, &root).expect("replace ambient root");
        let result = executor
            .execute_tool(
                "write",
                serde_json::json!({
                    "file_path": "proof.txt",
                    "content": "retained"
                }),
            )
            .await
            .expect("provider result");

        assert!(!result.is_error);
        assert_eq!(
            std::fs::read_to_string(retained.join("proof.txt")).expect("retained file"),
            "retained"
        );
        assert!(!outside.join("proof.txt").exists());
    }
}
