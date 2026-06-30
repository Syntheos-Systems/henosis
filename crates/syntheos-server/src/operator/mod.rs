//! The operator API module: authentication, RBAC, dashboard, and WebSocket hub.
//!
//! This module provides the in-process operator surface for the Henosis agent OS.
//! It is additive -- the default kernel server is unchanged when `OperatorState`
//! is not constructed. The operator routes mount only when a JWT secret is
//! configured (Task 7 wires the conditional mount into `app.rs`).
//!
//! Submodule layout:
//! - [`auth`]: JWT claims, sign/decode, `OperatorError` (Task 2).
//! - [`rbac`]: `OperatorAuth` extractor + `require(perm)` (Task 4).
//! - [`dashboard`]: `GET /api/dashboard` composition (Task 5).
//! - [`ws`]: `GET /ws` WebSocket hub over [`syntheos_axon::AxonBus`] (Task 6).

use std::sync::Arc;

use axum::routing::{get, post};
use axum::Router;

pub mod auth;
pub mod dashboard;
pub mod rbac;
/// The WebSocket event hub: `GET /ws` streaming org-filtered AxonBus events (Task 6).
pub mod ws;

/// Shared state for every operator route handler.
///
/// All fields are `Arc`-wrapped so the struct is cheap to clone -- axum's
/// `State` extractor clones the state per request.
#[derive(Clone)]
pub struct OperatorState {
    /// The operator-account store (argon2 passwords, email -> principal mapping).
    /// Used by the login handler and the bootstrap path in `main.rs`.
    pub accounts: Arc<syntheos_identity::SqliteDirectory>,

    /// The Plutus policy backend (RBAC, org status, quotas).
    ///
    /// Typed as `Arc<dyn PolicyBackend>` so handler tests can build a real
    /// `OperatorState` with a `MockPolicyBackend` instead of a live Postgres
    /// connection. In production `main.rs` wraps a `PlutusStore`, which
    /// implements `PolicyBackend`.
    pub plutus: Arc<dyn henosis_plutus::PolicyBackend>,

    /// The raw HS256 signing secret. All operator JWTs are signed and verified
    /// with this key.
    pub jwt_secret: Arc<Vec<u8>>,

    /// The Soma presence store (agent registry), shared with `AppState`.
    /// Consumed by `GET /api/dashboard`.
    pub soma: Arc<henosis_soma::SomaStore>,

    /// The Chiasm task store, shared with `AppState`.
    /// Consumed by `GET /api/dashboard`.
    pub chiasm: Arc<henosis_chiasm::ChiasmStore>,

    /// The Broca narration log, shared with `AppState`.
    /// Consumed by `GET /api/dashboard` (activity feed).
    pub broca: Arc<henosis_broca::BrocaStore>,

    /// The Thymus quality store, shared with `AppState`.
    /// Consumed by `GET /api/dashboard` (quality section).
    pub thymus: Arc<henosis_thymus::ThymusStore>,

    /// The Loom workflow engine, shared with `AppState`.
    /// Consumed by `GET /api/dashboard` (workflow section).
    pub loom: Arc<henosis_loom::LoomStore>,

    /// The in-process Axon event bus, shared with `AppState`.
    /// Consumed by `GET /ws` (org-filtered event stream).
    pub axon: Arc<syntheos_axon::AxonBus>,
}

/// Build the operator API router: all six operator routes bound to `state`.
///
/// Routes:
/// - `POST /api/auth/login` -- verify credentials and issue a 24-hour JWT.
/// - `GET  /api/auth/session` -- decode the Bearer token and return its claims.
/// - `POST /api/auth/refresh` -- re-sign a valid token for another 24 hours.
/// - `POST /api/auth/logout` -- client-side session termination (stateless; always 200).
/// - `GET  /api/dashboard` -- composed kernel-store snapshot (requires `OrgRead`).
/// - `GET  /ws` -- org-filtered WebSocket event hub over the AxonBus.
///
/// Called from `app::router` when an `OperatorState` is present (`Some`).
/// The resulting `Router` has its state bound and can be merged into the
/// kernel router without further configuration.
pub fn operator_router(state: OperatorState) -> Router {
    Router::new()
        .route("/api/auth/login", post(auth::login))
        .route("/api/auth/session", get(auth::session))
        .route("/api/auth/refresh", post(auth::refresh))
        .route("/api/auth/logout", post(auth::logout))
        .route("/api/dashboard", get(dashboard::dashboard))
        .route("/ws", get(ws::ws_handler))
        .with_state(state)
}
