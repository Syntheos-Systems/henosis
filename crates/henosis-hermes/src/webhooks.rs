//! Inbound webhook ingestion.
//!
//! Receives provider webhooks, verifies their signatures, normalizes them into a
//! [`WebhookEvent`], and publishes verified events to Axon. Signature
//! verification is the security boundary: an invalid signature is rejected (401)
//! and never published. All HMAC comparisons use the `hmac` crate's
//! constant-time [`Mac::verify_slice`], never a byte-by-byte `==` on the digest.

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::Json;
use hmac::{Hmac, Mac};
use serde::Serialize;
use serde_json::{json, Value};
use sha2::Sha256;
use tracing::warn;

use crate::AppState;

/// HMAC-SHA256 type alias used for all webhook signature verification.
type HmacSha256 = Hmac<Sha256>;

/// Slack's allowed clock skew for the request-timestamp replay window.
pub const SLACK_REPLAY_WINDOW_SECS: i64 = 300;

/// A provider whose webhooks Hermes can ingest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    /// GitHub (`X-Hub-Signature-256`, HMAC-SHA256).
    GitHub,
    /// Slack (`X-Slack-Signature` + timestamp, HMAC-SHA256 over a base string).
    Slack,
    /// Linear (`Linear-Signature`, HMAC-SHA256).
    Linear,
    /// Notion (`X-Notion-Signature`, HMAC-SHA256).
    Notion,
}

/// Implements the behavior exposed by Provider.
impl Provider {
    /// Parse a provider from the URL path segment.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "github" => Some(Provider::GitHub),
            "slack" => Some(Provider::Slack),
            "linear" => Some(Provider::Linear),
            "notion" => Some(Provider::Notion),
            _ => None,
        }
    }

    /// The provider's canonical lowercase name.
    pub fn as_str(self) -> &'static str {
        match self {
            Provider::GitHub => "github",
            Provider::Slack => "slack",
            Provider::Linear => "linear",
            Provider::Notion => "notion",
        }
    }
}

/// Compute the raw HMAC-SHA256 of `body` under `secret`. Used to build expected
/// signatures in tests and by [`github_signature`]; the verify path uses the
/// constant-time [`hmac_matches`] instead.
#[cfg(test)]
fn hmac_sha256(secret: &[u8], body: &[u8]) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(secret).expect("HMAC accepts any key length");
    mac.update(body);
    mac.finalize().into_bytes().to_vec()
}

/// Constant-time check that `body` under `secret` matches the HMAC bytes
/// `expected`. Uses the `hmac` crate's `verify_slice`, which compares in
/// constant time.
fn hmac_matches(secret: &[u8], body: &[u8], expected: &[u8]) -> bool {
    let mut mac = HmacSha256::new_from_slice(secret).expect("HMAC accepts any key length");
    mac.update(body);
    mac.verify_slice(expected).is_ok()
}

/// Decode an optionally-prefixed lowercase-hex signature header into raw bytes.
/// Returns `None` if the value (after stripping `prefix`) is not valid hex.
fn decode_hex_sig(header: &str, prefix: &str) -> Option<Vec<u8>> {
    let hex_part = header.strip_prefix(prefix)?;
    hex::decode(hex_part).ok()
}

/// Verify a GitHub webhook: `X-Hub-Signature-256: sha256=<hex>` over the raw
/// body under the shared secret.
pub fn verify_github(secret: &[u8], body: &[u8], signature_header: &str) -> bool {
    match decode_hex_sig(signature_header, "sha256=") {
        Some(expected) => hmac_matches(secret, body, &expected),
        None => false,
    }
}

/// Verify a Notion webhook: `X-Notion-Signature: sha256=<hex>` over the raw
/// body under the subscription verification token.
pub fn verify_notion(secret: &[u8], body: &[u8], signature_header: &str) -> bool {
    match decode_hex_sig(signature_header, "sha256=") {
        Some(expected) => hmac_matches(secret, body, &expected),
        None => false,
    }
}

/// Verify a Linear webhook: `Linear-Signature: <hex>` (no prefix) over the raw
/// body under the shared secret.
pub fn verify_linear(secret: &[u8], body: &[u8], signature_header: &str) -> bool {
    match decode_hex_sig(signature_header, "") {
        Some(expected) => hmac_matches(secret, body, &expected),
        None => false,
    }
}

/// Verify a Slack webhook: `X-Slack-Signature: v0=<hex>` over the base string
/// `v0:{timestamp}:{body}`, plus a replay-window check on `timestamp` against
/// `now` (both Unix seconds). Rejects timestamps outside
/// [`SLACK_REPLAY_WINDOW_SECS`].
pub fn verify_slack(
    signing_secret: &[u8],
    body: &[u8],
    timestamp: &str,
    signature_header: &str,
    now_unix: i64,
) -> bool {
    // Replay window: the request timestamp must be recent.
    let Ok(ts) = timestamp.parse::<i64>() else {
        return false;
    };
    if (now_unix - ts).abs() > SLACK_REPLAY_WINDOW_SECS {
        return false;
    }
    let Some(expected) = decode_hex_sig(signature_header, "v0=") else {
        return false;
    };
    // Base string binds the timestamp into the signature, defeating replay with
    // a fresh timestamp.
    let mut basestring = Vec::with_capacity(body.len() + timestamp.len() + 4);
    basestring.extend_from_slice(b"v0:");
    basestring.extend_from_slice(timestamp.as_bytes());
    basestring.push(b':');
    basestring.extend_from_slice(body);
    hmac_matches(signing_secret, &basestring, &expected)
}

/// A normalized inbound webhook event.
#[derive(Debug, Clone, Serialize)]
pub struct WebhookEvent {
    /// Originating provider.
    pub provider: String,
    /// Provider-specific event type (e.g. `push`, `issue.created`, `message`).
    pub event_type: String,
    /// The original payload, preserved verbatim.
    pub raw_event: serde_json::Value,
    /// Tenant the event is attributed to, if resolvable.
    pub tenant_id: Option<String>,
    /// When Hermes received the event (RFC3339).
    pub received_at: String,
    /// Whether signature verification passed.
    pub verified: bool,
}

/// Compute the GitHub signature header value for a body+secret. Helper for the
/// webhook-registration path (7.2) and signature tests.
#[cfg(test)]
pub fn github_signature(secret: &[u8], body: &[u8]) -> String {
    format!("sha256={}", hex::encode(hmac_sha256(secret, body)))
}

/// Read a header as a `&str`, or `None` if absent or non-ASCII.
fn header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|v| v.to_str().ok())
}

/// Derive the provider-specific event type from headers and body.
fn event_type(provider: Provider, headers: &HeaderMap, body: &Value) -> String {
    let from_body = |keys: &[&str]| -> Option<String> {
        keys.iter()
            .find_map(|k| body.pointer(k).and_then(|v| v.as_str()))
            .map(String::from)
    };
    match provider {
        Provider::GitHub => header(headers, "x-github-event")
            .map(String::from)
            .unwrap_or_else(|| "unknown".into()),
        Provider::Slack => from_body(&["/event/type", "/type"]).unwrap_or_else(|| "unknown".into()),
        Provider::Linear => from_body(&["/type", "/action"]).unwrap_or_else(|| "unknown".into()),
        Provider::Notion => from_body(&["/type"]).unwrap_or_else(|| "unknown".into()),
    }
}

/// Normalize one verified provider payload without accepting unsigned tenant
/// attribution from the public request URL.
fn normalize_event(
    provider: Provider,
    headers: &HeaderMap,
    raw_event: Value,
    verified: bool,
) -> WebhookEvent {
    WebhookEvent {
        provider: provider.as_str().to_string(),
        event_type: event_type(provider, headers, &raw_event),
        raw_event,
        tenant_id: None,
        received_at: now_rfc3339(),
        verified,
    }
}

/// `POST /webhooks/{provider}`: verify, normalize, and publish an inbound
/// webhook. The raw body is read before parsing so the signature is checked
/// over the exact bytes the provider signed. An invalid signature is rejected
/// (401) and never published; an unverifiable secret (missing/phylaxd error) also
/// fails closed.
pub async fn ingest(
    State(state): State<AppState>,
    Path(provider_str): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    let Some(provider) = Provider::parse(&provider_str) else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("unknown webhook provider '{provider_str}'") })),
        )
            .into_response();
    };

    let verified = match verify_inbound(&state, provider, &headers, &body).await {
        Ok(v) => v,
        Err(reason) => {
            warn!(provider = provider.as_str(), %reason, "webhook signature rejected");
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({ "error": "webhook signature verification failed" })),
            )
                .into_response();
        }
    };

    let raw_event: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let event = normalize_event(provider, &headers, raw_event, verified);
    let evt_type = event.event_type.clone();

    let channel = format!("hermes.webhook.{}.{}", provider.as_str(), evt_type);
    state.axon.publish(
        &channel,
        "received",
        serde_json::to_value(&event).unwrap_or(Value::Null),
    );

    (
        StatusCode::OK,
        Json(json!({ "received": true, "verified": verified })),
    )
        .into_response()
}

/// Verify an inbound webhook per provider. Returns `Ok(true)` when its
/// signature checked out, or `Err(reason)` when verification failed or its
/// secret could not be resolved.
async fn verify_inbound(
    state: &AppState,
    provider: Provider,
    headers: &HeaderMap,
    body: &[u8],
) -> Result<bool, String> {
    let secret = state
        .phylaxd
        .fetch_raw_secret("webhooks", &format!("{}-secret", provider.as_str()))
        .await
        .map_err(|e| format!("webhook secret unavailable: {e}"))?;

    let ok = match provider {
        Provider::GitHub => {
            let sig =
                header(headers, "x-hub-signature-256").ok_or("missing X-Hub-Signature-256")?;
            verify_github(secret.as_bytes(), body, sig)
        }
        Provider::Linear => {
            let sig = header(headers, "linear-signature").ok_or("missing Linear-Signature")?;
            verify_linear(secret.as_bytes(), body, sig)
        }
        Provider::Slack => {
            let sig = header(headers, "x-slack-signature").ok_or("missing X-Slack-Signature")?;
            let ts = header(headers, "x-slack-request-timestamp")
                .ok_or("missing X-Slack-Request-Timestamp")?;
            verify_slack(secret.as_bytes(), body, ts, sig, now_unix())
        }
        Provider::Notion => {
            let sig = header(headers, "x-notion-signature").ok_or("missing X-Notion-Signature")?;
            verify_notion(secret.as_bytes(), body, sig)
        }
    };

    if ok {
        Ok(true)
    } else {
        Err("signature mismatch".into())
    }
}

/// Current Unix time in seconds (Slack replay-window check).
fn now_unix() -> i64 {
    chrono::Utc::now().timestamp()
}

/// Current time as an RFC3339 string (webhook `received_at`).
fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339()
}

#[cfg(test)]
/// Contains focused unit tests for this module.
mod tests {
    use super::*;

    const SECRET: &[u8] = b"It's a Secret to Everybody";
    const BODY: &[u8] = b"Hello, World!";

    /// Published GitHub HMAC-SHA256 vector (the documented example).
    const GITHUB_VECTOR: &str =
        "sha256=757107ea0eb2509fc211221cce984b8a37570b6d7586c22c46f4379c8b043e17";

    /// The documented GitHub example verifies, and tampering with the body,
    /// secret, or signature all fail.
    #[test]
    fn github_known_vector_and_tamper() {
        assert!(verify_github(SECRET, BODY, GITHUB_VECTOR));
        assert!(!verify_github(b"wrong secret", BODY, GITHUB_VECTOR));
        assert!(!verify_github(SECRET, b"tampered body", GITHUB_VECTOR));
        assert!(!verify_github(SECRET, BODY, "sha256=deadbeef"));
        assert!(!verify_github(SECRET, BODY, "not-a-signature"));
        // Missing the sha256= prefix must not verify.
        assert!(!verify_github(
            SECRET,
            BODY,
            GITHUB_VECTOR.trim_start_matches("sha256=")
        ));
    }

    /// Linear uses a bare hex signature over the body.
    #[test]
    fn linear_roundtrip_and_tamper() {
        let sig = hex::encode(hmac_sha256(SECRET, BODY));
        assert!(verify_linear(SECRET, BODY, &sig));
        assert!(!verify_linear(b"wrong", BODY, &sig));
        assert!(!verify_linear(SECRET, b"other", &sig));
        assert!(!verify_linear(SECRET, BODY, "zzzz"));
    }

    /// Notion uses a sha256-prefixed HMAC and rejects altered payloads.
    #[test]
    fn notion_roundtrip_and_tamper() {
        let sig = format!("sha256={}", hex::encode(hmac_sha256(SECRET, BODY)));
        assert!(verify_notion(SECRET, BODY, &sig));
        assert!(!verify_notion(b"wrong", BODY, &sig));
        assert!(!verify_notion(SECRET, b"other", &sig));
        assert!(!verify_notion(SECRET, BODY, "sha256=zz"));
    }

    /// Normalization never trusts tenant attribution from an unsigned URL.
    #[test]
    fn normalized_event_has_no_caller_selected_tenant() {
        let event = normalize_event(
            Provider::Notion,
            &HeaderMap::new(),
            json!({"type": "page.content_updated"}),
            true,
        );
        assert_eq!(event.event_type, "page.content_updated");
        assert!(event.tenant_id.is_none());
        assert!(event.verified);
    }

    /// Slack verifies a correctly-signed, in-window request and rejects
    /// out-of-window, tampered, and malformed ones.
    #[test]
    fn slack_signing_and_replay_window() {
        let ts = "1700000000";
        let now = 1_700_000_010; // 10s later, inside the window
        let mut base = Vec::new();
        base.extend_from_slice(b"v0:");
        base.extend_from_slice(ts.as_bytes());
        base.push(b':');
        base.extend_from_slice(BODY);
        let sig = format!("v0={}", hex::encode(hmac_sha256(SECRET, &base)));

        assert!(verify_slack(SECRET, BODY, ts, &sig, now));
        // Outside the replay window -> reject even with a valid signature.
        assert!(!verify_slack(SECRET, BODY, ts, &sig, now + 1_000));
        // Tampered body / wrong secret / malformed sig -> reject.
        assert!(!verify_slack(SECRET, b"tampered", ts, &sig, now));
        assert!(!verify_slack(b"wrong", BODY, ts, &sig, now));
        assert!(!verify_slack(SECRET, BODY, ts, "v0=zz", now));
        assert!(!verify_slack(SECRET, BODY, "not-a-number", &sig, now));
    }

    /// The signature helper round-trips with the verifier.
    #[test]
    fn github_signature_helper_roundtrips() {
        let sig = github_signature(SECRET, BODY);
        assert!(verify_github(SECRET, BODY, &sig));
    }

    /// Provider path parsing.
    #[test]
    fn provider_parse() {
        assert_eq!(Provider::parse("github"), Some(Provider::GitHub));
        assert_eq!(Provider::parse("slack"), Some(Provider::Slack));
        assert_eq!(Provider::parse("linear"), Some(Provider::Linear));
        assert_eq!(Provider::parse("notion"), Some(Provider::Notion));
        assert_eq!(Provider::parse("unknown"), None);
    }
}
