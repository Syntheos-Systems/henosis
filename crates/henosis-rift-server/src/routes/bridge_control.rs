use axum::{Json, extract::State};
use serde::Serialize;
use sqlx::PgPool;

use crate::auth::middleware::AuthUser;
use crate::db;
use crate::error::AppError;

/// Response for bridge status.
#[derive(Serialize)]
pub struct BridgeStatus {
    /// Whether the bridge is currently paused.
    pub paused: bool,
}

/// Pause all agent activity. Human-only.
pub async fn pause_bridge(
    auth: AuthUser,
    State(pool): State<PgPool>,
) -> Result<Json<BridgeStatus>, AppError> {
    let user = db::get_user_by_id(&pool, auth.user_id)
        .await?
        .ok_or(AppError::NotFound("user".into()))?;

    if user.is_agent {
        return Err(AppError::Forbidden);
    }

    db::set_bridge_paused(&pool, true).await?;
    Ok(Json(BridgeStatus { paused: true }))
}

/// Resume agent activity. Human-only.
pub async fn resume_bridge(
    auth: AuthUser,
    State(pool): State<PgPool>,
) -> Result<Json<BridgeStatus>, AppError> {
    let user = db::get_user_by_id(&pool, auth.user_id)
        .await?
        .ok_or(AppError::NotFound("user".into()))?;

    if user.is_agent {
        return Err(AppError::Forbidden);
    }

    db::set_bridge_paused(&pool, false).await?;
    Ok(Json(BridgeStatus { paused: false }))
}

/// Get bridge status. Used by bridge to check if paused.
pub async fn bridge_status(State(pool): State<PgPool>) -> Result<Json<BridgeStatus>, AppError> {
    let paused = db::is_bridge_paused(&pool).await?;
    Ok(Json(BridgeStatus { paused }))
}
