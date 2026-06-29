//! Runtime configuration loaded from environment variables. Every field has a
//! documented default so the service can start with minimal configuration.

use std::path::PathBuf;
use std::time::Duration;

/// Deployment environment. Controls which credential providers are active.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeployEnv {
    /// Local / CI environment. Enables the credentials-file auth path.
    Dev,
    /// Production environment. Requires Plutus for tenant auth.
    Prod,
}

/// Which provider implementation the factory should build. Maps directly to
/// the `HEPHAESTUS_PROVIDER` env var; defaults to Anthropic to preserve
/// pre-refactor behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderKind {
    /// Hephaestus's claude-code OAuth path against the Anthropic Messages API.
    Anthropic,
    /// Generic OpenAI-compatible endpoint (OpenAI, Ollama, Azure, OpenRouter, ...).
    OpenAiCompat,
}

impl ProviderKind {
    /// Parse the `HEPHAESTUS_PROVIDER` env value. Named `parse_env` (rather
    /// than `from_str`) so it does not collide with the `std::str::FromStr`
    /// trait. Anything other than the known strings returns None; the
    /// caller decides whether to fall back to a default or hard-fail.
    pub fn parse_env(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "anthropic" => Some(Self::Anthropic),
            "openai" | "openai-compat" | "ollama" | "azure" | "openrouter" => {
                Some(Self::OpenAiCompat)
            }
            _ => None,
        }
    }
}

/// Full runtime configuration for the Hephaestus executor. Constructed once
/// at startup from environment variables via `Config::from_env`.
#[derive(Debug, Clone)]
pub struct Config {
    /// TCP port the HTTP server binds on. Default 4700.
    pub port: u16,
    /// Deployment environment (dev vs prod).
    pub env: DeployEnv,
    /// Base URL of the Kleos memory service.
    pub kleos_url: String,
    /// credd slot holding the Kleos bearer token.
    pub kleos_token_slot: String,
    /// Base URL of the Chiasm coordination service.
    pub chiasm_url: String,
    /// Agent name reported to Chiasm when creating tasks.
    pub chiasm_agent: String,
    /// Default project name for Chiasm tasks.
    pub chiasm_project: String,
    /// Optional credd slot for Chiasm auth (None = unauthenticated).
    pub chiasm_token_slot: Option<String>,
    /// Base URL of the Axon event bus (may be the same host as Kleos).
    pub axon_url: String,
    /// Optional base URL of the Plutus tenant credential service.
    pub plutus_url: Option<String>,
    /// Path to the local dev OAuth credentials file.
    pub dev_credentials_path: PathBuf,
    /// Anthropic model id to use for agent calls.
    pub model: String,
    /// Maximum tokens per Anthropic response.
    pub max_tokens: u32,
    /// Timeout for non-LLM HTTP requests (Kleos, Chiasm, Axon).
    pub http_timeout: Duration,
    /// Timeout for LLM HTTP requests.
    pub llm_timeout: Duration,
    /// Base URL of the Eidolon gate service.
    pub eidolon_url: String,
    /// Base URL of the Hermes tool gateway. Retained for backwards compat with
    /// existing Config literals (e.g. tests); no longer used for HTTP dispatch
    /// since story 5.4 wired Hermes in-process. Field reads `HERMES_URL` from
    /// the environment; callers may leave it at the default.
    #[allow(dead_code)]
    pub hermes_url: String,
    /// Full Anthropic Messages API URL (may include `?beta=true`).
    pub anthropic_url: String,
    /// Maximum number of tool-use loop iterations per task.
    pub max_tool_turns: usize,
    /// Podman sandbox execution timeout in seconds.
    pub sandbox_timeout: u64,
    /// Podman container memory limit (e.g. "256m").
    pub sandbox_memory: String,
    /// Path to the `agent-forge` CLI binary.
    pub agent_forge_bin: PathBuf,
    /// Optional path to the agent-forge SQLite database.
    pub agent_forge_db: Option<PathBuf>,
    /// Whether agent-forge integration (spec_task, verify) is active.
    pub agent_forge_enabled: bool,
    /// Whether the `cred` CLI is available for credential lookups.
    pub cred_enabled: bool,
    /// Which provider implementation to build via `providers::build_provider`.
    /// Set by `HEPHAESTUS_PROVIDER`; defaults to Anthropic.
    pub provider_kind: ProviderKind,
    /// Override URL for the active provider. For Anthropic this replaces the
    /// default Messages endpoint; for OpenAI-compat this is the base URL of
    /// the upstream service (without `/chat/completions`).
    pub provider_url: Option<String>,
    /// credd slot containing the API key for the active provider. Only used
    /// by OpenAI-compat; Anthropic resolves credentials through the existing
    /// `ProviderChain` regardless of this setting.
    pub provider_key_slot: Option<String>,
    /// Static API key for the active provider (env-only path). Same scope as
    /// `provider_key_slot` -- one or the other resolves the OpenAI-compat key
    /// at startup, with the env var winning if both are set.
    pub provider_api_key: Option<String>,
}

impl Config {
    /// Load all configuration from environment variables. Every field has a
    /// documented default; the service can start without any env vars set
    /// (it will use localhost defaults and dev auth).
    pub fn from_env() -> Self {
        let port = std::env::var("HEPHAESTUS_PORT")
            .ok()
            .and_then(|v| v.parse::<u16>().ok())
            .unwrap_or(4700);

        let env = match std::env::var("HEPHAESTUS_ENV").as_deref() {
            Ok("dev") => DeployEnv::Dev,
            _ => DeployEnv::Prod,
        };

        let dev_credentials_path = std::env::var("HEPHAESTUS_DEV_CREDENTIALS_PATH")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                dirs::home_dir()
                    .unwrap_or_else(|| PathBuf::from("/"))
                    .join(".claude")
                    .join(".credentials.json")
            });

        Self {
            port,
            env,
            kleos_url: env_or("KLEOS_URL", "http://127.0.0.1:4200"),
            kleos_token_slot: env_or("KLEOS_TOKEN_CRED_SLOT", "engram-rust/claude-code-wsl"),
            chiasm_url: env_or("CHIASM_URL", "http://127.0.0.1:4300"),
            chiasm_agent: env_or("CHIASM_AGENT", "hephaestus"),
            chiasm_project: env_or("CHIASM_PROJECT", "hephaestus-smoke"),
            chiasm_token_slot: std::env::var("CHIASM_TOKEN_CRED_SLOT").ok(),
            axon_url: env_or("AXON_URL", "http://127.0.0.1:4200"),
            plutus_url: std::env::var("PLUTUS_URL").ok(),
            dev_credentials_path,
            model: env_or("HEPHAESTUS_MODEL", "claude-haiku-4-5-20251001"),
            max_tokens: std::env::var("HEPHAESTUS_MAX_TOKENS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(1024),
            http_timeout: Duration::from_secs(10),
            llm_timeout: Duration::from_secs(60),
            eidolon_url: env_or("EIDOLON_URL", "http://127.0.0.1:7700"),
            hermes_url: env_or("HERMES_URL", "http://127.0.0.1:4800"),
            anthropic_url: env_or(
                "ANTHROPIC_URL",
                "https://api.anthropic.com/v1/messages?beta=true",
            ),
            max_tool_turns: std::env::var("HEPHAESTUS_MAX_TOOL_TURNS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(10),
            sandbox_timeout: std::env::var("HEPHAESTUS_SANDBOX_TIMEOUT")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(30),
            sandbox_memory: env_or("HEPHAESTUS_SANDBOX_MEMORY", "256m"),
            agent_forge_bin: std::env::var("AGENT_FORGE_BIN")
                .map(PathBuf::from)
                .unwrap_or_else(|_| PathBuf::from("agent-forge")),
            agent_forge_db: std::env::var("AGENT_FORGE_DB").ok().map(PathBuf::from),
            agent_forge_enabled: std::env::var("AGENT_FORGE_ENABLED")
                .map(|v| v != "0" && !v.eq_ignore_ascii_case("false"))
                .unwrap_or(true),
            cred_enabled: std::env::var("HEPHAESTUS_CRED_ENABLED")
                .map(|v| v != "0" && !v.eq_ignore_ascii_case("false"))
                .unwrap_or(true),
            provider_kind: std::env::var("HEPHAESTUS_PROVIDER")
                .ok()
                .and_then(|v| ProviderKind::parse_env(&v))
                .unwrap_or(ProviderKind::Anthropic),
            provider_url: std::env::var("HEPHAESTUS_PROVIDER_URL").ok(),
            provider_key_slot: std::env::var("HEPHAESTUS_PROVIDER_KEY_SLOT").ok(),
            provider_api_key: std::env::var("HEPHAESTUS_PROVIDER_KEY").ok(),
        }
    }
}

/// Return `std::env::var(key)` or `default` if the variable is absent or
/// empty. Used throughout `Config::from_env` for URL and string fields.
fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}
