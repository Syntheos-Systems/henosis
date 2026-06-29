//! HTTP client for the credd credential daemon.
//!
//! Adapters call `CreddClient::fetch_token` to resolve OAuth bearer tokens and
//! `fetch_raw_secret` for non-OAuth secrets (e.g. webhook signing keys). The
//! client also wires into `RefreshRegistry` so the background OAuth refresh
//! daemon knows which (tenant, provider) pairs to keep alive.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use crate::oauth_refresh::RefreshRegistry;

/// Errors returned by credd credential resolution calls.
#[derive(Debug, Error)]
pub enum CreddError {
    /// The credd daemon could not be reached over the network.
    #[error("credd unreachable at {url}: {source}")]
    Unreachable {
        /// The URL that was attempted.
        url: String,
        #[source]
        source: reqwest::Error,
    },
    /// The tenant has no provisioned credential for this provider.
    #[error("tenant not authorized for provider {provider} (credd slot {category}/{name})")]
    TenantNotAuthorized {
        /// The provider name (e.g. "google", "github").
        provider: String,
        /// The credd category (e.g. "google_oauth").
        category: String,
        /// The credd slot name (typically the tenant ID).
        name: String,
    },
    /// No `HERMES_CREDD_TOKEN` is set; the client cannot authenticate.
    #[error("credd auth missing -- set HERMES_CREDD_TOKEN")]
    AuthMissing,
    /// Credd returned a non-success HTTP status.
    #[error("credd returned {status}: {body}")]
    Upstream {
        /// HTTP status code.
        status: u16,
        /// Response body (truncated to 512 bytes).
        body: String,
    },
    /// The credd response did not contain an `access_token` or `value` field.
    #[error("credd response missing access_token field")]
    MalformedResponse,
}

/// Wire format for the credd `/resolve/raw` request body.
#[derive(Debug, Serialize)]
struct RawRequest<'a> {
    category: &'a str,
    name: &'a str,
}

/// Wire format for the credd `/resolve/raw` response body.
#[derive(Debug, Deserialize)]
struct RawResponse {
    #[allow(dead_code)]
    category: String,
    #[allow(dead_code)]
    name: String,
    value: Value,
}

/// Wire format for the credd `/secret/{category}/{name}` PUT request body.
#[derive(Debug, Serialize)]
struct StoreRequest<'a> {
    data: &'a Value,
}

/// HTTP client for the credd credential daemon. Cloneable; intended to be
/// shared via `Arc<CreddClient>` across all in-flight invocations.
#[derive(Debug, Clone)]
pub struct CreddClient {
    /// Shared HTTP client with connect and read timeouts.
    http: reqwest::Client,
    /// Base URL of the credd daemon.
    base_url: String,
    /// Bearer token for authenticating to credd.
    token: Option<String>,
    /// Optional refresh registry to register (tenant, provider) pairs after a
    /// successful token fetch.
    refresh_registry: Option<RefreshRegistry>,
}

impl CreddClient {
    /// Construct a new client for the given credd base URL and optional bearer
    /// token.
    pub fn new(base_url: String, token: Option<String>) -> Self {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(2))
            .timeout(Duration::from_secs(15))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("reqwest client build");
        Self {
            http,
            base_url,
            token,
            refresh_registry: None,
        }
    }

    /// Attach an OAuth refresh registry. Each successful `fetch_token` call
    /// registers `(tenant_id, provider)` so the background refresh daemon
    /// knows what to keep alive.
    pub fn with_refresh_registry(mut self, registry: RefreshRegistry) -> Self {
        self.refresh_registry = Some(registry);
        self
    }

    /// Fetch the OAuth bearer token for a tenant + provider.
    ///
    /// Slot mapping: category=`{provider}_oauth`, name=`{tenant_id}`.
    /// SecretData primary value is read as a JSON string, or as `.access_token` if structured.
    pub async fn fetch_token(&self, tenant_id: &str, provider: &str) -> Result<String, CreddError> {
        let token = self.token.as_deref().ok_or(CreddError::AuthMissing)?;

        let category = format!("{provider}_oauth");
        let url = format!("{}/resolve/raw", self.base_url.trim_end_matches('/'));

        let resp = self
            .http
            .post(&url)
            .bearer_auth(token)
            .json(&RawRequest {
                category: &category,
                name: tenant_id,
            })
            .send()
            .await
            .map_err(|source| CreddError::Unreachable {
                url: url.clone(),
                source,
            })?;

        let status = resp.status();
        if status.as_u16() == 404 {
            return Err(CreddError::TenantNotAuthorized {
                provider: provider.to_string(),
                category,
                name: tenant_id.to_string(),
            });
        }
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(CreddError::Upstream {
                status: status.as_u16(),
                body: truncate(&body, 512),
            });
        }

        let parsed: RawResponse = resp.json().await.map_err(|_| CreddError::MalformedResponse)?;

        // SecretData shapes we accept:
        // 1. JSON object with "access_token": "..."
        // 2. JSON object with "value": "..."  (legacy)
        // 3. Plain JSON string
        let token_str = match &parsed.value {
            Value::Object(map) => {
                if let Some(Value::String(s)) = map.get("access_token") {
                    s.clone()
                } else if let Some(Value::String(s)) = map.get("value") {
                    s.clone()
                } else {
                    return Err(CreddError::MalformedResponse);
                }
            }
            Value::String(s) => s.clone(),
            _ => return Err(CreddError::MalformedResponse),
        };

        if let Some(reg) = &self.refresh_registry {
            reg.register(tenant_id, provider).await;
        }
        Ok(token_str)
    }

    /// Resolve a raw secret string from an arbitrary `category`/`name` slot.
    ///
    /// Unlike [`fetch_token`](Self::fetch_token), which is OAuth-shaped
    /// (`{provider}_oauth`), this is the generic path used for non-OAuth secrets
    /// such as webhook signing secrets (`webhooks`/`{provider}-secret`). Accepts
    /// a plain JSON string, or an object carrying `value`/`access_token`.
    pub async fn fetch_raw_secret(
        &self,
        category: &str,
        name: &str,
    ) -> Result<String, CreddError> {
        let token = self.token.as_deref().ok_or(CreddError::AuthMissing)?;
        let url = format!("{}/resolve/raw", self.base_url.trim_end_matches('/'));

        let resp = self
            .http
            .post(&url)
            .bearer_auth(token)
            .json(&RawRequest { category, name })
            .send()
            .await
            .map_err(|source| CreddError::Unreachable {
                url: url.clone(),
                source,
            })?;

        let status = resp.status();
        if status.as_u16() == 404 {
            return Err(CreddError::TenantNotAuthorized {
                provider: name.to_string(),
                category: category.to_string(),
                name: name.to_string(),
            });
        }
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(CreddError::Upstream {
                status: status.as_u16(),
                body: truncate(&body, 512),
            });
        }

        let parsed: RawResponse = resp.json().await.map_err(|_| CreddError::MalformedResponse)?;
        match parsed.value {
            Value::String(s) => Ok(s),
            Value::Object(map) => map
                .get("value")
                .or_else(|| map.get("access_token"))
                .and_then(|v| v.as_str().map(String::from))
                .ok_or(CreddError::MalformedResponse),
            _ => Err(CreddError::MalformedResponse),
        }
    }

    /// Fetch the full secret record (access_token + refresh_token +
    /// expires_at + any other fields). Used by the OAuth refresh daemon.
    pub async fn fetch_full_record(
        &self,
        tenant_id: &str,
        provider: &str,
    ) -> Result<Value, CreddError> {
        let token = self.token.as_deref().ok_or(CreddError::AuthMissing)?;
        let category = format!("{provider}_oauth");
        let url = format!("{}/resolve/raw", self.base_url.trim_end_matches('/'));

        let resp = self
            .http
            .post(&url)
            .bearer_auth(token)
            .json(&RawRequest {
                category: &category,
                name: tenant_id,
            })
            .send()
            .await
            .map_err(|source| CreddError::Unreachable {
                url: url.clone(),
                source,
            })?;

        let status = resp.status();
        if status.as_u16() == 404 {
            return Err(CreddError::TenantNotAuthorized {
                provider: provider.to_string(),
                category,
                name: tenant_id.to_string(),
            });
        }
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(CreddError::Upstream {
                status: status.as_u16(),
                body: truncate(&body, 512),
            });
        }
        let parsed: RawResponse = resp.json().await.map_err(|_| CreddError::MalformedResponse)?;
        Ok(parsed.value)
    }

    /// Write a refreshed secret back via PUT /secret/{category}/{name}. The
    /// credd token must have master rights for this endpoint to accept the
    /// update; otherwise the call fails with 401/403 and the caller logs a
    /// warning.
    pub async fn update_secret(
        &self,
        tenant_id: &str,
        provider: &str,
        new_value: &Value,
    ) -> Result<(), CreddError> {
        let token = self.token.as_deref().ok_or(CreddError::AuthMissing)?;
        let category = format!("{provider}_oauth");
        let url = format!(
            "{}/secret/{}/{}",
            self.base_url.trim_end_matches('/'),
            category,
            tenant_id
        );

        let resp = self
            .http
            .put(&url)
            .bearer_auth(token)
            .json(&StoreRequest { data: new_value })
            .send()
            .await
            .map_err(|source| CreddError::Unreachable {
                url: url.clone(),
                source,
            })?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(CreddError::Upstream {
                status: status.as_u16(),
                body: truncate(&body, 512),
            });
        }
        Ok(())
    }
}

/// Truncate a string to `max` bytes, appending `...[truncated]` when clipped.
fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let mut out = s[..max].to_string();
        out.push_str("...[truncated]");
        out
    }
}
