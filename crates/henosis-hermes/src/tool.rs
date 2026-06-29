//! Core tool abstraction: the `Tool` trait, request/response envelopes,
//! invoke context, retry policy, and shared error helpers.

use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::credd_client::CreddClient;

/// JSON-Schema-like description of one tool: its ID, human name, description,
/// input/output schemas, category, and auth requirements. Serialized verbatim
/// for `GET /tools` and the MCP `tools/list` response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSchema {
    /// Stable dot-namespaced identifier (e.g. `github.create_issue`).
    pub tool_id: String,
    /// Human-readable display name.
    pub name: String,
    /// One-sentence description of what the tool does.
    pub description: String,
    /// JSON Schema object describing accepted input arguments.
    pub input_schema: Value,
    /// JSON Schema object describing the success result shape.
    pub output_schema: Value,
    /// Coarse grouping label (e.g. `email`, `calendar`, `development`).
    pub category: String,
    /// Whether the tool requires an OAuth credential to invoke.
    pub requires_auth: bool,
}

/// Caller-supplied invocation request: optional tenant context and free-form
/// JSON arguments.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InvokeRequest {
    /// Tenant on whose behalf the call runs. Used to resolve OAuth tokens from
    /// credd and to key the rate-limiter bucket. `None` routes as `_anon`.
    pub tenant_id: Option<String>,
    /// Tool-specific arguments, validated against `ToolSchema::input_schema`
    /// before the adapter sees them.
    pub args: Value,
}

/// Adapter invocation result envelope returned by every `Tool::invoke` call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InvokeResponse {
    /// The tool id that was invoked (echoed for correlation).
    pub tool_id: String,
    /// `true` iff the upstream call succeeded and `result` is populated.
    pub success: bool,
    /// Structured success payload. `None` on failure.
    pub result: Option<Value>,
    /// Structured error envelope `{code, message, hint?}`. `None` on success.
    pub error: Option<Value>,
    /// Wall-clock duration of the invocation in milliseconds. Filled in by the
    /// dispatcher after the adapter returns.
    pub duration_ms: u64,
}

/// Overridable upstream base URLs. Real hosts by default; tests point these
/// at a mock server. Only providers that read their base from the context are
/// listed here (Linear, Notion); older adapters use hardcoded URL constants.
#[derive(Debug, Clone)]
pub struct ProviderBases {
    /// Linear API base URL (default `https://api.linear.app`).
    pub linear: String,
    /// Notion API base URL (default `https://api.notion.com`).
    pub notion: String,
}

impl Default for ProviderBases {
    /// Returns the production base URLs for all providers.
    fn default() -> Self {
        Self {
            linear: "https://api.linear.app".to_string(),
            notion: "https://api.notion.com".to_string(),
        }
    }
}

/// Per-invocation context: shared resources an adapter needs to call an
/// upstream provider.
#[derive(Clone)]
pub struct InvokeContext {
    /// Credential daemon client for OAuth token and raw secret resolution.
    pub credd: Arc<CreddClient>,
    /// Overridable provider base URLs (real hosts in production, mock server
    /// in tests).
    pub bases: ProviderBases,
    /// Hermes's own externally-reachable base URL (`HERMES_PUBLIC_URL`), used to
    /// auto-populate the delivery URL of webhook-registration adapters. `None`
    /// when unset -- those adapters then require an explicit `url`.
    pub hermes_public_url: Option<String>,
}

/// Retry behaviour for an adapter's upstream HTTP calls. Consumed by
/// `adapters::common::send_with_retry`. Lives with the `Tool` trait because
/// each tool advertises its own policy via `Tool::retry_policy`.
#[derive(Debug, Clone)]
pub struct RetryPolicy {
    /// Maximum number of retries after the initial attempt (0 = no retry).
    pub max_retries: u32,
    /// Base backoff in milliseconds; grows exponentially per attempt.
    pub base_delay_ms: u64,
    /// Hard cap on a single backoff delay.
    pub max_delay_ms: u64,
    /// HTTP status codes that warrant a retry.
    pub retryable_statuses: Vec<u16>,
    /// Whether the operation is safe to replay. Network/timeout errors are
    /// only retried when this is true.
    pub idempotent: bool,
}

impl Default for RetryPolicy {
    /// Standard exponential-backoff policy: up to 3 retries, 500ms base delay,
    /// 30s cap, retryable on 429/500/502/503, idempotent.
    fn default() -> Self {
        Self {
            max_retries: 3,
            base_delay_ms: 500,
            max_delay_ms: 30_000,
            retryable_statuses: vec![429, 500, 502, 503],
            idempotent: true,
        }
    }
}

impl RetryPolicy {
    /// Policy for operations that must not be silently replayed (e.g. sending
    /// an email). No retries; network failures surface to the caller.
    pub fn non_idempotent() -> Self {
        Self {
            max_retries: 0,
            idempotent: false,
            ..Self::default()
        }
    }

    /// Whether a given HTTP status should trigger a retry.
    pub fn is_retryable_status(&self, status: u16) -> bool {
        self.retryable_statuses.contains(&status)
    }

    /// Exponential backoff for `attempt` (0-indexed), plus caller-supplied
    /// jitter, capped at `max_delay_ms`. Pure so it can be tested directly.
    pub fn backoff_delay_ms(&self, attempt: u32, jitter_ms: u64) -> u64 {
        // 1 << attempt, saturating; cap the shift so we never overflow.
        let factor = 1u64.checked_shl(attempt.min(32)).unwrap_or(u64::MAX);
        let base = self.base_delay_ms.saturating_mul(factor);
        base.saturating_add(jitter_ms).min(self.max_delay_ms)
    }
}

/// The tool abstraction: a named, schema-described unit of work that invokes
/// an external provider (Gmail, GitHub, Slack, etc.) on behalf of a tenant.
#[async_trait]
pub trait Tool: Send + Sync {
    /// Return the static schema describing this tool's ID, name, description,
    /// and input/output JSON schemas.
    fn schema(&self) -> ToolSchema;

    /// Execute the tool with the given context and request, returning a
    /// structured success or error envelope.
    async fn invoke(&self, ctx: &InvokeContext, req: InvokeRequest) -> InvokeResponse;

    /// Retry policy applied to this tool's upstream HTTP calls. Defaults to a
    /// standard exponential-backoff policy; non-idempotent tools override.
    fn retry_policy(&self) -> RetryPolicy {
        RetryPolicy::default()
    }

    /// Upstream provider this tool talks to (e.g. "google", "github",
    /// "slack"). Used to key the per-provider circuit breaker, so all tools
    /// for one provider must return the same string. Defaults to "unknown".
    fn provider(&self) -> &'static str {
        "unknown"
    }
}

/// Build a structured error envelope: `{code, message, hint?}`.
pub fn err(code: &str, message: impl Into<String>, hint: Option<&str>) -> Value {
    let mut obj = serde_json::Map::new();
    obj.insert("code".into(), Value::String(code.into()));
    obj.insert("message".into(), Value::String(message.into()));
    if let Some(h) = hint {
        obj.insert("hint".into(), Value::String(h.into()));
    }
    Value::Object(obj)
}

/// Build a failed `InvokeResponse` with a structured error envelope. Shorthand
/// used throughout the adapters to return early on errors.
pub fn error_response(tool_id: &str, code: &str, message: impl Into<String>, hint: Option<&str>) -> InvokeResponse {
    InvokeResponse {
        tool_id: tool_id.to_string(),
        success: false,
        result: None,
        error: Some(err(code, message, hint)),
        duration_ms: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_grows_exponentially() {
        let p = RetryPolicy::default();
        assert_eq!(p.backoff_delay_ms(0, 0), 500);
        assert_eq!(p.backoff_delay_ms(1, 0), 1000);
        assert_eq!(p.backoff_delay_ms(2, 0), 2000);
    }

    #[test]
    fn backoff_caps_at_max_delay() {
        let p = RetryPolicy::default();
        // attempt 20 would be astronomically large; must clamp to max.
        assert_eq!(p.backoff_delay_ms(20, 0), p.max_delay_ms);
    }

    #[test]
    fn backoff_adds_jitter() {
        let p = RetryPolicy::default();
        assert_eq!(p.backoff_delay_ms(0, 123), 623);
    }

    #[test]
    fn non_idempotent_disables_retry() {
        let p = RetryPolicy::non_idempotent();
        assert_eq!(p.max_retries, 0);
        assert!(!p.idempotent);
    }

    #[test]
    fn default_retryable_statuses() {
        let p = RetryPolicy::default();
        assert!(p.is_retryable_status(429));
        assert!(p.is_retryable_status(503));
        assert!(!p.is_retryable_status(404));
        assert!(!p.is_retryable_status(200));
    }
}
