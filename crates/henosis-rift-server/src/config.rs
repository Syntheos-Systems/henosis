use axum::http::HeaderValue;
use std::env;

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
    /// Resolve the complete runtime configuration or fail on missing security settings.
    pub fn from_env() -> Self {
        let jwt_secret = env::var("JWT_SECRET").expect("JWT_SECRET required");
        let bridge_secret = env::var("RIFT_BRIDGE_SECRET").expect("RIFT_BRIDGE_SECRET required");
        validate_secrets(&jwt_secret, &bridge_secret)
            .expect("JWT_SECRET and RIFT_BRIDGE_SECRET must be strong and independent");
        Self {
            database_url: env::var("DATABASE_URL").expect("DATABASE_URL required"),
            jwt_secret,
            bridge_secret,
            listen_addr: env::var("LISTEN_ADDR").unwrap_or_else(|_| "127.0.0.1:3200".into()),
            cors_origins: parse_cors_origins(&env::var("RIFT_CORS_ORIGINS").unwrap_or_else(|_| {
                "http://localhost:5173,http://127.0.0.1:5173,tauri://localhost".into()
            }))
            .expect("RIFT_CORS_ORIGINS must contain valid HTTP header values"),
            upload_dir: env::var("UPLOAD_DIR").unwrap_or_else(|_| "./uploads".into()),
            max_upload_bytes: env::var("MAX_UPLOAD_BYTES")
                .unwrap_or_else(|_| "26214400".into())
                .parse()
                .unwrap_or(26214400),
        }
    }
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
    use super::{parse_cors_origins, validate_secrets};

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
}
