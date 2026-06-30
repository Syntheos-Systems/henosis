//! The operator API module: authentication, RBAC, dashboard, and WebSocket hub.
//!
//! This module provides the in-process operator surface for the Henosis agent OS.
//! It is additive -- the default kernel server is unchanged when `OperatorState`
//! is not constructed. The operator routes mount only when a JWT secret is
//! configured (Task 7 wires the conditional mount into `app.rs`).
//!
//! Submodule layout (tasks add submodules as they land):
//! - [`auth`]: JWT claims, sign/decode, `OperatorError` (Task 2).
//! - `rbac` (Task 4): `OperatorAuth` extractor + `require(perm)`.
//! - `dashboard` (Task 5): `GET /api/dashboard` composition.
//! - `ws` (Task 6): `GET /ws` WebSocket hub over [`syntheos_axon::AxonBus`].

use std::sync::Arc;

pub mod auth;
pub mod rbac;

/// Shared state for every operator route handler.
///
/// All fields are `Arc`-wrapped so the struct is cheap to clone -- axum's
/// `State` extractor clones the state per request. Fields that are not yet
/// consumed by existing tasks are documented with the task that will use them.
#[derive(Clone)]
pub struct OperatorState {
    /// The operator-account store (argon2 passwords, email -> principal mapping).
    /// Consumed by Task 3 (login handler) and Task 7 (bootstrap).
    pub accounts: Arc<syntheos_identity::SqliteDirectory>,

    /// The Plutus policy store (RBAC, org status, quotas).
    /// Consumed by Task 3 (org/role resolution) and Task 4 (RBAC extractor).
    pub plutus: Arc<henosis_plutus::PlutusStore>,

    /// The raw HS256 signing secret.  All operator JWTs are signed and verified
    /// with this key.  Task 2 uses it; Task 3 passes it to `sign`/`decode`.
    pub jwt_secret: Arc<Vec<u8>>,

    /// The Soma presence store (agent registry).
    /// Consumed by Task 5 (dashboard `/api/dashboard` composition).
    pub soma: Arc<henosis_soma::SomaStore>,

    /// The Chiasm task store.
    /// Consumed by Task 5 (dashboard `/api/dashboard` composition).
    pub chiasm: Arc<henosis_chiasm::ChiasmStore>,

    /// The Broca narration log.
    /// Consumed by Task 5 (dashboard `/api/dashboard` activity feed).
    pub broca: Arc<henosis_broca::BrocaStore>,

    /// The Thymus quality store.
    /// Consumed by Task 5 (dashboard `/api/dashboard` quality section).
    pub thymus: Arc<henosis_thymus::ThymusStore>,

    /// The Loom workflow engine.
    /// Consumed by Task 5 (dashboard `/api/dashboard` workflow section).
    pub loom: Arc<henosis_loom::LoomStore>,

    /// The in-process Axon event bus.
    /// Consumed by Task 6 (`GET /ws` WebSocket hub, org-filtered event stream).
    pub axon: Arc<syntheos_axon::AxonBus>,
}
