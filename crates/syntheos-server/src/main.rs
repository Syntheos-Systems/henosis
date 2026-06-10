//! `syntheos-server` binary: the single entry point that boots the Henosis foundation and serves
//! the Phase 0 HTTP surface.

use std::sync::Arc;
use std::time::Duration;

use axum::extract::DefaultBodyLimit;
use axum::http::StatusCode;
use henosis_chiasm::ChiasmStore;
use syntheos_axon::AxonBus;
use syntheos_dispatch::deny::{deny_gate_chain, DenyExecutor};
use syntheos_dispatch::Dispatcher;
use syntheos_identity::{InMemoryDirectory, PrincipalDirectory};
use syntheos_server::{router, AppState};
use tower::limit::GlobalConcurrencyLimitLayer;
use tower_http::timeout::TimeoutLayer;
use tracing_subscriber::EnvFilter;

/// Largest request body the server accepts, in bytes (1 MiB). Phase 0 payloads are small JSON;
/// anything bigger is rejected before it can exhaust memory.
const MAX_BODY_BYTES: usize = 1024 * 1024;

/// How long a single request may run before the server answers `408 Request Timeout`.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Maximum requests in flight at once across the whole surface; excess connections queue on the
/// shared semaphore instead of piling onto the runtime.
const MAX_IN_FLIGHT: usize = 1024;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    // Wire the Phase 0 foundation: bus, directory, and the dispatcher. The live chain is
    // deny-by-default (every action is denied at the first gate) until real authorities land --
    // fail-closed posture, enforced again by `Dispatcher::new` rejecting an invalid chain.
    let bus = Arc::new(AxonBus::new());
    let directory: Arc<dyn PrincipalDirectory> = Arc::new(InMemoryDirectory::new());
    let dispatcher = Arc::new(Dispatcher::new(
        deny_gate_chain(),
        Box::new(DenyExecutor),
        bus.clone(),
    )?);

    // Chiasm: the first Phase 1 kernel service, persistent SQLite at a configurable path
    // (migrations apply on open). The parent directory is created if absent.
    let chiasm_db =
        std::env::var("SYNTHEOS_CHIASM_DB").unwrap_or_else(|_| "data/chiasm.sqlite".to_string());
    if let Some(parent) = std::path::Path::new(&chiasm_db).parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let chiasm = Arc::new(ChiasmStore::open(&chiasm_db, bus.clone())?);
    tracing::info!(path = %chiasm_db, "chiasm task store open");

    let state = AppState::new(dispatcher, directory, bus, chiasm);

    // Resource limits around the whole surface: cap the body size, time out slow requests, and
    // bound how many run concurrently.
    let app = router(state)
        .layer(GlobalConcurrencyLimitLayer::new(MAX_IN_FLIGHT))
        .layer(TimeoutLayer::with_status_code(
            StatusCode::REQUEST_TIMEOUT,
            REQUEST_TIMEOUT,
        ))
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES));

    let addr = std::env::var("SYNTHEOS_ADDR").unwrap_or_else(|_| "127.0.0.1:8088".to_string());
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!(%addr, "syntheos-server listening (deny-by-default gate chain)");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

/// Resolve when a shutdown signal is received, so `axum` can drain in-flight requests.
///
/// Listens for both SIGINT (Ctrl-C) and, on Unix, SIGTERM -- the latter is what systemd sends on
/// `stop`/`restart`, so service management drains cleanly instead of being killed.
async fn shutdown_signal() {
    /// Wait for SIGINT (Ctrl-C); an install failure is logged and the arm never resolves.
    async fn sigint() {
        if let Err(err) = tokio::signal::ctrl_c().await {
            tracing::error!(error = %err, "failed to install Ctrl-C handler");
            std::future::pending::<()>().await;
        }
    }

    /// Wait for SIGTERM on Unix; an install failure is logged and the arm never resolves.
    #[cfg(unix)]
    async fn sigterm() {
        use tokio::signal::unix::{signal, SignalKind};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                term.recv().await;
            }
            Err(err) => {
                tracing::error!(error = %err, "failed to install SIGTERM handler");
                std::future::pending::<()>().await;
            }
        }
    }

    /// Non-Unix platforms have no SIGTERM; this arm never resolves.
    #[cfg(not(unix))]
    async fn sigterm() {
        std::future::pending::<()>().await;
    }

    tokio::select! {
        _ = sigint() => {},
        _ = sigterm() => {},
    }
    tracing::info!("shutdown signal received, draining");
}
