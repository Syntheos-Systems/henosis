//! `syntheos-server` binary: the single entry point that boots the Henosis foundation and serves
//! the Phase 0 HTTP surface.

use std::sync::Arc;
use std::time::Duration;

use axum::extract::DefaultBodyLimit;
use axum::http::StatusCode;
use henosis_broca::BrocaStore;
use henosis_chiasm::ChiasmStore;
use henosis_loom::{LoomStore, TransformExecutor};
use henosis_soma::SomaStore;
use henosis_eidolon::{EidolonOutputFilter, EidolonPolicy};
use henosis_thymus::ThymusStore;
use syntheos_server::{live_gate_chain, SomaQualitySink};
use syntheos_axon::AxonBus;
use syntheos_dispatch::deny::DenyExecutor;
use syntheos_dispatch::Dispatcher;
use syntheos_identity::{PrincipalDirectory, SqliteDirectory};
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

    // Wire the foundation: bus and directory first, stores next, then the dispatcher (its
    // eidolon gate reads drift from the Thymus store, so Thymus must exist first).
    // The directory is the persistent SqliteDirectory (G2): with persistent Chiasm/Soma stores,
    // an in-memory directory would orphan every projection row on restart.
    let bus = Arc::new(AxonBus::new());
    let identity_db = db_path("SYNTHEOS_IDENTITY_DB", "data/identity.sqlite")?;
    let directory: Arc<dyn PrincipalDirectory> = Arc::new(SqliteDirectory::open(&identity_db)?);
    tracing::info!(path = %identity_db, "principal directory open");

    // Phase 1 kernel services: persistent SQLite at configurable paths (migrations apply on
    // open).
    let chiasm_db = db_path("SYNTHEOS_CHIASM_DB", "data/chiasm.sqlite")?;
    let chiasm = Arc::new(ChiasmStore::open(&chiasm_db, bus.clone())?);
    tracing::info!(path = %chiasm_db, "chiasm task store open");
    let soma_db = db_path("SYNTHEOS_SOMA_DB", "data/soma.sqlite")?;
    let soma = Arc::new(SomaStore::open(&soma_db, bus.clone(), directory.clone())?);
    tracing::info!(path = %soma_db, "soma presence store open");
    // No LLM narrator is attached in Phase 1 (template-or-nothing); a Synapse/Foundry-backed
    // Narrator plugs in via BrocaStore::with_narrator when Phase 4 lands.
    let broca_db = db_path("SYNTHEOS_BROCA_DB", "data/broca.sqlite")?;
    let broca = Arc::new(BrocaStore::open(&broca_db, bus.clone())?);
    tracing::info!(path = %broca_db, "broca narration log open");
    // The built-in transform executor runs pure-JSON steps inline; Hephaestus swaps in the
    // real executor in Phase 5.
    let loom_db = db_path("SYNTHEOS_LOOM_DB", "data/loom.sqlite")?;
    let loom = Arc::new(
        LoomStore::open(&loom_db, bus.clone())?.with_executor(Box::new(TransformExecutor)),
    );
    tracing::info!(path = %loom_db, "loom workflow engine open");
    // Evaluations and drift propagate into the agents' Soma presence via the sink adapter.
    let thymus_db = db_path("SYNTHEOS_THYMUS_DB", "data/thymus.sqlite")?;
    let thymus = Arc::new(
        ThymusStore::open(&thymus_db, bus.clone())?
            .with_quality_sink(Box::new(SomaQualitySink(soma.clone()))),
    );
    tracing::info!(path = %thymus_db, "thymus quality store open");

    // The Phase 2 dispatcher (Story 2.6): the REAL EidolonGate in the eidolon slot (its drift
    // policy reads the Thymus store), fail-closed deny-stubs in the other four slots until those
    // authorities land, the deny executor (no real executor until Hermes, Phase 5), and the
    // eidolon output filter scrubbing credential fields from any executor result.
    // `Dispatcher::new` re-validates the chain is exactly canonical at boot.
    let policy = EidolonPolicy::default();
    let dispatcher = Arc::new(
        Dispatcher::new(
            live_gate_chain(&policy, thymus.clone())?,
            Box::new(DenyExecutor),
            bus.clone(),
        )?
        .with_output_filter(Box::new(EidolonOutputFilter::new(&policy)?)),
    );

    let state = AppState::new(dispatcher, directory, bus, chiasm, soma, broca, loom, thymus);

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
    tracing::info!(%addr, "syntheos-server listening (eidolon live; pistis/plutus/human/phylax deny-stubbed)");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

/// Resolve a service database path from `var` (default `default`), creating the parent
/// directory if absent so `Connection::open` can create the file.
fn db_path(var: &str, default: &str) -> Result<String, std::io::Error> {
    let path = std::env::var(var).unwrap_or_else(|_| default.to_string());
    if let Some(parent) = std::path::Path::new(&path).parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    Ok(path)
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
