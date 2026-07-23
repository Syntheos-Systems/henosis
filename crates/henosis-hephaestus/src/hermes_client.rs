//! In-process Hermes tool dispatch. Replaces the former HTTP client.
//!
//! `HermesClient` now holds an `Arc<ToolRegistry>`, a `CircuitRegistry`, and an
//! `InvokeContext` assembled at startup. `call_tool` looks up the tool by name in
//! the registry and calls `invoke_with_circuit` directly -- no HTTP round-trip.
//!
//! The `ToolResult`, `ToolDef`, and `builtin_tools` items are kept in this module
//! because `tasks.rs` and `orchestrator.rs` import them here; their shapes are
//! unchanged.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tracing::warn;

use henosis_hermes::{
    ToolRegistry,
    circuit::{CircuitRegistry, invoke_with_circuit},
    tool::{InvokeContext, InvokeRequest},
};

/// Tool definition serialized into Anthropic's tool format.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDef {
    /// Stable tool name used in tool_use blocks and dispatch routing.
    pub name: String,
    /// Human-readable description shown to the model.
    pub description: String,
    /// JSON Schema for the tool's input parameters.
    pub input_schema: Value,
}

/// Result returned by a tool call dispatched through Hermes.
#[derive(Debug, Clone)]
pub struct ToolResult {
    /// Matches the `tool_use_id` from the assistant's tool_use block.
    pub tool_use_id: String,
    /// Serialized result or error payload returned to the model.
    pub content: String,
    /// True when the tool call itself failed (not a logical error from the tool).
    pub is_error: bool,
}

/// In-process Hermes tool invoker. Holds a fully-built registry, a circuit
/// breaker registry, and the per-invocation context (phylaxd client + provider
/// base URLs). Replaces the former `reqwest`-backed HTTP client.
pub struct HermesClient {
    /// The populated tool registry from `henosis_hermes::registry::build_registry`.
    registry: Arc<ToolRegistry>,
    /// Per-provider circuit breaker state.
    circuits: Arc<CircuitRegistry>,
    /// Shared invocation context threaded into every `invoke_with_circuit` call.
    ctx: InvokeContext,
}

/// Implements the behavior exposed by HermesClient.
impl HermesClient {
    /// Construct an in-process Hermes client. The caller is responsible for
    /// building the registry, circuits, and context before calling this.
    pub fn new(
        registry: Arc<ToolRegistry>,
        circuits: Arc<CircuitRegistry>,
        ctx: InvokeContext,
    ) -> Self {
        Self {
            registry,
            circuits,
            ctx,
        }
    }

    /// Dispatch a tool call in-process through the Hermes registry and circuit
    /// breaker. Returns a `ToolResult` in all cases -- unknown tools and
    /// invocation failures surface as `is_error: true` with a structured
    /// `{code, message}` envelope so the LLM can reason about them.
    pub async fn call_tool(
        &self,
        tool_name: &str,
        tool_use_id: &str,
        tenant_id: Option<&str>,
        tool_input: &Value,
    ) -> ToolResult {
        // Resolve the tool from the registry; surface a structured error for
        // unknown tool names (matches the former HTTP 404 response semantics).
        let tool = match self.registry.get(tool_name) {
            Some(t) => t,
            None => {
                warn!(
                    tool = tool_name,
                    "hermes in-process: tool not found in registry"
                );
                return error_result(
                    tool_use_id,
                    "tool_not_found",
                    &format!("hermes does not know tool '{tool_name}'"),
                );
            }
        };

        let req = InvokeRequest {
            tenant_id: tenant_id.map(String::from),
            args: tool_input.clone(),
        };

        // Build a per-call context that threads the tenant_id through. The
        // underlying InvokeContext is Clone so we can cheaply specialise it.
        let ctx = self.ctx.clone();

        let (resp, _retries) =
            invoke_with_circuit(&self.circuits, &tool, tool_name, &ctx, req).await;

        if resp.success {
            let content = resp
                .result
                .map(|v| serde_json::to_string(&v).unwrap_or_else(|_| v.to_string()))
                .unwrap_or_default();
            ToolResult {
                tool_use_id: tool_use_id.to_string(),
                content,
                is_error: false,
            }
        } else {
            let err_payload = resp
                .error
                .unwrap_or_else(|| json!({"code": "unknown", "message": "tool returned failure"}));
            let content =
                serde_json::to_string(&err_payload).unwrap_or_else(|_| err_payload.to_string());
            ToolResult {
                tool_use_id: tool_use_id.to_string(),
                content,
                is_error: true,
            }
        }
    }
}

/// Build a structured-error `ToolResult` so the LLM sees `{code, message}`
/// content even on dispatch failures.
fn error_result(tool_use_id: &str, code: &str, message: &str) -> ToolResult {
    let envelope = json!({ "code": code, "message": message });
    ToolResult {
        tool_use_id: tool_use_id.to_string(),
        content: envelope.to_string(),
        is_error: true,
    }
}

/// Built-in tool definitions that callers may include in their tools list.
/// These are handled directly by the executor (not dispatched to Hermes).
pub fn builtin_tools() -> Vec<ToolDef> {
    vec![
        ToolDef {
            name: "ask_human".to_string(),
            description: "Ask the human operator a question and wait for their response. \
                Use when you need clarification, approval, or information only a human can provide."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "question": {
                        "type": "string",
                        "description": "The question to ask the human operator."
                    }
                },
                "required": ["question"]
            }),
        },
        ToolDef {
            name: "code_exec".to_string(),
            description: "Execute code in an isolated sandboxed container with no network access. \
                Supports python, bash, and javascript."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "language": {
                        "type": "string",
                        "enum": ["python", "bash", "javascript"],
                        "description": "Programming language to execute."
                    },
                    "code": {
                        "type": "string",
                        "description": "Code to execute."
                    }
                },
                "required": ["language", "code"]
            }),
        },
    ]
}
