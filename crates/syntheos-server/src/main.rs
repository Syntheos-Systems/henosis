//! `syntheos-server` binary: the single entry point that boots the Henosis foundation and serves
//! the Phase 0 HTTP surface.

use std::sync::Arc;

use syntheos_axon::AxonBus;
use syntheos_dispatch::stubs::{stub_gate_chain, EchoExecutor};
use syntheos_dispatch::Dispatcher;
use syntheos_identity::{InMemoryDirectory, PrincipalDirectory};
use syntheos_server::{router, AppState};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    // Wire the Phase 0 foundation: bus, directory, and the dispatcher (stub gate chain + echo
    // executor). Real gates and executors swap in by trait object as later phases land.
    let bus = Arc::new(AxonBus::new());
    let directory: Arc<dyn PrincipalDirectory> = Arc::new(InMemoryDirectory::new());
    let dispatcher = Arc::new(Dispatcher::new(
        stub_gate_chain(),
        Box::new(EchoExecutor),
        bus.clone(),
    ));
    let state = AppState::new(dispatcher, directory, bus);

    let addr = std::env::var("SYNTHEOS_ADDR").unwrap_or_else(|_| "127.0.0.1:8088".to_string());
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!(%addr, "syntheos-server listening");

    axum::serve(listener, router(state))
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

/// Resolve when a shutdown signal (Ctrl-C) is received, so `axum` can drain in-flight requests.
async fn shutdown_signal() {
    if let Err(err) = tokio::signal::ctrl_c().await {
        tracing::error!(error = %err, "failed to install Ctrl-C handler");
    }
    tracing::info!("shutdown signal received, draining");
}
