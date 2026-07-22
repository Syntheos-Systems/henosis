//! HTTP control server for execution approvals.

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use hmac::{Hmac, Mac};
use serde::Deserialize;
use serde_json::json;
use sha2::Sha256;
use tokio::sync::mpsc;

use crate::config::ControlConfig;
use crate::execution::approval::{ApprovalRegistry, ProposalState};
use crate::execution::{PendingProposal, ProposalId};

/// HMAC type used to compare control credentials through fixed-size tags.
type HmacSha256 = Hmac<Sha256>;

/// JSON body for an approval action.
#[derive(Debug, Deserialize)]
pub struct ControlAction {
    /// "approve" or "reject".
    pub action: String,
}

/// Shared state for the control server.
#[derive(Clone)]
struct ControlState {
    /// Shared approval registry.
    registry: ApprovalRegistry,
    /// Channel that approved proposals are dispatched on.
    approved_tx: mpsc::Sender<PendingProposal>,
    /// Bearer token required on requests.
    auth_token: String,
}

/// Check the Authorization header against the configured bearer token.
pub fn authorize(header: Option<&str>, expected: &str) -> bool {
    let Some(presented) = header.and_then(|value| value.strip_prefix("Bearer ")) else {
        return false;
    };
    let mut presented_mac =
        HmacSha256::new_from_slice(presented.as_bytes()).expect("HMAC accepts any key length");
    presented_mac.update(b"henosis-rift-bridge-control-token");
    let presented_tag = presented_mac.finalize().into_bytes();

    let mut expected_mac =
        HmacSha256::new_from_slice(expected.as_bytes()).expect("HMAC accepts any key length");
    expected_mac.update(b"henosis-rift-bridge-control-token");
    expected_mac.verify_slice(&presented_tag).is_ok()
}

/// Build the control router with the given state.
fn router(state: ControlState) -> Router {
    Router::new()
        .route("/control/approvals", get(list_approvals))
        .route("/control/approvals/{id}", post(act_on_approval))
        .with_state(state)
}

/// GET handler: list pending approvals.
async fn list_approvals(
    headers: HeaderMap,
    State(state): State<ControlState>,
) -> impl IntoResponse {
    if !authorized(&headers, &state) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "unauthorized"})),
        )
            .into_response();
    }
    // Report state so an operator can tell a proposal still awaiting a
    // decision from one already approved and waiting on unpause. Held
    // approvals used to be absent from this list entirely.
    let pending: Vec<_> = state
        .registry
        .list_with_state()
        .into_iter()
        .map(|(p, s)| {
            let state = match s {
                ProposalState::Pending => "pending",
                ProposalState::Approved => "approved_held",
            };
            json!({
                "id": p.id.0,
                "agent": p.agent,
                "task_id": p.task_id,
                "scope": p.scope_summary,
                "state": state,
            })
        })
        .collect();
    (StatusCode::OK, Json(json!({ "pending": pending }))).into_response()
}

/// POST handler: approve or reject a proposal by id.
async fn act_on_approval(
    headers: HeaderMap,
    State(state): State<ControlState>,
    Path(id): Path<u64>,
    Json(body): Json<ControlAction>,
) -> impl IntoResponse {
    if !authorized(&headers, &state) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "unauthorized"})),
        )
            .into_response();
    }
    let proposal_id = ProposalId(id);
    match body.action.as_str() {
        "approve" => match state.registry.approve(proposal_id) {
            Some(proposal) => {
                if state.approved_tx.send(proposal).await.is_err() {
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({"error": "dispatch channel closed"})),
                    )
                        .into_response();
                }
                (StatusCode::OK, Json(json!({"status": "approved"}))).into_response()
            }
            None => (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "unknown proposal"})),
            )
                .into_response(),
        },
        "reject" => {
            if state.registry.reject(proposal_id) {
                (StatusCode::OK, Json(json!({"status": "rejected"}))).into_response()
            } else {
                (
                    StatusCode::NOT_FOUND,
                    Json(json!({"error": "unknown proposal"})),
                )
                    .into_response()
            }
        }
        _ => (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "unknown action"})),
        )
            .into_response(),
    }
}

/// Header-level authorization check shared by handlers.
fn authorized(headers: &HeaderMap, state: &ControlState) -> bool {
    let header = headers.get("authorization").and_then(|h| h.to_str().ok());
    authorize(header, &state.auth_token)
}

/// Spawn the control server on the configured address.
///
/// Approved proposals are sent on `approved_tx` for the main loop to dispatch.
pub async fn serve(
    config: ControlConfig,
    registry: ApprovalRegistry,
    approved_tx: mpsc::Sender<PendingProposal>,
) -> Result<(), crate::error::BridgeError> {
    let bind_addr = config.validate(&[])?;
    let state = ControlState {
        registry,
        approved_tx,
        auth_token: config.auth_token.clone(),
    };
    let listener = tokio::net::TcpListener::bind(bind_addr)
        .await
        .map_err(|e| crate::error::BridgeError::Execution(format!("control bind failed: {e}")))?;
    tracing::info!("control server listening on {bind_addr}");
    axum::serve(listener, router(state))
        .await
        .map_err(|e| crate::error::BridgeError::Execution(format!("control server error: {e}")))?;
    Ok(())
}

/// Covers control-server request authorization and body parsing.
#[cfg(test)]
mod tests {
    use super::{ControlAction, authorize};

    /// Verifies a matching bearer token authorizes the request.
    #[test]
    fn test_authorize_accepts_matching_token() {
        assert!(authorize(Some("Bearer secret"), "secret"));
    }

    /// Verifies a wrong or missing token is rejected.
    #[test]
    fn test_authorize_rejects_bad_token() {
        assert!(!authorize(Some("Bearer nope"), "secret"));
        assert!(!authorize(None, "secret"));
        assert!(!authorize(Some("secret"), "secret")); // missing Bearer prefix
    }

    /// Verifies the control action deserializes from JSON.
    #[test]
    fn test_control_action_deserializes() {
        let a: ControlAction = serde_json::from_str(r#"{"action":"approve"}"#).unwrap();
        assert!(matches!(a, ControlAction { action } if action == "approve"));
    }
}
