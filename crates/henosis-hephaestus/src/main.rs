//! Binary entry point for the henosis-hephaestus executor service.
//! Wires configuration from environment variables, recovers in-flight tasks
//! from Kleos checkpoints, and serves the axum HTTP API until shutdown.

use tokio::net::TcpListener;
use tokio::signal;
use tracing::info;
use tracing_subscriber::EnvFilter;

use henosis_hephaestus::config::ServerConfig;
use henosis_hephaestus::{
    Config, SERVICE, VERSION, build_router, build_state, recover_in_flight_tasks,
};

/// Main entry point. Initialises tracing, loads config from environment,
/// recovers in-flight tasks, binds the HTTP listener, and serves until
/// SIGINT or SIGTERM.
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let cfg = Config::from_env();
    let server = ServerConfig::from_env(cfg.port, cfg.provider_api_key.as_deref())
        .map_err(std::io::Error::other)?;
    let state = build_state(cfg);

    let resumed = recover_in_flight_tasks(&state).await;
    info!("resumed {resumed} task(s) from Kleos checkpoints");

    let app = build_router(state, server.api_token);

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

/// Wait for SIGINT (Ctrl-C) or SIGTERM. On non-Unix platforms only SIGINT
/// is available; SIGTERM handling compiles away via `#[cfg(unix)]`.
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
