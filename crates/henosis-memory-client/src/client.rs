//! `Client` is the HTTP client used to talk to a Kleos-protocol memory server.
//!
//! The client provides the generic string-path HTTP surface used by Synapse memory tools:
//! `get`, `post`, `put`, `patch`, and `delete`. It intentionally omits typed route helpers,
//! MCP passthrough, multipart requests, byte responses, and per-request timeout variants.

use serde_json::Value;

use crate::signer::RequestSigner;

/// HTTP client wrapper that handles auth, session capture, and base-URL composition.
///
/// Supports comma-separated URLs in the base URL for failover: the first URL is
/// the primary, subsequent URLs are tried on connection-level failures (timeout,
/// refused, unreachable). HTTP-level errors (4xx, 5xx) are NOT retried.
pub struct Client {
    /// Underlying reqwest client (owns the connection pool).
    http: reqwest::Client,
    /// Configured base URLs in failover order.
    urls: Vec<String>,
    /// Bearer API key, used when no signer is set or signing fails.
    api_key: Option<String>,
    /// Software Ed25519 request signer (preferred over the API key).
    pub signer: Option<RequestSigner>,
}

/// Constructor and HTTP request helpers for `Client`.
impl Client {
    /// Constructs a new `Client`. `base_url` may be comma-separated for failover
    /// (e.g. `"http://primary-host:4200,http://backup-host:4200"`).
    pub fn new(base_url: String, api_key: Option<String>, signer: Option<RequestSigner>) -> Self {
        let urls: Vec<String> = base_url
            .split(',')
            .map(|s| s.trim().trim_end_matches('/').to_string())
            .filter(|s| !s.is_empty())
            .collect();
        Self {
            http: reqwest::Client::new(),
            urls,
            api_key,
            signer,
        }
    }

    /// Returns the primary (first) base URL.
    pub fn base_url(&self) -> &str {
        self.urls.first().map(|s| s.as_str()).unwrap_or("")
    }

    /// Applies signed headers or bearer-token auth to a pending request.
    pub fn apply_auth(
        &self,
        req: reqwest::RequestBuilder,
        method: &str,
        path: &str,
        body: &[u8],
    ) -> reqwest::RequestBuilder {
        if let Some(signer) = &self.signer {
            if let Some(session) = signer.cached_session() {
                return req.header("X-Kleos-Session", session);
            }
            let (url_path, query) = match path.split_once('?') {
                Some((p, q)) => (p, q),
                None => (path, ""),
            };
            match signer.sign_request(method, url_path, query, body) {
                Ok(signed) => return signed.apply_headers(req),
                Err(e) => {
                    eprintln!("warning: request signing failed, falling back to API key: {e}");
                }
            }
        }
        if let Some(key) = &self.api_key {
            return req.bearer_auth(key);
        }
        req
    }

    /// Core request dispatcher with URL failover. Tries each configured URL in
    /// order; on connection-level failures (timeout, refused, unreachable) falls
    /// through to the next URL. HTTP errors (4xx, 5xx) are returned immediately.
    async fn execute(
        &self,
        http: &reqwest::Client,
        method: &str,
        path: &str,
        body: Option<&[u8]>,
        content_type: Option<&str>,
    ) -> Result<reqwest::Response, String> {
        let mut last_err = String::new();
        for (i, base) in self.urls.iter().enumerate() {
            let url = format!("{base}{path}");
            let mut req = match method {
                "GET" => http.get(&url),
                "POST" => http.post(&url),
                "PUT" => http.put(&url),
                "PATCH" => http.patch(&url),
                "DELETE" => http.delete(&url),
                _ => return Err(format!("unsupported HTTP method: {method}")),
            };
            if let Some(ct) = content_type {
                req = req.header("content-type", ct);
            }
            if let Some(b) = body {
                req = req.body(b.to_vec());
            }
            req = self.apply_auth(req, method, path, body.unwrap_or(b""));
            match req.send().await {
                Ok(resp) => return Ok(resp),
                Err(e) if is_connection_error(&e) && i + 1 < self.urls.len() => {
                    eprintln!(
                        "warning: {method} {url} failed ({}), trying next URL",
                        format_error_chain(&e)
                    );
                    last_err = format_reqwest_error(method, &url, &e);
                }
                Err(e) => return Err(format_reqwest_error(method, &url, &e)),
            }
        }
        Err(last_err)
    }

    /// Reads any session token issued by the server and caches it in the signer.
    pub fn capture_session(&self, resp: &reqwest::Response) {
        if let Some(signer) = &self.signer {
            if let Some(token) = resp.headers().get("x-kleos-session-issued") {
                if let Ok(t) = token.to_str() {
                    signer.set_session(t.to_string());
                }
            }
        }
    }

    /// Sends an authenticated GET request and returns the parsed JSON body.
    pub async fn get(&self, path: &str) -> Result<Value, String> {
        let resp = self.execute(&self.http, "GET", path, None, None).await?;
        self.capture_session(&resp);
        self.handle_response("GET", path, resp).await
    }

    /// Sends an authenticated POST request with a JSON body and returns the parsed JSON response.
    pub async fn post(&self, path: &str, body: Value) -> Result<Value, String> {
        let body_bytes = serde_json::to_vec(&body).unwrap_or_default();
        let resp = self
            .execute(
                &self.http,
                "POST",
                path,
                Some(&body_bytes),
                Some("application/json"),
            )
            .await?;
        self.capture_session(&resp);
        self.handle_response("POST", path, resp).await
    }

    /// Sends an authenticated PUT request with a JSON body and returns the parsed JSON response.
    pub async fn put(&self, path: &str, body: Value) -> Result<Value, String> {
        let body_bytes = serde_json::to_vec(&body).unwrap_or_default();
        let resp = self
            .execute(
                &self.http,
                "PUT",
                path,
                Some(&body_bytes),
                Some("application/json"),
            )
            .await?;
        self.capture_session(&resp);
        self.handle_response("PUT", path, resp).await
    }

    /// Sends an authenticated PATCH request with a JSON body and returns the parsed JSON response.
    pub async fn patch(&self, path: &str, body: Value) -> Result<Value, String> {
        let body_bytes = serde_json::to_vec(&body).unwrap_or_default();
        let resp = self
            .execute(
                &self.http,
                "PATCH",
                path,
                Some(&body_bytes),
                Some("application/json"),
            )
            .await?;
        self.capture_session(&resp);
        self.handle_response("PATCH", path, resp).await
    }

    /// Sends an authenticated DELETE request and returns the parsed JSON response.
    pub async fn delete(&self, path: &str) -> Result<Value, String> {
        let resp = self.execute(&self.http, "DELETE", path, None, None).await?;
        self.capture_session(&resp);
        self.handle_response("DELETE", path, resp).await
    }

    /// Interprets an HTTP response, returning parsed JSON on success or an error string on failure.
    pub async fn handle_response(
        &self,
        method: &str,
        path: &str,
        resp: reqwest::Response,
    ) -> Result<Value, String> {
        let status = resp.status();
        let url = resp.url().to_string();

        if status == reqwest::StatusCode::UNAUTHORIZED {
            if let Some(signer) = &self.signer {
                signer.clear_session();
            }
        }

        let bytes = resp.bytes().await.map_err(|e| {
            format!(
                "{} {} succeeded but reading response body failed: {}",
                method,
                path,
                format_error_chain(&e)
            )
        })?;
        let parsed: Result<Value, _> = serde_json::from_slice(&bytes);
        if status.is_success() {
            parsed.or_else(|_| {
                let text = String::from_utf8_lossy(&bytes).into_owned();
                Ok(serde_json::json!({ "content": text }))
            })
        } else {
            let msg = parsed
                .as_ref()
                .ok()
                .and_then(|b| {
                    b.get("error")
                        .or_else(|| b.get("message"))
                        .and_then(|v| v.as_str())
                        .map(ToOwned::to_owned)
                })
                .unwrap_or_else(|| body_excerpt(&bytes));
            Err(format!("HTTP {status} {url}: {msg}"))
        }
    }

    /// Returns the agent label from the signer, or "claude-code" when no signer is configured.
    pub fn agent_label(&self) -> String {
        self.signer
            .as_ref()
            .map(|s| s.agent_label().to_string())
            .unwrap_or_else(|| "claude-code".to_string())
    }
}

/// Formats a reqwest transport error with method and URL context.
pub fn format_reqwest_error(method: &str, url: &str, err: &reqwest::Error) -> String {
    format!("{} {} failed: {}", method, url, format_error_chain(err))
}

/// Walks the error source chain and concatenates messages with " -> " separators.
pub fn format_error_chain<E: std::error::Error + ?Sized>(err: &E) -> String {
    let mut out = err.to_string();
    let mut source: Option<&dyn std::error::Error> = err.source();
    for _ in 0..16 {
        let Some(cause) = source else { break };
        out.push_str(" -> ");
        out.push_str(&cause.to_string());
        source = cause.source();
    }
    out
}

/// Returns true for connection-level failures that warrant URL failover.
fn is_connection_error(e: &reqwest::Error) -> bool {
    e.is_connect() || e.is_timeout()
}

/// Returns up to 512 bytes of the response body as a UTF-8 string for error messages.
pub fn body_excerpt(bytes: &[u8]) -> String {
    const MAX: usize = 512;
    let s = String::from_utf8_lossy(bytes);
    if s.len() <= MAX {
        return s.into_owned();
    }
    let mut end = MAX;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}... ({} bytes total)", &s[..end], bytes.len())
}
