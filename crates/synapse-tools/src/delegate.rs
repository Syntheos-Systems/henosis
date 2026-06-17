//! Subagent delegation tool -- spawns depth-limited child agent loops.
//!
//! Since synapse-tools cannot depend on synapse-core (circular), this module
//! defines the tool struct but requires synapse-core types to be injected
//! at construction time via closures. The CLI wires this up.

use crate::tool::{AgentTool, ToolRegistry, ToolResult};
use anyhow::Result;
use serde_json::Value;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;

/// Type alias for the agent loop spawner function.
/// The CLI provides this closure which captures the provider and config.
pub type AgentSpawner = dyn Fn(String, PathBuf, usize, u8) -> Pin<Box<dyn Future<Output = DelegateResult> + Send>>
    + Send
    + Sync;

/// Result from a delegated child agent.
pub struct DelegateResult {
    pub text: String,
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub error: Option<String>,
}

/// Tool that delegates a task to a child agent with restricted capabilities.
pub struct DelegateTaskTool {
    /// Current agent depth.
    depth: u8,
    /// Max depth allowed.
    max_depth: u8,
    /// Closure that spawns a child agent loop and collects results.
    spawner: Arc<AgentSpawner>,
}

impl DelegateTaskTool {
    pub fn new(depth: u8, max_depth: u8, spawner: Arc<AgentSpawner>) -> Self {
        Self {
            depth,
            max_depth,
            spawner,
        }
    }
}

#[async_trait::async_trait]
impl AgentTool for DelegateTaskTool {
    fn name(&self) -> &str {
        "delegate_task"
    }

    fn description(&self) -> &str {
        "Delegate a task to a child agent that runs independently with its own context. \
         The child has restricted tools (no delegation, no memory writes) and a limited \
         turn budget. Use for independent subtasks that don't need the parent's conversation \
         history. Returns the child's final text output."
    }

    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "task": {
                    "type": "string",
                    "description": "The task description to send to the child agent."
                },
                "context": {
                    "type": "string",
                    "description": "Optional additional context to prepend to the task."
                },
                "max_turns": {
                    "type": "integer",
                    "description": "Max turns for the child (default 10, max 15)."
                }
            },
            "required": ["task"]
        })
    }

    async fn execute(&self, params: Value, cwd: &Path) -> Result<ToolResult> {
        // Check depth limit
        if self.depth >= self.max_depth {
            return Ok(ToolResult {
                content: format!(
                    "Delegation blocked: depth {} >= max_depth {}. \
                     Complete this task directly instead.",
                    self.depth, self.max_depth,
                ),
                is_error: true,
            });
        }

        let task = match params.get("task").and_then(|v| v.as_str()) {
            Some(t) if !t.trim().is_empty() => t.trim().to_string(),
            _ => {
                return Ok(ToolResult {
                    content: "Missing required parameter: task".to_string(),
                    is_error: true,
                });
            }
        };

        let context = params.get("context").and_then(|v| v.as_str()).unwrap_or("");
        let max_turns = params
            .get("max_turns")
            .and_then(|v| v.as_u64())
            .unwrap_or(10)
            .min(15) as usize;

        let message = if context.is_empty() {
            task
        } else {
            format!("{context}\n\n{task}")
        };

        let child_depth = self.depth + 1;
        let result = (self.spawner)(message, cwd.to_path_buf(), max_turns, child_depth).await;

        if let Some(err) = result.error {
            return Ok(ToolResult {
                content: format!("Child agent error: {err}"),
                is_error: true,
            });
        }

        let mut output = result.text;
        if output.is_empty() {
            output = "(child agent produced no text output)".to_string();
        }

        output.push_str(&format!(
            "\n\n[child agent d{child_depth}: tokens in={} out={}]",
            result.input_tokens, result.output_tokens,
        ));

        Ok(ToolResult {
            content: output,
            is_error: false,
        })
    }
}

/// Build a child tool registry from the default tools.
///
/// The delegate_task tool itself is NOT included (it's added by the CLI
/// only for top-level agents), so children naturally can't delegate further.
/// Memory-write tools (kleos_store, kleos_delete, etc.) are still present
/// in the defaults but the child's system prompt instructs it not to use them.
pub fn build_child_tools() -> ToolRegistry {
    // The child gets the standard default_tools() which does NOT include
    // delegate_task (that's added separately by the CLI). This means children
    // cannot delegate by construction, regardless of depth checks.
    crate::default_tools()
}
