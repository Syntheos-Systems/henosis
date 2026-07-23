//! Runtime configuration for the memory gateway, sourced entirely from the
//! environment so that no deployment-specific values are baked into the binary.

use std::env;
use std::net::SocketAddr;

use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

/// Minimum entropy-bearing length accepted for the inbound bearer token.
const MIN_API_TOKEN_BYTES: usize = 32;
/// Maximum inbound bearer-token length retained long enough to hash at startup.
const MAX_API_TOKEN_BYTES: usize = 512;

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
    /// SHA-256 digest of the bearer token accepted from gateway clients.
    pub inbound_token_digest: [u8; 32],
}

/// Configuration loading from environment variables.
impl Config {
    /// Build configuration from environment variables, applying localhost
    /// defaults that are safe to commit to a public repository. The real
    /// Kleos endpoint, signing key, and inbound token are supplied via the
    /// environment at runtime.
    pub fn from_env() -> Result<Self, String> {
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
        let inbound_token =
            Zeroizing::new(env::var("SYNTHEOS_GATEWAY_API_TOKEN").map_err(|_| {
                "SYNTHEOS_GATEWAY_API_TOKEN is required; inject it through cred at startup"
                    .to_string()
            })?);
        let inbound_token_digest = validate_and_digest_api_token(inbound_token.as_str())?;
        Ok(Self {
            bind_addr,
            kleos_base_url,
            signing_host,
            signing_agent,
            signing_model,
            inbound_token_digest,
        })
    }
}

/// Validate one configured bearer token and retain only its SHA-256 digest.
fn validate_and_digest_api_token(token: &str) -> Result<[u8; 32], String> {
    if token.len() < MIN_API_TOKEN_BYTES {
        return Err(format!(
            "SYNTHEOS_GATEWAY_API_TOKEN must contain at least {MIN_API_TOKEN_BYTES} bytes"
        ));
    }
    if token.len() > MAX_API_TOKEN_BYTES {
        return Err(format!(
            "SYNTHEOS_GATEWAY_API_TOKEN must contain at most {MAX_API_TOKEN_BYTES} bytes"
        ));
    }
    if !token.bytes().all(|byte| {
        byte.is_ascii_alphanumeric()
            || matches!(byte, b'-' | b'.' | b'_' | b'~' | b'+' | b'/' | b'=')
    }) {
        return Err(
            "SYNTHEOS_GATEWAY_API_TOKEN contains characters outside the bearer-token alphabet"
                .to_string(),
        );
    }
    Ok(Sha256::digest(token.as_bytes()).into())
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

/// Parse the gateway bind address and reject remote exposure unless explicitly overridden.
pub fn validated_bind_addr(
    bind_addr: &str,
    allow_insecure_remote: Option<&str>,
) -> Result<SocketAddr, String> {
    let address: SocketAddr = bind_addr
        .parse()
        .map_err(|error| format!("invalid SYNTHEOS_GATEWAY_ADDR {bind_addr:?}: {error}"))?;
    if address.ip().is_loopback() || allow_insecure_remote == Some("1") {
        return Ok(address);
    }
    Err(format!(
        "SYNTHEOS_GATEWAY_ADDR {bind_addr:?} is not loopback; set \
         SYNTHEOS_GATEWAY_ALLOW_INSECURE_REMOTE=1 only behind a trusted authenticated boundary"
    ))
}

#[cfg(test)]
/// Exercises fail-closed validation of gateway bind addresses.
mod tests {
    use super::{validate_and_digest_api_token, validated_bind_addr, MAX_API_TOKEN_BYTES};

    /// A high-entropy header-safe token is reduced to a deterministic digest.
    #[test]
    fn api_token_validation_accepts_safe_input() {
        let token = "gateway-token-that-is-at-least-thirty-two-bytes";
        let first = validate_and_digest_api_token(token).expect("valid token");
        let second = validate_and_digest_api_token(token).expect("valid token");
        assert_eq!(first, second);
        assert_ne!(first, [0; 32]);
    }

    /// Missing entropy, ambiguous whitespace, controls, and excess length fail closed.
    #[test]
    fn api_token_validation_rejects_unsafe_input() {
        for invalid in [
            "",
            "too-short",
            "gateway token that contains whitespace and is long enough",
            "gateway-token-with-a-control-byte-\n-and-padding",
        ] {
            assert!(
                validate_and_digest_api_token(invalid).is_err(),
                "{invalid:?}"
            );
        }
        assert!(validate_and_digest_api_token(&"a".repeat(MAX_API_TOKEN_BYTES + 1)).is_err());
    }

    /// IPv4 and IPv6 loopback addresses are safe without an override.
    #[test]
    fn loopback_addresses_are_accepted() {
        assert!(validated_bind_addr("127.0.0.1:4510", None).is_ok());
        assert!(validated_bind_addr("[::1]:4510", None).is_ok());
    }

    /// Wildcard and private-network binds fail closed by default.
    #[test]
    fn remote_addresses_are_rejected_by_default() {
        assert!(validated_bind_addr("0.0.0.0:4510", None).is_err());
        assert!(validated_bind_addr("192.0.2.10:4510", None).is_err());
    }

    /// Only the exact documented override enables a deliberate remote bind.
    #[test]
    fn exact_override_allows_remote_address() {
        assert!(validated_bind_addr("0.0.0.0:4510", Some("1")).is_ok());
        assert!(validated_bind_addr("0.0.0.0:4510", Some("true")).is_err());
    }

    /// Malformed addresses fail before the server attempts to bind.
    #[test]
    fn malformed_address_is_rejected() {
        assert!(validated_bind_addr("localhost:4510", None).is_err());
    }
}
