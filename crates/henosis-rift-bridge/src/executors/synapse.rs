//! Factory for constructing a synapse-core `SynapseExecutor` from bridge config.
//!
//! Maps bridge-level config (provider name, host, token, model) into
//! synapse-provider's `ProviderConfig` and synapse-core's `SynapseExecutor`.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{bail, Context, Result};

use synapse_core::executors::SynapseExecutor;
use synapse_core::hooks::HookConfig;
use synapse_core::types::AgentConfig as SynapseAgentConfig;
use synapse_provider::{create_provider, ProviderConfig, ToolExecutor};
use synapse_tools::{default_tools, ToolRegistryExecutor};

/// Build a `SynapseExecutor` from bridge-level configuration values.
///
/// This is the bridge between TOML config and the synapse-core executor.
/// Provider construction, tool registry setup, and base config assembly
/// all happen here.
#[allow(clippy::too_many_arguments)]
pub fn build_synapse_executor(
    provider_type: &str,
    model: Option<String>,
    host: Option<String>,
    token: Option<String>,
    api_key: Option<String>,
    max_tokens: Option<u32>,
    max_turns: Option<usize>,
    cwd: Option<PathBuf>,
) -> Result<SynapseExecutor> {
    let working_dir = match cwd {
        Some(path) => path,
        None => std::env::current_dir().context("resolving the default task root")?,
    };
    let model_str = model.unwrap_or_else(|| "claude-sonnet-4-6".to_string());

    // Build tool registry for execution mode.
    let tools = Arc::new(default_tools());

    // Construct the provider from config.
    let provider_config = match provider_type {
        "foundry-anthropic" => {
            let host = host.context("foundry-anthropic provider requires 'host'")?;
            let token = token.context("foundry-anthropic provider requires 'token'")?;
            ProviderConfig::FoundryAnthropic { host, token }
        }
        "foundry-openai" => {
            let host = host.context("foundry-openai provider requires 'host'")?;
            let token = token.context("foundry-openai provider requires 'token'")?;
            ProviderConfig::FoundryOpenAI { host, token }
        }
        "claude-max" | "claude-cli" => {
            // Claude Max owns its MCP loop inside a persistent provider. That
            // loop cannot inherit this executor's per-task Pistis gate or
            // worktree safely, so keep it text-only until providers are scoped
            // to one authorized task.
            let tool_executor: Arc<dyn ToolExecutor> = Arc::new(ToolRegistryExecutor::disabled());
            ProviderConfig::ClaudeMax {
                model: Some(model_str.clone()),
                cli_path: None,
                cred_namespace: None,
                cred_key: None,
                tools: tool_executor,
            }
        }
        "anthropic" => {
            let key = api_key
                .or_else(|| std::env::var("ANTHROPIC_API_KEY").ok())
                .context("anthropic provider requires 'api_key' or ANTHROPIC_API_KEY env var")?;
            ProviderConfig::Anthropic { api_key: key }
        }
        "anthropic-auto" => ProviderConfig::AnthropicAuto,
        // Generic OpenAI-compatible endpoint (DeepSeek, TEI-fronted models,
        // vLLM, ...): `host` is the base URL, key from config or env. This is
        // the cheap-agent path for live room testing.
        "proxy" | "openai" => {
            let base_url = host.context("proxy provider requires 'host' (the base URL)")?;
            let key = api_key
                .or_else(|| std::env::var("SYNAPSE_PROXY_KEY").ok())
                .context("proxy provider requires 'api_key' or SYNAPSE_PROXY_KEY env var")?;
            ProviderConfig::Proxy {
                base_url,
                api_key: key,
            }
        }
        other => bail!("unknown provider type: {other}"),
    };

    let provider = create_provider(provider_config).context("failed to create LLM provider")?;

    // Wrap in Arc for the executor.
    let provider: Arc<dyn synapse_provider::Provider + Send + Sync> =
        Arc::from(provider as Box<dyn synapse_provider::Provider + Send + Sync>);

    let hooks = Arc::new(HookConfig::default());

    let base_config = SynapseAgentConfig {
        model: model_str,
        system_prompt: String::new(),
        cwd: working_dir,
        max_turns: max_turns.unwrap_or(1),
        max_tokens: max_tokens.unwrap_or(4096),
        session_store: None,
        session_id: None,
        depth: 0,
        compression: None,
        router: None,
        max_tool_result_tokens: 0,
        tool_gate: None,
        hooks: None,
    };

    Ok(SynapseExecutor::new(
        provider,
        tools,
        hooks,
        None,
        base_config,
    ))
}
