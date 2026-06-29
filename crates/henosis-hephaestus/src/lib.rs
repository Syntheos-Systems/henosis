//! Hephaestus library surface. The binary at `src/main.rs` re-exports these
//! modules via the `henosis_hephaestus` crate name; integration tests under
//! `tests/` also import from here.
//!
//! Absorbed from ~/projects/hephaestus (Phase 5 / Story 5.3, copy-and-own).
//! Upstream hephaestus stays standalone; this is a snapshot wired into the
//! henosis workspace.

pub mod agent_forge;
pub mod anthropic_auth;
pub mod checkpoint;
pub mod clients;
pub mod config;
pub mod gate;
pub mod hermes_client;
pub mod orchestrator;
pub mod provider;
pub mod providers;
pub mod sandbox;
pub mod services;
pub mod streaming;
pub mod tasks;

use std::sync::Arc;

use axum::{routing::get, routing::post, Json, Router};
use serde_json::json;
use tracing::info;

pub use clients::Clients;
pub use config::Config;
pub use tasks::{run_task_to_completion, AppState, CreateTaskBody, TaskRecord, TaskStatus, TaskStore};

/// Canonical service name used in logs, health responses, and version output.
pub const SERVICE: &str = "hephaestus";

/// Crate version forwarded from `Cargo.toml` at compile time.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Build the axum Router for the public API. Exposed for tests so they can
/// drive the full request path against an in-process server.
pub fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/version", get(version))
        .route("/tasks", post(tasks::create_task))
        .route("/tasks/{id}", get(tasks::get_task))
        .route("/tasks/{id}/resume", post(tasks::resume_task))
        .route("/tasks/{id}/stream", get(streaming::stream_task))
        .with_state(state)
}

/// Construct the AppState used by the binary and tests.
pub fn build_state(cfg: Config) -> AppState {
    AppState {
        clients: Arc::new(Clients::new(cfg)),
        store: Arc::new(TaskStore::default()),
        streams: Arc::new(streaming::StreamHub::new()),
    }
}

/// Replay tasks that were in flight when the binary last shut down. For each
/// recoverable record, hydrate the TaskRecord, insert it into the in-memory
/// store, and spawn `resume_task_from_kleos`. Returns the number of tasks
/// resumed.
pub async fn recover_in_flight_tasks(state: &AppState) -> usize {
    let recoverable = state.clients.kleos_recover_tasks().await;
    let mut resumed = 0usize;
    let mut seen = std::collections::HashSet::<String>::new();
    for mem in recoverable {
        let Some(rec) = parse_task_record(&mem) else { continue };
        if !seen.insert(rec.id.clone()) {
            continue;
        }
        if matches!(rec.status, TaskStatus::Completed | TaskStatus::Failed) {
            continue;
        }
        info!(task_id = %rec.id, status = ?rec.status, "resuming task from Kleos");
        state.store.insert(rec.clone()).await;
        let state_spawn = state.clone();
        tokio::spawn(async move {
            tasks::resume_task_from_kleos(state_spawn, rec).await;
        });
        resumed += 1;
    }
    resumed
}

/// Pull the inner `TaskRecord` out of a Kleos search hit. Each hit's
/// `content` field is the JSON-serialized record produced by
/// `kleos_store_task`.
pub fn parse_task_record(mem: &serde_json::Value) -> Option<TaskRecord> {
    let content = mem.get("content").and_then(|c| c.as_str())?;
    serde_json::from_str(content).ok()
}

/// Axum handler for `GET /health`. Returns a JSON object confirming the
/// service is alive. No dependencies checked -- liveness only.
async fn health() -> Json<serde_json::Value> {
    Json(json!({ "status": "ok", "service": SERVICE }))
}

/// Axum handler for `GET /version`. Returns the service name and compiled
/// version string from `Cargo.toml`.
async fn version() -> Json<serde_json::Value> {
    Json(json!({ "name": SERVICE, "version": VERSION }))
}
