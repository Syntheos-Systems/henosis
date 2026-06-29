//! Eidolon gate client. All checks are best-effort: network errors and
//! non-2xx responses always produce `Allow` so the pipeline is never blocked
//! by a missing or degraded gate service.

use std::time::Duration;

use reqwest::Client;
use serde_json::{json, Value};
use tracing::warn;

/// Result of an Eidolon gate check. Gate is best-effort -- errors always
/// produce Allow so the pipeline is never blocked by a missing gate.
#[derive(Debug)]
#[allow(dead_code)]
pub enum GateVerdict {
    /// Action is permitted.
    Allow,
    /// Action was denied. Contains the human-readable reason from Eidolon.
    Deny(String),
    /// Gate check failed (network, parse, timeout). Treated as Allow.
    Error(String),
}

/// HTTP client for the Eidolon gate service.
pub struct GateClient {
    /// Base URL of the Eidolon service (no trailing slash).
    pub base_url: String,
    /// Shared reqwest client for connection-pool reuse.
    pub http: Client,
}

impl GateClient {
    /// Construct a gate client pointing at `base_url`.
    pub fn new(base_url: &str, http: Client) -> Self {
        Self {
            base_url: base_url.to_string(),
            http,
        }
    }

    /// Check whether an action is permitted. On any error (connection refused,
    /// timeout, parse failure) returns GateVerdict::Allow with a warn! log.
    /// The 2s timeout ensures gate checks never stall the pipeline.
    pub async fn check(&self, action: &str, context: &Value) -> GateVerdict {
        let url = format!("{}/gate/check", self.base_url.trim_end_matches('/'));
        let body = json!({ "action": action, "context": context });

        let req = self
            .http
            .post(&url)
            .timeout(Duration::from_secs(2))
            .json(&body);

        let resp = match req.send().await {
            Ok(r) => r,
            Err(e) => {
                warn!(action, error = %e, "gate check failed -- allowing (best-effort)");
                return GateVerdict::Allow;
            }
        };

        let status = resp.status();
        let text = match resp.text().await {
            Ok(t) => t,
            Err(e) => {
                warn!(action, error = %e, "gate check body read failed -- allowing");
                return GateVerdict::Allow;
            }
        };

        if !status.is_success() {
            warn!(action, %status, body = %text, "gate check non-2xx -- allowing");
            return GateVerdict::Allow;
        }

        let parsed: Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(e) => {
                warn!(action, error = %e, "gate check parse failed -- allowing");
                return GateVerdict::Allow;
            }
        };

        // Expect {"allow": true/false, "reason": "..."}
        match parsed.get("allow").and_then(|v| v.as_bool()) {
            Some(true) => GateVerdict::Allow,
            Some(false) => {
                let reason = parsed
                    .get("reason")
                    .and_then(|r| r.as_str())
                    .unwrap_or("denied by gate")
                    .to_string();
                GateVerdict::Deny(reason)
            }
            None => {
                warn!(action, "gate check response missing 'allow' field -- allowing");
                GateVerdict::Allow
            }
        }
    }
}
