//! Best-effort Axon event publishing.
//!
//! A small shared client over the Kleos Axon ingest endpoint
//! (`POST {AXON_URL}/axon/publish`). Publishing is fire-and-forget: a failure is
//! logged and swallowed so observability never blocks or fails a tool
//! invocation. This consolidates the inline publishers that previously lived in
//! `circuit` and `rate_limit`; new P4/P5 events (audit batches, tool/oauth
//! events, normalized webhooks) all go through here.

use std::time::Duration;

use serde_json::{json, Value};
use tracing::warn;

/// A reusable best-effort publisher for Axon events.
#[derive(Clone)]
pub struct AxonPublisher {
    /// Base Axon URL (`AXON_URL`); `None` disables publishing.
    url: Option<String>,
    /// Shared HTTP client with short connect/read timeouts.
    http: reqwest::Client,
}

impl AxonPublisher {
    /// Construct a publisher, reading `AXON_URL` from the environment.
    pub fn from_env() -> Self {
        Self {
            url: std::env::var("AXON_URL").ok(),
            http: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(2))
                .timeout(Duration::from_secs(5))
                .build()
                .expect("reqwest client build"),
        }
    }

    /// Whether publishing is enabled (an `AXON_URL` is configured).
    pub fn enabled(&self) -> bool {
        self.url.is_some()
    }

    /// Emit `hermes.tool.invoked` for every completed invocation.
    pub fn tool_invoked(
        &self,
        tool_id: &str,
        tenant_id: Option<&str>,
        outcome: &str,
        duration_ms: u64,
    ) {
        self.publish(
            "hermes.tool",
            "hermes.tool.invoked",
            json!({
                "tool_id": tool_id,
                "tenant_id": tenant_id,
                "outcome": outcome,
                "duration_ms": duration_ms,
            }),
        );
    }

    /// Emit `hermes.tool.failed` when an invocation errored after retries.
    pub fn tool_failed(&self, tool_id: &str, error_code: Option<&str>, retries_attempted: u32) {
        self.publish(
            "hermes.tool",
            "hermes.tool.failed",
            json!({
                "tool_id": tool_id,
                "error_code": error_code,
                "retries_attempted": retries_attempted,
            }),
        );
    }

    /// Publish one event on `channel` with `action` and `payload`. Spawns the
    /// request and returns immediately; a non-2xx or transport error is logged,
    /// never propagated.
    pub fn publish(&self, channel: &str, action: &str, payload: Value) {
        let Some(base) = &self.url else {
            return;
        };
        let url = format!("{}/axon/publish", base.trim_end_matches('/'));
        let body = json!({
            "channel": channel,
            "action": action,
            "payload": payload,
            "source": "hermes",
        });
        let req = self.http.post(&url).json(&body).send();
        let action = action.to_string();
        tokio::spawn(async move {
            match req.await {
                Ok(r) if r.status().is_success() => {}
                Ok(r) => warn!(status = %r.status(), %action, "axon publish non-2xx"),
                Err(e) => warn!(error = %e, %action, "axon publish failed"),
            }
        });
    }
}

impl Default for AxonPublisher {
    /// Construct from environment, same as [`AxonPublisher::from_env`].
    fn default() -> Self {
        Self::from_env()
    }
}
