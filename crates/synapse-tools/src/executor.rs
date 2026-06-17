//! Adapter that exposes a `ToolRegistry` as a
//! `synapse_provider::ToolExecutor`.
//!
//! The Claude Max provider runs an MCP bridge to the `claude` CLI
//! subprocess and needs (a) tool schemas in Anthropic format and (b)
//! a way to execute tool calls when the CLI requests them. Both
//! capabilities already exist on `ToolRegistry`; this adapter just
//! pins them behind the `ToolExecutor` trait so synapse-provider
//! does not have to know about synapse-tools' types.

use crate::tool::{ToolRegistry, ToolResult};
use anyhow::Result;
use async_trait::async_trait;
use std::path::PathBuf;
use std::sync::Arc;
use synapse_provider::{ToolExecutionResult, ToolExecutor};

/// Wrap a `ToolRegistry` with a static cwd so the provider's MCP bridge
/// can execute tools without knowing the agent loop's configuration.
pub struct ToolRegistryExecutor {
    /// Shared registry. Cloned so the executor is `Send + Sync` and
    /// agent and provider can hold simultaneous references.
    registry: Arc<ToolRegistry>,
    /// Working directory passed to each tool's `execute` call.
    /// Captured at construction time -- the CLI runs in a single cwd
    /// for the duration of the session, and the executor outlives
    /// individual tool invocations.
    cwd: PathBuf,
}

impl ToolRegistryExecutor {
    /// Build an executor wrapping `registry` with the given `cwd`.
    pub fn new(registry: Arc<ToolRegistry>, cwd: PathBuf) -> Self {
        Self { registry, cwd }
    }
}

#[async_trait]
impl ToolExecutor for ToolRegistryExecutor {
    fn tool_schemas(&self) -> Vec<serde_json::Value> {
        self.registry.all_tool_schemas()
    }

    async fn execute_tool(
        &self,
        name: &str,
        input: serde_json::Value,
    ) -> Result<ToolExecutionResult> {
        let result = match self.registry.get(name) {
            Some(tool) => tool.execute(input, &self.cwd).await,
            None => Ok(ToolResult {
                content: format!("unknown tool: {name}"),
                is_error: true,
            }),
        };

        // Map any error in the tool's own Result into a failed
        // ToolExecutionResult so the MCP bridge can surface it to the
        // CLI as a tool-error message rather than crashing the bridge.
        match result {
            Ok(r) => Ok(ToolExecutionResult {
                output: r.content,
                is_error: r.is_error,
            }),
            Err(e) => Ok(ToolExecutionResult {
                output: format!("tool {name} failed: {e}"),
                is_error: true,
            }),
        }
    }
}
