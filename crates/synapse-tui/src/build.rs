//! Construct the shared runtime dependencies for the TUI.
//!
//! Provider selection mirrors synapse-cli: read `provider` from
//! `~/.synapse/config.json` (or the `SYNAPSE_PROVIDER` env var), then build the
//! matching provider. NO API key is required -- Foundry (host+token in config),
//! Claude Max (subprocess), auto-detected Anthropic OAuth, Ollama, proxy, etc.
//! all work from configuration.
//!
//! NOTE: this resolution duplicates synapse-cli's provider match; lifting a
//! shared resolver into synapse-core is a tracked follow-up.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use synapse_core::cost::PricingTable;
use synapse_core::system_prompt::SystemPromptBuilder;
use synapse_core::types::AgentConfig;
use synapse_provider::{Provider, ProviderConfig, create_provider};
use synapse_session::SessionStore;
use synapse_tools::{ToolRegistry, ToolRegistryExecutor, default_tools};

/// Adapts `Box<dyn Provider>` (from `create_provider`) into something `Arc`-able
/// as `dyn Provider + Send + Sync`. Needed because `dyn Provider` and
/// `dyn Provider + Send + Sync` are distinct trait-object types even though
/// `Provider: Send + Sync`. Mirrors the same newtype in synapse-cli.
struct ProviderWrapper(Box<dyn Provider>);

#[async_trait::async_trait]
impl Provider for ProviderWrapper {
    /// Report the provider name from the wrapped implementation.
    fn name(&self) -> &str {
        self.0.name()
    }

    /// Delegate one-shot chat to the wrapped provider.
    async fn send(
        &self,
        req: &synapse_provider::ChatRequest,
    ) -> Result<synapse_provider::ChatResponse> {
        self.0.send(req).await
    }

    /// Delegate streaming chat to the wrapped provider.
    fn send_streaming(
        &self,
        req: &synapse_provider::ChatRequest,
    ) -> std::pin::Pin<Box<dyn futures::Stream<Item = Result<synapse_provider::StreamEvent>> + Send>>
    {
        self.0.send_streaming(req)
    }
}

/// Bundle of everything `SessionManager::new` needs.
pub struct Runtime {
    /// Provider wrapped for `Arc<dyn Provider + Send + Sync>` compatibility.
    pub provider: Arc<dyn Provider + Send + Sync>,
    /// Registered tool implementations shared across all sessions.
    pub tools: Arc<ToolRegistry>,
    /// Token pricing table used for cost telemetry.
    pub pricing: Arc<PricingTable>,
    /// Persistent session store (None if the default path could not be opened).
    pub store: Option<Arc<SessionStore>>,
    /// Template config cloned into each new session by `SessionManager::spawn`.
    pub base_config: AgentConfig,
}

/// Load a value from an env var, then fall back to `~/.synapse/config.json`.
/// Mirrors synapse-cli's `config_key`.
fn config_key(env_var: &str, json_field: &str) -> String {
    if let Ok(val) = std::env::var(env_var)
        && !val.is_empty()
    {
        return val;
    }
    let config_path = dirs::home_dir()
        .map(|h| h.join(".synapse").join("config.json"))
        .unwrap_or_default();
    if let Ok(data) = std::fs::read_to_string(&config_path)
        && let Ok(v) = serde_json::from_str::<serde_json::Value>(&data)
        && let Some(s) = v.get(json_field).and_then(|x| x.as_str())
    {
        return s.to_owned();
    }
    String::new()
}

/// Resolve the provider id from config/env, canonicalizing aliases. Defaults to
/// `anthropic` (auto-detected OAuth, no explicit key) when nothing is set.
fn resolve_provider_name() -> String {
    let name = config_key("SYNAPSE_PROVIDER", "provider");
    let name = if name.is_empty() {
        "anthropic".to_string()
    } else {
        name
    };
    match name.as_str() {
        "codex" => "openai-codex".to_string(),
        "zen" | "opencode" => "opencode-zen".to_string(),
        _ => name,
    }
}

/// Resolve the model from config/env, defaulting to a sane Anthropic model.
fn resolve_model() -> String {
    let m = config_key("SYNAPSE_MODEL", "model");
    if m.is_empty() {
        "claude-sonnet-4-6".to_string()
    } else {
        m
    }
}

/// Read Foundry host + token from config/env, erroring if either is missing.
fn foundry_creds() -> Result<(String, String)> {
    let host = config_key("SYNAPSE_FOUNDRY_HOST", "foundry_host");
    let token = config_key("SYNAPSE_FOUNDRY_TOKEN", "foundry_token");
    if host.is_empty() || token.is_empty() {
        anyhow::bail!(
            "foundry provider requires a host and token -- set SYNAPSE_FOUNDRY_HOST + \
             SYNAPSE_FOUNDRY_TOKEN, or \"foundry_host\" + \"foundry_token\" in ~/.synapse/config.json"
        );
    }
    Ok((host, token))
}

/// Build a `ProviderConfig` for `name`, reading credentials from config/env.
/// Returns `Err` (never exits) on missing credentials or an unknown provider.
fn provider_config_for(
    name: &str,
    tools: &Arc<ToolRegistry>,
    cwd: &Path,
) -> Result<ProviderConfig> {
    let cfg = match name {
        "anthropic" => ProviderConfig::AnthropicAuto,
        "claude-max" => {
            // LIMITATION: the provider is shared across all sessions, so this
            // tool executor is bound to the launch directory -- claude-max tool
            // calls do NOT run in each session's own worktree. Foundry/Anthropic
            // providers are unaffected (they do not use a tool executor). Fixing
            // this for claude-max would require per-session provider instances.
            let executor = Arc::new(ToolRegistryExecutor::new(
                Arc::clone(tools),
                cwd.to_path_buf(),
            ));
            // ClaudeMax takes an Option<model>; None lets the provider pick its
            // own default, so we do not reuse resolve_model() (which substitutes
            // a concrete fallback).
            let model = config_key("SYNAPSE_MODEL", "model");
            ProviderConfig::ClaudeMax {
                model: if model.is_empty() { None } else { Some(model) },
                cli_path: None,
                cred_namespace: None,
                cred_key: None,
                tools: executor,
            }
        }
        "ollama" => {
            let v = config_key("SYNAPSE_OLLAMA_URL", "ollama_url");
            ProviderConfig::Ollama {
                base_url: if v.is_empty() { None } else { Some(v) },
            }
        }
        "proxy" => {
            let base_url = config_key("SYNAPSE_PROXY_URL", "openai_base_url");
            let api_key = config_key("SYNAPSE_PROXY_KEY", "openai_api_key");
            if base_url.is_empty() || api_key.is_empty() {
                anyhow::bail!(
                    "proxy provider requires SYNAPSE_PROXY_URL + SYNAPSE_PROXY_KEY (or \
                     openai_base_url / openai_api_key in ~/.synapse/config.json)"
                );
            }
            ProviderConfig::Proxy { base_url, api_key }
        }
        "opencode-zen" => {
            let base_url = {
                let v = config_key("SYNAPSE_OPENCODE_URL", "opencode_zen_url");
                if v.is_empty() { None } else { Some(v) }
            };
            let key = config_key("SYNAPSE_OPENCODE_KEY", "opencode_zen_key");
            if !key.is_empty() {
                ProviderConfig::OpenCodeZen {
                    api_key: key,
                    base_url,
                }
            } else if synapse_provider::opencode_zen::load_subscription_token().is_some() {
                ProviderConfig::OpenCodeZenAuto { base_url }
            } else {
                anyhow::bail!(
                    "opencode-zen has no credential -- log in with `opencode providers`, \
                     set SYNAPSE_OPENCODE_KEY, or set \"opencode_zen_key\" in ~/.synapse/config.json"
                );
            }
        }
        "openai-codex" => {
            let base_url = {
                let v = config_key("SYNAPSE_OPENAI_CODEX_URL", "openai_codex_url");
                if v.is_empty() { None } else { Some(v) }
            };
            ProviderConfig::OpenAICodexAuto {
                auth_path: None,
                base_url,
            }
        }
        "azure" => {
            let endpoint = config_key("SYNAPSE_AZURE_ENDPOINT", "azure_endpoint");
            let deployment = config_key("SYNAPSE_AZURE_DEPLOYMENT", "azure_deployment");
            let api_key = config_key("SYNAPSE_AZURE_KEY", "azure_api_key");
            if endpoint.is_empty() || deployment.is_empty() || api_key.is_empty() {
                anyhow::bail!(
                    "azure provider requires endpoint, deployment, and api_key (env \
                     SYNAPSE_AZURE_* or azure_* fields in ~/.synapse/config.json)"
                );
            }
            let api_version = {
                let v = config_key("SYNAPSE_AZURE_API_VERSION", "azure_api_version");
                if v.is_empty() { None } else { Some(v) }
            };
            ProviderConfig::Azure {
                endpoint,
                deployment,
                api_key,
                api_version,
            }
        }
        "foundry-openai" => {
            let (host, token) = foundry_creds()?;
            ProviderConfig::FoundryOpenAI { host, token }
        }
        "foundry-anthropic" => {
            let (host, token) = foundry_creds()?;
            ProviderConfig::FoundryAnthropic { host, token }
        }
        other => anyhow::bail!(
            "unknown provider {other:?} -- set \"provider\" in ~/.synapse/config.json \
             (anthropic, claude-max, foundry-anthropic, foundry-openai, ollama, proxy, \
             opencode-zen, openai-codex, azure)"
        ),
    };
    Ok(cfg)
}

/// Build the runtime by resolving the provider from config/env (no API key
/// required). Errors only if the configured provider cannot be constructed
/// (e.g. missing Foundry credentials).
pub fn build() -> Result<Runtime> {
    let cwd = std::env::current_dir()?;
    let tools = Arc::new(default_tools());
    let name = resolve_provider_name();
    let model = resolve_model();
    let cfg = provider_config_for(&name, &tools, &cwd)
        .with_context(|| format!("resolving provider {name:?}"))?;
    let provider: Arc<dyn Provider + Send + Sync> = Arc::new(ProviderWrapper(
        create_provider(cfg).with_context(|| format!("constructing provider {name:?}"))?,
    ));
    Ok(build_runtime(provider, tools, model, cwd))
}

/// Assemble the Runtime from explicit parts. Separated from `build` so tests can
/// exercise it without touching config/env or the network.
fn build_runtime(
    provider: Arc<dyn Provider + Send + Sync>,
    tools: Arc<ToolRegistry>,
    model: String,
    cwd: PathBuf,
) -> Runtime {
    let pricing = Arc::new(PricingTable::load());
    let store = SessionStore::open_default().ok().map(Arc::new);
    let system_prompt = SystemPromptBuilder::with_default_base().render();
    let base_config = AgentConfig {
        model,
        system_prompt,
        // Template default only -- each SessionManager::spawn overrides cwd with
        // the session's own (worktree) directory.
        cwd,
        // Higher than the CLI defaults (20 turns / 4000 tool tokens): TUI
        // sessions are long-lived and multi-step.
        max_turns: 40,
        max_tokens: 8192,
        session_store: store.clone(),
        session_id: None,
        depth: 0,
        compression: None,
        router: None,
        max_tool_result_tokens: 8000,
        tool_gate: None,
        hooks: None,
    };
    Runtime {
        provider,
        tools,
        pricing,
        store,
        base_config,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal stub provider used only to exercise `build_runtime`.
    struct P;

    #[async_trait::async_trait]
    impl Provider for P {
        /// Return a fixed name for the stub.
        fn name(&self) -> &str {
            "stub"
        }

        /// Not called in tests -- always panics.
        async fn send(
            &self,
            _r: &synapse_provider::ChatRequest,
        ) -> Result<synapse_provider::ChatResponse> {
            unreachable!()
        }

        /// Returns an immediately-empty stream.
        fn send_streaming(
            &self,
            _r: &synapse_provider::ChatRequest,
        ) -> std::pin::Pin<
            Box<dyn futures::Stream<Item = Result<synapse_provider::StreamEvent>> + Send>,
        > {
            Box::pin(futures::stream::empty())
        }
    }

    #[test]
    fn build_runtime_sets_defaults() {
        let rt = build_runtime(
            Arc::new(P),
            Arc::new(ToolRegistry::new()),
            "my-model".into(),
            PathBuf::from("/tmp"),
        );
        assert_eq!(rt.base_config.model, "my-model");
        assert_eq!(rt.base_config.max_turns, 40);
        assert_eq!(rt.base_config.max_tokens, 8192);
        assert_eq!(rt.base_config.max_tool_result_tokens, 8000);
    }

    #[test]
    fn unknown_provider_errors() {
        let tools = Arc::new(ToolRegistry::new());
        assert!(provider_config_for("totally-unknown", &tools, Path::new("/tmp")).is_err());
    }

    #[test]
    fn anthropic_and_ollama_need_no_credentials() {
        let tools = Arc::new(ToolRegistry::new());
        assert!(matches!(
            provider_config_for("anthropic", &tools, Path::new("/tmp")),
            Ok(ProviderConfig::AnthropicAuto)
        ));
        assert!(matches!(
            provider_config_for("ollama", &tools, Path::new("/tmp")),
            Ok(ProviderConfig::Ollama { .. })
        ));
    }

    #[test]
    fn claude_max_builds_with_tool_executor() {
        let tools = Arc::new(ToolRegistry::new());
        assert!(matches!(
            provider_config_for("claude-max", &tools, Path::new("/tmp")),
            Ok(ProviderConfig::ClaudeMax { .. })
        ));
    }
}
