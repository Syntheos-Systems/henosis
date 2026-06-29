//! HTTP client for the Hermes tool gateway. Every call returns a `ToolResult`
//! even on transport failure so the LLM always receives a structured response
//! it can reason about.

use std::time::Duration;

use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tracing::warn;

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

/// Hermes' invoke response shape (mirrors `hermes::tool::InvokeResponse`).
#[derive(Debug, Deserialize)]
struct HermesInvokeResponse {
    /// Hermes-assigned tool invocation id. Not used by this client.
    #[allow(dead_code)]
    tool_id: Option<String>,
    /// True if the tool executed without transport or dispatch failure.
    success: bool,
    /// Tool output on success.
    result: Option<Value>,
    /// Tool error payload on failure.
    error: Option<Value>,
    /// Wall-clock tool execution time in milliseconds. Informational only.
    #[allow(dead_code)]
    duration_ms: Option<u64>,
}

/// HTTP client for the Hermes tool gateway.
pub struct HermesClient {
    /// Base URL of the Hermes service (no trailing slash).
    pub base_url: String,
    /// Shared reqwest client for connection-pool reuse.
    pub http: Client,
    /// Per-request timeout. Defaults to 30s to accommodate slow tools.
    pub timeout: Duration,
}

impl HermesClient {
    /// Construct a Hermes client. Trims trailing slashes from `base_url` so
    /// callers do not have to be consistent about trailing-slash presence.
    pub fn new(base_url: &str, http: Client) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            http,
            timeout: Duration::from_secs(30),
        }
    }

    /// Dispatch a tool call to Hermes' POST /tools/{tool_id}/invoke.
    /// Always returns a `ToolResult` -- transport failures are surfaced as
    /// `is_error: true` with a structured `{code, message, hint}` envelope so
    /// the LLM can react to them.
    pub async fn call_tool(
        &self,
        tool_name: &str,
        tool_use_id: &str,
        tenant_id: Option<&str>,
        tool_input: &Value,
    ) -> ToolResult {
        let url = format!("{}/tools/{}/invoke", self.base_url, tool_name);
        let body = json!({
            "tenant_id": tenant_id,
            "args": tool_input,
        });

        let resp = self
            .http
            .post(&url)
            .timeout(self.timeout)
            .json(&body)
            .send()
            .await;

        let resp = match resp {
            Ok(r) => r,
            Err(e) => {
                warn!(tool = tool_name, error = %e, "hermes transport failure");
                return error_result(tool_use_id, "hermes_transport", &e.to_string());
            }
        };

        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();

        if status == reqwest::StatusCode::NOT_FOUND {
            return error_result(
                tool_use_id,
                "tool_not_found",
                &format!("hermes does not know tool '{tool_name}'"),
            );
        }

        if !status.is_success() {
            warn!(tool = tool_name, %status, body = %text, "hermes non-2xx");
            return error_result(
                tool_use_id,
                "hermes_http_error",
                &format!("status={status}: {text}"),
            );
        }

        let parsed: HermesInvokeResponse = match serde_json::from_str(&text) {
            Ok(p) => p,
            Err(e) => {
                warn!(tool = tool_name, error = %e, body = %text, "hermes invalid response");
                return error_result(
                    tool_use_id,
                    "hermes_invalid_response",
                    &format!("could not parse: {e}"),
                );
            }
        };

        if parsed.success {
            let content = parsed
                .result
                .map(|v| serde_json::to_string(&v).unwrap_or_else(|_| v.to_string()))
                .unwrap_or_default();
            ToolResult {
                tool_use_id: tool_use_id.to_string(),
                content,
                is_error: false,
            }
        } else {
            let err_payload = parsed
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
/// content even on transport failures.
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
