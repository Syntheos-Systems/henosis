//! Background OAuth refresh daemon.
//!
//! Each successful `CreddClient::fetch_token` call registers the
//! `(tenant_id, provider)` pair in a `RefreshRegistry`. The daemon ticks
//! every `interval` seconds (default 60s), inspects each registered slot's
//! `expires_at`, and if it's within `skew` of now, calls the provider's
//! refresh endpoint and writes the new bundle back via credd.
//!
//! Currently implements Google OAuth refresh (used by gmail/gdrive/gcal).
//! GitHub PATs and Slack bot tokens generally do not expire, so they are
//! skipped unless an `expires_at` is present.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

use crate::credd_client::{CreddClient, CreddError};

/// Thread-safe set of `(tenant_id, provider)` pairs that need periodic OAuth
/// refresh. Populated by `CreddClient::fetch_token` and consumed by
/// `OAuthRefreshDaemon`.
#[derive(Debug, Clone, Default)]
pub struct RefreshRegistry {
    /// Inner set protected by an async read-write lock.
    inner: Arc<RwLock<HashSet<(String, String)>>>,
}

impl RefreshRegistry {
    /// Register a `(tenant_id, provider)` pair for periodic refresh. Idempotent.
    pub async fn register(&self, tenant_id: &str, provider: &str) {
        let mut guard = self.inner.write().await;
        guard.insert((tenant_id.to_string(), provider.to_string()));
    }

    /// Snapshot the current set as a `Vec` for one daemon tick.
    pub async fn snapshot(&self) -> Vec<(String, String)> {
        self.inner.read().await.iter().cloned().collect()
    }

    /// Current number of registered pairs.
    pub async fn len(&self) -> usize {
        self.inner.read().await.len()
    }
}

/// Google OAuth client credentials used for token refresh.
#[derive(Debug, Clone)]
pub struct GoogleClient {
    /// Google OAuth client ID.
    pub client_id: String,
    /// Google OAuth client secret.
    pub client_secret: String,
}

impl GoogleClient {
    /// Read `HERMES_GOOGLE_CLIENT_ID` and `HERMES_GOOGLE_CLIENT_SECRET` from
    /// the environment. Returns `None` when either is absent or empty.
    pub fn from_env() -> Option<Self> {
        let client_id = std::env::var("HERMES_GOOGLE_CLIENT_ID").ok()?;
        let client_secret = std::env::var("HERMES_GOOGLE_CLIENT_SECRET").ok()?;
        if client_id.is_empty() || client_secret.is_empty() {
            return None;
        }
        Some(Self {
            client_id,
            client_secret,
        })
    }
}

/// Background daemon that keeps registered OAuth tokens from expiring.
#[derive(Debug, Clone)]
pub struct OAuthRefreshDaemon {
    /// Source of (tenant, provider) pairs to refresh.
    pub registry: RefreshRegistry,
    /// Credd client for reading and writing token records.
    pub credd: Arc<CreddClient>,
    /// Axon URL for publishing refresh events; `None` disables publishing.
    pub axon_url: Option<String>,
    /// Google OAuth client credentials; `None` disables Google refresh.
    pub google: Option<GoogleClient>,
    /// How often the daemon ticks (default 60s).
    pub interval: Duration,
    /// How far ahead of expiry to trigger a refresh (default 5 minutes).
    pub skew: Duration,
    /// Shared HTTP client for upstream refresh calls.
    pub http: reqwest::Client,
}

impl OAuthRefreshDaemon {
    /// Construct a daemon from the given registry and credd client, reading
    /// `AXON_URL` and Google credentials from the environment.
    pub fn new(registry: RefreshRegistry, credd: Arc<CreddClient>) -> Self {
        Self {
            registry,
            credd,
            axon_url: std::env::var("AXON_URL").ok(),
            google: GoogleClient::from_env(),
            interval: Duration::from_secs(60),
            skew: Duration::from_secs(5 * 60),
            http: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(2))
                .timeout(Duration::from_secs(15))
                .build()
                .expect("reqwest client build"),
        }
    }

    /// Spawn the daemon onto the current tokio runtime. Returns the join
    /// handle so callers can supervise it if they want.
    pub fn spawn(self) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            if self.google.is_none() {
                info!(
                    "oauth refresh daemon: HERMES_GOOGLE_CLIENT_ID/SECRET not set -- \
                     google refresh disabled"
                );
            }
            let mut ticker = tokio::time::interval(self.interval);
            // First tick fires immediately; skip it so we don't slam credd
            // before any tokens have been registered.
            ticker.tick().await;
            loop {
                ticker.tick().await;
                self.tick_once().await;
            }
        })
    }

    /// Run one refresh cycle: snapshot the registry and attempt to refresh
    /// each (tenant, provider) pair whose token is near expiry.
    pub async fn tick_once(&self) {
        let targets = self.registry.snapshot().await;
        if targets.is_empty() {
            return;
        }
        debug!(count = targets.len(), "oauth refresh tick");
        for (tenant, provider) in targets {
            if let Err(e) = self.refresh_one(&tenant, &provider).await {
                warn!(%tenant, %provider, error = %e, "refresh attempt failed");
                self.publish_refresh_failed(&tenant, &provider, &e.to_string())
                    .await;
            }
        }
    }

    /// Attempt to refresh one (tenant, provider) token. Reads the current
    /// record from credd, checks expiry, calls the provider's refresh endpoint,
    /// and writes the merged bundle back.
    async fn refresh_one(&self, tenant: &str, provider: &str) -> Result<(), RefreshError> {
        let record = self
            .credd
            .fetch_full_record(tenant, provider)
            .await
            .map_err(|e| RefreshError::Credd(e.to_string()))?;

        let expires_at = record
            .get("expires_at")
            .and_then(|v| v.as_str())
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|d| d.with_timezone(&Utc));

        let Some(expiry) = expires_at else {
            // No expiry recorded -- nothing to do (treat as non-expiring).
            return Ok(());
        };

        let now = Utc::now();
        let until = expiry.signed_duration_since(now);
        if until.num_seconds() > self.skew.as_secs() as i64 {
            return Ok(());
        }

        let refresh_token = record
            .get("refresh_token")
            .and_then(|v| v.as_str())
            .ok_or(RefreshError::NoRefreshToken)?
            .to_string();

        let new_bundle = match provider {
            "google" => {
                let client = self
                    .google
                    .as_ref()
                    .ok_or(RefreshError::ProviderClientMissing)?;
                self.google_refresh(client, &refresh_token).await?
            }
            other => return Err(RefreshError::UnsupportedProvider(other.to_string())),
        };

        // Carry over fields the provider response did not return (for example
        // the original refresh_token if Google omits it from the response).
        let merged = merge_refresh_response(&record, &new_bundle);

        self.credd
            .update_secret(tenant, provider, &merged)
            .await
            .map_err(|e| RefreshError::Credd(e.to_string()))?;

        info!(%tenant, %provider, "oauth token refreshed");
        self.publish_axon(tenant, provider).await;
        Ok(())
    }

    /// Call the Google token refresh endpoint and return the new token bundle.
    async fn google_refresh(
        &self,
        client: &GoogleClient,
        refresh_token: &str,
    ) -> Result<Value, RefreshError> {
        let resp = self
            .http
            .post("https://oauth2.googleapis.com/token")
            .form(&[
                ("client_id", client.client_id.as_str()),
                ("client_secret", client.client_secret.as_str()),
                ("refresh_token", refresh_token),
                ("grant_type", "refresh_token"),
            ])
            .send()
            .await
            .map_err(|e| RefreshError::Provider(e.to_string()))?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(RefreshError::Provider(format!(
                "google returned HTTP {status}: {body}"
            )));
        }
        serde_json::from_str(&body).map_err(|e| RefreshError::Provider(e.to_string()))
    }

    /// Publish a `hermes.oauth.refreshed` event to Axon after a successful
    /// refresh. Best-effort; errors are logged and swallowed.
    async fn publish_axon(&self, tenant: &str, provider: &str) {
        let Some(axon_url) = &self.axon_url else {
            return;
        };
        let url = format!("{}/axon/publish", axon_url.trim_end_matches('/'));
        let body = json!({
            "channel": "hermes.oauth",
            "action": "hermes.oauth.refreshed",
            "payload": {
                "tenant_id": tenant,
                "provider": provider,
            },
            "source": "hermes",
        });
        match self.http.post(&url).json(&body).send().await {
            Ok(r) if r.status().is_success() => {}
            Ok(r) => warn!(status = %r.status(), "axon publish non-2xx"),
            Err(e) => warn!(error = %e, "axon publish failed"),
        }
    }

    /// Best-effort `hermes.oauth.refresh_failed` event when a token refresh
    /// attempt errors out.
    async fn publish_refresh_failed(&self, tenant: &str, provider: &str, error: &str) {
        let Some(axon_url) = &self.axon_url else {
            return;
        };
        let url = format!("{}/axon/publish", axon_url.trim_end_matches('/'));
        let body = json!({
            "channel": "hermes.oauth",
            "action": "hermes.oauth.refresh_failed",
            "payload": {
                "tenant_id": tenant,
                "provider": provider,
                "error": error,
            },
            "source": "hermes",
        });
        match self.http.post(&url).json(&body).send().await {
            Ok(r) if r.status().is_success() => {}
            Ok(r) => warn!(status = %r.status(), "axon publish non-2xx"),
            Err(e) => warn!(error = %e, "axon publish failed"),
        }
    }
}

/// Errors that can occur during an OAuth token refresh attempt.
#[derive(Debug, thiserror::Error)]
pub enum RefreshError {
    /// The stored token record has no `refresh_token` field.
    #[error("no refresh_token in stored record")]
    NoRefreshToken,
    /// No provider client credentials are configured.
    #[error("no client configured for this provider")]
    ProviderClientMissing,
    /// The provider is not supported by the refresh daemon.
    #[error("unsupported provider: {0}")]
    UnsupportedProvider(String),
    /// The upstream provider returned an error.
    #[error("provider error: {0}")]
    Provider(String),
    /// A credd operation failed.
    #[error("credd error: {0}")]
    Credd(String),
}

/// Merge the OAuth refresh response with the prior secret record so we keep
/// any fields the provider did not echo back (notably `refresh_token` for
/// Google, which often returns only a new `access_token` + `expires_in`).
/// Computes a new `expires_at` from `expires_in` if present.
fn merge_refresh_response(prior: &Value, response: &Value) -> Value {
    let mut merged = prior.clone();
    let merged_obj = match merged.as_object_mut() {
        Some(m) => m,
        None => return response.clone(),
    };
    let resp_obj = match response.as_object() {
        Some(m) => m,
        None => return prior.clone(),
    };
    for (k, v) in resp_obj {
        merged_obj.insert(k.clone(), v.clone());
    }
    if let Some(seconds) = resp_obj.get("expires_in").and_then(|v| v.as_i64()) {
        let new_expires =
            (Utc::now() + chrono::Duration::seconds(seconds.saturating_sub(30))).to_rfc3339();
        merged_obj.insert("expires_at".to_string(), Value::String(new_expires));
    }
    Value::Object(merged_obj.clone())
}

#[allow(unused)]
fn _ignored(c: &CreddError) {
    // Silence dead-code lint when no provider is configured at compile time.
    let _ = c;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn registry_dedups() {
        let r = RefreshRegistry::default();
        r.register("t1", "google").await;
        r.register("t1", "google").await;
        r.register("t2", "google").await;
        assert_eq!(r.len().await, 2);
    }

    #[test]
    fn merge_carries_refresh_token() {
        let prior = json!({
            "access_token": "old",
            "refresh_token": "rt-1",
            "expires_at": "2020-01-01T00:00:00Z"
        });
        let response = json!({
            "access_token": "new",
            "expires_in": 3600,
            "token_type": "Bearer"
        });
        let merged = merge_refresh_response(&prior, &response);
        let m = merged.as_object().unwrap();
        assert_eq!(m.get("access_token").unwrap().as_str(), Some("new"));
        assert_eq!(m.get("refresh_token").unwrap().as_str(), Some("rt-1"));
        assert!(m.contains_key("expires_at"));
        assert_ne!(
            m.get("expires_at").unwrap().as_str(),
            Some("2020-01-01T00:00:00Z")
        );
    }
}
