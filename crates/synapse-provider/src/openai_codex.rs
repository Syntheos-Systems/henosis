//! OpenAI Codex auth storage and provider helpers.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use async_stream::stream;
use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use futures::Stream;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::types::{
    ChatMessage, ChatRequest, ChatResponse, ContentBlock, Provider, Role, StopReason, StreamEvent,
    Usage,
};

/// Stable provider identifier used in Synapse auth storage.
pub const PROVIDER_ID: &str = "openai-codex";
/// Default ChatGPT backend base URL for OpenAI Codex browser OAuth traffic.
pub const DEFAULT_BASE_URL: &str = "https://chatgpt.com/backend-api";
/// Default model identifier used by the OpenAI Codex provider.
pub const DEFAULT_MODEL: &str = "codex-mini-latest";
/// OpenAI browser OAuth authorization endpoint used by the Codex login flow.
pub const AUTHORIZATION_ENDPOINT: &str = "https://auth.openai.com/oauth/authorize";
/// OpenAI browser OAuth token endpoint used for code exchange and refresh.
pub const TOKEN_ENDPOINT: &str = "https://auth.openai.com/oauth/token";
/// Browser OAuth client identifier used by the Codex CLI-compatible flow.
pub const CODEX_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";

/// Top-level auth file persisted under Synapse config storage.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthFile {
    /// Auth file schema version.
    #[serde(default = "default_version")]
    pub version: u32,
    /// Provider-specific auth payloads keyed by provider id.
    #[serde(default)]
    pub providers: BTreeMap<String, serde_json::Value>,
}

/// Provides convenience helpers for reading and writing provider entries inside the auth file.
impl AuthFile {
    /// Builds an auth file containing only an OpenAI Codex provider entry.
    pub fn with_openai_codex(entry: ProviderEntry) -> Self {
        let mut auth = Self::default();
        auth.set_openai_codex(entry);
        auth
    }

    /// Decodes the OpenAI Codex provider entry if one is present.
    pub fn openai_codex(&self) -> Result<Option<ProviderEntry>> {
        match self.providers.get(PROVIDER_ID) {
            Some(value) => {
                let entry = serde_json::from_value(value.clone())
                    .map_err(|error| anyhow!("invalid OpenAI Codex provider entry: {error}"))?;
                Ok(Some(entry))
            }
            None => Ok(None),
        }
    }

    /// Inserts or replaces the OpenAI Codex provider entry.
    pub fn set_openai_codex(&mut self, entry: ProviderEntry) {
        self.providers.insert(
            PROVIDER_ID.to_string(),
            serde_json::to_value(entry).expect("ProviderEntry serializes"),
        );
    }
}

/// Provides the default empty auth file layout.
impl Default for AuthFile {
    /// Builds an auth file with the current schema version and no provider payloads.
    fn default() -> Self {
        Self {
            version: default_version(),
            providers: BTreeMap::new(),
        }
    }
}

/// Serialized OpenAI Codex auth entry stored inside the auth file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderEntry {
    /// Auth flow used to obtain the tokens.
    pub auth_mode: String,
    /// Base URL used with the current token set.
    pub base_url: String,
    /// OAuth token bundle used for authenticated requests.
    pub tokens: CodexTokens,
    /// Optional account metadata returned by the auth flow.
    #[serde(default)]
    pub account: Option<AccountInfo>,
    /// Unix-seconds timestamp string for the last write.
    pub updated_at: String,
}

/// Serialized OpenAI Codex OAuth tokens.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodexTokens {
    /// Bearer token used for authenticated API calls.
    pub access_token: String,
    /// Refresh token used to renew expired access tokens.
    #[serde(default)]
    pub refresh_token: Option<String>,
    /// Optional ID token returned by the upstream auth flow.
    #[serde(default)]
    pub id_token: Option<String>,
    /// Expiration time expressed as Unix seconds.
    pub expires_at: u64,
}

/// Optional account details persisted alongside the OpenAI Codex auth entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountInfo {
    /// Upstream account identifier when available.
    #[serde(default)]
    pub account_id: Option<String>,
    /// Account email when available.
    #[serde(default)]
    pub email: Option<String>,
    /// Subscription plan when available.
    #[serde(default)]
    pub plan: Option<String>,
}

/// PKCE verifier/challenge pair used by the browser OAuth flow.
#[derive(Debug, Clone)]
pub struct PkcePair {
    /// Random verifier sent only to the token endpoint.
    pub verifier: String,
    /// SHA-256 challenge sent in the browser authorization request.
    pub challenge: String,
}

/// Classification of the current OpenAI Codex auth state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthStatus {
    /// No usable auth entry exists yet.
    Missing,
    /// A non-expired access token is available for immediate use.
    Ready { expires_at: u64 },
    /// An auth entry exists, but the access token is expired or nearly expired.
    RefreshNeeded,
}

/// Helper for locating and reading Synapse's OpenAI Codex auth storage.
#[derive(Debug, Clone)]
pub struct CodexAuth {
    /// Path to the Synapse auth.json file.
    path: PathBuf,
}

/// Locates persisted OpenAI Codex auth state and validates whether it is ready to use.
impl CodexAuth {
    /// Creates a helper bound to an explicit auth file path.
    pub fn from_path(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// Returns the default Synapse auth.json location.
    pub fn default_path() -> PathBuf {
        if let Some(config) = dirs::config_dir() {
            return config.join("synapse").join("auth.json");
        }
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".config")
            .join("synapse")
            .join("auth.json")
    }

    /// Returns the underlying auth.json path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Classifies the current auth file into ready, refresh-needed, or missing.
    pub fn status(&self) -> Result<AuthStatus> {
        let Some(entry) = load_auth_file(&self.path)?.openai_codex()? else {
            return Ok(AuthStatus::Missing);
        };

        if entry.tokens.access_token.is_empty() {
            bail!("invalid OpenAI Codex auth entry: access token is empty");
        }
        if entry.tokens.expires_at <= unix_seconds().saturating_add(60) {
            return Ok(AuthStatus::RefreshNeeded);
        }

        Ok(AuthStatus::Ready {
            expires_at: entry.tokens.expires_at,
        })
    }

    /// Loads the current provider entry only when the auth state is ready.
    pub fn ready_entry(&self) -> Result<ProviderEntry> {
        match self.status()? {
            AuthStatus::Ready { .. } => load_auth_file(&self.path)?
                .openai_codex()?
                .ok_or_else(|| anyhow!("missing OpenAI Codex provider entry")),
            AuthStatus::RefreshNeeded => {
                bail!("OpenAI Codex token is expired -- run `synapse login openai-codex`")
            }
            AuthStatus::Missing => {
                bail!("OpenAI Codex auth missing -- run `synapse login openai-codex`")
            }
        }
    }

    /// Loads the provider entry for runtime use, tolerating an expired access token
    /// when a refresh token is available. The runtime provider refreshes on first use.
    /// Only a truly unusable entry (absent, or no tokens at all) is rejected here.
    pub fn entry_for_runtime(&self) -> Result<ProviderEntry> {
        let Some(entry) = load_auth_file(&self.path)?.openai_codex()? else {
            bail!("OpenAI Codex auth missing -- run `synapse login openai-codex`");
        };
        if entry.tokens.access_token.is_empty() && entry.tokens.refresh_token.is_none() {
            bail!(
                "OpenAI Codex auth entry has no usable tokens -- run `synapse login openai-codex`"
            );
        }
        Ok(entry)
    }
}

/// Runtime provider that translates Synapse chat requests onto OpenAI Responses.
#[derive(Debug, Clone)]
pub struct OpenAICodexProvider {
    /// Shared HTTP client used for API requests.
    client: reqwest::Client,
    /// Base URL for the ChatGPT backend or a direct Responses-compatible endpoint.
    base_url: String,
    /// OAuth entry guarded for in-place refresh across cloned provider handles.
    entry: Arc<tokio::sync::RwLock<ProviderEntry>>,
    /// Auth file to persist refreshed tokens to, when this provider owns one.
    auth_path: Option<PathBuf>,
    /// Token endpoint used for refresh; overridable in tests.
    token_endpoint: String,
}

/// Adds constructor, refresh, and endpoint helpers for the OpenAI Codex runtime provider.
impl OpenAICodexProvider {
    /// Creates a runtime provider from a resolved base URL and a static bearer token.
    ///
    /// This path carries no refresh token, so the token is treated as long-lived and
    /// `maybe_refresh` becomes a no-op. Used for the explicit `OpenAICodex { access_token }`
    /// config where the caller supplied a raw token.
    pub fn new(client: reqwest::Client, base_url: String, access_token: String) -> Self {
        let entry = ProviderEntry {
            auth_mode: "browser_oauth".into(),
            base_url: base_url.clone(),
            tokens: CodexTokens {
                access_token,
                refresh_token: None,
                id_token: None,
                expires_at: u64::MAX,
            },
            account: None,
            updated_at: unix_seconds().to_string(),
        };
        Self::from_entry(client, base_url, entry, None)
    }

    /// Creates a runtime provider that can refresh and persist its OAuth tokens.
    ///
    /// When `auth_path` is `Some`, refreshed tokens are written back atomically so the
    /// next process launch starts from a fresh access token.
    pub fn from_entry(
        client: reqwest::Client,
        base_url: String,
        entry: ProviderEntry,
        auth_path: Option<PathBuf>,
    ) -> Self {
        Self {
            client,
            base_url,
            entry: Arc::new(tokio::sync::RwLock::new(entry)),
            auth_path,
            token_endpoint: TOKEN_ENDPOINT.to_string(),
        }
    }

    /// Returns the effective Responses endpoint for the configured base URL.
    pub fn endpoint(&self) -> String {
        response_endpoint(&self.base_url)
    }

    /// Refreshes the access token in place when it is expired and a refresh token exists.
    async fn maybe_refresh(&self) -> Result<()> {
        let needs_refresh = {
            let entry = self.entry.read().await;
            entry_needs_refresh(&entry)
        };
        if !needs_refresh {
            return Ok(());
        }

        let mut entry = self.entry.write().await;
        if !entry_needs_refresh(&entry) {
            return Ok(());
        }

        log::info!("refreshing expired OpenAI Codex OAuth token");
        let refreshed =
            refresh_entry_with_endpoint(&self.client, &self.token_endpoint, &entry).await?;
        if let Some(path) = &self.auth_path {
            save_provider_entry(path, refreshed.clone())?;
        }
        *entry = refreshed;
        Ok(())
    }

    /// Returns the current bearer token, refreshing first when required.
    async fn bearer_token(&self) -> Result<String> {
        self.maybe_refresh().await?;
        Ok(self.entry.read().await.tokens.access_token.clone())
    }
}

/// Returns whether a refreshable OpenAI Codex auth entry needs a new access token.
fn entry_needs_refresh(entry: &ProviderEntry) -> bool {
    entry.tokens.refresh_token.is_some()
        && (entry.tokens.access_token.is_empty()
            || entry.tokens.expires_at <= unix_seconds().saturating_add(60))
}

/// Maps a configured base URL onto the correct OpenAI Responses endpoint.
pub fn response_endpoint(base_url: &str) -> String {
    let base = base_url.trim_end_matches('/');
    if base.ends_with("/responses") || base.ends_with("/codex/responses") {
        base.to_string()
    } else if base.contains("chatgpt.com/backend-api") {
        format!("{base}/codex/responses")
    } else {
        format!("{base}/responses")
    }
}

/// Translates a Synapse chat request into the OpenAI Responses request body.
fn build_responses_body(req: &ChatRequest) -> serde_json::Value {
    serde_json::json!({
        "model": req.model,
        "input": req.messages.iter().flat_map(build_responses_input_items).collect::<Vec<_>>(),
        "instructions": req.system.clone().unwrap_or_default(),
        "tools": req.tools.clone().unwrap_or_default().into_iter().map(|tool| serde_json::json!({
            "type": "function",
            "name": tool["name"].clone(),
            "description": tool["description"].clone(),
            "parameters": tool["input_schema"].clone(),
        })).collect::<Vec<_>>(),
        "max_output_tokens": req.max_tokens,
        "stream": false,
        "store": false,
    })
}

/// Converts a single Synapse message into ordered OpenAI Responses input items.
fn build_responses_input_items(message: &ChatMessage) -> Vec<serde_json::Value> {
    let mut items = Vec::new();
    let mut pending_text = Vec::new();

    for block in &message.content {
        match block {
            ContentBlock::Text { text } => pending_text.push(text.clone()),
            ContentBlock::ToolUse { id, name, input } => {
                push_text_input_item(&mut items, &message.role, &mut pending_text);
                items.push(serde_json::json!({
                    "type": "function_call",
                    "call_id": id,
                    "name": name,
                    "arguments": serde_json::to_string(input).unwrap_or_else(|_| "{}".into()),
                }));
            }
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error: _,
            } => {
                push_text_input_item(&mut items, &message.role, &mut pending_text);
                items.push(serde_json::json!({
                    "type": "function_call_output",
                    "call_id": tool_use_id,
                    "output": content,
                }));
            }
        }
    }

    push_text_input_item(&mut items, &message.role, &mut pending_text);
    items
}

/// Flushes pending text into a role-appropriate Responses input item.
fn push_text_input_item(
    items: &mut Vec<serde_json::Value>,
    role: &Role,
    pending_text: &mut Vec<String>,
) {
    if pending_text.is_empty() {
        return;
    }

    let text = pending_text.join("\n");
    pending_text.clear();
    items.push(build_text_input_item(role, text));
}

/// Builds a Responses text item for the supported Synapse message roles.
fn build_text_input_item(role: &Role, text: String) -> serde_json::Value {
    match role {
        Role::System => serde_json::json!({
            "role": "system",
            "content": [{"type": "input_text", "text": text}],
        }),
        Role::Assistant => serde_json::json!({
            "role": "assistant",
            "content": [{"type": "output_text", "text": text}],
        }),
        Role::User | Role::Tool => serde_json::json!({
            "role": "user",
            "content": [{"type": "input_text", "text": text}],
        }),
    }
}

/// Parses an OpenAI Responses payload into Synapse's chat response shape.
fn parse_responses_body(value: serde_json::Value) -> ChatResponse {
    let content = parse_output_items(&value);
    let stop_reason = parse_responses_stop_reason(&value, &content);

    ChatResponse {
        id: value
            .get("id")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("openai-codex-response")
            .to_string(),
        content,
        stop_reason,
        usage: parse_responses_usage(value.get("usage")),
    }
}

/// Maps the Responses payload status into Synapse's stop-reason enum.
fn parse_responses_stop_reason(value: &serde_json::Value, content: &[ContentBlock]) -> StopReason {
    if content
        .iter()
        .any(|block| matches!(block, ContentBlock::ToolUse { .. }))
    {
        return StopReason::ToolUse;
    }

    if value.get("status").and_then(serde_json::Value::as_str) == Some("incomplete")
        && value
            .get("incomplete_details")
            .and_then(|details| details.get("reason"))
            .and_then(serde_json::Value::as_str)
            == Some("max_output_tokens")
    {
        return StopReason::MaxTokens;
    }

    StopReason::EndTurn
}

/// Converts OpenAI Responses output items into Synapse content blocks.
fn parse_output_items(value: &serde_json::Value) -> Vec<ContentBlock> {
    let mut content = Vec::new();
    if let Some(output) = value.get("output").and_then(serde_json::Value::as_array) {
        for item in output {
            match item.get("type").and_then(serde_json::Value::as_str) {
                Some("message") => {
                    let text = item
                        .get("content")
                        .and_then(serde_json::Value::as_array)
                        .into_iter()
                        .flatten()
                        .filter_map(|part| part.get("text").and_then(serde_json::Value::as_str))
                        .collect::<Vec<_>>()
                        .join("\n");
                    if !text.is_empty() {
                        content.push(ContentBlock::Text { text });
                    }
                }
                Some("function_call") => {
                    content.push(ContentBlock::ToolUse {
                        id: item
                            .get("call_id")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or("call_openai_codex")
                            .into(),
                        name: item
                            .get("name")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or_default()
                            .into(),
                        input: serde_json::from_str(
                            item.get("arguments")
                                .and_then(serde_json::Value::as_str)
                                .unwrap_or("{}"),
                        )
                        .unwrap_or_else(|_| serde_json::json!({})),
                    });
                }
                _ => {}
            }
        }
    }
    content
}

/// Converts Responses usage counters into Synapse's usage struct.
fn parse_responses_usage(usage: Option<&serde_json::Value>) -> Usage {
    let Some(usage) = usage else {
        return Usage::default();
    };

    Usage {
        input_tokens: usage
            .get("input_tokens")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0) as u32,
        output_tokens: usage
            .get("output_tokens")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0) as u32,
        cache_read_tokens: usage
            .get("input_tokens_details")
            .and_then(|d| d.get("cached_tokens"))
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0) as u32,
        cache_write_tokens: 0,
    }
}

/// Adapts a one-shot chat response into the StreamEvent sequence consumed by synapse-core.
fn stream_events_from_response(response: &ChatResponse) -> Vec<StreamEvent> {
    let mut events = Vec::new();

    for block in &response.content {
        match block {
            ContentBlock::Text { text } => {
                if !text.is_empty() {
                    events.push(StreamEvent::ContentDelta(text.clone()));
                }
            }
            ContentBlock::ToolUse { id, name, input } => {
                events.push(StreamEvent::ToolUseStart {
                    id: id.clone(),
                    name: name.clone(),
                });
                events.push(StreamEvent::ToolUseInputDelta(
                    serde_json::to_string(input).unwrap_or_else(|_| "{}".into()),
                ));
            }
            ContentBlock::ToolResult { .. } => {}
        }
    }

    events.push(StreamEvent::MessageStop(response.stop_reason.clone()));
    events.push(StreamEvent::Usage(response.usage.clone()));
    events
}

/// Implements the Synapse provider contract for the OpenAI Codex Responses runtime.
#[async_trait]
impl Provider for OpenAICodexProvider {
    /// Sends a one-shot chat request through the OpenAI Responses API.
    async fn send(&self, request: &ChatRequest) -> Result<ChatResponse> {
        let token = self.bearer_token().await?;
        let body = build_responses_body(request);
        let response = self
            .client
            .post(self.endpoint())
            .header("Authorization", format!("Bearer {token}"))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let text = response
                .text()
                .await
                .unwrap_or_else(|error| format!("(failed to read body: {error})"));
            bail!("openai codex error {status}: {text}");
        }

        let value = response
            .json::<serde_json::Value>()
            .await
            .context("parse OpenAI Codex Responses payload")?;
        Ok(parse_responses_body(value))
    }

    /// Adapts the one-shot Responses API into Synapse's streaming event contract.
    fn send_streaming(
        &self,
        request: &ChatRequest,
    ) -> Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>> {
        let provider = self.clone();
        let request = request.clone();

        Box::pin(stream! {
            let response = match provider.send(&request).await {
                Ok(response) => response,
                Err(error) => {
                    yield Err(error);
                    return;
                }
            };

            for event in stream_events_from_response(&response) {
                yield Ok(event);
            }
        })
    }

    /// Reports the provider identity used by Synapse telemetry.
    fn name(&self) -> &str {
        PROVIDER_ID
    }
}

/// Generates a PKCE verifier and matching S256 challenge for browser OAuth.
pub fn generate_pkce() -> PkcePair {
    let verifier = URL_SAFE_NO_PAD.encode(rand::random::<[u8; 32]>());
    let digest = Sha256::digest(verifier.as_bytes());
    let challenge = URL_SAFE_NO_PAD.encode(digest);
    PkcePair {
        verifier,
        challenge,
    }
}

/// Builds the browser authorization URL for the OpenAI Codex OAuth flow.
pub fn authorization_url(redirect_uri: &str, state: &str, code_challenge: &str) -> String {
    let params = [
        ("response_type", "code"),
        ("client_id", CODEX_CLIENT_ID),
        ("redirect_uri", redirect_uri),
        (
            "scope",
            "openid profile email offline_access api.connectors.read api.connectors.invoke",
        ),
        ("code_challenge", code_challenge),
        ("code_challenge_method", "S256"),
        ("state", state),
        ("codex_cli_simplified_flow", "true"),
        ("id_token_add_organizations", "true"),
    ];
    let query = params
        .iter()
        .map(|(key, value)| format!("{key}={}", urlencoding::encode(value)))
        .collect::<Vec<_>>()
        .join("&");
    format!("{AUTHORIZATION_ENDPOINT}?{query}")
}

/// Exchanges an OAuth authorization code for a persisted provider entry.
pub async fn exchange_code(
    client: &reqwest::Client,
    code: &str,
    verifier: &str,
    redirect_uri: &str,
) -> Result<ProviderEntry> {
    exchange_code_with_endpoint(client, TOKEN_ENDPOINT, code, verifier, redirect_uri).await
}

/// Exchanges an OAuth authorization code against an injectable token endpoint for tests.
async fn exchange_code_with_endpoint(
    client: &reqwest::Client,
    token_endpoint: &str,
    code: &str,
    verifier: &str,
    redirect_uri: &str,
) -> Result<ProviderEntry> {
    let response = client
        .post(token_endpoint)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .form(&[
            ("grant_type", "authorization_code"),
            ("client_id", CODEX_CLIENT_ID),
            ("code", code),
            ("code_verifier", verifier),
            ("redirect_uri", redirect_uri),
        ])
        .send()
        .await?;

    token_response_to_entry(response).await
}

/// Refreshes a provider entry against an injectable token endpoint for tests.
async fn refresh_entry_with_endpoint(
    client: &reqwest::Client,
    token_endpoint: &str,
    entry: &ProviderEntry,
) -> Result<ProviderEntry> {
    let refresh_token = entry
        .tokens
        .refresh_token
        .as_deref()
        .ok_or_else(|| anyhow!("OpenAI Codex refresh token missing"))?;
    let response = client
        .post(token_endpoint)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .form(&[
            ("grant_type", "refresh_token"),
            ("client_id", CODEX_CLIENT_ID),
            ("refresh_token", refresh_token),
        ])
        .send()
        .await?;

    let mut refreshed = token_response_to_entry(response).await?;
    refreshed.auth_mode = entry.auth_mode.clone();
    refreshed.base_url = entry.base_url.clone();
    refreshed.account = entry.account.clone();
    if refreshed.tokens.refresh_token.is_none() {
        refreshed.tokens.refresh_token = entry.tokens.refresh_token.clone();
    }
    Ok(refreshed)
}

/// Refreshes a persisted provider entry using its stored refresh token.
pub async fn refresh_entry(
    client: &reqwest::Client,
    entry: &ProviderEntry,
) -> Result<ProviderEntry> {
    refresh_entry_with_endpoint(client, TOKEN_ENDPOINT, entry).await
}

/// Wire format returned by the OpenAI OAuth token endpoint.
#[derive(Debug, Deserialize)]
struct TokenResponse {
    /// New access token for authenticated requests.
    access_token: String,
    /// Optional rotated refresh token.
    #[serde(default)]
    refresh_token: Option<String>,
    /// Optional ID token returned by the upstream auth flow.
    #[serde(default)]
    id_token: Option<String>,
    /// Access-token lifetime in seconds.
    #[serde(default)]
    expires_in: Option<u64>,
}

/// Converts a token-endpoint response into Synapse's persisted provider-entry shape.
async fn token_response_to_entry(response: reqwest::Response) -> Result<ProviderEntry> {
    let response = oauth_success_response(response).await?;
    let token: TokenResponse = response.json().await?;
    let now = unix_seconds();
    let expires_at = now.saturating_add(token.expires_in.unwrap_or(3600));
    Ok(ProviderEntry {
        auth_mode: "browser_oauth".into(),
        base_url: DEFAULT_BASE_URL.into(),
        tokens: CodexTokens {
            access_token: token.access_token,
            refresh_token: token.refresh_token,
            id_token: token.id_token,
            expires_at,
        },
        account: None,
        updated_at: now.to_string(),
    })
}

/// Converts non-success OAuth HTTP responses into actionable errors before JSON decoding.
async fn oauth_success_response(response: reqwest::Response) -> Result<reqwest::Response> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }

    let body = response
        .text()
        .await
        .unwrap_or_else(|error| format!("failed to read OAuth error response body: {error}"));
    bail!("OpenAI Codex OAuth request failed with status {status}: {body}");
}

/// Saves the full auth file atomically with user-only permissions on Unix.
pub fn save_auth_file(path: &Path, auth: &AuthFile) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temp_path = path.with_extension(format!("json.tmp-{}", uuid::Uuid::new_v4()));
    let data = serde_json::to_vec_pretty(auth)?;
    std::fs::write(&temp_path, data)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&temp_path, std::fs::Permissions::from_mode(0o600))?;
    }
    std::fs::rename(&temp_path, path)?;
    Ok(())
}

/// Loads the auth file from disk, returning an empty auth file when absent.
pub fn load_auth_file(path: &Path) -> Result<AuthFile> {
    if !path.exists() {
        return Ok(AuthFile::default());
    }
    let data = std::fs::read_to_string(path)?;
    Ok(serde_json::from_str(&data)?)
}

/// Saves or updates the OpenAI Codex provider entry in the auth file.
pub fn save_provider_entry(path: &Path, entry: ProviderEntry) -> Result<()> {
    let mut auth = load_auth_file(path)?;
    auth.set_openai_codex(entry);
    save_auth_file(path, &auth)
}

/// Removes the OpenAI Codex provider entry from the auth file.
pub fn remove_provider(path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let mut auth = load_auth_file(path)?;
    auth.providers.remove(PROVIDER_ID);
    save_auth_file(path, &auth)
}

/// Returns the current auth file schema version.
pub fn default_version() -> u32 {
    1
}

/// Returns the current Unix time in seconds.
pub fn unix_seconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
/// Exercises the OpenAI Codex auth and runtime translation helpers.
mod tests {
    use super::*;
    use futures::StreamExt;
    use std::fs;
    use tokio::sync::oneshot;

    /// Builds a representative OpenAI Codex provider entry for tests.
    fn sample_entry(expires_at: u64) -> ProviderEntry {
        ProviderEntry {
            auth_mode: "browser_oauth".into(),
            base_url: DEFAULT_BASE_URL.into(),
            tokens: CodexTokens {
                access_token: "access".into(),
                refresh_token: Some("refresh".into()),
                id_token: Some("id".into()),
                expires_at,
            },
            account: Some(AccountInfo {
                account_id: Some("acct_123".into()),
                email: Some("user@example.com".into()),
                plan: Some("plus".into()),
            }),
            updated_at: "2026-05-28T00:00:00Z".into(),
        }
    }

    /// Builds a representative OpenAI Codex provider entry with a caller-provided access token.
    fn sample_entry_with_access_token(access_token: &str, expires_at: u64) -> ProviderEntry {
        let mut entry = sample_entry(expires_at);
        entry.tokens.access_token = access_token.into();
        entry
    }

    /// Verifies that an OpenAI Codex provider entry round-trips through the auth file container.
    #[test]
    fn auth_file_round_trips_openai_codex_entry() {
        let entry = sample_entry(1_800_000_000);

        let auth = AuthFile::with_openai_codex(entry.clone());
        let decoded = auth.openai_codex().unwrap().expect("entry present");

        assert_eq!(decoded.base_url, DEFAULT_BASE_URL);
        assert_eq!(decoded.tokens.refresh_token.as_deref(), Some("refresh"));
        assert_eq!(
            decoded.account.unwrap().email.as_deref(),
            Some("user@example.com")
        );
    }

    /// Verifies that auth status reports missing when no auth file exists yet.
    #[test]
    fn status_reports_missing_when_file_absent() {
        let dir = tempfile::tempdir().unwrap();
        let auth = CodexAuth::from_path(dir.path().join("auth.json"));

        assert_eq!(auth.status().unwrap(), AuthStatus::Missing);
    }

    /// Verifies that a valid non-expired entry reports Ready.
    #[test]
    fn status_reports_ready_for_fresh_token() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");

        save_provider_entry(&path, sample_entry(unix_seconds().saturating_add(3_600))).unwrap();

        assert_eq!(
            CodexAuth::from_path(&path).status().unwrap(),
            AuthStatus::Ready {
                expires_at: load_auth_file(&path)
                    .unwrap()
                    .openai_codex()
                    .unwrap()
                    .unwrap()
                    .tokens
                    .expires_at
            }
        );
    }

    /// Verifies that an expiring token reports RefreshNeeded.
    #[test]
    fn status_reports_refresh_needed_for_expiring_token() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");

        save_provider_entry(&path, sample_entry(unix_seconds().saturating_add(30))).unwrap();

        assert_eq!(
            CodexAuth::from_path(path).status().unwrap(),
            AuthStatus::RefreshNeeded
        );
    }

    /// Returns an expired entry for runtime use when a refresh token is available.
    #[test]
    fn entry_for_runtime_returns_expired_entry_when_refreshable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        let mut entry = sample_entry(1);
        entry.tokens.refresh_token = Some("refresh".into());
        save_provider_entry(&path, entry).unwrap();

        let resolved = CodexAuth::from_path(&path).entry_for_runtime().unwrap();

        assert_eq!(resolved.tokens.refresh_token.as_deref(), Some("refresh"));
    }

    /// Errors when no auth entry exists at all.
    #[test]
    fn entry_for_runtime_errors_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");

        let error = CodexAuth::from_path(&path)
            .entry_for_runtime()
            .unwrap_err()
            .to_string();

        assert!(error.contains("login openai-codex"));
    }

    /// Errors when the persisted entry has neither an access token nor a refresh token.
    #[test]
    fn entry_for_runtime_errors_when_no_usable_tokens() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        let mut entry = sample_entry(unix_seconds().saturating_add(3_600));
        entry.tokens.access_token.clear();
        entry.tokens.refresh_token = None;
        save_provider_entry(&path, entry).unwrap();

        let error = CodexAuth::from_path(&path)
            .entry_for_runtime()
            .unwrap_err()
            .to_string();

        assert!(error.contains("no usable tokens"));
    }

    /// Verifies that corrupt OpenAI Codex provider entries surface as errors.
    #[test]
    fn corrupt_openai_codex_entry_surfaces_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        let mut auth = AuthFile::default();
        auth.providers.insert(
            PROVIDER_ID.to_string(),
            serde_json::json!({ "auth_mode": "browser_oauth" }),
        );
        save_auth_file(&path, &auth).unwrap();

        assert!(load_auth_file(&path).unwrap().openai_codex().is_err());
        assert!(CodexAuth::from_path(path).status().is_err());
    }

    /// Verifies that an empty persisted access token surfaces as invalid auth state.
    #[test]
    fn empty_access_token_surfaces_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");

        save_provider_entry(
            &path,
            sample_entry_with_access_token("", unix_seconds().saturating_add(3_600)),
        )
        .unwrap();

        let error = CodexAuth::from_path(path).status().unwrap_err().to_string();
        assert!(error.contains("access token is empty"));
    }

    /// Verifies that saving a provider entry can later be read back from storage.
    #[test]
    fn save_provider_entry_persists_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");

        save_provider_entry(&path, sample_entry(unix_seconds().saturating_add(3_600))).unwrap();

        let decoded = load_auth_file(&path)
            .unwrap()
            .openai_codex()
            .unwrap()
            .expect("entry present");
        assert_eq!(decoded.tokens.access_token, "access");
        assert_eq!(decoded.account.unwrap().plan.as_deref(), Some("plus"));
    }

    /// Verifies that auth writes can overwrite an existing auth.json through repeated updates.
    #[test]
    fn save_provider_entry_overwrites_existing_auth_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        let second_expires_at = unix_seconds().saturating_add(7_200);

        save_provider_entry(
            &path,
            sample_entry_with_access_token("first", unix_seconds().saturating_add(3_600)),
        )
        .unwrap();
        save_provider_entry(
            &path,
            sample_entry_with_access_token("second", second_expires_at),
        )
        .unwrap();

        let decoded = load_auth_file(&path)
            .unwrap()
            .openai_codex()
            .unwrap()
            .expect("entry present");
        assert_eq!(decoded.tokens.access_token, "second");
        assert_eq!(decoded.tokens.expires_at, second_expires_at);
    }

    /// Verifies that removing the codex entry preserves unrelated providers.
    #[test]
    fn remove_provider_preserves_unrelated_providers() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        let mut auth =
            AuthFile::with_openai_codex(sample_entry(unix_seconds().saturating_add(3_600)));
        auth.providers.insert(
            "other-provider".into(),
            serde_json::json!({"token":"keep-me"}),
        );
        save_auth_file(&path, &auth).unwrap();

        remove_provider(&path).unwrap();

        let decoded = load_auth_file(&path).unwrap();
        assert!(decoded.openai_codex().unwrap().is_none());
        assert_eq!(
            decoded.providers.get("other-provider"),
            Some(&serde_json::json!({"token":"keep-me"}))
        );
    }

    /// Verifies that updating the codex entry preserves unrelated providers.
    #[test]
    fn save_provider_entry_preserves_unrelated_providers() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        let mut auth = AuthFile::default();
        auth.providers.insert(
            "other-provider".into(),
            serde_json::json!({"token":"keep-me"}),
        );
        save_auth_file(&path, &auth).unwrap();

        save_provider_entry(&path, sample_entry(unix_seconds().saturating_add(3_600))).unwrap();

        let decoded = load_auth_file(&path).unwrap();
        assert_eq!(
            decoded.providers.get("other-provider"),
            Some(&serde_json::json!({"token":"keep-me"}))
        );
        assert!(decoded.openai_codex().unwrap().is_some());
    }

    /// Verifies that removing the codex entry deletes only that provider payload from disk.
    #[test]
    fn remove_provider_deletes_only_codex_entry() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");

        save_provider_entry(&path, sample_entry(unix_seconds().saturating_add(3_600))).unwrap();
        remove_provider(&path).unwrap();

        let raw = fs::read_to_string(&path).unwrap();
        assert!(!raw.contains(PROVIDER_ID));
    }

    /// Verifies that removing a provider from a clean system does not create auth.json.
    #[test]
    fn remove_provider_is_noop_when_auth_file_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");

        remove_provider(&path).unwrap();

        assert!(!path.exists());
    }

    /// Captured request data from the one-shot local token test server.
    #[derive(Debug)]
    struct CapturedRequest {
        /// Raw HTTP request line received by the server.
        request_line: String,
        /// Decoded request body received by the server.
        body: String,
    }

    /// Spawns a one-shot local HTTP server that captures the request and returns a fixed response.
    async fn spawn_token_server(
        status_line: &'static str,
        response_body: &'static str,
    ) -> (String, oneshot::Receiver<CapturedRequest>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        let (sender, receiver) = oneshot::channel();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buffer = [0_u8; 2048];
            let bytes_read = socket.read(&mut buffer).await.unwrap();
            let request = String::from_utf8_lossy(&buffer[..bytes_read]).into_owned();
            let mut sections = request.split("\r\n\r\n");
            let headers = sections.next().unwrap_or_default();
            let body = sections.next().unwrap_or_default().to_string();
            let request_line = headers.lines().next().unwrap_or_default().to_string();
            let _ = sender.send(CapturedRequest { request_line, body });
            let response = format!(
                "HTTP/1.1 {status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                response_body.len(),
                response_body
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        });
        (format!("http://{addr}"), receiver)
    }

    /// Verifies that the browser authorization URL includes PKCE and CSRF state parameters.
    #[test]
    fn authorization_url_contains_pkce_and_state() {
        let url = authorization_url(
            "http://localhost:1455/auth/callback?from=cli flow",
            "state-123",
            "challenge+/=",
        );

        assert!(url.starts_with(AUTHORIZATION_ENDPOINT));
        assert!(url.contains("response_type=code"));
        assert!(url.contains(&format!("client_id={CODEX_CLIENT_ID}")));
        assert!(url.contains("state=state-123"));
        assert!(url.contains("code_challenge=challenge%2B%2F%3D"));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains(
            "redirect_uri=http%3A%2F%2Flocalhost%3A1455%2Fauth%2Fcallback%3Ffrom%3Dcli%20flow"
        ));
        assert!(url.contains("scope=openid%20profile%20email%20offline_access%20api.connectors.read%20api.connectors.invoke"));
    }

    /// Verifies that ChatGPT backend URLs route to the Codex Responses endpoint.
    #[test]
    fn response_endpoint_appends_codex_responses_for_chatgpt_backend() {
        assert_eq!(
            response_endpoint("https://chatgpt.com/backend-api"),
            "https://chatgpt.com/backend-api/codex/responses"
        );
    }

    /// Verifies that Responses request bodies map model, instructions, and tool schemas.
    #[test]
    fn build_responses_body_maps_tools_and_text() {
        let request = ChatRequest {
            model: "codex-mini-latest".into(),
            messages: vec![ChatMessage {
                role: Role::User,
                content: vec![ContentBlock::Text {
                    text: "list files".into(),
                }],
            }],
            tools: Some(vec![serde_json::json!({
                "name": "bash",
                "description": "run shell",
                "input_schema": {"type": "object"}
            })]),
            max_tokens: 64,
            system: Some("You are helpful.".into()),
            stream: false,
        };

        let body = build_responses_body(&request);
        assert_eq!(body["model"], "codex-mini-latest");
        assert_eq!(body["instructions"], "You are helpful.");
        assert_eq!(body["tools"][0]["name"], "bash");
        assert_eq!(
            body["tools"][0]["parameters"],
            serde_json::json!({"type": "object"})
        );
        assert_eq!(body["input"][0]["role"], "user");
        assert_eq!(body["input"][0]["content"][0]["text"], "list files");
    }

    /// Verifies that request translation preserves assistant tool calls and tool outputs as distinct Responses items.
    #[test]
    fn build_responses_body_preserves_tool_history_items() {
        let request = ChatRequest {
            model: "codex-mini-latest".into(),
            messages: vec![
                ChatMessage {
                    role: Role::User,
                    content: vec![ContentBlock::Text {
                        text: "list files".into(),
                    }],
                },
                ChatMessage {
                    role: Role::Assistant,
                    content: vec![
                        ContentBlock::Text {
                            text: "Checking.".into(),
                        },
                        ContentBlock::ToolUse {
                            id: "call_1".into(),
                            name: "bash".into(),
                            input: serde_json::json!({"command": "ls"}),
                        },
                    ],
                },
                ChatMessage {
                    role: Role::User,
                    content: vec![ContentBlock::ToolResult {
                        tool_use_id: "call_1".into(),
                        content: "a.txt\nb.txt".into(),
                        is_error: false,
                    }],
                },
            ],
            tools: None,
            max_tokens: 64,
            system: None,
            stream: false,
        };

        let body = build_responses_body(&request);
        let input = body["input"].as_array().expect("input array");

        assert_eq!(input.len(), 4);
        assert_eq!(input[0]["role"], "user");
        assert_eq!(input[0]["content"][0]["type"], "input_text");
        assert_eq!(input[0]["content"][0]["text"], "list files");
        assert_eq!(input[1]["role"], "assistant");
        assert_eq!(input[1]["content"][0]["type"], "output_text");
        assert_eq!(input[1]["content"][0]["text"], "Checking.");
        assert_eq!(input[2]["type"], "function_call");
        assert_eq!(input[2]["call_id"], "call_1");
        assert_eq!(input[2]["name"], "bash");
        assert_eq!(input[2]["arguments"], "{\"command\":\"ls\"}");
        assert_eq!(input[3]["type"], "function_call_output");
        assert_eq!(input[3]["call_id"], "call_1");
        assert_eq!(input[3]["output"], "a.txt\nb.txt");
    }

    /// Verifies that system-role history remains system text instead of being flattened into user input.
    #[test]
    fn build_responses_body_preserves_system_role_history() {
        let request = ChatRequest {
            model: "codex-mini-latest".into(),
            messages: vec![ChatMessage {
                role: Role::System,
                content: vec![ContentBlock::Text {
                    text: "Follow the repo style guide.".into(),
                }],
            }],
            tools: None,
            max_tokens: 64,
            system: None,
            stream: false,
        };

        let body = build_responses_body(&request);

        assert_eq!(body["input"][0]["role"], "system");
        assert_eq!(body["input"][0]["content"][0]["type"], "input_text");
        assert_eq!(
            body["input"][0]["content"][0]["text"],
            "Follow the repo style guide."
        );
    }

    /// Verifies that function_call_output items do not include the undocumented is_error field.
    #[test]
    fn build_responses_body_omits_is_error_from_function_call_output() {
        let request = ChatRequest {
            model: "codex-mini-latest".into(),
            messages: vec![ChatMessage {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "call_1".into(),
                    content: "permission denied".into(),
                    is_error: true,
                }],
            }],
            tools: None,
            max_tokens: 64,
            system: None,
            stream: false,
        };

        let body = build_responses_body(&request);
        let item = &body["input"][0];

        assert_eq!(item["type"], "function_call_output");
        assert_eq!(item["call_id"], "call_1");
        assert_eq!(item["output"], "permission denied");
        assert!(item.get("is_error").is_none());
    }

    /// Verifies that Responses output parses back into Synapse text, tool, stop, and usage fields.
    #[test]
    fn parse_responses_body_maps_text_tool_use_and_usage() {
        let response = parse_responses_body(serde_json::json!({
            "id": "resp_123",
            "output": [
                {
                    "type": "message",
                    "content": [
                        {"type": "output_text", "text": "I can do that."}
                    ]
                },
                {
                    "type": "function_call",
                    "call_id": "call_1",
                    "name": "bash",
                    "arguments": "{\"command\":\"ls\"}"
                }
            ],
            "usage": {
                "input_tokens": 12,
                "output_tokens": 7
            }
        }));

        assert_eq!(response.id, "resp_123");
        assert_eq!(response.stop_reason, StopReason::ToolUse);
        assert_eq!(response.usage.input_tokens, 12);
        assert_eq!(response.usage.output_tokens, 7);
        assert!(matches!(
            &response.content[0],
            ContentBlock::Text { text } if text == "I can do that."
        ));
        assert!(matches!(
            &response.content[1],
            ContentBlock::ToolUse { id, name, input }
                if id == "call_1" && name == "bash" && *input == serde_json::json!({"command":"ls"})
        ));
    }

    /// Verifies that incomplete Responses with max_output_tokens map to StopReason::MaxTokens.
    #[test]
    fn parse_responses_body_maps_incomplete_max_output_tokens_to_max_tokens() {
        let response = parse_responses_body(serde_json::json!({
            "id": "resp_incomplete",
            "status": "incomplete",
            "incomplete_details": {
                "reason": "max_output_tokens"
            },
            "output": [
                {
                    "type": "message",
                    "content": [
                        {"type": "output_text", "text": "Cut off"}
                    ]
                }
            ]
        }));

        assert_eq!(response.id, "resp_incomplete");
        assert_eq!(response.stop_reason, StopReason::MaxTokens);
    }

    /// Verifies that one-shot chat responses can be adapted into a coherent StreamEvent sequence.
    #[test]
    fn stream_events_from_response_emits_text_tool_stop_and_usage() {
        let response = ChatResponse {
            id: "resp_stream".into(),
            content: vec![
                ContentBlock::Text {
                    text: "Checking files".into(),
                },
                ContentBlock::ToolUse {
                    id: "call_1".into(),
                    name: "bash".into(),
                    input: serde_json::json!({"command": "ls"}),
                },
            ],
            stop_reason: StopReason::ToolUse,
            usage: Usage {
                input_tokens: 11,
                output_tokens: 5,
                ..Default::default()
            },
        };

        let events = stream_events_from_response(&response);

        assert_eq!(events.len(), 5);
        assert!(matches!(
            &events[0],
            StreamEvent::ContentDelta(delta) if delta == "Checking files"
        ));
        assert!(matches!(
            &events[1],
            StreamEvent::ToolUseStart { id, name } if id == "call_1" && name == "bash"
        ));
        assert!(matches!(
            &events[2],
            StreamEvent::ToolUseInputDelta(delta) if delta == "{\"command\":\"ls\"}"
        ));
        assert!(matches!(
            &events[3],
            StreamEvent::MessageStop(reason) if reason == &StopReason::ToolUse
        ));
        assert!(matches!(
            &events[4],
            StreamEvent::Usage(Usage { input_tokens, output_tokens, .. })
                if input_tokens == &11 && output_tokens == &5
        ));
    }

    /// Verifies that PKCE generation derives the S256 challenge from the verifier.
    #[test]
    fn generate_pkce_returns_verifier_and_challenge() {
        let pair = generate_pkce();
        let expected_challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(pair.verifier.as_bytes()));

        assert!(!pair.verifier.is_empty());
        assert!(!pair.challenge.is_empty());
        assert_eq!(pair.challenge, expected_challenge);
    }

    /// Verifies that the OAuth code exchange response is parsed into a provider entry.
    #[tokio::test]
    async fn exchange_code_parses_token_response() {
        let (server, request) = spawn_token_server(
            "200 OK",
            r#"{"access_token":"new-access","refresh_token":"new-refresh","id_token":"new-id","expires_in":3600}"#,
        )
        .await;
        let client = reqwest::Client::new();
        let redirect_uri = "http://localhost:1455/auth/callback?from=cli flow";

        let entry =
            exchange_code_with_endpoint(&client, &server, "code-123", "verifier-456", redirect_uri)
                .await
                .unwrap();
        let captured = request.await.unwrap();

        assert_eq!(entry.auth_mode, "browser_oauth");
        assert_eq!(entry.base_url, DEFAULT_BASE_URL);
        assert_eq!(entry.tokens.access_token, "new-access");
        assert_eq!(entry.tokens.refresh_token.as_deref(), Some("new-refresh"));
        assert_eq!(entry.tokens.id_token.as_deref(), Some("new-id"));
        assert!(entry.tokens.expires_at >= unix_seconds().saturating_add(3_500));
        assert!(entry.updated_at.parse::<u64>().is_ok());
        assert_eq!(captured.request_line, "POST / HTTP/1.1");
        assert!(captured.body.contains("grant_type=authorization_code"));
        assert!(
            captured
                .body
                .contains(&format!("client_id={CODEX_CLIENT_ID}"))
        );
        assert!(captured.body.contains("code=code-123"));
        assert!(captured.body.contains("code_verifier=verifier-456"));
        assert!(captured.body.contains(
            "redirect_uri=http%3A%2F%2Flocalhost%3A1455%2Fauth%2Fcallback%3Ffrom%3Dcli+flow"
        ));
    }

    /// Verifies that HTTP failures surface status and body instead of a vague decode error.
    #[tokio::test]
    async fn exchange_code_surfaces_http_failures() {
        let (server, _request) = spawn_token_server(
            "401 Unauthorized",
            r#"{"error":"invalid_grant","error_description":"bad code"}"#,
        )
        .await;
        let client = reqwest::Client::new();

        let error = exchange_code_with_endpoint(
            &client,
            &server,
            "bad-code",
            "verifier-456",
            "http://localhost:1455/auth/callback",
        )
        .await
        .unwrap_err()
        .to_string();

        assert!(error.contains("401"));
        assert!(error.contains("invalid_grant"));
    }

    /// Verifies that refresh preserves the stored refresh token when the server omits a replacement.
    #[tokio::test]
    async fn refresh_entry_keeps_existing_refresh_token_when_server_omits_one() {
        let (server, request) = spawn_token_server(
            "200 OK",
            r#"{"access_token":"new-access","expires_in":3600}"#,
        )
        .await;
        let client = reqwest::Client::new();
        let entry = ProviderEntry {
            auth_mode: "browser_oauth".into(),
            base_url: "https://chatgpt.com/custom-backend".into(),
            tokens: CodexTokens {
                access_token: "old-access".into(),
                refresh_token: Some("keep-me".into()),
                id_token: None,
                expires_at: 1,
            },
            account: Some(AccountInfo {
                account_id: Some("acct_keep".into()),
                email: Some("keep@example.com".into()),
                plan: Some("team".into()),
            }),
            updated_at: "2026-05-28T00:00:00Z".into(),
        };

        let refreshed = refresh_entry_with_endpoint(&client, &server, &entry)
            .await
            .unwrap();
        let captured = request.await.unwrap();

        assert_eq!(refreshed.tokens.refresh_token.as_deref(), Some("keep-me"));
        assert_eq!(refreshed.base_url, entry.base_url);
        assert_eq!(
            refreshed
                .account
                .as_ref()
                .and_then(|account| account.email.as_deref()),
            Some("keep@example.com")
        );
        assert!(refreshed.updated_at.parse::<u64>().is_ok());
        assert!(captured.body.contains("grant_type=refresh_token"));
        assert!(
            captured
                .body
                .contains(&format!("client_id={CODEX_CLIENT_ID}"))
        );
        assert!(captured.body.contains("refresh_token=keep-me"));
    }

    /// Verifies that refresh also surfaces HTTP failures with status and body details.
    #[tokio::test]
    async fn refresh_entry_surfaces_http_failures() {
        let (server, _request) = spawn_token_server(
            "400 Bad Request",
            r#"{"error":"invalid_request","error_description":"refresh expired"}"#,
        )
        .await;
        let client = reqwest::Client::new();
        let entry = sample_entry(1);

        let error = refresh_entry_with_endpoint(&client, &server, &entry)
            .await
            .unwrap_err()
            .to_string();

        assert!(error.contains("400"));
        assert!(error.contains("refresh expired"));
    }

    /// Verifies the provider refreshes an expired token before a request and persists it.
    #[tokio::test]
    async fn maybe_refresh_renews_expired_token_and_persists() {
        let (server, request) = spawn_token_server(
            "200 OK",
            r#"{"access_token":"fresh-access","refresh_token":"rotated-refresh","expires_in":3600}"#,
        )
        .await;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");

        let entry = ProviderEntry {
            auth_mode: "browser_oauth".into(),
            base_url: DEFAULT_BASE_URL.into(),
            tokens: CodexTokens {
                access_token: "stale-access".into(),
                refresh_token: Some("old-refresh".into()),
                id_token: None,
                expires_at: 1,
            },
            account: None,
            updated_at: "2026-05-30T00:00:00Z".into(),
        };

        let mut provider = OpenAICodexProvider::from_entry(
            reqwest::Client::new(),
            DEFAULT_BASE_URL.into(),
            entry,
            Some(path.clone()),
        );
        provider.token_endpoint = server;

        let token = provider.bearer_token().await.unwrap();
        let captured = request.await.unwrap();

        assert_eq!(token, "fresh-access");
        assert!(captured.body.contains("grant_type=refresh_token"));
        assert!(captured.body.contains("refresh_token=old-refresh"));

        let persisted = load_auth_file(&path)
            .unwrap()
            .openai_codex()
            .unwrap()
            .unwrap();
        assert_eq!(persisted.tokens.access_token, "fresh-access");
        assert_eq!(
            persisted.tokens.refresh_token.as_deref(),
            Some("rotated-refresh")
        );
    }

    /// Verifies a refreshable entry with no access token refreshes before returning a bearer token.
    #[tokio::test]
    async fn maybe_refresh_renews_empty_access_token_even_when_not_expired() {
        let (server, request) = spawn_token_server(
            "200 OK",
            r#"{"access_token":"fresh-access","refresh_token":"rotated-refresh","expires_in":3600}"#,
        )
        .await;
        let entry = ProviderEntry {
            auth_mode: "browser_oauth".into(),
            base_url: DEFAULT_BASE_URL.into(),
            tokens: CodexTokens {
                access_token: String::new(),
                refresh_token: Some("old-refresh".into()),
                id_token: None,
                expires_at: unix_seconds().saturating_add(3_600),
            },
            account: None,
            updated_at: "2026-05-30T00:00:00Z".into(),
        };

        let mut provider = OpenAICodexProvider::from_entry(
            reqwest::Client::new(),
            DEFAULT_BASE_URL.into(),
            entry,
            None,
        );
        provider.token_endpoint = server;

        let token = provider.bearer_token().await.unwrap();
        assert_eq!(token, "fresh-access");

        let captured = request.await.unwrap();
        assert!(captured.body.contains("grant_type=refresh_token"));
        assert!(captured.body.contains("refresh_token=old-refresh"));
    }

    /// Verifies a static-token provider never contacts the token endpoint when no refresh token exists.
    #[tokio::test]
    async fn maybe_refresh_is_noop_without_refresh_token() {
        let provider = OpenAICodexProvider::new(
            reqwest::Client::new(),
            DEFAULT_BASE_URL.into(),
            "static-access".into(),
        );

        let token = provider.bearer_token().await.unwrap();

        assert_eq!(token, "static-access");
    }

    /// Verifies that the real send_streaming method emits the adapted StreamEvent sequence on success.
    #[tokio::test]
    async fn send_streaming_emits_adapted_events_for_successful_response() {
        let (server, request) = spawn_token_server(
            "200 OK",
            r#"{"id":"resp_stream","output":[{"type":"message","content":[{"type":"output_text","text":"Checking files"}]},{"type":"function_call","call_id":"call_1","name":"bash","arguments":"{\"command\":\"ls\"}"}],"usage":{"input_tokens":11,"output_tokens":5}}"#,
        )
        .await;
        let provider = OpenAICodexProvider::new(
            reqwest::Client::new(),
            server.clone(),
            "test-access-token".into(),
        );
        let request_body = ChatRequest {
            model: "codex-mini-latest".into(),
            messages: vec![ChatMessage {
                role: Role::User,
                content: vec![ContentBlock::Text {
                    text: "list files".into(),
                }],
            }],
            max_tokens: 64,
            system: None,
            tools: None,
            stream: true,
        };

        let mut stream = provider.send_streaming(&request_body);
        let mut events = Vec::new();
        while let Some(event) = stream.next().await {
            events.push(event.expect("successful stream event"));
        }
        let captured = request.await.unwrap();

        assert_eq!(captured.request_line, "POST /responses HTTP/1.1");
        assert_eq!(events.len(), 5);
        assert!(matches!(
            &events[0],
            StreamEvent::ContentDelta(delta) if delta == "Checking files"
        ));
        assert!(matches!(
            &events[1],
            StreamEvent::ToolUseStart { id, name } if id == "call_1" && name == "bash"
        ));
        assert!(matches!(
            &events[2],
            StreamEvent::ToolUseInputDelta(delta) if delta == "{\"command\":\"ls\"}"
        ));
        assert!(matches!(
            &events[3],
            StreamEvent::MessageStop(reason) if reason == &StopReason::ToolUse
        ));
        assert!(matches!(
            &events[4],
            StreamEvent::Usage(Usage { input_tokens, output_tokens, .. })
                if input_tokens == &11 && output_tokens == &5
        ));
    }

    /// Responses usage with `input_tokens_details.cached_tokens` populates cache_read.
    #[test]
    fn parse_responses_usage_captures_cached_tokens() {
        let value = serde_json::json!({
            "input_tokens": 120,
            "output_tokens": 30,
            "input_tokens_details": { "cached_tokens": 90 }
        });
        let usage = parse_responses_usage(Some(&value));
        assert_eq!(usage.input_tokens, 120);
        assert_eq!(usage.output_tokens, 30);
        assert_eq!(usage.cache_read_tokens, 90);
        assert_eq!(usage.cache_write_tokens, 0);
    }

    /// Responses usage without `input_tokens_details` yields zero cache tokens.
    #[test]
    fn parse_responses_usage_without_details_is_zero_cache() {
        let value = serde_json::json!({
            "input_tokens": 50,
            "output_tokens": 10
        });
        let usage = parse_responses_usage(Some(&value));
        assert_eq!(usage.input_tokens, 50);
        assert_eq!(usage.cache_read_tokens, 0);
        assert_eq!(usage.cache_write_tokens, 0);
    }

    /// Verifies that the real send_streaming method surfaces send failures through the returned stream.
    #[tokio::test]
    async fn send_streaming_propagates_send_failures() {
        let (server, request) =
            spawn_token_server("500 Internal Server Error", r#"{"error":"backend failed"}"#).await;
        let provider = OpenAICodexProvider::new(
            reqwest::Client::new(),
            server.clone(),
            "test-access-token".into(),
        );
        let request_body = ChatRequest {
            model: "codex-mini-latest".into(),
            messages: vec![ChatMessage {
                role: Role::User,
                content: vec![ContentBlock::Text {
                    text: "list files".into(),
                }],
            }],
            max_tokens: 64,
            system: None,
            tools: None,
            stream: true,
        };

        let mut stream = provider.send_streaming(&request_body);
        let error = stream
            .next()
            .await
            .expect("error event")
            .unwrap_err()
            .to_string();
        let captured = request.await.unwrap();

        assert_eq!(captured.request_line, "POST /responses HTTP/1.1");
        assert!(error.contains("500"));
        assert!(error.contains("backend failed"));
        assert!(stream.next().await.is_none());
    }
}
