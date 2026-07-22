//! synapse-provider: LLM provider abstraction for Anthropic, Ollama, and proxies.

use anyhow::Result;
use std::path::PathBuf;
use std::sync::Arc;

pub mod anthropic;
pub mod azure;
pub mod claude_max;
pub mod ollama;
pub mod openai_codex;
pub mod opencode_zen;
pub mod proxy;
pub mod streaming;
pub mod types;

pub use claude_max::{ClaudeMaxProvider, ToolExecutionResult, ToolExecutor};
pub use types::{
    ChatMessage, ChatRequest, ChatResponse, ContentBlock, Provider, Role, StopReason, StreamEvent,
    Usage,
};

/// Configuration for selecting and constructing a provider.
pub enum ProviderConfig {
    /// Anthropic with explicit API key or OAuth token (auto-detected from prefix).
    Anthropic { api_key: String },
    /// Anthropic auto-loaded from OpenCode's auth.json (OAuth).
    AnthropicAuto,
    /// OpenAI-compatible proxy with custom base URL and API key.
    Proxy { base_url: String, api_key: String },
    /// Ollama local model. No auth, defaults to localhost:11434.
    Ollama { base_url: Option<String> },
    /// OpenCode Zen aggregator. OpenAI-compatible; defaults to canonical base URL
    /// when `base_url` is None.
    OpenCodeZen {
        api_key: String,
        base_url: Option<String>,
    },
    /// OpenCode Zen with token auto-loaded from the OpenCode CLI's auth.json
    /// (written by `opencode providers` login). Use this when the user has logged
    /// into a Zen subscription rather than configuring a raw API key.
    OpenCodeZenAuto { base_url: Option<String> },
    /// OpenAI Codex with an explicit browser OAuth access token.
    OpenAICodex {
        access_token: String,
        base_url: Option<String>,
    },
    /// OpenAI Codex with auth auto-loaded from Synapse's auth.json.
    OpenAICodexAuto {
        auth_path: Option<PathBuf>,
        base_url: Option<String>,
    },
    /// Azure OpenAI deployment with api-key auth and Azure-specific URL shape.
    Azure {
        endpoint: String,
        deployment: String,
        api_key: String,
        api_version: Option<String>,
    },
    /// Palantir Foundry AIP proxy -- OpenAI chat completions endpoint.
    FoundryOpenAI { host: String, token: String },
    /// Palantir Foundry AIP proxy -- Anthropic messages endpoint.
    FoundryAnthropic { host: String, token: String },
    /// Claude Max subscription via the `claude` CLI subprocess.
    /// Routes requests through the official binary for Max billing.
    ClaudeMax {
        /// Model identifier. Default: claude-sonnet-4-6
        model: Option<String>,
        /// Path to the claude binary. Default: resolved via $PATH
        cli_path: Option<PathBuf>,
        /// Cred namespace for the OAuth token. Default: "anthropic"
        cred_namespace: Option<String>,
        /// Cred key for the OAuth token. Default: "claude-oauth-token"
        cred_key: Option<String>,
        /// Tool executor providing schemas and execution to the MCP bridge.
        tools: Arc<dyn ToolExecutor>,
    },
}

/// Build a provider from a config.
pub fn create_provider(config: ProviderConfig) -> Result<Box<dyn Provider>> {
    let client = reqwest::Client::builder()
        .user_agent("synapse/0.1.0")
        .build()?;

    match config {
        ProviderConfig::Anthropic { api_key } => {
            Ok(Box::new(anthropic::AnthropicProvider::new(client, api_key)))
        }
        ProviderConfig::AnthropicAuto => {
            match anthropic::load_opencode_anthropic_token() {
                Some((access, refresh, expires)) => Ok(Box::new(
                    anthropic::AnthropicProvider::new_oauth(client, access, Some(refresh), expires),
                )),
                None => {
                    // Fallback: check ANTHROPIC_API_KEY env var
                    let key = std::env::var("ANTHROPIC_API_KEY")
                        .map_err(|_| anyhow::anyhow!(
                            "no Anthropic OAuth token in OpenCode auth.json and ANTHROPIC_API_KEY not set"
                        ))?;
                    Ok(Box::new(anthropic::AnthropicProvider::new(client, key)))
                }
            }
        }
        ProviderConfig::Proxy { base_url, api_key } => Ok(Box::new(proxy::ProxyProvider::new(
            client, base_url, api_key,
        ))),
        ProviderConfig::Ollama { base_url } => match base_url {
            Some(url) => Ok(Box::new(ollama::OllamaProvider::with_url(client, url))),
            None => Ok(Box::new(ollama::OllamaProvider::new(client))),
        },
        ProviderConfig::OpenCodeZen { api_key, base_url } => {
            let url = base_url.unwrap_or_else(|| opencode_zen::DEFAULT_BASE_URL.to_string());
            Ok(Box::new(
                proxy::ProxyProvider::new(client, url, api_key).with_name("opencode-zen"),
            ))
        }
        ProviderConfig::OpenCodeZenAuto { base_url } => {
            let api_key = opencode_zen::load_subscription_token().ok_or_else(|| {
                anyhow::anyhow!(
                    "no OpenCode Zen token in auth.json -- run `opencode providers` to log in"
                )
            })?;
            let url = base_url.unwrap_or_else(|| opencode_zen::DEFAULT_BASE_URL.to_string());
            Ok(Box::new(
                proxy::ProxyProvider::new(client, url, api_key).with_name("opencode-zen"),
            ))
        }
        ProviderConfig::OpenAICodex {
            access_token,
            base_url,
        } => {
            let url = base_url.unwrap_or_else(|| openai_codex::DEFAULT_BASE_URL.to_string());
            Ok(Box::new(openai_codex::OpenAICodexProvider::new(
                client,
                url,
                access_token,
            )))
        }
        ProviderConfig::OpenAICodexAuto {
            auth_path,
            base_url,
        } => {
            let path = auth_path.unwrap_or_else(openai_codex::CodexAuth::default_path);
            let entry = openai_codex::CodexAuth::from_path(&path).entry_for_runtime()?;
            let url = base_url.unwrap_or_else(|| entry.base_url.clone());
            Ok(Box::new(openai_codex::OpenAICodexProvider::from_entry(
                client,
                url,
                entry,
                Some(path),
            )))
        }
        ProviderConfig::Azure {
            endpoint,
            deployment,
            api_key,
            api_version,
        } => {
            let mut p = azure::AzureProvider::new(client, endpoint, deployment, api_key);
            if let Some(v) = api_version {
                p = p.with_api_version(v);
            }
            Ok(Box::new(p))
        }
        ProviderConfig::FoundryOpenAI { host, token } => {
            let base_url = format!("https://{host}/api/v2/llm/proxy/openai/v1");
            Ok(Box::new(
                proxy::ProxyProvider::new(client, base_url, token)
                    .with_name("foundry-openai")
                    // Foundry's Azure OpenAI backend rejects `max_tokens` for
                    // gpt-5-class models; emit `max_completion_tokens` instead.
                    .with_max_completion_tokens(true),
            ))
        }
        ProviderConfig::FoundryAnthropic { host, token } => {
            let base_url = format!("https://{host}/api/v2/llm/proxy/anthropic/v1");
            Ok(Box::new(anthropic::AnthropicProvider::new_foundry(
                client, base_url, token,
            )))
        }
        ProviderConfig::ClaudeMax {
            model,
            cli_path,
            cred_namespace,
            cred_key,
            tools,
        } => Ok(Box::new(claude_max::ClaudeMaxProvider::new(
            model,
            cli_path,
            cred_namespace,
            cred_key,
            tools,
        ))),
    }
}
