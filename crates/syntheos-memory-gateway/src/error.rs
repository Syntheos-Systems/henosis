//! Error type for the gateway and its mapping to HTTP responses that line up
//! with the wire contract's documented error codes (401/404/5xx).

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

/// Errors that can occur while translating a request to Kleos.
#[derive(Debug, thiserror::Error)]
pub enum GatewayError {
    /// The upstream Kleos request could not be performed (network/transport).
    #[error("upstream request failed: {0}")]
    Upstream(#[from] reqwest::Error),
    /// The supplied memory id was not a valid Kleos id.
    #[error("invalid memory id: {0}")]
    InvalidId(String),
    /// The memory was not found upstream.
    #[error("memory not found")]
    NotFound,
    /// Kleos returned a non-success status that the gateway forwards verbatim.
    #[error("kleos returned status {0}")]
    KleosStatus(StatusCode),
    /// Request signing failed (key not loaded or crypto error).
    #[error("signing error: {0}")]
    Signing(String),
}

/// HTTP response mapping for gateway errors.
impl IntoResponse for GatewayError {
    /// Map each error variant to the closest HTTP status and a JSON error body.
    fn into_response(self) -> Response {
        let status = match &self {
            GatewayError::InvalidId(_) | GatewayError::NotFound => StatusCode::NOT_FOUND,
            GatewayError::KleosStatus(s) => *s,
            GatewayError::Upstream(_) => StatusCode::BAD_GATEWAY,
            GatewayError::Signing(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        // Variants that wrap internal detail (transport error chains, crypto/key
        // internals) get a generic external message so network topology and
        // secrets are not leaked to clients; the full error is logged
        // server-side only.
        let external = match &self {
            GatewayError::InvalidId(_) => "invalid memory id".to_string(),
            GatewayError::NotFound => "memory not found".to_string(),
            GatewayError::KleosStatus(s) => format!("upstream returned status {s}"),
            GatewayError::Upstream(_) => "upstream request failed".to_string(),
            GatewayError::Signing(_) => "internal signing error".to_string(),
        };
        if matches!(self, GatewayError::Upstream(_) | GatewayError::Signing(_)) {
            tracing::warn!(error = %self, "gateway error (detail withheld from client)");
        }
        (status, Json(json!({ "error": external }))).into_response()
    }
}
