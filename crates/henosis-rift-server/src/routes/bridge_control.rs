//! Human and daemon bridge-state routes with separate authentication boundaries.

use axum::{
    Json,
    extract::{Path, State},
    http::HeaderMap,
};
use sqlx::PgPool;
use uuid::Uuid;

use crate::auth::middleware::AuthUser;
use crate::config::Config;
use crate::db;
use crate::error::AppError;
use crate::models::agent_control::BridgeStatus;
use crate::models::permissions::perms;

/// Return whether a human member has authority to control one server's bridge.
fn can_control_bridge(is_agent: bool, is_member: bool, permissions: i64) -> bool {
    !is_agent && is_member && perms::has(permissions, perms::MANAGE_SERVER)
}

/// Return whether a human room member may read public bridge status.
fn can_view_bridge_status(is_agent: bool, is_member: bool) -> bool {
    !is_agent && is_member
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

/// Require human membership before exposing public bridge status.
async fn require_bridge_viewer(
    pool: &PgPool,
    server_id: Uuid,
    user_id: Uuid,
) -> Result<(), AppError> {
    let user = db::get_user_by_id(pool, user_id)
        .await?
        .ok_or(AppError::NotFound("user".into()))?;
    let is_member = db::is_member(pool, server_id, user_id).await?;
    if !can_view_bridge_status(user.is_agent, is_member) {
        return Err(AppError::Forbidden);
    }
    Ok(())
}

/// Build the complete public bridge status from durable Rift state.
async fn load_bridge_status(pool: &PgPool, server_id: Uuid) -> Result<BridgeStatus, AppError> {
    let paused = db::is_bridge_paused(pool, server_id).await?;
    let roster = db::agent_control::read_room_agent_roster(pool, server_id).await?;
    Ok(BridgeStatus {
        paused,
        desired_revision: roster.desired_revision,
        active_revision: roster.active_revision,
        last_good_revision: roster.last_good_revision,
        apply_state: roster.apply_state,
        apply_error_code: roster.apply_error_code,
        apply_error_message: roster.apply_error_message,
    })
}

/// Pause agent activity for one managed server.
pub async fn pause_bridge(
    auth: AuthUser,
    State(pool): State<PgPool>,
    Path(server_id): Path<Uuid>,
) -> Result<Json<BridgeStatus>, AppError> {
    require_bridge_controller(&pool, server_id, auth.user_id).await?;
    db::set_bridge_paused(&pool, server_id, true).await?;
    Ok(Json(load_bridge_status(&pool, server_id).await?))
}

/// Resume agent activity for one managed server.
pub async fn resume_bridge(
    auth: AuthUser,
    State(pool): State<PgPool>,
    Path(server_id): Path<Uuid>,
) -> Result<Json<BridgeStatus>, AppError> {
    require_bridge_controller(&pool, server_id, auth.user_id).await?;
    db::set_bridge_paused(&pool, server_id, false).await?;
    Ok(Json(load_bridge_status(&pool, server_id).await?))
}

/// Return public bridge status to an authenticated human room member.
///
/// Membership is required because an open route disclosed per-server bridge
/// state to anonymous callers and answered as a server-existence oracle for
/// arbitrary identifiers. The bridge daemon's pause poller does not use this
/// route; it reads through the bridge-secret boundary below.
pub async fn bridge_status(
    auth: AuthUser,
    State(pool): State<PgPool>,
    Path(server_id): Path<Uuid>,
) -> Result<Json<BridgeStatus>, AppError> {
    require_bridge_viewer(&pool, server_id, auth.user_id).await?;
    Ok(Json(load_bridge_status(&pool, server_id).await?))
}

/// Return bridge status to the daemon through the bridge-secret boundary.
pub async fn daemon_bridge_status(
    State(pool): State<PgPool>,
    State(config): State<Config>,
    Path(server_id): Path<Uuid>,
    headers: HeaderMap,
) -> Result<Json<BridgeStatus>, AppError> {
    if !super::bridge::bridge_authorized(&headers, &config) {
        return Err(AppError::Unauthorized);
    }
    Ok(Json(load_bridge_status(&pool, server_id).await?))
}

#[cfg(test)]
/// Covers human bridge control, public visibility, and daemon auth boundaries.
mod tests {
    use axum::http::HeaderValue;

    use super::*;

    /// Build minimal runtime configuration for bridge-secret predicate tests.
    fn config_with_secret(secret: &str) -> Config {
        Config {
            database_url: String::new(),
            jwt_secret: "jwt-secret".to_string(),
            bridge_secret: secret.to_string(),
            listen_addr: String::new(),
            cors_origins: Vec::new(),
            upload_dir: String::new(),
            max_upload_bytes: 0,
        }
    }

    /// Reject agents even when their role carries server-management authority.
    #[test]
    fn agents_cannot_control_bridge() {
        assert!(!can_control_bridge(true, true, perms::MANAGE_SERVER));
        assert!(!can_view_bridge_status(true, true));
    }

    /// Reject humans who do not belong to the target server.
    #[test]
    fn nonmembers_cannot_control_or_view_bridge() {
        assert!(!can_control_bridge(false, false, perms::MANAGE_SERVER));
        assert!(!can_view_bridge_status(false, false));
    }

    /// Ordinary human members may view status but cannot mutate bridge state.
    #[test]
    fn ordinary_members_have_read_only_status_access() {
        assert!(!can_control_bridge(false, true, perms::DEFAULT));
        assert!(can_view_bridge_status(false, true));
    }

    /// Human managers may control the bridge through direct or administrator authority.
    #[test]
    fn managers_can_control_bridge() {
        assert!(can_control_bridge(false, true, perms::MANAGE_SERVER));
        assert!(can_control_bridge(false, true, perms::ADMINISTRATOR));
    }

    /// The daemon status boundary accepts only the dedicated bridge secret.
    #[test]
    fn daemon_status_requires_bridge_secret() {
        let config = config_with_secret("bridge-secret");
        let mut correct = HeaderMap::new();
        correct.insert(
            "authorization",
            HeaderValue::from_static("Bearer bridge-secret"),
        );
        let mut jwt = HeaderMap::new();
        jwt.insert(
            "authorization",
            HeaderValue::from_static("Bearer jwt-secret"),
        );
        assert!(super::super::bridge::bridge_authorized(&correct, &config));
        assert!(!super::super::bridge::bridge_authorized(&jwt, &config));
        assert!(!super::super::bridge::bridge_authorized(
            &HeaderMap::new(),
            &config
        ));
    }
}
