use axum::{
    extract::{Path, State},
    Json,
};
use sqlx::PgPool;
use uuid::Uuid;

use crate::auth::middleware::AuthUser;
use crate::db;
use crate::error::AppError;
use crate::models::channel::{Channel, CreateChannelRequest, UpdateChannelRequest};
use crate::models::permissions::perms;
use crate::ws::gateway::{Gateway, GatewayEvent};

/// POST /api/servers/:server_id/channels
pub async fn create_channel(
    State(pool): State<PgPool>,
    State(gateway): State<Gateway>,
    auth: AuthUser,
    Path(server_id): Path<Uuid>,
    Json(req): Json<CreateChannelRequest>,
) -> Result<Json<Channel>, AppError> {
    require_permission(&pool, server_id, auth.user_id, perms::MANAGE_CHANNELS).await?;

    let name = req.name.trim();
    if name.is_empty() || name.len() > 100 {
        return Err(AppError::BadRequest("Channel name must be 1-100 characters".into()));
    }

    let channel_type = req.channel_type.as_deref().unwrap_or("text");
    let channel =
        db::create_channel(&pool, server_id, name, req.topic.as_deref(), channel_type).await?;

    // Broadcast to server
    gateway.broadcast_to_server(
        server_id,
        GatewayEvent::ChannelCreate {
            id: channel.id,
            server_id,
            name: channel.name.clone(),
            channel_type: channel.channel_type.clone(),
        },
    );

    Ok(Json(channel))
}

/// GET /api/servers/:server_id/channels
pub async fn list_channels(
    State(pool): State<PgPool>,
    auth: AuthUser,
    Path(server_id): Path<Uuid>,
) -> Result<Json<Vec<Channel>>, AppError> {
    require_member(&pool, server_id, auth.user_id).await?;
    let channels = db::get_server_channels(&pool, server_id).await?;
    Ok(Json(channels))
}

/// PATCH /api/channels/:channel_id
pub async fn update_channel(
    State(pool): State<PgPool>,
    auth: AuthUser,
    Path(channel_id): Path<Uuid>,
    Json(req): Json<UpdateChannelRequest>,
) -> Result<Json<Channel>, AppError> {
    let channel = db::get_channel_by_id(&pool, channel_id)
        .await?
        .ok_or(AppError::NotFound("Channel not found".into()))?;

    require_permission(&pool, channel.server_id, auth.user_id, perms::MANAGE_CHANNELS).await?;

    let updated = db::update_channel(
        &pool,
        channel_id,
        req.name.as_deref(),
        req.topic.as_deref(),
        req.position,
    )
    .await?;

    Ok(Json(updated))
}

/// DELETE /api/channels/:channel_id
pub async fn delete_channel(
    State(pool): State<PgPool>,
    State(gateway): State<Gateway>,
    auth: AuthUser,
    Path(channel_id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, AppError> {
    let channel = db::get_channel_by_id(&pool, channel_id)
        .await?
        .ok_or(AppError::NotFound("Channel not found".into()))?;

    require_permission(&pool, channel.server_id, auth.user_id, perms::MANAGE_CHANNELS).await?;

    let server_id = channel.server_id;
    db::delete_channel(&pool, channel_id).await?;

    gateway.broadcast_to_server(
        server_id,
        GatewayEvent::ChannelDelete {
            id: channel_id,
            server_id,
        },
    );

    Ok(Json(serde_json::json!({ "ok": true })))
}

// ─── Helpers ───

async fn require_member(pool: &PgPool, server_id: Uuid, user_id: Uuid) -> Result<(), AppError> {
    if !db::is_member(pool, server_id, user_id).await? {
        return Err(AppError::Forbidden);
    }
    Ok(())
}

async fn require_permission(
    pool: &PgPool,
    server_id: Uuid,
    user_id: Uuid,
    permission: i64,
) -> Result<(), AppError> {
    require_member(pool, server_id, user_id).await?;
    let user_perms = db::get_member_permissions(pool, server_id, user_id).await?;
    if !perms::has(user_perms, permission) {
        return Err(AppError::Forbidden);
    }
    Ok(())
}
