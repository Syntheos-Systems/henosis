use std::net::SocketAddr;

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
    /// Explicit standalone-server bind address from `HERMES_LISTEN_ADDR`.
    /// When absent, the server derives `127.0.0.1:<port>`.
    pub listen_addr: Option<String>,
    /// Dedicated Bearer token accepted by protected standalone HTTP routes.
    pub api_token: Option<String>,
    /// Exact acknowledgement required before the standalone server binds to a
    /// non-loopback address without providing its own TLS termination.
    pub allow_insecure_remote: bool,
}

/// Validated security-sensitive settings used only by the standalone server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerConfig {
    /// Socket address that the standalone Hermes server may bind.
    pub listen_addr: SocketAddr,
    /// Dedicated Bearer token protecting every non-public route.
    pub api_token: String,
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
            listen_addr: std::env::var("HERMES_LISTEN_ADDR")
                .ok()
                .filter(|s| !s.is_empty()),
            api_token: std::env::var("HERMES_API_TOKEN")
                .ok()
                .filter(|s| !s.is_empty()),
            allow_insecure_remote: std::env::var("HERMES_ALLOW_INSECURE_REMOTE").as_deref()
                == Ok("1"),
        }
    }

    /// Validate the standalone server's bind and inbound authentication
    /// boundary before any socket is opened.
    pub fn validate_server(&self) -> Result<ServerConfig, String> {
        let raw_addr = self
            .listen_addr
            .clone()
            .unwrap_or_else(|| format!("127.0.0.1:{}", self.port));
        let listen_addr = raw_addr
            .parse::<SocketAddr>()
            .map_err(|error| format!("invalid HERMES_LISTEN_ADDR '{raw_addr}': {error}"))?;

        if !listen_addr.ip().is_loopback() && !self.allow_insecure_remote {
            return Err(format!(
                "refusing non-loopback HERMES_LISTEN_ADDR {listen_addr}; set HERMES_ALLOW_INSECURE_REMOTE=1 only behind a trusted TLS boundary"
            ));
        }

        let api_token = self
            .api_token
            .as_deref()
            .ok_or_else(|| "HERMES_API_TOKEN is required for the standalone server".to_string())?;
        if api_token.len() < 32 {
            return Err("HERMES_API_TOKEN must contain at least 32 bytes".to_string());
        }
        if api_token.trim() != api_token || api_token.chars().any(char::is_whitespace) {
            return Err("HERMES_API_TOKEN must not contain whitespace".to_string());
        }
        if self.phylaxd_token.as_deref() == Some(api_token) {
            return Err("HERMES_API_TOKEN must be distinct from HERMES_PHYLAXD_TOKEN".to_string());
        }

        Ok(ServerConfig {
            listen_addr,
            api_token: api_token.to_string(),
        })
    }
}

/// Read an environment variable, returning `default` as a `String` when absent.
fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

#[cfg(test)]
/// Tests for standalone-server configuration validation.
mod tests {
    use super::*;

    /// Build a valid base configuration for pure validation tests.
    fn valid_config() -> Config {
        Config {
            port: 4800,
            phylaxd_url: "http://127.0.0.1:3100".to_string(),
            phylaxd_token: Some("phylaxd-token-that-is-long-and-distinct".to_string()),
            kleos_url: "http://127.0.0.1:4200".to_string(),
            kleos_token_slot: "henosis/kleos-api".to_string(),
            public_url: None,
            listen_addr: None,
            api_token: Some("hermes-api-token-that-is-at-least-32-bytes".to_string()),
            allow_insecure_remote: false,
        }
    }

    /// The default standalone listener remains confined to loopback.
    #[test]
    fn defaults_to_loopback() {
        let server = valid_config()
            .validate_server()
            .expect("valid server config");
        assert!(server.listen_addr.ip().is_loopback());
        assert_eq!(server.listen_addr.port(), 4800);
    }

    /// A wildcard listener fails closed without explicit acknowledgement.
    #[test]
    fn rejects_unacknowledged_remote_bind() {
        let mut config = valid_config();
        config.listen_addr = Some("0.0.0.0:4800".to_string());
        let error = config.validate_server().expect_err("remote bind must fail");
        assert!(error.contains("HERMES_ALLOW_INSECURE_REMOTE=1"));
    }

    /// A remote listener is accepted only after the exact acknowledgement.
    #[test]
    fn accepts_acknowledged_remote_bind() {
        let mut config = valid_config();
        config.listen_addr = Some("[::]:4800".to_string());
        config.allow_insecure_remote = true;
        let server = config.validate_server().expect("acknowledged remote bind");
        assert_eq!(server.listen_addr, "[::]:4800".parse().unwrap());
    }

    /// Missing, weak, or reused inbound credentials are rejected.
    #[test]
    fn rejects_invalid_api_tokens() {
        let mut config = valid_config();
        config.api_token = None;
        assert!(config.validate_server().is_err());

        config.api_token = Some("short".to_string());
        assert!(config.validate_server().is_err());

        config.api_token = config.phylaxd_token.clone();
        assert!(config.validate_server().is_err());
    }
}
