//! Anthropic provider.
//! Supports both direct API keys (x-api-key) and OAuth tokens (Authorization: Bearer).
//! OAuth tokens come from OpenCode's auth.json and start with `sk-ant-oat01-`.

use std::pin::Pin;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use async_stream::stream;
use async_trait::async_trait;
use futures::Stream;
use reqwest_eventsource::{Event, RequestBuilderExt};
use serde::{Deserialize, Serialize};

use crate::streaming::parse_anthropic_sse;
use crate::types::{
    ChatRequest, ChatResponse, ContentBlock, Provider, Role, StopReason, StreamEvent, Usage,
};

const BASE_URL: &str = "https://api.anthropic.com/v1/messages";
const BASE_URL_OAUTH: &str = "https://api.anthropic.com/v1/messages?beta=true";
const ANTHROPIC_VERSION: &str = "2023-06-01";
const ANTHROPIC_BETA: &str = "oauth-2025-04-20,interleaved-thinking-2025-05-14";
const ANTHROPIC_TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";

// ──────────────────────────────────────────────────────────────────────────────
// Anthropic wire types (native format, no conversion needed)
// ──────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
struct AnthRequest<'a> {
    model: &'a str,
    messages: Vec<AnthMessage<'a>>,
    max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<&'a Vec<serde_json::Value>>,
    stream: bool,
}

#[derive(Debug, Serialize)]
struct AnthMessage<'a> {
    role: &'static str,
    content: Vec<AnthContentBlock<'a>>,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum AnthContentBlock<'a> {
    Text {
        text: &'a str,
    },
    ToolUse {
        id: &'a str,
        name: &'a str,
        input: &'a serde_json::Value,
    },
    ToolResult {
        tool_use_id: &'a str,
        content: &'a str,
        is_error: bool,
    },
}

fn role_str(role: &Role) -> &'static str {
    match role {
        Role::System => "user", // Anthropic doesn't have system role in messages
        Role::User | Role::Tool => "user",
        Role::Assistant => "assistant",
    }
}

fn build_anth_request<'a>(req: &'a ChatRequest) -> AnthRequest<'a> {
    let messages = req
        .messages
        .iter()
        .map(|msg| AnthMessage {
            role: role_str(&msg.role),
            content: msg
                .content
                .iter()
                .map(|block| match block {
                    ContentBlock::Text { text } => AnthContentBlock::Text { text },
                    ContentBlock::ToolUse { id, name, input } => {
                        AnthContentBlock::ToolUse { id, name, input }
                    }
                    ContentBlock::ToolResult {
                        tool_use_id,
                        content,
                        is_error,
                    } => AnthContentBlock::ToolResult {
                        tool_use_id,
                        content,
                        is_error: *is_error,
                    },
                })
                .collect(),
        })
        .collect();

    AnthRequest {
        model: &req.model,
        messages,
        max_tokens: req.max_tokens,
        system: req.system.as_deref(),
        tools: req.tools.as_ref(),
        stream: req.stream,
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Non-streaming response deserialization
// ──────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct AnthResponse {
    id: String,
    content: Vec<AnthResponseBlock>,
    stop_reason: Option<String>,
    usage: AnthUsage,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum AnthResponseBlock {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
}

#[derive(Debug, Deserialize)]
struct AnthUsage {
    input_tokens: u32,
    output_tokens: u32,
    /// Tokens read from the prompt cache (billed at the discounted read rate).
    #[serde(default)]
    cache_read_input_tokens: u32,
    /// Tokens written to the prompt cache (billed at the creation premium).
    #[serde(default)]
    cache_creation_input_tokens: u32,
}

fn anth_response_to_chat(resp: AnthResponse) -> ChatResponse {
    let stop_reason = match resp.stop_reason.as_deref() {
        Some("tool_use") => StopReason::ToolUse,
        Some("max_tokens") => StopReason::MaxTokens,
        Some("stop_sequence") => StopReason::StopSequence,
        _ => StopReason::EndTurn,
    };

    let content = resp
        .content
        .into_iter()
        .map(|block| match block {
            AnthResponseBlock::Text { text } => ContentBlock::Text { text },
            AnthResponseBlock::ToolUse { id, name, input } => {
                ContentBlock::ToolUse { id, name, input }
            }
        })
        .collect();

    ChatResponse {
        id: resp.id,
        content,
        stop_reason,
        usage: Usage {
            input_tokens: resp.usage.input_tokens
                + resp.usage.cache_read_input_tokens
                + resp.usage.cache_creation_input_tokens,
            output_tokens: resp.usage.output_tokens,
            cache_read_tokens: resp.usage.cache_read_input_tokens,
            cache_write_tokens: resp.usage.cache_creation_input_tokens,
        },
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// AnthropicProvider
// ──────────────────────────────────────────────────────────────────────────────

/// Auth mode for Anthropic: either a raw API key or an OAuth token pair from OpenCode.
#[derive(Debug, Clone)]
pub enum AnthropicAuth {
    /// Classic `x-api-key` header.
    ApiKey(String),
    /// OAuth token from OpenCode's auth.json. Uses `Authorization: Bearer`.
    OAuth {
        access_token: String,
        refresh_token: Option<String>,
        /// Expiry as unix milliseconds (0 = no expiry known).
        expires_ms: u64,
    },
    /// Plain Bearer token (e.g. Palantir Foundry). No prefix detection, no refresh.
    Bearer(String),
}

impl AnthropicAuth {
    /// Detect auth mode from a token string.
    /// OAuth access tokens start with `sk-ant-oat01-`.
    pub fn from_token(token: String) -> Self {
        if token.starts_with("sk-ant-oat01-") {
            AnthropicAuth::OAuth {
                access_token: token,
                refresh_token: None,
                expires_ms: 0,
            }
        } else {
            AnthropicAuth::ApiKey(token)
        }
    }

    fn is_expired(&self) -> bool {
        match self {
            AnthropicAuth::OAuth { expires_ms, .. } if *expires_ms > 0 => {
                let now_ms = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as u64;
                // Refresh 60s before expiry
                now_ms + 60_000 >= *expires_ms
            }
            AnthropicAuth::Bearer(_) => false,
            _ => false,
        }
    }
}

pub struct AnthropicProvider {
    client: reqwest::Client,
    auth: tokio::sync::RwLock<AnthropicAuth>,
    /// Custom base URL for proxies (e.g. Foundry). None = api.anthropic.com.
    base_url: Option<String>,
    /// Strip eager_input_streaming from tool definitions (Foundry compatibility).
    strip_eager_streaming: bool,
}

impl AnthropicProvider {
    pub fn new(client: reqwest::Client, api_key: String) -> Self {
        Self {
            client,
            auth: tokio::sync::RwLock::new(AnthropicAuth::from_token(api_key)),
            base_url: None,
            strip_eager_streaming: false,
        }
    }

    pub fn new_oauth(
        client: reqwest::Client,
        access_token: String,
        refresh_token: Option<String>,
        expires_ms: u64,
    ) -> Self {
        Self {
            client,
            auth: tokio::sync::RwLock::new(AnthropicAuth::OAuth {
                access_token,
                refresh_token,
                expires_ms,
            }),
            base_url: None,
            strip_eager_streaming: false,
        }
    }

    /// Construct for Palantir Foundry (or any proxy needing Bearer auth + custom URL).
    pub fn new_foundry(client: reqwest::Client, base_url: String, token: String) -> Self {
        Self {
            client,
            auth: tokio::sync::RwLock::new(AnthropicAuth::Bearer(token)),
            base_url: Some(base_url),
            strip_eager_streaming: true,
        }
    }

    /// Returns true if using OAuth auth mode.
    async fn is_oauth(&self) -> bool {
        matches!(&*self.auth.read().await, AnthropicAuth::OAuth { .. })
    }

    /// Get the correct endpoint URL.
    async fn endpoint(&self) -> String {
        if let Some(ref base) = self.base_url {
            format!("{}/messages", base.trim_end_matches('/'))
        } else if self.is_oauth().await {
            BASE_URL_OAUTH.to_string()
        } else {
            BASE_URL.to_string()
        }
    }

    /// Add auth headers based on token type.
    async fn add_headers(
        &self,
        builder: reqwest::RequestBuilder,
    ) -> Result<reqwest::RequestBuilder> {
        self.maybe_refresh().await?;
        let auth = self.auth.read().await;
        let builder = match &*auth {
            AnthropicAuth::ApiKey(key) => builder.header("x-api-key", key),
            AnthropicAuth::OAuth { access_token, .. } => builder
                .header("Authorization", format!("Bearer {}", access_token))
                .header("anthropic-beta", ANTHROPIC_BETA),
            AnthropicAuth::Bearer(token) => {
                builder.header("Authorization", format!("Bearer {}", token))
            }
        };
        Ok(builder
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("content-type", "application/json"))
    }

    /// Refresh the OAuth token if expired.
    async fn maybe_refresh(&self) -> Result<()> {
        let needs_refresh = {
            let auth = self.auth.read().await;
            auth.is_expired()
        };

        if !needs_refresh {
            return Ok(());
        }

        let mut auth = self.auth.write().await;
        // Double-check after acquiring write lock
        if !auth.is_expired() {
            return Ok(());
        }

        if let AnthropicAuth::OAuth {
            refresh_token: Some(ref refresh),
            ..
        } = *auth
        {
            log::info!("refreshing expired Anthropic OAuth token");
            match refresh_anthropic_token(&self.client, refresh).await {
                Ok((new_access, new_expires)) => {
                    *auth = AnthropicAuth::OAuth {
                        access_token: new_access,
                        refresh_token: Some(refresh.clone()),
                        expires_ms: new_expires,
                    };
                }
                Err(e) => {
                    log::warn!("failed to refresh Anthropic OAuth token: {}", e);
                    // Try to re-read from OpenCode auth.json as fallback
                    if let Some((access, refresh_tok, expires)) = load_opencode_anthropic_token() {
                        *auth = AnthropicAuth::OAuth {
                            access_token: access,
                            refresh_token: Some(refresh_tok),
                            expires_ms: expires,
                        };
                    } else {
                        return Err(e);
                    }
                }
            }
        }

        Ok(())
    }
}

#[async_trait]
impl Provider for AnthropicProvider {
    async fn send(&self, request: &ChatRequest) -> Result<ChatResponse> {
        let mut req = request.clone();
        if self.strip_eager_streaming {
            strip_eager_input_streaming(&mut req);
        }
        let anth_req = build_anth_request(&req);
        let body = serde_json::to_string(&anth_req)?;
        log::debug!("anthropic request body: {}", &body[..body.len().min(500)]);

        let url = self.endpoint().await;
        let builder = self.client.post(&url);
        let builder = self.add_headers(builder).await?;
        let resp = builder.body(body).send().await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp
                .text()
                .await
                .unwrap_or_else(|e| format!("(failed to read body: {})", e));
            bail!("anthropic error {}: {}", status, text);
        }

        let anth_resp: AnthResponse = resp.json().await.context("parse anthropic response")?;
        Ok(anth_response_to_chat(anth_resp))
    }

    fn send_streaming(
        &self,
        request: &ChatRequest,
    ) -> Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>> {
        let client = self.client.clone();
        // Snapshot auth state before entering the stream.
        let (auth_header_name, auth_header_value, is_oauth) = {
            let auth = self.auth.try_read();
            match auth {
                Ok(guard) => match &*guard {
                    AnthropicAuth::ApiKey(key) => ("x-api-key".to_string(), key.clone(), false),
                    AnthropicAuth::OAuth { access_token, .. } => (
                        "Authorization".to_string(),
                        format!("Bearer {}", access_token),
                        true,
                    ),
                    AnthropicAuth::Bearer(token) => (
                        "Authorization".to_string(),
                        format!("Bearer {}", token),
                        false,
                    ),
                },
                Err(_) => {
                    log::warn!("auth lock contention in send_streaming, falling back to empty key");
                    ("x-api-key".to_string(), String::new(), false)
                }
            }
        };
        let url = if let Some(ref base) = self.base_url {
            format!("{}/messages", base.trim_end_matches('/'))
        } else if is_oauth {
            BASE_URL_OAUTH.to_string()
        } else {
            BASE_URL.to_string()
        };

        let mut req_owned = request.clone();
        if self.strip_eager_streaming {
            strip_eager_input_streaming(&mut req_owned);
        }
        let anth_req_serialized = serde_json::to_value(build_anth_request(&req_owned));
        let mut anth_req_owned = match anth_req_serialized {
            Ok(v) => v,
            Err(e) => {
                return Box::pin(stream! { yield Err(e.into()); });
            }
        };
        if let Some(obj) = anth_req_owned.as_object_mut() {
            obj.insert("stream".to_owned(), serde_json::Value::Bool(true));
        }

        Box::pin(stream! {
            let body = match serde_json::to_string(&anth_req_owned) {
                Ok(b) => b,
                Err(e) => { yield Err(e.into()); return; }
            };

            let mut request_builder = client
                .post(url)
                .header(&auth_header_name, &auth_header_value)
                .header("anthropic-version", ANTHROPIC_VERSION)
                .header("content-type", "application/json");
            if is_oauth {
                request_builder = request_builder.header("anthropic-beta", ANTHROPIC_BETA);
            }
            let request_builder = request_builder.body(body);

            let mut es = match request_builder.eventsource() {
                Ok(es) => es,
                Err(e) => { yield Err(anyhow::anyhow!("{}", e)); return; }
            };

            while let Some(event) = {
                use futures::StreamExt;
                es.next().await
            } {
                match event {
                    Ok(Event::Message(msg)) => {
                        let event_type = if msg.event.is_empty() {
                            "message"
                        } else {
                            &msg.event
                        };
                        let events = parse_anthropic_sse(event_type, &msg.data);
                        for ev in events {
                            yield Ok(ev);
                        }
                    }
                    Ok(Event::Open) => {}
                    Err(reqwest_eventsource::Error::StreamEnded) => {
                        break;
                    }
                    Err(e) => {
                        yield Err(anyhow::anyhow!("sse error: {}", e));
                        break;
                    }
                }
            }
        })
    }

    fn name(&self) -> &str {
        "anthropic"
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Foundry compatibility helpers
// ──────────────────────────────────────────────────────────────────────────────

/// Strip `eager_input_streaming` from tool definitions.
/// Foundry's Anthropic proxy returns 500 if this field is present.
fn strip_eager_input_streaming(req: &mut ChatRequest) {
    if let Some(ref mut tools) = req.tools {
        for tool in tools.iter_mut() {
            if let Some(obj) = tool.as_object_mut() {
                obj.remove("eager_input_streaming");
            }
        }
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// OpenCode auth.json token loading + OAuth refresh
// ──────────────────────────────────────────────────────────────────────────────

/// Load Anthropic OAuth tokens from any available source.
/// Tries Claude Code credentials first (freshest), then OpenCode auth.json.
/// Returns (access_token, refresh_token, expires_ms) or None.
pub fn load_opencode_anthropic_token() -> Option<(String, String, u64)> {
    // Try Claude Code credentials first (usually freshest)
    if let Some(token) = load_claude_code_token() {
        return Some(token);
    }

    // Fall back to OpenCode auth.json
    let paths = opencode_auth_paths();
    for path in &paths {
        if let Ok(data) = std::fs::read_to_string(path)
            && let Ok(v) = serde_json::from_str::<serde_json::Value>(&data)
            && let Some(anthro) = v.get("anthropic")
        {
            let access = anthro.get("access").and_then(|t| t.as_str()).unwrap_or("");
            let refresh = anthro.get("refresh").and_then(|t| t.as_str()).unwrap_or("");
            let expires = anthro.get("expires").and_then(|t| t.as_u64()).unwrap_or(0);

            if !access.is_empty() {
                log::info!("loaded Anthropic OAuth token from {}", path.display());
                return Some((access.to_string(), refresh.to_string(), expires));
            }
        }
    }
    None
}

pub fn opencode_auth_paths() -> Vec<std::path::PathBuf> {
    let mut paths = Vec::new();

    // Windows: %LOCALAPPDATA%/Temp/opencode-auth.json
    if let Ok(local) = std::env::var("LOCALAPPDATA") {
        paths.push(
            std::path::PathBuf::from(&local)
                .join("Temp")
                .join("opencode-auth.json"),
        );
    }

    // data_dir/../opencode/auth.json (Windows: AppData/Local/../opencode/auth.json)
    if let Some(data_dir) = dirs::data_dir() {
        paths.push(data_dir.join("../opencode/auth.json"));
    }

    // ~/.local/share/opencode/auth.json (Linux/WSL)
    if let Some(home) = dirs::home_dir() {
        paths.push(home.join(".local/share/opencode/auth.json"));
    }

    paths
}

/// Load Anthropic OAuth token from Claude Code's credentials.
/// Returns (access_token, refresh_token, expires_ms) or None.
pub fn load_claude_code_token() -> Option<(String, String, u64)> {
    let path = dirs::home_dir()?.join(".claude/.credentials.json");
    let data = std::fs::read_to_string(&path).ok()?;
    let v: serde_json::Value = serde_json::from_str(&data).ok()?;
    let oauth = v.get("claudeAiOauth")?;
    let access = oauth.get("accessToken")?.as_str()?;
    let refresh = oauth.get("refreshToken")?.as_str().unwrap_or("");
    let expires = oauth.get("expiresAt")?.as_u64().unwrap_or(0);

    if !access.is_empty() {
        log::info!("loaded Anthropic OAuth token from Claude Code credentials");
        Some((access.to_string(), refresh.to_string(), expires))
    } else {
        None
    }
}

const ANTHROPIC_OAUTH_CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";

/// Refresh an Anthropic OAuth token using the refresh token.
/// Uses form-encoded POST matching OpenCode's auth plugin format.
async fn refresh_anthropic_token(
    client: &reqwest::Client,
    refresh_token: &str,
) -> Result<(String, u64)> {
    #[derive(Deserialize)]
    struct RefreshResponse {
        access_token: String,
        expires_in: Option<u64>,
    }

    let resp = client
        .post(ANTHROPIC_TOKEN_URL)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("client_id", ANTHROPIC_OAUTH_CLIENT_ID),
        ])
        .send()
        .await?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp
            .text()
            .await
            .unwrap_or_else(|e| format!("(failed to read body: {})", e));
        bail!("anthropic token refresh failed {}: {}", status, body);
    }

    let token_resp: RefreshResponse = resp.json().await?;
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    let expires_ms = now_ms + token_resp.expires_in.unwrap_or(3600) * 1000;

    // Update the OpenCode auth.json with the fresh token
    if let Err(e) = save_refreshed_token(&token_resp.access_token, refresh_token, expires_ms) {
        log::warn!("failed to save refreshed token to auth.json: {}", e);
    }

    Ok((token_resp.access_token, expires_ms))
}

/// Save refreshed token back to OpenCode's auth.json so both tools stay in sync.
fn save_refreshed_token(access: &str, refresh: &str, expires_ms: u64) -> Result<()> {
    let paths = opencode_auth_paths();
    for path in &paths {
        if path.exists()
            && let Ok(data) = std::fs::read_to_string(path)
            && let Ok(mut v) = serde_json::from_str::<serde_json::Value>(&data)
            && let Some(anthro) = v.get_mut("anthropic")
        {
            anthro["access"] = serde_json::Value::String(access.to_string());
            anthro["refresh"] = serde_json::Value::String(refresh.to_string());
            anthro["expires"] = serde_json::json!(expires_ms);
            std::fs::write(path, serde_json::to_string_pretty(&v)?)?;
            log::info!("saved refreshed Anthropic token to {}", path.display());
            return Ok(());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anth_response_normalizes_total_and_captures_cache() {
        let json = r#"{
            "id": "msg-1",
            "content": [{"type": "text", "text": "hi"}],
            "stop_reason": "end_turn",
            "usage": {
                "input_tokens": 10,
                "output_tokens": 5,
                "cache_read_input_tokens": 80,
                "cache_creation_input_tokens": 12
            }
        }"#;
        let resp: AnthResponse = serde_json::from_str(json).unwrap();
        let chat = anth_response_to_chat(resp);
        // input_tokens is normalized to the true total: 10 + 80 + 12 = 102.
        assert_eq!(chat.usage.input_tokens, 102);
        assert_eq!(chat.usage.output_tokens, 5);
        assert_eq!(chat.usage.cache_read_tokens, 80);
        assert_eq!(chat.usage.cache_write_tokens, 12);
    }
}
