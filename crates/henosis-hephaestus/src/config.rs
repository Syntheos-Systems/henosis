//! Runtime configuration loaded from environment variables. Every field has a
//! documented default so the service can start with minimal configuration.

use std::net::SocketAddr;
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

/// Implements the behavior exposed by ProviderKind.
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
    /// phylaxd slot holding the Kleos bearer token.
    pub kleos_token_slot: String,
    /// Base URL of the Chiasm coordination service.
    pub chiasm_url: String,
    /// Agent name reported to Chiasm when creating tasks.
    pub chiasm_agent: String,
    /// Default project name for Chiasm tasks.
    pub chiasm_project: String,
    /// Optional phylaxd slot for Chiasm auth (None = unauthenticated).
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
    /// because Hermes dispatches in-process. Field reads `HERMES_URL` from
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
    /// Path to the Crucible SQLite database.
    pub crucible_db: PathBuf,
    /// Whether Crucible specification and verification gates are active.
    pub crucible_enabled: bool,
    /// Whether the `cred` CLI is available for credential lookups.
    pub cred_enabled: bool,
    /// Which provider implementation to build via `providers::build_provider`.
    /// Set by `HEPHAESTUS_PROVIDER`; defaults to Anthropic.
    pub provider_kind: ProviderKind,
    /// Override URL for the active provider. For Anthropic this replaces the
    /// default Messages endpoint; for OpenAI-compat this is the base URL of
    /// the upstream service (without `/chat/completions`).
    pub provider_url: Option<String>,
    /// phylaxd slot containing the API key for the active provider. Only used
    /// by OpenAI-compat; Anthropic resolves credentials through the existing
    /// `ProviderChain` regardless of this setting.
    pub provider_key_slot: Option<String>,
    /// Static API key for the active provider (env-only path). Same scope as
    /// `provider_key_slot` -- one or the other resolves the OpenAI-compat key
    /// at startup, with the env var winning if both are set.
    pub provider_api_key: Option<String>,
}

/// Validated network and authentication settings for the standalone server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerConfig {
    /// Socket address that the standalone server may bind.
    pub listen_addr: SocketAddr,
    /// Dedicated Bearer token protecting task-control routes.
    pub api_token: String,
}

/// Builds and validates the standalone server boundary.
impl ServerConfig {
    /// Load standalone settings from the environment and reject unsafe values.
    pub fn from_env(port: u16, provider_api_key: Option<&str>) -> Result<Self, String> {
        let raw_addr =
            std::env::var("HEPHAESTUS_LISTEN_ADDR").unwrap_or_else(|_| format!("127.0.0.1:{port}"));
        let api_token = std::env::var("HEPHAESTUS_API_TOKEN")
            .map_err(|_| "HEPHAESTUS_API_TOKEN is required for the standalone server")?;
        let allow_insecure_remote =
            std::env::var("HEPHAESTUS_ALLOW_INSECURE_REMOTE").as_deref() == Ok("1");
        Self::validate(
            &raw_addr,
            api_token,
            allow_insecure_remote,
            provider_api_key,
        )
    }

    /// Validate explicit values so startup and regression tests share one policy.
    fn validate(
        raw_addr: &str,
        api_token: String,
        allow_insecure_remote: bool,
        provider_api_key: Option<&str>,
    ) -> Result<Self, String> {
        let listen_addr = raw_addr
            .parse::<SocketAddr>()
            .map_err(|error| format!("invalid HEPHAESTUS_LISTEN_ADDR '{raw_addr}': {error}"))?;
        if !listen_addr.ip().is_loopback() && !allow_insecure_remote {
            return Err(format!(
                "refusing non-loopback HEPHAESTUS_LISTEN_ADDR {listen_addr}; set HEPHAESTUS_ALLOW_INSECURE_REMOTE=1 only behind a trusted TLS boundary"
            ));
        }
        if !(32..=256).contains(&api_token.len()) {
            return Err("HEPHAESTUS_API_TOKEN must contain 32 to 256 bytes".to_string());
        }
        if !api_token.is_ascii()
            || api_token.trim() != api_token
            || api_token.chars().any(char::is_whitespace)
        {
            return Err(
                "HEPHAESTUS_API_TOKEN must contain only non-whitespace ASCII characters"
                    .to_string(),
            );
        }
        if provider_api_key == Some(api_token.as_str()) {
            return Err(
                "HEPHAESTUS_API_TOKEN must be distinct from HEPHAESTUS_PROVIDER_KEY".to_string(),
            );
        }

        Ok(Self {
            listen_addr,
            api_token,
        })
    }
}

/// Implements the behavior exposed by Config.
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
            kleos_token_slot: env_or("KLEOS_TOKEN_CRED_SLOT", "henosis/kleos-api"),
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
            crucible_db: crucible::db::default_database_path(),
            crucible_enabled: std::env::var("CRUCIBLE_ENABLED")
                .or_else(|_| std::env::var("AGENT_FORGE_ENABLED"))
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

#[cfg(test)]
/// Tests for standalone-server security configuration.
mod server_config_tests {
    use super::*;

    /// Produce a valid service token for boundary tests.
    fn token() -> String {
        "hephaestus-api-token-that-is-at-least-32-bytes".to_string()
    }

    /// Loopback listeners are accepted without a remote acknowledgement.
    #[test]
    fn accepts_loopback_listener() {
        let server = ServerConfig::validate("[::1]:4700", token(), false, None).unwrap();
        assert!(server.listen_addr.ip().is_loopback());
    }

    /// Wildcard listeners fail closed unless the exact acknowledgement is present.
    #[test]
    fn remote_listener_requires_acknowledgement() {
        assert!(ServerConfig::validate("0.0.0.0:4700", token(), false, None).is_err());
        assert!(ServerConfig::validate("0.0.0.0:4700", token(), true, None).is_ok());
    }

    /// Weak, oversized, non-ASCII, whitespace-bearing, and reused tokens are rejected.
    #[test]
    fn rejects_unsafe_service_tokens() {
        assert!(ServerConfig::validate("127.0.0.1:4700", "short".into(), false, None).is_err());
        assert!(ServerConfig::validate("127.0.0.1:4700", "x".repeat(257), false, None).is_err());
        assert!(
            ServerConfig::validate(
                "127.0.0.1:4700",
                "token with whitespace that is long enough".into(),
                false,
                None,
            )
            .is_err()
        );
        assert!(ServerConfig::validate("127.0.0.1:4700", "é".repeat(32), false, None).is_err());
        let reused = token();
        assert!(
            ServerConfig::validate("127.0.0.1:4700", reused.clone(), false, Some(&reused),)
                .is_err()
        );
    }
}
