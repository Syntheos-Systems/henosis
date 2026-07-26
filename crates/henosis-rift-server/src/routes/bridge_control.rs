use axum::{
    Json,
    extract::{Path, State},
    http::HeaderMap,
};
use serde::Serialize;
use sqlx::PgPool;
use uuid::Uuid;

use crate::auth::middleware::AuthUser;
use crate::config::Config;
use crate::db;
use crate::error::AppError;
use crate::models::permissions::perms;
use crate::routes::bridge::bridge_authorized;

/// Response for bridge status.
#[derive(Serialize)]
pub struct BridgeStatus {
    /// Whether the bridge is currently paused.
    pub paused: bool,
}

/// Return whether a human member has authority to control one server's bridge.
fn can_control_bridge(is_agent: bool, is_member: bool, permissions: i64) -> bool {
    !is_agent && is_member && perms::has(permissions, perms::MANAGE_SERVER)
}

/// Require server-management authority from a human bridge controller.
async fn require_bridge_controller(
    pool: &PgPool,
    server_id: Uuid,
    user_id: Uuid,
) -> Result<(), AppError> {
    let user = db::get_user_by_id(pool, user_id)
        .await?
        .ok_or(AppError::NotFound("user".into()))?;
    let is_member = db::is_member(pool, server_id, user_id).await?;
    let permissions = if is_member {
        db::get_member_permissions(pool, server_id, user_id).await?
    } else {
        0
    };
    if !can_control_bridge(user.is_agent, is_member, permissions) {
        return Err(AppError::Forbidden);
    }
    Ok(())
}

/// Pause agent activity for one managed server.
pub async fn pause_bridge(
    auth: AuthUser,
    State(pool): State<PgPool>,
    Path(server_id): Path<Uuid>,
) -> Result<Json<BridgeStatus>, AppError> {
    require_bridge_controller(&pool, server_id, auth.user_id).await?;
    db::set_bridge_paused(&pool, server_id, true).await?;
    Ok(Json(BridgeStatus { paused: true }))
}

/// Resume agent activity for one managed server.
pub async fn resume_bridge(
    auth: AuthUser,
    State(pool): State<PgPool>,
    Path(server_id): Path<Uuid>,
) -> Result<Json<BridgeStatus>, AppError> {
    require_bridge_controller(&pool, server_id, auth.user_id).await?;
    db::set_bridge_paused(&pool, server_id, false).await?;
    Ok(Json(BridgeStatus { paused: false }))
}

/// Get one server's bridge status.
///
/// Two callers legitimately read this: the bridge daemon's pause poller, which
/// presents the shared bridge secret, and a human operator inspecting the same
/// control they can toggle. Anyone else is refused -- left open, this route
/// disclosed per-server bridge state to anonymous callers and answered as a
/// server-existence oracle for arbitrary identifiers.
pub async fn bridge_status(
    State(pool): State<PgPool>,
    State(config): State<Config>,
    headers: HeaderMap,
    auth: Option<AuthUser>,
    Path(server_id): Path<Uuid>,
) -> Result<Json<BridgeStatus>, AppError> {
    if !bridge_authorized(&headers, &config) {
        let auth = auth.ok_or(AppError::Unauthorized)?;
        require_bridge_controller(&pool, server_id, auth.user_id).await?;
    }
    let paused = db::is_bridge_paused(&pool, server_id).await?;
    Ok(Json(BridgeStatus { paused }))
}

/// Covers the pure bridge-control authorization predicate.
#[cfg(test)]
mod tests {
    use super::*;

    /// Reject agents even when their role carries server-management authority.
    #[test]
    fn agents_cannot_control_bridge() {
        assert!(!can_control_bridge(true, true, perms::MANAGE_SERVER));
    }

    /// Reject humans who do not belong to the target server.
    #[test]
    fn nonmembers_cannot_control_bridge() {
        assert!(!can_control_bridge(false, false, perms::MANAGE_SERVER));
    }

    /// Reject ordinary human members without the management bit.
    #[test]
    fn ordinary_members_cannot_control_bridge() {
        assert!(!can_control_bridge(false, true, perms::DEFAULT));
    }

    /// Allow human members with direct or administrator management authority.
    #[test]
    fn managers_can_control_bridge() {
        assert!(can_control_bridge(false, true, perms::MANAGE_SERVER));
        assert!(can_control_bridge(false, true, perms::ADMINISTRATOR));
    }
}
