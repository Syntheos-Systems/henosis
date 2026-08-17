use axum::http::HeaderValue;
use std::env;
use std::net::SocketAddr;

/// Configuration failures detected before the Rift runtime binds a listener.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// A required environment variable was not present or contained invalid Unicode.
    #[error("{name}: {source}")]
    Environment {
        /// Name of the environment variable that could not be read.
        name: &'static str,
        /// Standard-library reason the environment lookup failed.
        source: env::VarError,
    },
    /// A present setting violated a Rift security or parsing invariant.
    #[error("{0}")]
    Invalid(String),
}

/// Default attachment request ceiling: 25 MiB.
pub const DEFAULT_MAX_UPLOAD_BYTES: usize = 25 * 1024 * 1024;

/// Hard attachment request ceiling: 100 MiB.
pub const MAX_UPLOAD_BYTES_CEILING: usize = 100 * 1024 * 1024;

/// Runtime settings for the standalone Rift HTTP and WebSocket server.
#[derive(Clone)]
pub struct Config {
    /// PostgreSQL connection string for Rift persistence.
    pub database_url: String,
    /// HMAC root for human sessions and domain-separated managed-agent signers.
    pub jwt_secret: String,
    /// Independent HMAC secret used only to validate bridge-issued agent JWTs.
    pub agent_jwt_secret: String,
    /// Dedicated bearer secret for bridge-only provisioning and notification routes.
    pub bridge_secret: String,
    /// Socket address on which the server accepts requests.
    pub listen_addr: String,
    /// Loopback-only socket address for bridge-secret operations.
    pub bridge_listen_addr: String,
    /// Explicit acknowledgement permitting a non-loopback public listener.
    pub allow_remote_listen: bool,
    /// Browser origins allowed to call the Rift HTTP API.
    pub cors_origins: Vec<HeaderValue>,
    /// Directory from which uploaded files are served.
    pub upload_dir: String,
    /// Maximum accepted upload size in bytes.
    pub max_upload_bytes: usize,
}

/// Loads Rift server settings from environment variables.
impl Config {
    /// Resolve the complete runtime configuration as a typed startup result.
    pub fn try_from_env() -> Result<Self, ConfigError> {
        let jwt_secret = required_env("JWT_SECRET")?;
        let agent_jwt_secret = required_env("AGENT_JWT_SECRET")?;
        let bridge_secret = required_env("RIFT_BRIDGE_SECRET")?;
        let listen_addr = env_or_default("LISTEN_ADDR", "127.0.0.1:3200")?;
        let bridge_listen_addr = env_or_default("BRIDGE_LISTEN_ADDR", "127.0.0.1:3201")?;
        let remote_ack = optional_env("RIFT_ALLOW_REMOTE_LISTEN")?;
        let allow_remote_listen =
            parse_remote_listen_ack(remote_ack.as_deref()).map_err(ConfigError::Invalid)?;
        let cors = env_or_default(
            "RIFT_CORS_ORIGINS",
            "http://localhost:5173,http://127.0.0.1:5173,tauri://localhost",
        )?;
        let max_upload = optional_env("MAX_UPLOAD_BYTES")?;
        let config = Self {
            database_url: required_env("DATABASE_URL")?,
            jwt_secret,
            agent_jwt_secret,
            bridge_secret,
            listen_addr,
            bridge_listen_addr,
            allow_remote_listen,
            cors_origins: parse_cors_origins(&cors).map_err(ConfigError::Invalid)?,
            upload_dir: env_or_default("UPLOAD_DIR", "./uploads")?,
            max_upload_bytes: parse_upload_limit(max_upload.as_deref())
                .map_err(ConfigError::Invalid)?,
        };
        config.validate_runtime()?;
        Ok(config)
    }

    /// Resolve the complete runtime configuration or fail on missing security settings.
    pub fn from_env() -> Self {
        Self::try_from_env().expect("Rift runtime configuration is invalid")
    }

    /// Enforce every security and resource invariant on a runnable configuration.
    pub fn validate_runtime(&self) -> Result<(SocketAddr, SocketAddr), ConfigError> {
        validate_secrets(
            &self.jwt_secret,
            &self.agent_jwt_secret,
            &self.bridge_secret,
        )
        .map_err(ConfigError::Invalid)?;
        let listeners = validate_listener_topology(
            &self.listen_addr,
            &self.bridge_listen_addr,
            self.allow_remote_listen,
        )
        .map_err(ConfigError::Invalid)?;
        if self.database_url.trim().is_empty() {
            return Err(ConfigError::Invalid(
                "DATABASE_URL must not be empty".to_string(),
            ));
        }
        if self.cors_origins.is_empty() {
            return Err(ConfigError::Invalid(
                "CORS origin allowlist is empty".to_string(),
            ));
        }
        if self.upload_dir.trim().is_empty() {
            return Err(ConfigError::Invalid(
                "UPLOAD_DIR must not be empty".to_string(),
            ));
        }
        if self.max_upload_bytes == 0 || self.max_upload_bytes > MAX_UPLOAD_BYTES_CEILING {
            return Err(ConfigError::Invalid(format!(
                "MAX_UPLOAD_BYTES must be between 1 and {MAX_UPLOAD_BYTES_CEILING}"
            )));
        }
        Ok(listeners)
    }
}

/// Read one mandatory Unicode environment setting without collapsing failure modes.
fn required_env(name: &'static str) -> Result<String, ConfigError> {
    env::var(name).map_err(|source| ConfigError::Environment { name, source })
}

/// Read one optional Unicode environment value without hiding invalid Unicode.
fn optional_env(name: &'static str) -> Result<Option<String>, ConfigError> {
    match env::var(name) {
        Ok(value) => Ok(Some(value)),
        Err(env::VarError::NotPresent) => Ok(None),
        Err(source) => Err(ConfigError::Environment { name, source }),
    }
}

/// Read one optional Unicode environment value with a deterministic default.
fn env_or_default(name: &'static str, default: &str) -> Result<String, ConfigError> {
    Ok(optional_env(name)?.unwrap_or_else(|| default.to_string()))
}

/// Parse the exact acknowledgement required for non-loopback public ingress.
pub fn parse_remote_listen_ack(value: Option<&str>) -> Result<bool, String> {
    match value.unwrap_or("0") {
        "0" => Ok(false),
        "1" => Ok(true),
        _ => Err("RIFT_ALLOW_REMOTE_LISTEN must be exactly \"0\" or \"1\"".to_string()),
    }
}

/// Validate the two Rift listener trust boundaries before any socket is bound.
pub fn validate_listener_topology(
    listen_addr: &str,
    bridge_listen_addr: &str,
    allow_remote_listen: bool,
) -> Result<(SocketAddr, SocketAddr), String> {
    let public = listen_addr
        .parse::<SocketAddr>()
        .map_err(|error| format!("LISTEN_ADDR must be an IP-literal socket address: {error}"))?;
    let bridge = bridge_listen_addr.parse::<SocketAddr>().map_err(|error| {
        format!("BRIDGE_LISTEN_ADDR must be an IP-literal socket address: {error}")
    })?;
    if public.port() == 0 {
        return Err("LISTEN_ADDR must use an explicit nonzero port".to_string());
    }
    if bridge.port() == 0 {
        return Err("BRIDGE_LISTEN_ADDR must use an explicit nonzero port".to_string());
    }
    if !bridge.ip().is_loopback() {
        return Err("BRIDGE_LISTEN_ADDR must use an IPv4 or IPv6 loopback address".to_string());
    }
    if !public.ip().is_loopback() && !allow_remote_listen {
        return Err("non-loopback LISTEN_ADDR requires RIFT_ALLOW_REMOTE_LISTEN=1".to_string());
    }
    if public == bridge {
        return Err("LISTEN_ADDR and BRIDGE_LISTEN_ADDR must be distinct sockets".to_string());
    }
    Ok((public, bridge))
}

/// Parse an optional attachment limit and enforce the server's allocation ceiling.
pub fn parse_upload_limit(value: Option<&str>) -> Result<usize, String> {
    let Some(value) = value else {
        return Ok(DEFAULT_MAX_UPLOAD_BYTES);
    };
    let parsed = value
        .parse::<usize>()
        .map_err(|error| format!("invalid MAX_UPLOAD_BYTES: {error}"))?;
    if parsed == 0 || parsed > MAX_UPLOAD_BYTES_CEILING {
        return Err(format!(
            "MAX_UPLOAD_BYTES must be between 1 and {MAX_UPLOAD_BYTES_CEILING}"
        ));
    }
    Ok(parsed)
}

/// Reject short or pairwise-reused secrets before Rift exposes either trust boundary.
pub fn validate_secrets(
    jwt_secret: &str,
    agent_jwt_secret: &str,
    bridge_secret: &str,
) -> Result<(), String> {
    validate_secret("JWT_SECRET", jwt_secret)?;
    validate_secret("AGENT_JWT_SECRET", agent_jwt_secret)?;
    validate_secret("RIFT_BRIDGE_SECRET", bridge_secret)?;
    if jwt_secret == agent_jwt_secret
        || jwt_secret == bridge_secret
        || agent_jwt_secret == bridge_secret
    {
        return Err(
            "JWT_SECRET, AGENT_JWT_SECRET, and RIFT_BRIDGE_SECRET must differ pairwise".to_string(),
        );
    }
    Ok(())
}

/// Reject secrets whose size or character set makes transport and comparison ambiguous.
fn validate_secret(name: &str, secret: &str) -> Result<(), String> {
    if !(32..=256).contains(&secret.len()) || !secret.bytes().all(|byte| byte.is_ascii_graphic()) {
        return Err(format!(
            "{name} must contain 32 through 256 printable non-whitespace ASCII bytes"
        ));
    }
    Ok(())
}

/// Parse a comma-separated browser-origin allowlist and reject empty or malformed entries.
pub fn parse_cors_origins(value: &str) -> Result<Vec<HeaderValue>, String> {
    let origins: Result<Vec<_>, _> = value
        .split(',')
        .map(str::trim)
        .filter(|origin| !origin.is_empty())
        .map(|origin| {
            origin
                .parse::<HeaderValue>()
                .map_err(|error| format!("invalid CORS origin {origin:?}: {error}"))
        })
        .collect();
    let origins = origins?;
    if origins.is_empty() {
        return Err("CORS origin allowlist is empty".to_string());
    }
    Ok(origins)
}

#[cfg(test)]
/// Exercises fail-closed parsing for Rift origins and trust-boundary secrets.
mod tests {
    use super::{
        Config, DEFAULT_MAX_UPLOAD_BYTES, MAX_UPLOAD_BYTES_CEILING, parse_cors_origins,
        parse_remote_listen_ack, parse_upload_limit, validate_listener_topology, validate_secrets,
    };

    /// The parser trims and retains each explicitly allowed origin.
    #[test]
    fn cors_origins_parse_as_an_explicit_list() {
        let origins =
            parse_cors_origins("http://localhost:5173, tauri://localhost").expect("valid origins");
        assert_eq!(origins.len(), 2);
        assert_eq!(origins[0], "http://localhost:5173");
    }

    /// Empty lists and header-injection attempts fail closed.
    #[test]
    fn invalid_cors_origins_are_rejected() {
        assert!(parse_cors_origins(" , ").is_err());
        assert!(parse_cors_origins("https://example.com\r\nx-evil: 1").is_err());
    }

    /// Human JWT, agent JWT, and bridge secrets must be strong and pairwise independent.
    #[test]
    fn secrets_must_be_strong_and_distinct() {
        let jwt = "j".repeat(32);
        let agent = "a".repeat(32);
        let bridge = "b".repeat(32);
        assert!(validate_secrets(&jwt, &agent, &bridge).is_ok());
        assert!(validate_secrets("short", &agent, &bridge).is_err());
        assert!(validate_secrets(&jwt, &jwt, &bridge).is_err());
        assert!(validate_secrets(&jwt, &agent, &agent).is_err());
        assert!(validate_secrets(&jwt, &bridge, &bridge).is_err());
        assert!(validate_secrets(&"j".repeat(256), &"a".repeat(256), &"b".repeat(256)).is_ok());
        assert!(validate_secrets(&"j".repeat(257), &agent, &bridge).is_err());
        assert!(validate_secrets(&jwt, &agent, &format!("{} ", "b".repeat(31))).is_err());
        assert!(validate_secrets(&jwt, &format!("{}é", "a".repeat(31)), &bridge).is_err());
    }

    /// Programmatic construction cannot bypass the complete runtime security gate.
    #[test]
    fn direct_config_runtime_validation_rejects_weak_secrets() {
        let config = Config {
            database_url: "postgresql://localhost/rift".to_string(),
            jwt_secret: "short".to_string(),
            agent_jwt_secret: "a".repeat(32),
            bridge_secret: "b".repeat(32),
            listen_addr: "127.0.0.1:3200".to_string(),
            bridge_listen_addr: "127.0.0.1:3201".to_string(),
            allow_remote_listen: false,
            cors_origins: vec!["http://127.0.0.1:5173".parse().unwrap()],
            upload_dir: "uploads".to_string(),
            max_upload_bytes: DEFAULT_MAX_UPLOAD_BYTES,
        };

        assert!(config.validate_runtime().is_err());
    }

    /// Listener validation permits only an explicitly separated loopback topology by default.
    #[test]
    fn listener_topology_defaults_to_two_loopback_sockets() {
        assert!(validate_listener_topology("127.0.0.1:3200", "127.0.0.1:3201", false).is_ok());
        assert!(validate_listener_topology("[::1]:3200", "[::1]:3201", false).is_ok());
        assert!(validate_listener_topology("127.0.0.1:3200", "127.0.0.1:3200", false).is_err());
    }

    /// Remote public exposure requires an exact acknowledgement and never widens bridge ingress.
    #[test]
    fn remote_public_listener_requires_acknowledgement() {
        for public in ["0.0.0.0:3200", "192.0.2.10:3200", "[::]:3200"] {
            assert!(validate_listener_topology(public, "127.0.0.1:3201", false).is_err());
            assert!(validate_listener_topology(public, "127.0.0.1:3201", true).is_ok());
        }
        for bridge in ["0.0.0.0:3201", "192.0.2.10:3201", "[::]:3201"] {
            assert!(validate_listener_topology("127.0.0.1:3200", bridge, true).is_err());
        }
    }

    /// Hostnames and malformed addresses cannot hide the interface selected for either boundary.
    #[test]
    fn listener_topology_requires_ip_literal_socket_addresses() {
        assert!(validate_listener_topology("localhost:3200", "127.0.0.1:3201", false).is_err());
        assert!(validate_listener_topology("127.0.0.1:3200", "localhost:3201", false).is_err());
        assert!(validate_listener_topology("not-an-address", "127.0.0.1:3201", false).is_err());
    }

    /// Ephemeral port zero cannot produce stable public or bridge client coordinates.
    #[test]
    fn listener_topology_requires_explicit_nonzero_ports() {
        assert!(validate_listener_topology("127.0.0.1:0", "127.0.0.1:3201", false).is_err());
        assert!(validate_listener_topology("127.0.0.1:3200", "127.0.0.1:0", false).is_err());
    }

    /// The remote-listen gate accepts only its two documented numeric values.
    #[test]
    fn remote_listener_acknowledgement_is_exact() {
        assert!(!parse_remote_listen_ack(None).unwrap());
        assert!(!parse_remote_listen_ack(Some("0")).unwrap());
        assert!(parse_remote_listen_ack(Some("1")).unwrap());
        for invalid in ["", "true", "yes", "2", "1 "] {
            assert!(parse_remote_listen_ack(Some(invalid)).is_err());
        }
    }

    /// Upload limits default safely and reject malformed or excessive allocations.
    #[test]
    fn upload_limit_is_bounded() {
        assert_eq!(parse_upload_limit(None).unwrap(), DEFAULT_MAX_UPLOAD_BYTES);
        assert_eq!(
            parse_upload_limit(Some(&MAX_UPLOAD_BYTES_CEILING.to_string())).unwrap(),
            MAX_UPLOAD_BYTES_CEILING
        );
        assert!(parse_upload_limit(Some("0")).is_err());
        assert!(parse_upload_limit(Some("not-a-number")).is_err());
        assert!(parse_upload_limit(Some(&(MAX_UPLOAD_BYTES_CEILING + 1).to_string())).is_err());
    }
}
