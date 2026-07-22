//! Bootstrap-bearer resolver for the memory client.
//!
//! Copy-and-owned from `kleos-lib/src/cred/bootstrap.rs` (Story 4.1). Talks to
//! phylaxd's `/bootstrap/kleos-bearer?agent=<slot>` endpoint to fetch the
//! per-agent bearer at process startup without any plaintext key on disk.
//!
//! Resolution order:
//!
//! 1. `KLEOS_API_KEY` / `ENGRAM_API_KEY` env vars (test/debug overrides).
//! 2. phylaxd via `PHYLAXD_SOCKET` (Unix domain) or `PHYLAXD_BIND` (TCP, default
//!    `127.0.0.1:3100`). Auth is the value of `PHYLAXD_AGENT_KEY` (a scoped
//!    bootstrap-agent token).
//!
//! The Kleos source also offered an ECDH/PIV bootstrap path that shells to a
//! YubiKey via python3. This crate is software-key only (no PIV/PKCS#11 runtime
//! dependency), so that branch is intentionally dropped; the token path is the
//! only phylaxd transport here.
//!
//! Results are cached in process memory keyed by agent slot; the cache honors
//! the `expires_at` field returned by phylaxd so a leaked bearer goes stale on its
//! own TTL.

use std::collections::HashMap;
use std::env;
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use thiserror::Error;

/// Errors produced by [`resolve_api_key`].
#[derive(Debug, Error)]
pub enum CredError {
    /// `PHYLAXD_AGENT_KEY` env var is missing; cannot authenticate to phylaxd.
    #[error("PHYLAXD_AGENT_KEY is not set; cannot authenticate to phylaxd")]
    NoAgentKey,

    /// phylaxd is unreachable (socket not found, connection refused, etc.).
    #[error("phylaxd unreachable: {0}")]
    Unreachable(String),

    /// phylaxd returned a response that could not be parsed.
    #[error("bad response from phylaxd: {0}")]
    BadResponse(String),

    /// phylaxd response did not include a `key` field.
    #[error("phylaxd response is missing the 'key' field")]
    MissingKey,

    /// Caller supplied an invalid argument (e.g. an unsafe agent slot).
    #[error("invalid input: {0}")]
    InvalidInput(String),
}

/// Cached entry: the resolved bearer plus when it goes stale.
#[derive(Clone)]
struct CacheEntry {
    /// The resolved bearer token.
    key: String,
    /// Absolute time at which this entry must be discarded.
    expires_at: SystemTime,
}

/// Process-lifetime cache: slot -> (key, expires_at). A miss or expired hit
/// triggers a fresh fetch from phylaxd.
static KEY_CACHE: Mutex<Option<HashMap<String, CacheEntry>>> = Mutex::new(None);

/// Retrieve a cached bearer for `slot`, evicting it if expired.
fn cache_get(slot: &str) -> Option<String> {
    // Recover from a poisoned lock rather than cascading a panic to every
    // future caller; a poisoned credential cache is non-fatal (next call
    // re-fetches). Matches the poison-recovery pattern in signer.rs.
    let mut guard = KEY_CACHE.lock().unwrap_or_else(|p| p.into_inner());
    let map = guard.as_mut()?;
    let entry = map.get(slot)?.clone();
    if SystemTime::now() >= entry.expires_at {
        map.remove(slot);
        return None;
    }
    Some(entry.key)
}

/// Insert (or replace) a cached bearer for `slot`.
fn cache_set(slot: String, key: String, expires_at: SystemTime) {
    // See cache_get: recover from poison instead of panicking the caller.
    let mut guard = KEY_CACHE.lock().unwrap_or_else(|p| p.into_inner());
    guard
        .get_or_insert_with(HashMap::new)
        .insert(slot, CacheEntry { key, expires_at });
}

/// Returns the agent slot string to use for this process.
///
/// `KLEOS_AGENT_SLOT` env wins. Falls back to `claude-code-<user>-<hostname>`
/// where `user` is `$USER` / `$USERNAME` (or `unknown` if unset) and hostname
/// comes from `/proc/sys/kernel/hostname` or `HOSTNAME` (or `unknown-host`).
///
/// The `<user>` segment exists so two users on the same shared host don't
/// collide on a single cred slot. Existing single-user installs that prefer
/// the old `claude-code-<host>` form should set `KLEOS_AGENT_SLOT` explicitly.
pub fn current_agent_slot() -> String {
    if let Ok(slot) = env::var("KLEOS_AGENT_SLOT") {
        if !slot.is_empty() {
            return slot;
        }
    }
    let user = env::var("USER")
        .or_else(|_| env::var("USERNAME"))
        .unwrap_or_else(|_| "unknown".to_string());
    let hostname = read_hostname();
    format!("claude-code-{user}-{hostname}")
}

/// Resolve the host name from `/proc` or the `HOSTNAME` env, with a fallback.
fn read_hostname() -> String {
    if let Ok(h) = std::fs::read_to_string("/proc/sys/kernel/hostname") {
        let trimmed = h.trim().to_string();
        if !trimmed.is_empty() {
            return trimmed;
        }
    }
    if let Ok(h) = env::var("HOSTNAME") {
        if !h.is_empty() {
            return h;
        }
    }
    "unknown-host".to_string()
}

/// Resolve the Kleos API key for `agent_slot`. See module docs for order.
pub async fn resolve_api_key(agent_slot: &str) -> Result<String, CredError> {
    // SECURITY (L7): agent_slot is interpolated into the phylaxd request path
    // (/bootstrap/kleos-bearer?agent=...). Reject anything outside a safe
    // identifier charset so it cannot inject extra query parameters, path
    // segments, or CR/LF into the request line.
    if agent_slot.is_empty()
        || !agent_slot
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        return Err(CredError::InvalidInput(format!(
            "invalid agent slot: {agent_slot:?} (allowed: alphanumeric, '-', '_', '.')"
        )));
    }

    // Env overrides (test/debug). KLEOS_API_KEY wins, then legacy ENGRAM_API_KEY.
    if let Ok(k) = env::var("KLEOS_API_KEY") {
        if !k.is_empty() {
            return Ok(k);
        }
    }
    if let Ok(k) = env::var("ENGRAM_API_KEY") {
        if !k.is_empty() {
            return Ok(k);
        }
    }

    if let Some(cached) = cache_get(agent_slot) {
        return Ok(cached);
    }

    let token = env::var("PHYLAXD_AGENT_KEY").map_err(|_| CredError::NoAgentKey)?;
    if token.is_empty() {
        return Err(CredError::NoAgentKey);
    }

    let path = format!("/bootstrap/kleos-bearer?agent={agent_slot}");

    let body: serde_json::Value = if let Ok(sock) = env::var("PHYLAXD_SOCKET") {
        unix_get_json(&sock, &path, &token).await?
    } else {
        let bind = env::var("PHYLAXD_BIND").unwrap_or_else(|_| "127.0.0.1:3100".into());
        tcp_get_json(&bind, &path, &token).await?
    };

    let key = body["key"]
        .as_str()
        .ok_or(CredError::MissingKey)?
        .to_string();

    let expires_at = parse_expires_at(&body).unwrap_or_else(|| {
        // No TTL hint -> default 1h from now.
        SystemTime::now() + Duration::from_secs(3600)
    });

    cache_set(agent_slot.to_string(), key.clone(), expires_at);
    Ok(key)
}

/// Parse `expires_at` (RFC 3339) or fall back to `ttl_secs`.
fn parse_expires_at(body: &serde_json::Value) -> Option<SystemTime> {
    if let Some(s) = body.get("expires_at").and_then(|v| v.as_str()) {
        if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
            // A pre-epoch (negative) timestamp would wrap when cast to u64 and
            // yield an absurd far-future expiry, so treat it as "no usable
            // expiry" and fall through to the ttl/default path.
            let ts = dt.timestamp();
            if ts < 0 {
                return None;
            }
            return Some(SystemTime::UNIX_EPOCH + Duration::from_secs(ts as u64));
        }
    }
    if let Some(secs) = body.get("ttl_secs").and_then(|v| v.as_u64()) {
        return Some(SystemTime::now() + Duration::from_secs(secs));
    }
    None
}

/// Raw HTTP/1.1 GET over a Unix socket.
#[cfg(unix)]
async fn unix_get_json(
    sock_path: &str,
    path: &str,
    token: &str,
) -> Result<serde_json::Value, CredError> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::UnixStream;

    let mut stream = UnixStream::connect(sock_path)
        .await
        .map_err(|e| CredError::Unreachable(format!("unix socket {sock_path}: {e}")))?;

    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {token}\r\nConnection: close\r\n\r\n"
    );

    stream
        .write_all(request.as_bytes())
        .await
        .map_err(|e| CredError::Unreachable(format!("write: {e}")))?;

    // Cap the response and bound the read so a rogue local phylaxd cannot OOM or
    // stall the caller (CWE-400): the bootstrap socket is local but untrusted.
    let mut response = Vec::new();
    tokio::time::timeout(
        std::time::Duration::from_secs(30),
        (&mut stream).take(1024 * 1024).read_to_end(&mut response),
    )
    .await
    .map_err(|_| CredError::Unreachable("read: timed out".into()))?
    .map_err(|e| CredError::Unreachable(format!("read: {e}")))?;

    parse_http_response_body(&response)
}

/// Non-unix fallback: Unix domain sockets are unavailable.
#[cfg(not(unix))]
async fn unix_get_json(
    sock_path: &str,
    _path: &str,
    _token: &str,
) -> Result<serde_json::Value, CredError> {
    Err(CredError::Unreachable(format!(
        "Unix sockets not supported on this platform ({})",
        sock_path
    )))
}

/// Raw HTTP/1.1 GET over a TCP stream.
async fn tcp_get_json(bind: &str, path: &str, token: &str) -> Result<serde_json::Value, CredError> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    let mut stream = TcpStream::connect(bind)
        .await
        .map_err(|e| CredError::Unreachable(format!("tcp {bind}: {e}")))?;

    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: {bind}\r\nAuthorization: Bearer {token}\r\nConnection: close\r\n\r\n"
    );

    stream
        .write_all(request.as_bytes())
        .await
        .map_err(|e| CredError::Unreachable(format!("write: {e}")))?;

    // Cap the response and bound the read so a rogue local phylaxd cannot OOM or
    // stall the caller (CWE-400): the bootstrap socket is local but untrusted.
    let mut response = Vec::new();
    tokio::time::timeout(
        std::time::Duration::from_secs(30),
        (&mut stream).take(1024 * 1024).read_to_end(&mut response),
    )
    .await
    .map_err(|_| CredError::Unreachable("read: timed out".into()))?
    .map_err(|e| CredError::Unreachable(format!("read: {e}")))?;

    parse_http_response_body(&response)
}

/// Split raw HTTP/1.1 response bytes, parse body as JSON.
fn parse_http_response_body(response: &[u8]) -> Result<serde_json::Value, CredError> {
    let sep = b"\r\n\r\n";
    let body_start = response
        .windows(sep.len())
        .position(|w| w == sep)
        .map(|p| p + sep.len())
        .ok_or_else(|| CredError::BadResponse("no header/body separator".into()))?;

    let body = &response[body_start..];

    if let Some(status_line) = response
        .split(|&b| b == b'\n')
        .next()
        .and_then(|l| std::str::from_utf8(l).ok())
    {
        let code: Option<u16> = status_line
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse().ok());
        if let Some(code) = code {
            if code != 200 {
                let body_str = std::str::from_utf8(body).unwrap_or("(non-utf8 body)");
                return Err(CredError::BadResponse(format!(
                    "HTTP {}: {}",
                    code,
                    body_str.trim()
                )));
            }
        }
    }

    serde_json::from_slice(body).map_err(|e| CredError::BadResponse(format!("JSON parse: {e}")))
}

#[cfg(test)]
/// Unit tests for this module.
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Serializes tests that mutate process-global env vars.
    static ENV_GUARD: Mutex<()> = Mutex::new(());

    // The ENV_GUARD lock is held across .await on purpose: these tests must
    // serialize because they mutate process-global env vars. Using a sync
    // Mutex is correct here; clippy's await_holding_lock lint does not apply.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn env_override_kleos_api_key() {
        let _g = ENV_GUARD.lock().unwrap_or_else(|p| p.into_inner());
        env::remove_var("ENGRAM_API_KEY");
        env::set_var("KLEOS_API_KEY", "test-key-12345");
        let result = resolve_api_key("test-slot-env-1").await;
        env::remove_var("KLEOS_API_KEY");
        assert_eq!(result.unwrap(), "test-key-12345");
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    /// Test: env override engram api key.
    async fn env_override_engram_api_key() {
        let _g = ENV_GUARD.lock().unwrap_or_else(|p| p.into_inner());
        env::remove_var("KLEOS_API_KEY");
        env::set_var("ENGRAM_API_KEY", "legacy-key-99");
        let result = resolve_api_key("test-slot-2").await;
        env::remove_var("ENGRAM_API_KEY");
        assert_eq!(result.unwrap(), "legacy-key-99");
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    /// Test: no env no phylaxd returns error.
    async fn no_env_no_phylaxd_returns_error() {
        let _g = ENV_GUARD.lock().unwrap_or_else(|p| p.into_inner());
        env::remove_var("KLEOS_API_KEY");
        env::remove_var("ENGRAM_API_KEY");
        env::remove_var("PHYLAXD_AGENT_KEY");
        env::remove_var("PHYLAXD_SOCKET");
        env::remove_var("PHYLAXD_BIND");
        // With the ECDH/PIV branch dropped, resolution falls straight through to
        // the token path, so a missing PHYLAXD_AGENT_KEY yields NoAgentKey.
        let result = resolve_api_key("no-phylaxd-slot-unique-xyz").await;
        assert!(
            matches!(result, Err(CredError::NoAgentKey)),
            "expected NoAgentKey, got {result:?}"
        );
    }

    #[tokio::test]
    /// Test: rejects unsafe agent slot.
    async fn rejects_unsafe_agent_slot() {
        // A slot with a query-injection character must be refused before any
        // network access (the InvalidInput guard).
        let result = resolve_api_key("bad slot&x=1").await;
        assert!(matches!(result, Err(CredError::InvalidInput(_))));
    }

    #[test]
    /// Test: current agent slot uses env.
    fn current_agent_slot_uses_env() {
        let _g = ENV_GUARD.lock().unwrap_or_else(|p| p.into_inner());
        env::set_var("KLEOS_AGENT_SLOT", "my-custom-slot");
        let slot = current_agent_slot();
        env::remove_var("KLEOS_AGENT_SLOT");
        assert_eq!(slot, "my-custom-slot");
    }

    #[test]
    /// Test: current agent slot default includes user and host.
    fn current_agent_slot_default_includes_user_and_host() {
        let _g = ENV_GUARD.lock().unwrap_or_else(|p| p.into_inner());
        env::remove_var("KLEOS_AGENT_SLOT");
        env::set_var("USER", "testuser");
        env::set_var("HOSTNAME", "testhost");
        let slot = current_agent_slot();
        env::remove_var("USER");
        env::remove_var("HOSTNAME");
        assert!(slot.starts_with("claude-code-"), "slot was {slot}");
        assert!(slot.contains("testuser"), "slot was {slot}");
        let after = slot.trim_start_matches("claude-code-");
        let parts: Vec<&str> = after.splitn(2, '-').collect();
        assert_eq!(parts.len(), 2, "slot was {slot}");
        assert_eq!(parts[0], "testuser");
        assert!(!parts[1].is_empty(), "slot was {slot}");
    }

    #[test]
    /// Test: parse expires at rfc3339.
    fn parse_expires_at_rfc3339() {
        let body = serde_json::json!({"expires_at": "2030-01-01T00:00:00Z"});
        let t = parse_expires_at(&body).expect("should parse");
        let now = SystemTime::now();
        assert!(t > now, "year 2030 should be in the future");
    }

    /// Test: a pre-epoch (negative) RFC3339 timestamp yields None instead of a
    /// wrapped far-future u64 expiry.
    #[test]
    fn parse_expires_at_rejects_negative() {
        let body = serde_json::json!({"expires_at": "1969-01-01T00:00:00Z"});
        assert!(parse_expires_at(&body).is_none());
    }

    #[test]
    /// Test: parse expires at ttl fallback.
    fn parse_expires_at_ttl_fallback() {
        let body = serde_json::json!({"ttl_secs": 60});
        let t = parse_expires_at(&body).expect("should parse");
        let in_30s = SystemTime::now() + Duration::from_secs(30);
        let in_2m = SystemTime::now() + Duration::from_secs(120);
        assert!(
            t > in_30s && t < in_2m,
            "ttl 60s puts expiry inside 30s..2m"
        );
    }
}
