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
use std::time::Duration;

use axum::http::{header, HeaderValue, Method};
use axum::routing::{get, post};
use axum::Router;
use tower_http::cors::CorsLayer;

pub mod auth;
pub mod dashboard;
pub mod rbac;
/// The WebSocket event hub: `GET /ws` streaming org-filtered AxonBus events (Task 6).
pub mod ws;

/// Default CORS origins for the operator surface when `SYNTHEOS_OPERATOR_CORS_ORIGINS` is
/// unset: the Tauri webview origins used by the Athena desktop app on Linux/Windows.
const DEFAULT_CORS_ORIGINS: [&str; 2] = ["tauri://localhost", "http://tauri.localhost"];

/// Parse the operator surface's allowed CORS origins from the environment.
///
/// The operator API (`/api/auth/*`, `/api/dashboard`, `/ws`) is served to browser-origin
/// clients, not server-to-server callers: the Athena Tauri app (webview origin
/// `tauri://localhost` / `http://tauri.localhost`) and, in development, the Vite dev server.
/// Those clients issue CORS-preflighted requests (JSON `content-type` + `Authorization`
/// header), so this surface -- and only this surface, never the kernel routes -- needs an
/// explicit origin allow-list or every browser/webview client is silently blocked.
///
/// `SYNTHEOS_OPERATOR_CORS_ORIGINS` is a comma-separated list of exact origins, each trimmed
/// of surrounding whitespace. When unset (or empty), the Tauri webview origins are the
/// default so the desktop app works with no configuration; local development overrides via
/// the env var, e.g. to add `http://localhost:5173` for the Vite dev server.
///
/// An entry that fails to parse as a valid HTTP header value is a hard boot error --
/// misconfiguration here must never fail silently into "every browser client is blocked" or,
/// worse, a typo that resolves to an unintended allow-list.
pub fn cors_origins_from_env() -> Result<Vec<HeaderValue>, String> {
    parse_cors_origins(std::env::var("SYNTHEOS_OPERATOR_CORS_ORIGINS").ok())
}

/// The pure parsing logic behind [`cors_origins_from_env`], split out so it is testable
/// without mutating the real process environment (env vars are process-global and racy
/// across parallel `#[test]` threads; taking the raw value as a parameter sidesteps that).
fn parse_cors_origins(raw: Option<String>) -> Result<Vec<HeaderValue>, String> {
    let entries: Vec<String> = match raw {
        Some(v) if !v.trim().is_empty() => v
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect(),
        _ => DEFAULT_CORS_ORIGINS.iter().map(|s| s.to_string()).collect(),
    };
    entries
        .into_iter()
        .map(|origin| {
            HeaderValue::from_str(&origin)
                .map_err(|e| format!("invalid CORS origin {origin:?}: {e}"))
        })
        .collect()
}

/// Build the operator surface's CORS layer from a resolved origin allow-list.
///
/// Scoped to exactly the methods and headers the operator API uses (`GET`/`POST`/`OPTIONS`,
/// `content-type`/`authorization`). The operator surface authenticates with Bearer tokens
/// carried in the `Authorization` header, never cookies, so `allow_credentials` is
/// deliberately left off -- there is nothing browser-managed to protect.
pub fn operator_cors_layer(origins: Vec<HeaderValue>) -> CorsLayer {
    CorsLayer::new()
        .allow_origin(origins)
        .allow_methods([Method::GET, Method::POST, Method::OPTIONS])
        .allow_headers([header::CONTENT_TYPE, header::AUTHORIZATION])
        .max_age(Duration::from_secs(3600))
}

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

    /// The resolved CORS origin allow-list for this operator surface.
    ///
    /// Populated at boot from [`cors_origins_from_env`] (falls back to the Tauri webview
    /// origins when unset). Consumed by [`operator_router`] to build the CORS layer applied
    /// only to the operator routes -- the kernel router is never CORS-gated.
    pub cors_origins: Arc<Vec<HeaderValue>>,
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
    // The operator surface is the only browser/webview-facing part of this server (the kernel
    // routes are server-to-server); the CORS layer is scoped here, not at the top-level router,
    // so the kernel surface's behaviour is unaffected.
    let cors = operator_cors_layer((*state.cors_origins).clone());
    Router::new()
        .route("/api/auth/login", post(auth::login))
        .route("/api/auth/session", get(auth::session))
        .route("/api/auth/refresh", post(auth::refresh))
        .route("/api/auth/logout", post(auth::logout))
        .route("/api/dashboard", get(dashboard::dashboard))
        .route("/ws", get(ws::ws_handler))
        .with_state(state)
        .layer(cors)
}

#[cfg(test)]
/// Unit tests for the CORS origin-list parsing (`SYNTHEOS_OPERATOR_CORS_ORIGINS`).
mod tests {
    use super::*;

    /// Unset (`None`) falls back to the two Tauri webview origins, in order.
    #[test]
    fn parse_cors_origins_defaults_to_tauri_when_unset() {
        let origins = parse_cors_origins(None).expect("default origins parse");
        assert_eq!(
            origins,
            vec![
                HeaderValue::from_static("tauri://localhost"),
                HeaderValue::from_static("http://tauri.localhost"),
            ]
        );
    }

    /// An empty string is treated the same as unset (falls back to the defaults), since a
    /// blank env var is very unlikely to be an intentional "allow nothing".
    #[test]
    fn parse_cors_origins_defaults_when_empty_string() {
        let origins = parse_cors_origins(Some("   ".to_string())).expect("default origins parse");
        assert_eq!(origins.len(), 2);
    }

    /// A comma-separated list splits and trims surrounding whitespace around each entry.
    #[test]
    fn parse_cors_origins_splits_and_trims() {
        let origins = parse_cors_origins(Some(
            " http://localhost:5173 , tauri://localhost ,http://tauri.localhost".to_string(),
        ))
        .expect("configured origins parse");
        assert_eq!(
            origins,
            vec![
                HeaderValue::from_static("http://localhost:5173"),
                HeaderValue::from_static("tauri://localhost"),
                HeaderValue::from_static("http://tauri.localhost"),
            ]
        );
    }

    /// An entry that is not a valid HTTP header value is a hard error, not a silent skip.
    #[test]
    fn parse_cors_origins_rejects_invalid_entry() {
        // A raw newline byte is rejected by `HeaderValue::from_str` (control character).
        let result = parse_cors_origins(Some("http://localhost:5173,bad\norigin".to_string()));
        assert!(
            result.is_err(),
            "an entry that is not a valid header value must be a hard error, got: {result:?}"
        );
    }
}
