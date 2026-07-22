//! Thin binary wrapper over the `henosis-hermes` library.
//!
//! Assembles `AppState`, wires the axum router, and serves the Hermes HTTP API.
//! All business logic (tool registry, adapters, circuit breakers, rate limiting,
//! audit, webhooks, MCP bridge) lives in the library crate (`lib.rs`).

use axum::extract::{Request, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::{routing::get, routing::post, Json, Router};
use hmac::{Hmac, Mac};
use serde_json::json;
use sha2::Sha256;
use tokio::net::TcpListener;
use tokio::signal;
use tracing::info;
use tracing_subscriber::EnvFilter;

use henosis_hermes::{config::Config, mcp_bridge, routes, webhooks, AppState};

/// Service name embedded in health and version responses.
const SERVICE: &str = "hermes";

/// Package version from Cargo.toml.
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// HMAC type used to compare inbound API tokens without ordinary string equality.
type HmacSha256 = Hmac<Sha256>;

/// Entry point: initialize tracing, load config, build state, and serve.
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let cfg = Config::from_env();
    let server = cfg.validate_server().map_err(std::io::Error::other)?;
    let phylaxd_url = cfg.phylaxd_url.clone();
    let phylaxd_token_missing = cfg.phylaxd_token.is_none();
    let state = AppState::from_config(cfg);

    info!(
        "{SERVICE} {VERSION} -- {} tools registered, phylaxd at {}",
        state.registry.list().len(),
        phylaxd_url,
    );
    if phylaxd_token_missing {
        info!(
            "HERMES_PHYLAXD_TOKEN not set -- adapters needing OAuth will return phylaxd_auth_missing"
        );
    }

    let app = build_router(state, server.api_token, mcp_bridge::is_enabled());

    let addr = server.listen_addr;
    let listener = TcpListener::bind(addr)
        .await
        .map_err(|e| format!("failed to bind {addr}: {e}"))?;

    info!("{SERVICE} {VERSION} listening on {addr}");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    info!("shutdown complete");
    Ok(())
}

/// Build the standalone HTTP router with a narrow public surface and one
/// fail-closed authorization layer around every sensitive route.
fn build_router(state: AppState, api_token: String, mcp_enabled: bool) -> Router {
    let public = Router::new()
        .route("/health", get(health))
        .route("/version", get(version))
        .route("/webhooks/{provider}", post(webhooks::ingest));

    let mut protected = Router::new()
        .route("/health/adapters", get(routes::adapters_health))
        .route("/tools", get(routes::list_tools))
        .route("/tools/{tool_id}/invoke", post(routes::invoke_tool))
        .route("/tools/{tool_id}/health", get(routes::tool_health))
        .route("/metrics", get(routes::metrics))
        .route("/audit", get(routes::audit))
        .route(
            "/admin/tenants/{tenant_id}/adapters",
            get(routes::list_tenant_adapters),
        )
        .route(
            "/admin/tenants/{tenant_id}/adapters/{provider}",
            axum::routing::put(routes::set_tenant_adapter),
        )
        .route(
            "/admin/tenants/{tenant_id}/adapters/{provider}/disable",
            axum::routing::put(routes::disable_tenant_adapter),
        );
    if mcp_enabled {
        info!("MCP bridge enabled at POST /mcp");
        protected = protected.route("/mcp", post(mcp_bridge::jsonrpc_handler));
    }
    let protected =
        protected.route_layer(middleware::from_fn_with_state(api_token, require_api_token));

    public.merge(protected).with_state(state)
}

/// Reject requests whose Authorization header does not carry the configured
/// standalone Hermes service token.
async fn require_api_token(
    State(expected): State<String>,
    request: Request,
    next: Next,
) -> Response {
    if authorize_api_token(request.headers(), &expected) {
        return next.run(request).await;
    }

    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, "Bearer")],
    )
        .into_response()
}

/// Validate one Authorization header against the configured token.
fn authorize_api_token(headers: &HeaderMap, expected: &str) -> bool {
    let Some(presented) = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
    else {
        return false;
    };

    api_token_matches(expected, presented)
}

/// Compare service tokens through fixed-size HMAC tags so matching tokens do
/// not use ordinary string equality.
fn api_token_matches(expected: &str, presented: &str) -> bool {
    let mut presented_mac =
        HmacSha256::new_from_slice(presented.as_bytes()).expect("HMAC accepts any key length");
    presented_mac.update(b"henosis-hermes-inbound-api-token");
    let presented_tag = presented_mac.finalize().into_bytes();

    let mut expected_mac =
        HmacSha256::new_from_slice(expected.as_bytes()).expect("HMAC accepts any key length");
    expected_mac.update(b"henosis-hermes-inbound-api-token");
    expected_mac.verify_slice(&presented_tag).is_ok()
}

/// `GET /health`: simple liveness probe.
async fn health() -> Json<serde_json::Value> {
    Json(json!({ "status": "ok", "service": SERVICE }))
}

/// `GET /version`: service name and compiled version string.
async fn version() -> Json<serde_json::Value> {
    Json(json!({ "name": SERVICE, "version": VERSION }))
}

/// Wait for SIGINT (Ctrl-C) or SIGTERM and return, triggering graceful shutdown.
async fn shutdown_signal() {
    let ctrl_c = async {
        signal::ctrl_c().await.expect("install ctrl-c handler");
    };

    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => info!("received SIGINT"),
        _ = terminate => info!("received SIGTERM"),
    }
}

#[cfg(test)]
/// Tests for the standalone HTTP authorization boundary.
mod tests {
    use super::*;
    use std::sync::Arc;

    use henosis_hermes::audit::AuditTrail;
    use henosis_hermes::axon::AxonPublisher;
    use henosis_hermes::circuit::CircuitRegistry;
    use henosis_hermes::metrics::MetricsRegistry;
    use henosis_hermes::phylaxd_client::PhylaxdClient;
    use henosis_hermes::rate_limit::{RateLimitConfig, RateLimiter};
    use henosis_hermes::tenant_config::TenantConfigStore;
    use henosis_hermes::ToolRegistry;

    /// Build isolated state for transport-boundary tests.
    fn test_state() -> AppState {
        let axon = AxonPublisher::from_env();
        AppState {
            registry: Arc::new(ToolRegistry::new()),
            phylaxd: Arc::new(PhylaxdClient::new("http://127.0.0.1:1".to_string(), None)),
            rate_limiter: Arc::new(RateLimiter::new(RateLimitConfig::default())),
            circuits: Arc::new(CircuitRegistry::new()),
            metrics: Arc::new(MetricsRegistry::new()),
            audit: Arc::new(AuditTrail::new(axon.clone())),
            axon,
            tenant_config: Arc::new(TenantConfigStore::new()),
            public_url: None,
        }
    }

    /// Missing, malformed, and incorrect credentials fail authentication.
    #[test]
    fn rejects_invalid_authorization_headers() {
        let expected = "hermes-api-token-that-is-at-least-32-bytes";
        let mut headers = HeaderMap::new();
        assert!(!authorize_api_token(&headers, expected));

        headers.insert(header::AUTHORIZATION, "not-bearer".parse().unwrap());
        assert!(!authorize_api_token(&headers, expected));

        headers.insert(
            header::AUTHORIZATION,
            "Bearer hermes-api-token-that-is-at-least-32-byteS"
                .parse()
                .unwrap(),
        );
        assert!(!authorize_api_token(&headers, expected));
    }

    /// The exact configured Bearer credential authenticates successfully.
    #[test]
    fn accepts_exact_authorization_token() {
        let expected = "hermes-api-token-that-is-at-least-32-bytes";
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            format!("Bearer {expected}").parse().unwrap(),
        );
        assert!(authorize_api_token(&headers, expected));
    }

    /// The assembled router leaves only health, version, and provider webhooks
    /// outside the service-token boundary.
    #[tokio::test]
    async fn router_protects_sensitive_routes() {
        let token = "hermes-api-token-that-is-at-least-32-bytes";
        let app = build_router(test_state(), token.to_string(), true);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = reqwest::Client::new();

        let health = client
            .get(format!("http://{addr}/health"))
            .send()
            .await
            .unwrap();
        assert_eq!(health.status(), StatusCode::OK);

        for path in ["/tools", "/audit", "/metrics"] {
            let rejected = client
                .get(format!("http://{addr}{path}"))
                .send()
                .await
                .unwrap();
            assert_eq!(rejected.status(), StatusCode::UNAUTHORIZED, "{path}");
        }

        let rejected_mcp = client
            .post(format!("http://{addr}/mcp"))
            .send()
            .await
            .unwrap();
        assert_eq!(rejected_mcp.status(), StatusCode::UNAUTHORIZED);

        let accepted = client
            .get(format!("http://{addr}/tools"))
            .bearer_auth(token)
            .send()
            .await
            .unwrap();
        assert_eq!(accepted.status(), StatusCode::OK);

        let webhook = client
            .post(format!("http://{addr}/webhooks/unknown"))
            .send()
            .await
            .unwrap();
        assert_eq!(webhook.status(), StatusCode::NOT_FOUND);

        server.abort();
    }
}
