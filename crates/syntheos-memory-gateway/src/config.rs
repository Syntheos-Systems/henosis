//! Runtime configuration for the memory gateway, sourced entirely from the
//! environment so that no deployment-specific values are baked into the binary.

use std::env;

/// Resolved gateway configuration.
#[derive(Debug, Clone)]
pub struct Config {
    /// Socket address the gateway binds to.
    pub bind_addr: String,
    /// Base URL of the upstream Kleos instance, with any trailing slash removed.
    pub kleos_base_url: String,
    /// Host label embedded in the identity hash and request headers.
    pub signing_host: String,
    /// Agent label embedded in the identity hash and request headers.
    pub signing_agent: String,
    /// Model label embedded in the identity hash and request headers.
    pub signing_model: String,
}

/// Configuration loading from environment variables.
impl Config {
    /// Build configuration from environment variables, applying localhost
    /// defaults that are safe to commit to a public repository.  The real
    /// Kleos endpoint and signing key are supplied via the environment at
    /// runtime.
    pub fn from_env() -> Self {
        let bind_addr =
            env::var("SYNTHEOS_GATEWAY_ADDR").unwrap_or_else(|_| "127.0.0.1:4510".to_string());
        let kleos_base_url = env::var("KLEOS_BASE_URL")
            .unwrap_or_else(|_| "http://127.0.0.1:4200".to_string())
            .trim_end_matches('/')
            .to_string();
        let signing_host = env::var("SYNTHEOS_HOST_LABEL").unwrap_or_else(|_| {
            // Fall back to the system hostname, or "unknown" if that fails.
            hostname()
        });
        let signing_agent =
            env::var("SYNTHEOS_AGENT_LABEL").unwrap_or_else(|_| "syntheos-gateway".to_string());
        let signing_model = env::var("SYNTHEOS_MODEL_LABEL").unwrap_or_else(|_| "none".to_string());
        Self {
            bind_addr,
            kleos_base_url,
            signing_host,
            signing_agent,
            signing_model,
        }
    }
}

/// Query the OS for the machine hostname, returning "unknown" on failure.
///
/// Reads `/proc/sys/kernel/hostname` directly rather than spawning a
/// PATH-resolved `hostname` binary, so a manipulated `PATH` cannot inject an
/// arbitrary host label into the signing identity.
fn hostname() -> String {
    match std::fs::read_to_string("/proc/sys/kernel/hostname") {
        Ok(h) if !h.trim().is_empty() => h.trim().to_string(),
        _ => "unknown".to_string(),
    }
}
