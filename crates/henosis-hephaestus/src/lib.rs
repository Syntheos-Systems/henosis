//! Hephaestus library surface. The binary at `src/main.rs` re-exports these
//! modules via the `henosis_hephaestus` crate name; integration tests under
//! `tests/` also import from here.
//!
//! Maintained in-tree as an owned Henosis component.

pub mod anthropic_auth;
pub mod checkpoint;
pub mod clients;
pub mod config;
pub mod crucible;
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

use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::{Json, Router, routing::get, routing::post};
use hmac::{Hmac, Mac};
use serde_json::json;
use sha2::Sha256;
use tracing::info;

pub use clients::Clients;
pub use config::Config;
pub use tasks::{
    AppState, CreateTaskBody, TaskRecord, TaskStatus, TaskStore, run_task_to_completion,
};

/// Canonical service name used in logs, health responses, and version output.
pub const SERVICE: &str = "hephaestus";

/// Crate version forwarded from `Cargo.toml` at compile time.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// HMAC type used to compare inbound service tokens without string equality.
type HmacSha256 = Hmac<Sha256>;

/// Build the axum router with public liveness routes and authenticated task control.
pub fn build_router(state: AppState, api_token: String) -> Router {
    let public = Router::new()
        .route("/health", get(health))
        .route("/version", get(version));
    let protected = Router::new()
        .route("/tasks", post(tasks::create_task))
        .route("/tasks/{id}", get(tasks::get_task))
        .route("/tasks/{id}/resume", post(tasks::resume_task))
        .route("/tasks/{id}/stream", get(streaming::stream_task))
        .route_layer(middleware::from_fn_with_state(api_token, require_api_token));

    public.merge(protected).with_state(state)
}

/// Reject task-control requests without the configured service credential.
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

/// Validate one Authorization header against the configured service token.
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

/// Compare service credentials through fixed-size HMAC tags.
fn api_token_matches(expected: &str, presented: &str) -> bool {
    let mut presented_mac =
        HmacSha256::new_from_slice(presented.as_bytes()).expect("HMAC accepts any key length");
    presented_mac.update(b"henosis-hephaestus-inbound-api-token");
    let presented_tag = presented_mac.finalize().into_bytes();

    let mut expected_mac =
        HmacSha256::new_from_slice(expected.as_bytes()).expect("HMAC accepts any key length");
    expected_mac.update(b"henosis-hephaestus-inbound-api-token");
    expected_mac.verify_slice(&presented_tag).is_ok()
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
        let Some(rec) = parse_task_record(&mem) else {
            continue;
        };
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

#[cfg(test)]
/// Tests for standalone request authentication.
mod authentication_tests {
    use super::*;

    /// Missing, malformed, and incorrect credentials fail authentication.
    #[test]
    fn rejects_invalid_authorization_headers() {
        let expected = "hephaestus-api-token-that-is-at-least-32-bytes";
        let mut headers = HeaderMap::new();
        assert!(!authorize_api_token(&headers, expected));

        headers.insert(header::AUTHORIZATION, "not-bearer".parse().unwrap());
        assert!(!authorize_api_token(&headers, expected));

        headers.insert(
            header::AUTHORIZATION,
            "Bearer hephaestus-api-token-that-is-at-least-32-byteS"
                .parse()
                .unwrap(),
        );
        assert!(!authorize_api_token(&headers, expected));
    }

    /// The exact configured credential authenticates successfully.
    #[test]
    fn accepts_exact_authorization_token() {
        let expected = "hephaestus-api-token-that-is-at-least-32-bytes";
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            format!("Bearer {expected}").parse().unwrap(),
        );
        assert!(authorize_api_token(&headers, expected));
    }
}
