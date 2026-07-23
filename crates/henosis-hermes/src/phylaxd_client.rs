//! HTTP client for the phylaxd credential authority.
//!
//! Adapters call `PhylaxdClient::fetch_token` to resolve OAuth bearer tokens and
//! `fetch_raw_secret` for non-OAuth secrets (e.g. webhook signing keys). The
//! client also wires into `RefreshRegistry` so the background OAuth refresh
//! daemon knows which (tenant, provider) pairs to keep alive.

use std::time::Duration;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use zeroize::Zeroizing;

use crate::oauth_refresh::RefreshRegistry;

/// Bounds ordinary credential-broker requests.
const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

/// Allows phylaxd's 20-second child deadline to return before its 30-second request cutoff.
const EXEC_REQUEST_TIMEOUT: Duration = Duration::from_secs(25);

/// Errors returned by phylaxd credential resolution calls.
#[derive(Debug, Error)]
pub enum PhylaxdError {
    /// The phylaxd daemon could not be reached over the network.
    #[error("phylaxd unreachable at {url}: {source}")]
    Unreachable {
        /// The URL that was attempted.
        url: String,
        #[source]
        source: reqwest::Error,
    },
    /// The tenant has no provisioned credential for this provider.
    #[error("tenant not authorized for provider {provider} (phylaxd slot {category}/{name})")]
    TenantNotAuthorized {
        /// The provider name (e.g. "google", "github").
        provider: String,
        /// The phylaxd category (e.g. "google_oauth").
        category: String,
        /// The phylaxd slot name (typically the tenant ID).
        name: String,
    },
    /// No `HERMES_PHYLAXD_TOKEN` is set; the client cannot authenticate.
    #[error("phylaxd auth missing -- set HERMES_PHYLAXD_TOKEN")]
    AuthMissing,
    /// Phylaxd returned a non-success HTTP status.
    #[error("phylaxd returned HTTP {status}")]
    Upstream {
        /// HTTP status code.
        status: u16,
    },
    /// The phylaxd response did not contain the expected operation fields.
    #[error("phylaxd response is malformed")]
    MalformedResponse,
}

/// Wire format for the phylaxd `/resolve/raw` request body.
#[derive(Debug, Serialize)]
struct RawRequest<'a> {
    category: &'a str,
    name: &'a str,
}

/// Wire format for the phylaxd `/resolve/raw` response body.
#[derive(Deserialize)]
struct RawResponse {
    #[allow(dead_code)]
    category: String,
    #[allow(dead_code)]
    name: String,
    value: Value,
}

/// Wire format for the phylaxd `/secret/{category}/{name}` PUT request body.
#[derive(Serialize)]
struct StoreRequest<'a> {
    data: &'a Value,
}

/// Wire format for a phylaxd sign request.
#[derive(Serialize)]
struct SignRequest<'a> {
    category: &'a str,
    name: &'a str,
    payload_b64: &'a str,
    algo: &'a str,
}

/// Successful result from a phylaxd sign operation.
#[derive(Debug, Deserialize)]
pub struct SignResult {
    /// Base64-encoded signature produced without releasing the signing key.
    pub signature_b64: String,
}

/// Wire format for a phylaxd verify request.
#[derive(Serialize)]
struct VerifyRequest<'a> {
    category: &'a str,
    name: &'a str,
    payload_b64: &'a str,
    signature_b64: &'a str,
    algo: &'a str,
}

/// Successful result from a phylaxd verify operation.
#[derive(Debug, Deserialize)]
pub struct VerifyResult {
    /// Whether the supplied signature is valid for the stored key.
    pub valid: bool,
}

/// Wire format for a phylaxd derive request.
#[derive(Debug, Serialize)]
struct DeriveRequest<'a> {
    category: &'a str,
    name: &'a str,
    purpose: &'a str,
    length: usize,
}

/// Successful result from a phylaxd derive operation.
#[derive(Deserialize)]
pub struct DeriveResult {
    /// Base64-encoded, domain-separated derived key material.
    pub derived_b64: String,
}

/// Wire format for a phylaxd exec request.
#[derive(Serialize)]
struct ExecRequest<'a> {
    category: &'a str,
    name: &'a str,
    argv: &'a [String],
    env_var: &'a str,
}

/// Successful result from a phylaxd mediated command execution.
#[derive(Deserialize)]
pub struct ExecResult {
    /// Whether the broker terminated the command at its deadline.
    pub timed_out: bool,
    /// Child exit code when one was available.
    pub exit_code: Option<i32>,
    /// Base64-encoded, broker-scrubbed standard output.
    pub stdout_b64: String,
    /// Base64-encoded, broker-scrubbed standard error.
    pub stderr_b64: String,
}

/// HTTP client for the phylaxd credential daemon, shared through `Arc` across
/// all in-flight invocations.
pub struct PhylaxdClient {
    /// Shared HTTP client with connect and read timeouts.
    http: reqwest::Client,
    /// Base URL of the phylaxd daemon.
    base_url: String,
    /// Bearer token for authenticating to phylaxd.
    token: Option<Zeroizing<String>>,
    /// Optional refresh registry to register (tenant, provider) pairs after a
    /// successful token fetch.
    refresh_registry: Option<RefreshRegistry>,
    /// Total request timeout reserved for broker-mediated command execution.
    exec_request_timeout: Duration,
}

/// Implements authenticated credential reads, refreshes, and updates against phylaxd.
impl PhylaxdClient {
    /// Construct a new client for the given phylaxd base URL and optional bearer
    /// token.
    pub fn new(base_url: String, token: Option<String>) -> Self {
        Self::new_with_timeouts(
            base_url,
            token,
            DEFAULT_REQUEST_TIMEOUT,
            EXEC_REQUEST_TIMEOUT,
        )
    }

    /// Build a client with explicit timeouts so contract tests can exercise deadline ordering.
    fn new_with_timeouts(
        base_url: String,
        token: Option<String>,
        default_request_timeout: Duration,
        exec_request_timeout: Duration,
    ) -> Self {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(2))
            .timeout(default_request_timeout)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("reqwest client build");
        Self {
            http,
            base_url,
            token: token.map(Zeroizing::new),
            refresh_registry: None,
            exec_request_timeout,
        }
    }

    /// Attach an OAuth refresh registry. Each successful `fetch_token` call
    /// registers `(tenant_id, provider)` so the background refresh daemon
    /// knows what to keep alive.
    pub fn with_refresh_registry(mut self, registry: RefreshRegistry) -> Self {
        self.refresh_registry = Some(registry);
        self
    }

    /// Sign a base64-encoded payload through phylaxd without exposing the key.
    pub async fn sign(
        &self,
        category: &str,
        name: &str,
        payload_b64: &str,
        algo: &str,
    ) -> Result<SignResult, PhylaxdError> {
        self.post_operation(
            "/resolve/sign",
            &SignRequest {
                category,
                name,
                payload_b64,
                algo,
            },
        )
        .await
    }

    /// Verify a base64-encoded signature through phylaxd without exposing the key.
    pub async fn verify(
        &self,
        category: &str,
        name: &str,
        payload_b64: &str,
        signature_b64: &str,
        algo: &str,
    ) -> Result<VerifyResult, PhylaxdError> {
        self.post_operation(
            "/resolve/verify",
            &VerifyRequest {
                category,
                name,
                payload_b64,
                signature_b64,
                algo,
            },
        )
        .await
    }

    /// Derive bounded key material through phylaxd without exposing the root secret.
    pub async fn derive(
        &self,
        category: &str,
        name: &str,
        purpose: &str,
        length: usize,
    ) -> Result<DeriveResult, PhylaxdError> {
        self.post_operation(
            "/resolve/derive",
            &DeriveRequest {
                category,
                name,
                purpose,
                length,
            },
        )
        .await
    }

    /// Run one broker-allowlisted command with a secret injected inside phylaxd.
    pub async fn exec(
        &self,
        category: &str,
        name: &str,
        argv: &[String],
        env_var: &str,
    ) -> Result<ExecResult, PhylaxdError> {
        self.post_operation_with_timeout(
            "/resolve/exec",
            &ExecRequest {
                category,
                name,
                argv,
                env_var,
            },
            Some(self.exec_request_timeout),
        )
        .await
    }

    /// Fetch the OAuth bearer token for a tenant + provider.
    ///
    /// Slot mapping: category=`{provider}_oauth`, name=`{tenant_id}`.
    /// SecretData primary value is read as a JSON string, or as `.access_token` if structured.
    pub async fn fetch_token(
        &self,
        tenant_id: &str,
        provider: &str,
    ) -> Result<String, PhylaxdError> {
        let token = self.token.as_deref().ok_or(PhylaxdError::AuthMissing)?;

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
            .map_err(|source| PhylaxdError::Unreachable {
                url: url.clone(),
                source,
            })?;

        let status = resp.status();
        if status.as_u16() == 404 {
            return Err(PhylaxdError::TenantNotAuthorized {
                provider: provider.to_string(),
                category,
                name: tenant_id.to_string(),
            });
        }
        if !status.is_success() {
            return Err(PhylaxdError::Upstream {
                status: status.as_u16(),
            });
        }

        let parsed: RawResponse = resp
            .json()
            .await
            .map_err(|_| PhylaxdError::MalformedResponse)?;

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
                    return Err(PhylaxdError::MalformedResponse);
                }
            }
            Value::String(s) => s.clone(),
            _ => return Err(PhylaxdError::MalformedResponse),
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
    ) -> Result<String, PhylaxdError> {
        let token = self.token.as_deref().ok_or(PhylaxdError::AuthMissing)?;
        let url = format!("{}/resolve/raw", self.base_url.trim_end_matches('/'));

        let resp = self
            .http
            .post(&url)
            .bearer_auth(token)
            .json(&RawRequest { category, name })
            .send()
            .await
            .map_err(|source| PhylaxdError::Unreachable {
                url: url.clone(),
                source,
            })?;

        let status = resp.status();
        if status.as_u16() == 404 {
            return Err(PhylaxdError::TenantNotAuthorized {
                provider: name.to_string(),
                category: category.to_string(),
                name: name.to_string(),
            });
        }
        if !status.is_success() {
            return Err(PhylaxdError::Upstream {
                status: status.as_u16(),
            });
        }

        let parsed: RawResponse = resp
            .json()
            .await
            .map_err(|_| PhylaxdError::MalformedResponse)?;
        match parsed.value {
            Value::String(s) => Ok(s),
            Value::Object(map) => map
                .get("value")
                .or_else(|| map.get("access_token"))
                .and_then(|v| v.as_str().map(String::from))
                .ok_or(PhylaxdError::MalformedResponse),
            _ => Err(PhylaxdError::MalformedResponse),
        }
    }

    /// Fetch the full secret record (access_token + refresh_token +
    /// expires_at + any other fields). Used by the OAuth refresh daemon.
    pub async fn fetch_full_record(
        &self,
        tenant_id: &str,
        provider: &str,
    ) -> Result<Value, PhylaxdError> {
        let token = self.token.as_deref().ok_or(PhylaxdError::AuthMissing)?;
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
            .map_err(|source| PhylaxdError::Unreachable {
                url: url.clone(),
                source,
            })?;

        let status = resp.status();
        if status.as_u16() == 404 {
            return Err(PhylaxdError::TenantNotAuthorized {
                provider: provider.to_string(),
                category,
                name: tenant_id.to_string(),
            });
        }
        if !status.is_success() {
            return Err(PhylaxdError::Upstream {
                status: status.as_u16(),
            });
        }
        let parsed: RawResponse = resp
            .json()
            .await
            .map_err(|_| PhylaxdError::MalformedResponse)?;
        Ok(parsed.value)
    }

    /// Write a refreshed secret back via PUT /secret/{category}/{name}. The
    /// phylaxd token must have master rights for this endpoint to accept the
    /// update; otherwise the call fails with 401/403 and the caller logs a
    /// warning.
    pub async fn update_secret(
        &self,
        tenant_id: &str,
        provider: &str,
        new_value: &Value,
    ) -> Result<(), PhylaxdError> {
        let token = self.token.as_deref().ok_or(PhylaxdError::AuthMissing)?;
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
            .map_err(|source| PhylaxdError::Unreachable {
                url: url.clone(),
                source,
            })?;

        let status = resp.status();
        if !status.is_success() {
            return Err(PhylaxdError::Upstream {
                status: status.as_u16(),
            });
        }
        Ok(())
    }

    /// Send one authenticated non-plaintext operation and decode its typed result.
    async fn post_operation<B, R>(&self, path: &str, body: &B) -> Result<R, PhylaxdError>
    where
        B: Serialize + ?Sized,
        R: DeserializeOwned,
    {
        self.post_operation_with_timeout(path, body, None).await
    }

    /// Send one operation with an optional request-level timeout override.
    async fn post_operation_with_timeout<B, R>(
        &self,
        path: &str,
        body: &B,
        timeout: Option<Duration>,
    ) -> Result<R, PhylaxdError>
    where
        B: Serialize + ?Sized,
        R: DeserializeOwned,
    {
        let token = self.token.as_deref().ok_or(PhylaxdError::AuthMissing)?;
        let url = format!("{}{}", self.base_url.trim_end_matches('/'), path);
        let request = self.http.post(&url).bearer_auth(token).json(body);
        let request = match timeout {
            Some(timeout) => request.timeout(timeout),
            None => request,
        };
        let response = request
            .send()
            .await
            .map_err(|source| PhylaxdError::Unreachable {
                url: url.clone(),
                source,
            })?;
        let status = response.status();
        if !status.is_success() {
            return Err(PhylaxdError::Upstream {
                status: status.as_u16(),
            });
        }
        response
            .json::<R>()
            .await
            .map_err(|_| PhylaxdError::MalformedResponse)
    }
}

#[cfg(test)]
/// Contract tests for authenticated phylaxd non-plaintext operations.
mod tests {
    use super::*;
    use wiremock::matchers::{body_json, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// All four broker operations use the reviewed phylaxd wire contract.
    #[tokio::test]
    async fn mediated_operations_match_phylaxd_contract() {
        let server = MockServer::start().await;
        let client = PhylaxdClient::new(server.uri(), Some("service-token".to_string()));

        Mock::given(method("POST"))
            .and(path("/resolve/sign"))
            .and(header("authorization", "Bearer service-token"))
            .and(body_json(serde_json::json!({
                "category": "release",
                "name": "manifest",
                "payload_b64": "aGVsbG8=",
                "algo": "hmac-sha256"
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "signature_b64": "c2ln"
            })))
            .mount(&server)
            .await;
        let signed = client
            .sign("release", "manifest", "aGVsbG8=", "hmac-sha256")
            .await
            .expect("sign");
        assert_eq!(signed.signature_b64, "c2ln");

        Mock::given(method("POST"))
            .and(path("/resolve/verify"))
            .and(body_json(serde_json::json!({
                "category": "release",
                "name": "manifest",
                "payload_b64": "aGVsbG8=",
                "signature_b64": "c2ln",
                "algo": "hmac-sha256"
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "valid": true
            })))
            .mount(&server)
            .await;
        assert!(
            client
                .verify("release", "manifest", "aGVsbG8=", "c2ln", "hmac-sha256")
                .await
                .expect("verify")
                .valid
        );

        Mock::given(method("POST"))
            .and(path("/resolve/derive"))
            .and(body_json(serde_json::json!({
                "category": "release",
                "name": "manifest",
                "purpose": "artifact",
                "length": 32
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "derived_b64": "ZGVyaXZlZA=="
            })))
            .mount(&server)
            .await;
        let derived = client
            .derive("release", "manifest", "artifact", 32)
            .await
            .expect("derive");
        assert_eq!(derived.derived_b64, "ZGVyaXZlZA==");

        let argv = vec!["/usr/bin/printf".to_string(), "ok".to_string()];
        Mock::given(method("POST"))
            .and(path("/resolve/exec"))
            .and(body_json(serde_json::json!({
                "category": "release",
                "name": "manifest",
                "argv": argv,
                "env_var": "SIGNING_KEY"
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "timed_out": false,
                "exit_code": 0,
                "stdout_b64": "b2s=",
                "stderr_b64": ""
            })))
            .mount(&server)
            .await;
        let executed = client
            .exec(
                "release",
                "manifest",
                &["/usr/bin/printf".to_string(), "ok".to_string()],
                "SIGNING_KEY",
            )
            .await
            .expect("exec");
        assert_eq!(executed.exit_code, Some(0));
        assert_eq!(executed.stdout_b64, "b2s=");
    }

    /// Exec's request override outlives the ordinary client timeout and receives the broker reply.
    #[tokio::test]
    async fn exec_timeout_override_preserves_delayed_broker_response() {
        let server = MockServer::start().await;
        let client = PhylaxdClient::new_with_timeouts(
            server.uri(),
            Some("service-token".to_string()),
            Duration::from_millis(20),
            Duration::from_millis(150),
        );
        Mock::given(method("POST"))
            .and(path("/resolve/exec"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(Duration::from_millis(75))
                    .set_body_json(serde_json::json!({
                        "timed_out": true,
                        "exit_code": null,
                        "stdout_b64": "",
                        "stderr_b64": ""
                    })),
            )
            .mount(&server)
            .await;

        let result = client
            .exec(
                "release",
                "manifest",
                &["/usr/bin/printf".to_string()],
                "SIGNING_KEY",
            )
            .await
            .expect("exec override must outlive the ordinary timeout");

        assert!(result.timed_out);
        assert_eq!(result.exit_code, None);
    }
}
