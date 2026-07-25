use axum::http::HeaderValue;
use std::env;

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
    /// HMAC secret used only to sign and validate user and agent JWTs.
    pub jwt_secret: String,
    /// Dedicated bearer secret for bridge-only provisioning and notification routes.
    pub bridge_secret: String,
    /// Socket address on which the server accepts requests.
    pub listen_addr: String,
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
        let bridge_secret = required_env("RIFT_BRIDGE_SECRET")?;
        validate_secrets(&jwt_secret, &bridge_secret).map_err(ConfigError::Invalid)?;
        Ok(Self {
            database_url: required_env("DATABASE_URL")?,
            jwt_secret,
            bridge_secret,
            listen_addr: env::var("LISTEN_ADDR").unwrap_or_else(|_| "127.0.0.1:3200".into()),
            cors_origins: parse_cors_origins(&env::var("RIFT_CORS_ORIGINS").unwrap_or_else(|_| {
                "http://localhost:5173,http://127.0.0.1:5173,tauri://localhost".into()
            }))
            .map_err(ConfigError::Invalid)?,
            upload_dir: env::var("UPLOAD_DIR").unwrap_or_else(|_| "./uploads".into()),
            max_upload_bytes: parse_upload_limit(env::var("MAX_UPLOAD_BYTES").ok().as_deref())
                .map_err(ConfigError::Invalid)?,
        })
    }

    /// Resolve the complete runtime configuration or fail on missing security settings.
    pub fn from_env() -> Self {
        Self::try_from_env().expect("Rift runtime configuration is invalid")
    }
}

/// Read one mandatory Unicode environment setting without collapsing failure modes.
fn required_env(name: &'static str) -> Result<String, ConfigError> {
    env::var(name).map_err(|source| ConfigError::Environment { name, source })
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

/// Reject short or reused secrets before the server exposes either trust boundary.
pub fn validate_secrets(jwt_secret: &str, bridge_secret: &str) -> Result<(), String> {
    if jwt_secret.len() < 32 {
        return Err("JWT_SECRET must contain at least 32 bytes".to_string());
    }
    if bridge_secret.len() < 32 {
        return Err("RIFT_BRIDGE_SECRET must contain at least 32 bytes".to_string());
    }
    if jwt_secret == bridge_secret {
        return Err("JWT_SECRET and RIFT_BRIDGE_SECRET must differ".to_string());
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
        DEFAULT_MAX_UPLOAD_BYTES, MAX_UPLOAD_BYTES_CEILING, parse_cors_origins, parse_upload_limit,
        validate_secrets,
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

    /// JWT and bridge secrets must be long enough and cryptographically independent.
    #[test]
    fn secrets_must_be_strong_and_distinct() {
        let jwt = "j".repeat(32);
        let bridge = "b".repeat(32);
        assert!(validate_secrets(&jwt, &bridge).is_ok());
        assert!(validate_secrets("short", &bridge).is_err());
        assert!(validate_secrets(&jwt, &jwt).is_err());
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
