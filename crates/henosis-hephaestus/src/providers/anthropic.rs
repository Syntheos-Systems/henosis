//! Hephaestus's Anthropic provider wraps the Synapse `Provider` trait but
//! routes through Hephaestus's existing OAuth-via-claude-code-credentials
//! path. Synapse's stock `AnthropicProvider` doesn't send the
//! `claude-code-20250219` beta header, the `claude-cli` user-agent, or the
//! `x-claude-code-session-id` Hephaestus depends on, so we keep the wire
//! layer here while reusing the request/response shapes and SSE parser from
//! synapse-provider.
//!
//! On 401 we refresh the OAuth token via `ProviderChain` and retry exactly
//! once. The provider is constructed per-request so the auth chain can swap
//! tokens between calls; the ProviderChain itself is shared and inexpensive
//! to clone.

use std::pin::Pin;
use std::sync::OnceLock;
use std::time::Duration;

use anyhow::{Result, anyhow};
use async_stream::stream;
use async_trait::async_trait;
use futures::{Stream, StreamExt};
use reqwest::Client;
use reqwest_eventsource::{Event, RequestBuilderExt};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::warn;
use uuid::Uuid;

use crate::anthropic_auth::{CLAUDE_CODE_IDENTITY, ProviderChain};
use crate::provider::{
    ChatRequest, ChatResponse, ContentBlock, Provider, Role, StopReason, StreamEvent, Usage,
    parse_anthropic_sse,
};

/// Anthropic API version pinned to the value Hephaestus has shipped with.
const ANTHROPIC_VERSION: &str = "2023-06-01";

/// Beta features required for claude-code OAuth + interleaved thinking. The
/// `claude-code-20250219` beta enables the OAuth flow Hephaestus uses; without
/// it Anthropic will reject Bearer-token requests.
const ANTHROPIC_BETA: &str =
    "claude-code-20250219,oauth-2025-04-20,interleaved-thinking-2025-05-14";

/// User-agent string Anthropic expects from the claude-code CLI surface.
const CLAUDE_CODE_USER_AGENT: &str = "claude-cli/2.1.114 (external, sdk-cli)";

/// A `Provider` implementation that speaks Anthropic Messages API with the
/// claude-code OAuth flow. Holds a shared reqwest client, a ProviderChain for
/// token refresh, and the URL the requests should target. The base URL is
/// configurable so tests (wiremock) and proxies can intercept.
pub struct HephaestusAnthropicProvider {
    /// Shared HTTP client; connection pool is unified across the application.
    http: Client,
    /// OAuth credential chain for token resolution and refresh.
    auth: ProviderChain,
    /// The full URL of the Anthropic Messages endpoint, including any
    /// `?beta=true` query param. Configurable so tests can point at wiremock.
    url: String,
    /// Tenant ID for token resolution. None for the default account.
    tenant_id: Option<String>,
    /// Anthropic model id (e.g. `claude-haiku-4-5-20251001`). Overrides any
    /// model field the caller may pass in `ChatRequest` -- the provider is
    /// configured at construction time to use one model, matching the
    /// pre-refactor behavior where `Config::model` was authoritative.
    model: String,
}

/// Constructors and helpers for the Hephaestus-flavoured Anthropic provider.
/// All methods here are private to Hephaestus; the trait surface is on the
/// `impl Provider` block below.
impl HephaestusAnthropicProvider {
    /// Construct a provider bound to a specific Anthropic Messages endpoint
    /// and a specific tenant. The reqwest client should be shared across the
    /// application.
    pub fn new(
        http: Client,
        auth: ProviderChain,
        url: impl Into<String>,
        model: impl Into<String>,
        tenant_id: Option<String>,
    ) -> Self {
        Self {
            http,
            auth,
            url: url.into(),
            tenant_id,
            model: model.into(),
        }
    }

    /// Build the wire-format Anthropic request body from a generic
    /// `ChatRequest`. The Hephaestus path always prepends the claude-code
    /// identity system block; if the caller provided a system prompt of its
    /// own, it is appended as a second system block.
    fn build_body(&self, request: &ChatRequest) -> Value {
        let mut system_blocks =
            vec![serde_json::json!({ "type": "text", "text": CLAUDE_CODE_IDENTITY })];
        if let Some(extra) = request.system.as_deref()
            && !extra.trim().is_empty()
        {
            system_blocks.push(serde_json::json!({ "type": "text", "text": extra }));
        }

        // Translate Hephaestus's generic ChatMessage shape into Anthropic's
        // content-block array form. ContentBlock variants map 1:1.
        let messages: Vec<Value> = request
            .messages
            .iter()
            .map(|msg| {
                let content: Vec<Value> = msg
                    .content
                    .iter()
                    .map(|block| match block {
                        ContentBlock::Text { text } => {
                            serde_json::json!({ "type": "text", "text": text })
                        }
                        ContentBlock::ToolUse { id, name, input } => serde_json::json!({
                            "type": "tool_use",
                            "id": id,
                            "name": name,
                            "input": input,
                        }),
                        ContentBlock::ToolResult {
                            tool_use_id,
                            content,
                            is_error,
                        } => serde_json::json!({
                            "type": "tool_result",
                            "tool_use_id": tool_use_id,
                            "content": content,
                            "is_error": is_error,
                        }),
                    })
                    .collect();
                serde_json::json!({
                    "role": role_str(&msg.role),
                    "content": content,
                })
            })
            .collect();

        let mut body = serde_json::json!({
            "model": self.model,
            "max_tokens": request.max_tokens,
            "system": system_blocks,
            "messages": messages,
        });
        if let Some(tools) = &request.tools
            && !tools.is_empty()
        {
            body["tools"] = Value::Array(tools.clone());
        }
        body
    }

    /// Send a single request with the given bearer token. Used by both `send`
    /// (twice on 401) and the SSE path.
    async fn post_once(&self, body: &Value, token: &str) -> Result<reqwest::Response> {
        let resp = self
            .http
            .post(&self.url)
            .header("Authorization", format!("Bearer {token}"))
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("anthropic-beta", ANTHROPIC_BETA)
            .header("anthropic-dangerous-direct-browser-access", "true")
            .header("User-Agent", CLAUDE_CODE_USER_AGENT)
            .header("x-app", "cli")
            .header("x-claude-code-session-id", session_uuid())
            .json(body)
            .send()
            .await?;
        Ok(resp)
    }
}

/// Implementation of the generic `Provider` trait. The orchestrator only
/// sees this surface; the constructors and the wire-format helpers above
/// remain Hephaestus-private.
#[async_trait]
impl Provider for HephaestusAnthropicProvider {
    /// Non-streaming send. Resolves a token, posts the request, retries once
    /// on 401 with a refreshed token, and parses the response into the
    /// generic ChatResponse shape.
    async fn send(&self, request: &ChatRequest) -> Result<ChatResponse> {
        let body = self.build_body(request);

        let token = self
            .auth
            .token(self.tenant_id.as_deref())
            .await
            .map_err(|e| anyhow!(e))?;
        let resp = self.post_once(&body, &token).await?;
        let status = resp.status();

        let resp = if status == reqwest::StatusCode::UNAUTHORIZED {
            warn!("anthropic 401 -- refreshing token and retrying");
            let fresh = self
                .auth
                .token(self.tenant_id.as_deref())
                .await
                .map_err(|e| anyhow!(e))?;
            self.post_once(&body, &fresh).await?
        } else {
            resp
        };

        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(anyhow!("anthropic {}: {}", status.as_u16(), text));
        }

        parse_response(&text)
    }

    /// Streaming send. Snapshots a token, opens an SSE stream, and yields
    /// generic `StreamEvent`s by parsing each Anthropic SSE event with the
    /// shared synapse-provider parser. Does NOT implement 401 retry on the
    /// streaming path; if Anthropic rejects the connection the stream emits
    /// a single Error event and ends. Callers that need 401 retry should use
    /// `send` for the initial reach and only stream on the second attempt.
    fn send_streaming(
        &self,
        request: &ChatRequest,
    ) -> Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>> {
        let mut body = self.build_body(request);
        if let Some(obj) = body.as_object_mut() {
            obj.insert("stream".to_owned(), Value::Bool(true));
        }
        let http = self.http.clone();
        let url = self.url.clone();
        let auth = self.auth.clone();
        let tenant = self.tenant_id.clone();

        Box::pin(stream! {
            let token = match auth.token(tenant.as_deref()).await {
                Ok(t) => t,
                Err(e) => {
                    yield Err(anyhow!(e));
                    return;
                }
            };

            let body_str = match serde_json::to_string(&body) {
                Ok(s) => s,
                Err(e) => { yield Err(anyhow!(e)); return; }
            };

            let req_builder = http
                .post(&url)
                .timeout(Duration::from_secs(120))
                .header("Authorization", format!("Bearer {token}"))
                .header("anthropic-version", ANTHROPIC_VERSION)
                .header("anthropic-beta", ANTHROPIC_BETA)
                .header("anthropic-dangerous-direct-browser-access", "true")
                .header("User-Agent", CLAUDE_CODE_USER_AGENT)
                .header("x-app", "cli")
                .header("x-claude-code-session-id", session_uuid())
                .header("content-type", "application/json")
                .body(body_str);

            let mut es = match req_builder.eventsource() {
                Ok(es) => es,
                Err(e) => { yield Err(anyhow!("eventsource init: {}", e)); return; }
            };

            while let Some(event) = es.next().await {
                match event {
                    Ok(Event::Open) => {}
                    Ok(Event::Message(msg)) => {
                        let event_type = if msg.event.is_empty() { "message" } else { &msg.event };
                        for ev in parse_anthropic_sse(event_type, &msg.data) {
                            yield Ok(ev);
                        }
                    }
                    Err(reqwest_eventsource::Error::StreamEnded) => break,
                    Err(e) => {
                        yield Err(anyhow!("sse error: {}", e));
                        break;
                    }
                }
            }
        })
    }

    /// Stable provider name used for telemetry and run-record bookkeeping.
    fn name(&self) -> &str {
        "hephaestus-anthropic"
    }
}

/// Translate a generic `Role` to the Anthropic-wire role string. The
/// Anthropic Messages API only knows `user` and `assistant`; system blocks
/// are passed via the top-level `system` field, and tool roles map to
/// `user` with tool_result content blocks.
fn role_str(role: &Role) -> &'static str {
    match role {
        Role::Assistant => "assistant",
        Role::User | Role::System | Role::Tool => "user",
    }
}

/// Parse the Anthropic non-streaming JSON response into a generic
/// `ChatResponse`. Mirrors the legacy `extract_content` logic but produces
/// typed ContentBlocks instead of `serde_json::Value`.
fn parse_response(text: &str) -> Result<ChatResponse> {
    /// Raw Anthropic non-streaming response wrapper. Mirrors the wire shape
    /// just enough to extract content blocks, stop_reason, and usage.
    #[derive(Deserialize)]
    struct AnthRespRaw {
        /// Anthropic message id. Forwarded into ChatResponse::id.
        id: Option<String>,
        /// Content blocks returned by the model.
        content: Option<Vec<Value>>,
        /// Stop reason string (end_turn / tool_use / max_tokens / stop_sequence).
        stop_reason: Option<String>,
        /// Token usage counters.
        usage: Option<RawUsage>,
    }
    /// Anthropic usage block: input + output tokens. Other fields (cache
    /// reads, etc.) are ignored.
    #[derive(Deserialize)]
    struct RawUsage {
        /// Number of tokens in the prompt.
        input_tokens: u32,
        /// Number of tokens in the completion.
        output_tokens: u32,
    }

    let raw: AnthRespRaw =
        serde_json::from_str(text).map_err(|e| anyhow!("parse anthropic response: {}", e))?;

    let mut content = Vec::new();
    if let Some(blocks) = raw.content {
        for block in blocks {
            match block.get("type").and_then(|t| t.as_str()) {
                Some("text") => {
                    if let Some(t) = block.get("text").and_then(|t| t.as_str()) {
                        content.push(ContentBlock::Text {
                            text: t.to_string(),
                        });
                    }
                }
                Some("tool_use") => {
                    let id = block
                        .get("id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let name = block
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let input = block.get("input").cloned().unwrap_or(Value::Null);
                    content.push(ContentBlock::ToolUse { id, name, input });
                }
                _ => {}
            }
        }
    }

    let stop_reason = match raw.stop_reason.as_deref() {
        Some("tool_use") => StopReason::ToolUse,
        Some("max_tokens") => StopReason::MaxTokens,
        Some("stop_sequence") => StopReason::StopSequence,
        _ => StopReason::EndTurn,
    };

    let usage = raw
        .usage
        .map(|u| Usage {
            input_tokens: u.input_tokens,
            output_tokens: u.output_tokens,
            // Cache token fields added by the workspace synapse-provider;
            // not present in the Anthropic non-streaming response parser so
            // default to zero here.
            cache_read_tokens: 0,
            cache_write_tokens: 0,
        })
        .unwrap_or(Usage {
            input_tokens: 0,
            output_tokens: 0,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
        });

    Ok(ChatResponse {
        id: raw.id.unwrap_or_default(),
        content,
        stop_reason,
        usage,
    })
}

/// One stable session UUID per process, mirroring the legacy claude-code
/// behavior. Anthropic uses this for telemetry; if it changes between
/// requests Anthropic may rate-limit.
fn session_uuid() -> &'static str {
    static UUID: OnceLock<String> = OnceLock::new();
    UUID.get_or_init(|| Uuid::new_v4().to_string())
}

/// Test-only inert struct used to satisfy `Serialize`/`Deserialize` lints
/// when the serde features are imported but not yet referenced. Kept private.
#[allow(dead_code)]
#[derive(Serialize, Deserialize)]
struct UnusedSerdeAnchor;
