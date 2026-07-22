//! Shared helpers used across HTTP-backed adapters: phylaxd error mapping,
//! HTTP client builder, a small `truncate` for log/error bodies, and the
//! retry/backoff wrapper that all adapters route their upstream calls through.

use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tracing::warn;

use crate::phylaxd_client::PhylaxdError;
use crate::tool::{err, error_response, InvokeResponse, RetryPolicy};

tokio::task_local! {
    /// Per-invocation retry counter. The dispatcher (`invoke_with_circuit`)
    /// establishes the scope around a tool's `invoke`; `send_with_retry` bumps
    /// it on each backoff. Ambient so adapters need no plumbing. Outside a
    /// scope (e.g. unit tests) the increments are silent no-ops.
    pub static RETRY_COUNTER: AtomicU32;
}

/// Increment the ambient retry counter, if one is in scope.
fn note_retry() {
    let _ = RETRY_COUNTER.try_with(|c| c.fetch_add(1, Ordering::Relaxed));
}

/// Build a reqwest HTTP client with conservative timeouts and no redirect
/// following (adapters handle redirects explicitly when needed).
pub fn build_http() -> Result<reqwest::Client, reqwest::Error> {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(2))
        .timeout(Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::none())
        .build()
}

/// Truncate `s` to at most `max` bytes, appending `...[truncated]` when cut.
pub fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let mut out = s[..max].to_string();
        out.push_str("...[truncated]");
        out
    }
}

/// Result of an upstream HTTP call once the (text) response body has been read.
pub struct HttpOutcome {
    /// HTTP status code returned by the upstream.
    pub status: reqwest::StatusCode,
    /// Response body decoded as UTF-8 (lossy).
    pub body: String,
}

/// Result of an upstream HTTP call with the raw response bytes preserved.
/// Used by adapters that handle binary payloads (e.g. file downloads).
pub struct HttpBytesOutcome {
    /// HTTP status code returned by the upstream.
    pub status: reqwest::StatusCode,
    /// Raw response bytes.
    pub body: Vec<u8>,
}

/// Send a request with retry/backoff per `policy`, returning the body as text.
///
/// Retries on the policy's retryable statuses (honoring `Retry-After` for
/// 429s) and, for idempotent operations, on transient network/timeout errors.
/// The request builder is cloned per attempt; if its body cannot be cloned the
/// call is made exactly once.
pub async fn send_with_retry(
    builder: reqwest::RequestBuilder,
    policy: &RetryPolicy,
) -> Result<HttpOutcome, reqwest::Error> {
    let raw = send_with_retry_bytes(builder, policy).await?;
    Ok(HttpOutcome {
        status: raw.status,
        body: String::from_utf8_lossy(&raw.body).into_owned(),
    })
}

/// Byte-preserving variant of [`send_with_retry`]. Shares the same retry
/// semantics; the text variant is a thin wrapper over this.
pub async fn send_with_retry_bytes(
    builder: reqwest::RequestBuilder,
    policy: &RetryPolicy,
) -> Result<HttpBytesOutcome, reqwest::Error> {
    let mut attempt: u32 = 0;
    loop {
        // Clone for this attempt so the original survives for the next one.
        let this = match builder.try_clone() {
            Some(b) => b,
            None => {
                // Non-cloneable body (e.g. a stream): single shot, no retry.
                let resp = builder.send().await?;
                let status = resp.status();
                let body = resp.bytes().await.map(|b| b.to_vec()).unwrap_or_default();
                return Ok(HttpBytesOutcome { status, body });
            }
        };

        match this.send().await {
            Ok(resp) => {
                let status = resp.status();
                if attempt < policy.max_retries && policy.is_retryable_status(status.as_u16()) {
                    let retry_after = retry_after_ms(resp.headers());
                    // Drain the body so the connection can be reused.
                    let _ = resp.bytes().await;
                    let delay = retry_after.unwrap_or_else(|| {
                        policy.backoff_delay_ms(attempt, jitter_ms(policy.base_delay_ms))
                    });
                    warn!(
                        status = status.as_u16(),
                        attempt,
                        delay_ms = delay,
                        "upstream returned retryable status; backing off"
                    );
                    sleep_ms(delay).await;
                    attempt += 1;
                    note_retry();
                    continue;
                }
                let body = resp.bytes().await.map(|b| b.to_vec()).unwrap_or_default();
                return Ok(HttpBytesOutcome { status, body });
            }
            Err(e) => {
                let transient = e.is_timeout() || e.is_connect() || e.is_request();
                if attempt < policy.max_retries && policy.idempotent && transient {
                    let delay = policy.backoff_delay_ms(attempt, jitter_ms(policy.base_delay_ms));
                    warn!(error = %e, attempt, delay_ms = delay, "transient upstream error; backing off");
                    sleep_ms(delay).await;
                    attempt += 1;
                    note_retry();
                    continue;
                }
                return Err(e);
            }
        }
    }
}

/// Parse a `Retry-After` header (delta-seconds form) into milliseconds.
fn retry_after_ms(headers: &reqwest::header::HeaderMap) -> Option<u64> {
    headers
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<u64>().ok())
        .map(|secs| secs.saturating_mul(1000))
}

/// Cheap jitter in `0..base` derived from the wall clock; avoids a `rand`
/// dependency. Returns 0 when `base` is 0.
fn jitter_ms(base: u64) -> u64 {
    if base == 0 {
        return 0;
    }
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0);
    nanos % base
}

/// Async sleep helper, thin wrapper over `tokio::time::sleep`.
async fn sleep_ms(ms: u64) {
    tokio::time::sleep(Duration::from_millis(ms)).await;
}

/// Map a `PhylaxdError` to a structured `InvokeResponse` error appropriate for
/// surfacing to the caller.
pub fn phylaxd_error_to_response(tool_id: &str, e: &PhylaxdError) -> InvokeResponse {
    match e {
        PhylaxdError::TenantNotAuthorized {
            provider,
            category,
            name,
        } => InvokeResponse {
            tool_id: tool_id.to_string(),
            success: false,
            result: None,
            error: Some(err(
                "tenant_not_authorized",
                format!("tenant has no provisioned {provider} OAuth credential"),
                Some(&format!("provision phylaxd slot {category}/{name}")),
            )),
            duration_ms: 0,
        },
        PhylaxdError::AuthMissing => error_response(
            tool_id,
            "phylaxd_auth_missing",
            e.to_string(),
            Some("set HERMES_PHYLAXD_TOKEN env var"),
        ),
        PhylaxdError::Unreachable { .. } => {
            error_response(tool_id, "phylaxd_unreachable", e.to_string(), None)
        }
        PhylaxdError::Upstream { .. } => {
            error_response(tool_id, "phylaxd_upstream_error", e.to_string(), None)
        }
        PhylaxdError::MalformedResponse => {
            error_response(tool_id, "phylaxd_malformed_response", e.to_string(), None)
        }
    }
}

#[cfg(test)]
/// Contains focused unit tests for this module.
mod tests {
    use super::*;
    use reqwest::header::{HeaderMap, HeaderValue, RETRY_AFTER};

    #[test]
    /// Verifies truncate short passthrough.
    fn truncate_short_passthrough() {
        assert_eq!(truncate("hi", 10), "hi");
    }

    #[test]
    /// Verifies truncate long marks truncation.
    fn truncate_long_marks_truncation() {
        let out = truncate("abcdef", 3);
        assert!(out.starts_with("abc"));
        assert!(out.ends_with("...[truncated]"));
    }

    #[test]
    /// Verifies retry after parses seconds to ms.
    fn retry_after_parses_seconds_to_ms() {
        let mut h = HeaderMap::new();
        h.insert(RETRY_AFTER, HeaderValue::from_static("2"));
        assert_eq!(retry_after_ms(&h), Some(2000));
    }

    #[test]
    /// Verifies retry after absent is none.
    fn retry_after_absent_is_none() {
        assert_eq!(retry_after_ms(&HeaderMap::new()), None);
    }

    #[test]
    /// Verifies jitter stays in range.
    fn jitter_stays_in_range() {
        for _ in 0..50 {
            assert!(jitter_ms(500) < 500);
        }
        assert_eq!(jitter_ms(0), 0);
    }

    /// Inside a RETRY_COUNTER scope, note_retry accumulates and is readable.
    #[tokio::test]
    async fn retry_counter_accumulates_in_scope() {
        let n = RETRY_COUNTER
            .scope(AtomicU32::new(0), async {
                note_retry();
                note_retry();
                note_retry();
                RETRY_COUNTER.with(|c| c.load(Ordering::Relaxed))
            })
            .await;
        assert_eq!(n, 3);
    }

    /// Outside any scope, note_retry is a silent no-op (must not panic).
    #[test]
    fn note_retry_outside_scope_is_noop() {
        note_retry();
    }
}
