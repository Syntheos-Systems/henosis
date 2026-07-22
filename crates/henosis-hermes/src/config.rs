/// Runtime configuration for the Hermes tool gateway, loaded from environment
/// variables with sensible defaults.
#[derive(Debug, Clone)]
pub struct Config {
    /// TCP port Hermes listens on (default 4800, override with `HERMES_PORT`).
    pub port: u16,
    /// Base URL of the phylaxd credential daemon (default `http://127.0.0.1:3100`).
    pub phylaxd_url: String,
    /// Bearer token for authenticating to phylaxd. When absent, OAuth-requiring
    /// adapters return `phylaxd_auth_missing`.
    pub phylaxd_token: Option<String>,
    #[allow(dead_code)]
    /// Base URL of the Kleos memory/event service (unused at runtime; reserved
    /// for future direct Kleos integration).
    pub kleos_url: String,
    #[allow(dead_code)]
    /// Phylaxd slot name for the Kleos API token.
    pub kleos_token_slot: String,
    /// Externally-reachable base URL for this Hermes instance (e.g.
    /// `https://hermes.example.com`). Auto-populates webhook delivery URLs in
    /// registration adapters. `None` when `HERMES_PUBLIC_URL` is unset.
    pub public_url: Option<String>,
}

/// Loads Hermes network, storage, and credential-broker settings from the environment.
impl Config {
    /// Construct a `Config` from environment variables, using defaults for any
    /// unset variables.
    pub fn from_env() -> Self {
        Self {
            port: std::env::var("HERMES_PORT")
                .ok()
                .and_then(|v| v.parse::<u16>().ok())
                .unwrap_or(4800),
            phylaxd_url: env_or("PHYLAXD_URL", "http://127.0.0.1:3100"),
            phylaxd_token: std::env::var("HERMES_PHYLAXD_TOKEN")
                .ok()
                .filter(|s| !s.is_empty()),
            kleos_url: env_or("KLEOS_URL", "http://127.0.0.1:4200"),
            kleos_token_slot: env_or("KLEOS_TOKEN_CRED_SLOT", "henosis/kleos-api"),
            public_url: std::env::var("HERMES_PUBLIC_URL")
                .ok()
                .filter(|s| !s.is_empty()),
        }
    }
}

/// Read an environment variable, returning `default` as a `String` when absent.
fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}
