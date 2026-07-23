//! Eidolon gate client. Every unavailable or invalid gate response fails
//! closed so the pipeline cannot proceed without an explicit authorization.

use std::time::Duration;

use reqwest::Client;
use serde_json::{Value, json};
use tracing::warn;

/// Maximum response body accepted from the authorization service.
const MAX_GATE_RESPONSE_BYTES: usize = 64 * 1024;
/// Maximum printable characters exposed from a policy denial reason.
const MAX_DENIAL_REASON_CHARS: usize = 512;

/// Result of an Eidolon gate check.
#[derive(Debug)]
pub enum GateVerdict {
    /// Action is permitted.
    Allow,
    /// Action was denied. Contains the human-readable reason from Eidolon.
    Deny(String),
    /// Gate check failed (network, response body, status, or parse error).
    Error(String),
}

/// HTTP client for the Eidolon gate service.
pub struct GateClient {
    /// Base URL of the Eidolon service (no trailing slash).
    pub base_url: String,
    /// Shared reqwest client for connection-pool reuse.
    pub http: Client,
}

/// Constructs the gate client and evaluates actions through Eidolon.
impl GateClient {
    /// Construct a gate client pointing at `base_url`.
    pub fn new(base_url: &str, http: Client) -> Self {
        Self {
            base_url: base_url.to_string(),
            http,
        }
    }

    /// Check whether an action is permitted. Operational failures and invalid
    /// responses return `GateVerdict::Error` so callers can fail closed. The
    /// 2s timeout ensures unavailable gate checks cannot stall the pipeline.
    pub async fn check(&self, action: &str, context: &Value) -> GateVerdict {
        let url = format!("{}/gate/check", self.base_url.trim_end_matches('/'));
        let body = json!({ "action": action, "context": context });

        let req = self
            .http
            .post(&url)
            .timeout(Duration::from_secs(2))
            .json(&body);

        let mut resp = match req.send().await {
            Ok(r) => r,
            Err(e) => {
                let detail = format!("request failed: {e}");
                warn!(action, error = %detail, "gate check failed");
                return GateVerdict::Error(detail);
            }
        };

        let status = resp.status();
        if !status.is_success() {
            let detail = format!("unexpected HTTP status {status}");
            warn!(action, error = %detail, "gate check returned non-success status");
            return GateVerdict::Error(detail);
        }

        let text = match read_limited_body(&mut resp).await {
            Ok(text) => text,
            Err(detail) => {
                warn!(action, error = %detail, "gate check body read failed");
                return GateVerdict::Error(detail);
            }
        };

        let parsed: Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(e) => {
                let detail = format!("malformed JSON response: {e}");
                warn!(action, error = %detail, "gate check parse failed");
                return GateVerdict::Error(detail);
            }
        };

        // Expect {"allow": true/false, "reason": "..."}
        match parsed.get("allow").and_then(|v| v.as_bool()) {
            Some(true) => GateVerdict::Allow,
            Some(false) => {
                let reason = sanitize_denial_reason(
                    parsed
                        .get("reason")
                        .and_then(|r| r.as_str())
                        .unwrap_or("denied by gate"),
                );
                GateVerdict::Deny(reason)
            }
            None => {
                let detail = "response missing boolean 'allow' field".to_string();
                warn!(action, error = %detail, "gate check response was invalid");
                GateVerdict::Error(detail)
            }
        }
    }
}

/// Bound and strip control characters from a policy reason before exposure.
fn sanitize_denial_reason(reason: &str) -> String {
    let sanitized = reason
        .chars()
        .filter(|character| !character.is_control())
        .take(MAX_DENIAL_REASON_CHARS)
        .collect::<String>();
    let sanitized = sanitized.trim();
    if sanitized.is_empty() {
        "denied by gate".to_string()
    } else {
        sanitized.to_string()
    }
}

/// Read one successful gate response without permitting unbounded allocation.
async fn read_limited_body(response: &mut reqwest::Response) -> Result<String, String> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_GATE_RESPONSE_BYTES as u64)
    {
        return Err(format!(
            "response body exceeds {MAX_GATE_RESPONSE_BYTES} bytes"
        ));
    }

    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| format!("response body read failed: {error}"))?
    {
        if body.len().saturating_add(chunk.len()) > MAX_GATE_RESPONSE_BYTES {
            return Err(format!(
                "response body exceeds {MAX_GATE_RESPONSE_BYTES} bytes"
            ));
        }
        body.extend_from_slice(&chunk);
    }
    String::from_utf8(body).map_err(|_| "response body is not valid UTF-8".to_string())
}

#[cfg(test)]
/// Focused gate-client behavior tests.
mod tests {
    use super::{GateClient, GateVerdict};
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// Invoke the gate client against one mocked response.
    async fn verdict_for(response: ResponseTemplate) -> GateVerdict {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/gate/check"))
            .respond_with(response)
            .mount(&server)
            .await;

        GateClient::new(&server.uri(), reqwest::Client::new())
            .check("llm.call", &json!({"turn": 0}))
            .await
    }

    /// An explicit `allow: true` response remains allowed.
    #[tokio::test]
    async fn check_preserves_allow() {
        assert!(matches!(
            verdict_for(ResponseTemplate::new(200).set_body_json(json!({"allow": true}))).await,
            GateVerdict::Allow
        ));
    }

    /// An explicit `allow: false` response preserves the denial reason.
    #[tokio::test]
    async fn check_preserves_deny() {
        match verdict_for(
            ResponseTemplate::new(200)
                .set_body_json(json!({"allow": false, "reason": "policy blocked"})),
        )
        .await
        {
            GateVerdict::Deny(reason) => assert_eq!(reason, "policy blocked"),
            other => panic!("expected GateVerdict::Deny, got {other:?}"),
        }
    }

    /// Denial reasons cannot inject controls or exceed the public error bound.
    #[tokio::test]
    async fn check_sanitizes_denial_reason() {
        let reason = format!(
            "\u{1b}[31m{}\n",
            "x".repeat(super::MAX_DENIAL_REASON_CHARS + 10)
        );
        match verdict_for(
            ResponseTemplate::new(200).set_body_json(json!({"allow": false, "reason": reason})),
        )
        .await
        {
            GateVerdict::Deny(reason) => {
                assert!(!reason.chars().any(char::is_control));
                assert_eq!(reason.chars().count(), super::MAX_DENIAL_REASON_CHARS);
            }
            other => panic!("expected GateVerdict::Deny, got {other:?}"),
        }
    }

    /// Non-success, malformed, and missing-allow responses all fail closed.
    #[tokio::test]
    async fn check_returns_error_for_invalid_responses() {
        for response in [
            ResponseTemplate::new(503).set_body_string("unavailable"),
            ResponseTemplate::new(200).set_body_string("{not json"),
            ResponseTemplate::new(200).set_body_json(json!({"reason": "missing allow"})),
            ResponseTemplate::new(200)
                .set_body_bytes(vec![b'x'; super::MAX_GATE_RESPONSE_BYTES + 1]),
        ] {
            assert!(matches!(verdict_for(response).await, GateVerdict::Error(_)));
        }
    }
}
