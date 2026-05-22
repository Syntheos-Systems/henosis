//! Thin HTTP client for an upstream Kleos instance.  All coupling to Kleos's
//! REST shapes lives here; the rest of the gateway speaks only the wire DTOs.
//! Kleos responses are parsed permissively (as JSON values) so the gateway
//! tolerates additive changes to Kleos's memory schema.
//!
//! Authentication uses the KLEOSv1 envelope signing protocol (Ed25519 or
//! PIV P-256 ECDSA) implemented in the `signing` module.  A session token from Kleos
//! in the `X-Kleos-Session-Issued` header is cached and replayed on subsequent
//! requests; a 401 response triggers a clear-and-retry cycle.

use crate::config::Config;
use crate::dto::{Filters, Memory};
use crate::error::GatewayError;
use crate::signing::RequestSigner;
use axum::http::StatusCode;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::Arc;

/// Shared HTTP client for an upstream Kleos instance.
#[derive(Clone)]
pub struct KleosClient {
    /// Underlying reqwest client (connection-pooled).
    http: reqwest::Client,
    /// Upstream Kleos base URL (no trailing slash).
    base_url: String,
    /// Optional signer used for KLEOSv1 envelope authentication.
    signer: Option<Arc<RequestSigner>>,
}

/// Request building, authentication, session caching, and Kleos response parsing.
impl KleosClient {
    /// Construct a client from gateway configuration and an optional signer.
    pub fn new(config: &Config, signer: Option<Arc<RequestSigner>>) -> Self {
        Self {
            http: reqwest::Client::new(),
            base_url: config.kleos_base_url.clone(),
            signer,
        }
    }

    /// Build an authenticated `RequestBuilder` for the given request parameters.
    ///
    /// If the signer has a cached session token, that token is sent as
    /// `X-Kleos-Session` and no signing takes place.  Otherwise the full
    /// KLEOSv1 envelope is signed and the `X-Kleos-*` header set is applied.
    /// When no signer is present the builder is returned unmodified.
    fn auth_request(
        &self,
        rb: reqwest::RequestBuilder,
        method: &str,
        path: &str,
        query: &str,
        body: &[u8],
    ) -> reqwest::RequestBuilder {
        let Some(signer) = &self.signer else {
            return rb;
        };

        // Use a cached session when available.
        if let Some(token) = signer.cached_session() {
            return rb.header("X-Kleos-Session", token);
        }

        // Full envelope signing.
        let headers = signer
            .sign_request(method, path, query, body)
            .unwrap_or_else(|e| {
                // Signing errors here cannot propagate via return type (the method
                // returns RequestBuilder, not Result).  Log and fall back to sending
                // the request unsigned so callers get a 401 rather than a panic.
                tracing::error!(error = %e, "signing failed; sending request unsigned");
                return crate::signing::SignedHeaders {
                    sig: String::new(),
                    algo: String::new(),
                    identity: String::new(),
                    ts: String::new(),
                    nonce: String::new(),
                    key_fp: String::new(),
                    host: String::new(),
                    agent: String::new(),
                    model: String::new(),
                };
            });
        headers.apply(rb)
    }

    /// Inspect a response for `X-Kleos-Session-Issued` and, if present, cache
    /// the token in the signer for future requests.
    fn capture_session(&self, resp: &reqwest::Response) {
        let Some(signer) = &self.signer else { return };
        if let Some(val) = resp.headers().get("X-Kleos-Session-Issued") {
            if let Ok(token) = val.to_str() {
                tracing::debug!("caching Kleos session token");
                signer.set_session(token);
            }
        }
    }

    /// Send an authenticated request, retrying once with full signing if the
    /// server returns 401 (indicating a stale or invalid session token).
    async fn send_authenticated(
        &self,
        method: &str,
        path: &str,
        query: &str,
        body: &[u8],
        build: impl Fn(reqwest::RequestBuilder) -> reqwest::RequestBuilder,
    ) -> Result<reqwest::Response, GatewayError> {
        let url = if query.is_empty() {
            format!("{}{}", self.base_url, path)
        } else {
            format!("{}{}?{}", self.base_url, path, query)
        };

        // Build the base request (method + URL).
        let base_rb = match method {
            "GET" => self.http.get(&url),
            "POST" => self.http.post(&url),
            _ => self.http.request(
                method.parse().expect("valid HTTP method"),
                &url,
            ),
        };

        // Apply Content-Type, body, or any other caller-supplied decoration.
        let base_rb = build(base_rb);

        // Authenticate (may use session or full signing).
        let rb = self.auth_request(base_rb, method, path, query, body);
        let resp = rb.send().await?;

        // On 401: if a session was used, clear it and retry with full signing.
        if resp.status() == StatusCode::UNAUTHORIZED {
            if let Some(signer) = &self.signer {
                let had_session = signer.cached_session().is_some();
                if had_session {
                    signer.clear_session();
                    tracing::debug!("session rejected (401), retrying with full signing");

                    let base_rb2 = match method {
                        "GET" => self.http.get(&url),
                        "POST" => self.http.post(&url),
                        _ => self.http.request(
                            method.parse().expect("valid HTTP method"),
                            &url,
                        ),
                    };
                    let base_rb2 = build(base_rb2);
                    let headers = signer.sign_request(method, path, query, body)?;
                    let rb2 = headers.apply(base_rb2);
                    let resp2 = rb2.send().await?;
                    self.capture_session(&resp2);
                    return Ok(resp2);
                }
            }
        }

        self.capture_session(&resp);
        Ok(resp)
    }

    /// Turn a non-success upstream status into the matching gateway error,
    /// mapping 404 to `NotFound` and forwarding any other failure status.
    async fn check_status(
        &self,
        resp: reqwest::Response,
    ) -> Result<reqwest::Response, GatewayError> {
        let status = resp.status();
        if status.is_success() {
            return Ok(resp);
        }
        if status.as_u16() == 404 {
            return Err(GatewayError::NotFound);
        }
        Err(GatewayError::KleosStatus(
            StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY),
        ))
    }

    /// Store a memory in Kleos and return its id (as a string) and timestamp.
    ///
    /// Caller metadata is not forwarded upstream (Kleos store has no generic
    /// metadata slot); it is retained only in the gateway response shape.
    pub async fn store(
        &self,
        text: &str,
        tags: &[String],
        _metadata: &BTreeMap<String, Value>,
    ) -> Result<(String, String), GatewayError> {
        let body_value = json!({
            "content": text,
            "category": "general",
            "source": "frameshift",
            "importance": 5,
            "tags": tags,
        });
        let body_bytes = serde_json::to_vec(&body_value)
            .expect("json serialisation of static structure never fails");

        let resp = self
            .send_authenticated("POST", "/store", "", &body_bytes, |rb| {
                rb.header("Content-Type", "application/json")
                    .body(body_bytes.clone())
            })
            .await?;
        let resp = self.check_status(resp).await?;
        let v: Value = resp.json().await?;
        let id = v
            .get("id")
            .and_then(Value::as_i64)
            .map(|i| i.to_string())
            .ok_or(GatewayError::KleosStatus(StatusCode::BAD_GATEWAY))?;
        let created_at = v
            .get("created_at")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        Ok((id, created_at))
    }

    /// Search Kleos and return matching memories in wire shape.
    pub async fn search(
        &self,
        query: &str,
        k: usize,
        filters: &Filters,
    ) -> Result<Vec<Memory>, GatewayError> {
        let mut body_value = json!({ "query": query, "limit": k });
        if !filters.tags.is_empty() {
            body_value["tags"] = json!(filters.tags);
        }
        // Tags are forwarded to Kleos; the remaining filters are part of the
        // wire contract but Kleos's search API does not yet accept date ranges
        // or metadata equality, so they are accepted and logged rather than
        // applied.  This keeps the gateway contract-complete without silently
        // pretending to filter.
        if filters.after.is_some() || filters.before.is_some() || !filters.metadata.is_empty() {
            tracing::debug!(
                after = ?filters.after,
                before = ?filters.before,
                metadata_keys = filters.metadata.len(),
                "after/before/metadata filters accepted but not yet forwarded to Kleos"
            );
        }
        let body_bytes = serde_json::to_vec(&body_value)
            .expect("json serialisation of static structure never fails");

        let resp = self
            .send_authenticated("POST", "/search", "", &body_bytes, |rb| {
                rb.header("Content-Type", "application/json")
                    .body(body_bytes.clone())
            })
            .await?;
        let resp = self.check_status(resp).await?;
        let v: Value = resp.json().await?;
        Ok(extract_memories(&v, "results"))
    }

    /// Recall a single memory by its (string) id.
    pub async fn get(&self, id: &str) -> Result<Memory, GatewayError> {
        let kid = parse_id(id)?;
        let path = format!("/memory/{}", kid);
        let resp = self
            .send_authenticated("GET", &path, "", &[], |rb| rb)
            .await?;
        let resp = self.check_status(resp).await?;
        let v: Value = resp.json().await?;
        Ok(memory_to_wire(&v))
    }

    /// List memories with limit/offset paging.
    pub async fn list(&self, limit: usize, offset: usize) -> Result<Vec<Memory>, GatewayError> {
        let query = format!("limit={}&offset={}", limit, offset);
        let resp = self
            .send_authenticated("GET", "/list", &query, &[], |rb| rb)
            .await?;
        let resp = self.check_status(resp).await?;
        let v: Value = resp.json().await?;
        Ok(extract_memories(&v, "results"))
    }

    /// Forget (soft-delete) a single memory by its (string) id.
    pub async fn forget(&self, id: &str) -> Result<(), GatewayError> {
        let kid = parse_id(id)?;
        let path = format!("/memory/{}/forget", kid);
        let body_value = json!({});
        let body_bytes = serde_json::to_vec(&body_value)
            .expect("json serialisation of static structure never fails");

        let resp = self
            .send_authenticated("POST", &path, "", &body_bytes, |rb| {
                rb.header("Content-Type", "application/json")
                    .body(body_bytes.clone())
            })
            .await?;
        self.check_status(resp).await?;
        Ok(())
    }

    /// Report whether the upstream Kleos health endpoint is reachable.
    ///
    /// Health checks are intentionally unauthenticated -- the `/health`
    /// endpoint on Kleos does not require a signed request.
    pub async fn health(&self) -> bool {
        match self
            .http
            .get(format!("{}/health", self.base_url))
            .send()
            .await
        {
            Ok(r) => r.status().is_success(),
            Err(_) => false,
        }
    }
}

/// Parse an opaque wire id back into the Kleos i64 it encodes.
fn parse_id(id: &str) -> Result<i64, GatewayError> {
    id.parse::<i64>()
        .map_err(|_| GatewayError::InvalidId(id.to_string()))
}

/// Pull the array under `key` from a Kleos response and map each element to a
/// wire `Memory`, tolerating a missing or non-array field by returning empty.
fn extract_memories(v: &Value, key: &str) -> Vec<Memory> {
    v.get(key)
        .and_then(Value::as_array)
        .map(|arr| arr.iter().map(memory_to_wire).collect())
        .unwrap_or_default()
}

/// Translate one Kleos memory JSON object into the wire `Memory` shape,
/// surfacing a few useful Kleos fields as flattened metadata.
fn memory_to_wire(v: &Value) -> Memory {
    let id = v
        .get("id")
        .and_then(Value::as_i64)
        .map(|i| i.to_string())
        .unwrap_or_default();
    let text = v
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let tags = v
        .get("tags")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|t| t.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let created_at = v.get("created_at").and_then(Value::as_str).map(String::from);
    let updated_at = v.get("updated_at").and_then(Value::as_str).map(String::from);
    let mut metadata = BTreeMap::new();
    for field in ["category", "source", "importance"] {
        if let Some(val) = v.get(field) {
            metadata.insert(field.to_string(), val.clone());
        }
    }
    Memory {
        id,
        text,
        tags,
        created_at,
        updated_at,
        metadata,
    }
}
