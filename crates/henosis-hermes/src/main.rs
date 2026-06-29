//! Thin binary wrapper over the `henosis-hermes` library.
//!
//! Assembles `AppState`, wires the axum router, and serves the Hermes HTTP API.
//! All business logic (tool registry, adapters, circuit breakers, rate limiting,
//! audit, webhooks, MCP bridge) lives in the library crate (`lib.rs`).

use std::net::SocketAddr;
use std::sync::Arc;

use axum::{routing::get, routing::post, Json, Router};
use serde_json::json;
use tokio::net::TcpListener;
use tokio::signal;
use tracing::info;
use tracing_subscriber::EnvFilter;

use henosis_hermes::{
    AppState,
    axon::AxonPublisher,
    audit::AuditTrail,
    circuit::CircuitRegistry,
    config::Config,
    credd_client::CreddClient,
    metrics::MetricsRegistry,
    mcp_bridge,
    oauth_refresh::{OAuthRefreshDaemon, RefreshRegistry},
    rate_limit::{RateLimitConfig, RateLimiter},
    registry::build_registry,
    routes,
    tenant_config::TenantConfigStore,
    webhooks,
};

/// Service name embedded in health and version responses.
const SERVICE: &str = "hermes";

/// Package version from Cargo.toml.
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Entry point: initialize tracing, load config, build state, and serve.
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let cfg = Config::from_env();
    let port = cfg.port;
    let registry = Arc::new(build_registry());
    let refresh_registry = RefreshRegistry::default();
    let credd = Arc::new(
        CreddClient::new(cfg.credd_url.clone(), cfg.credd_token.clone())
            .with_refresh_registry(refresh_registry.clone()),
    );

    info!(
        "{SERVICE} {VERSION} -- {} tools registered, credd at {}",
        registry.list().len(),
        cfg.credd_url,
    );
    if cfg.credd_token.is_none() {
        info!("HERMES_CREDD_TOKEN not set -- adapters needing OAuth will return credd_auth_missing");
    }

    OAuthRefreshDaemon::new(refresh_registry, credd.clone()).spawn();

    let rate_limiter = Arc::new(RateLimiter::new(RateLimitConfig::default()));
    let circuits = Arc::new(CircuitRegistry::new());
    let metrics = Arc::new(MetricsRegistry::new());
    let axon_publisher = AxonPublisher::from_env();
    let audit = Arc::new(AuditTrail::new(axon_publisher.clone()));
    audit.clone().spawn_publisher();

    let state = AppState {
        registry,
        credd,
        rate_limiter,
        circuits,
        metrics,
        audit,
        axon: axon_publisher,
        tenant_config: Arc::new(TenantConfigStore::with_path(
            std::env::var("HERMES_TENANT_CONFIG_PATH")
                .unwrap_or_else(|_| "data/tenant_config.json".to_string())
                .into(),
        )),
        public_url: cfg.public_url.clone(),
    };

    let mut app = Router::new()
        .route("/health", get(health))
        .route("/version", get(version))
        .route("/health/adapters", get(routes::adapters_health))
        .route("/tools", get(routes::list_tools))
        .route("/tools/{tool_id}/invoke", post(routes::invoke_tool))
        .route("/tools/{tool_id}/health", get(routes::tool_health))
        .route("/metrics", get(routes::metrics))
        .route("/audit", get(routes::audit))
        .route("/webhooks/{provider}", post(webhooks::ingest))
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
    if mcp_bridge::is_enabled() {
        info!("MCP bridge enabled at POST /mcp");
        app = app.route("/mcp", post(mcp_bridge::jsonrpc_handler));
    }
    let app = app.with_state(state);

    let addr = SocketAddr::from(([0, 0, 0, 0], port));
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
