//! Thin binary wrapper over the `henosis-hermes` library.
//!
//! Assembles `AppState`, wires the axum router, and serves the Hermes HTTP API.
//! All business logic (tool registry, adapters, circuit breakers, rate limiting,
//! audit, webhooks, MCP bridge) lives in the library crate (`lib.rs`).

use axum::{routing::get, routing::post, Json, Router};
use serde_json::json;
use std::net::SocketAddr;
use tokio::net::TcpListener;
use tokio::signal;
use tracing::info;
use tracing_subscriber::EnvFilter;

use henosis_hermes::{config::Config, mcp_bridge, routes, webhooks, AppState};

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
