//! Internal bridge endpoints for notifying the gateway about externally-created messages.
//! Secured via the JWT secret passed as a Bearer token (same secret the bridge already has).

use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    Json,
};
use serde::Deserialize;
use sqlx::PgPool;
use uuid::Uuid;

use crate::config::Config;
use crate::db;
use crate::ws::gateway::{Gateway, GatewayEvent};

#[derive(Deserialize)]
pub struct NotifyRequest {
    pub channel_id: Uuid,
    pub message_id: Uuid,
}

/// POST /api/bridge/notify
///
/// Called by the bridge after it inserts a message directly into the DB.
/// Fetches the full message and broadcasts a MessageCreate event via the gateway.
pub async fn notify_message(
    State(pool): State<PgPool>,
    State(gateway): State<Gateway>,
    State(config): State<Config>,
    headers: HeaderMap,
    Json(req): Json<NotifyRequest>,
) -> StatusCode {
    // Authenticate with JWT secret as bearer token
    let auth = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));

    match auth {
        Some(token) if token == config.jwt_secret => {}
        _ => return StatusCode::UNAUTHORIZED,
    }

    // Fetch the full message with author info
    let msg = match db::get_message_by_id(&pool, req.message_id).await {
        Ok(Some(m)) => m,
        _ => return StatusCode::NOT_FOUND,
    };

    // Fetch attachments
    let attachments = db::get_attachments_for_message(&pool, req.message_id)
        .await
        .unwrap_or_default();

    // Broadcast to channel subscribers
    gateway.broadcast_to_channel(
        req.channel_id,
        GatewayEvent::MessageCreate {
            id: msg.id,
            channel_id: msg.channel_id,
            author_id: msg.author_id,
            author_username: msg.author_username.clone(),
            author_display_name: msg.author_display_name.clone(),
            author_avatar_url: msg.author_avatar_url.clone(),
            content: msg.content.clone(),
            attachments,
            message_type: msg.message_type.clone(),
            created_at: msg.created_at.to_rfc3339(),
        },
    );

    StatusCode::OK
}
