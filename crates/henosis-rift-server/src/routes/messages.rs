use axum::{
    extract::{Path, Query, State},
    Json,
};
use sqlx::PgPool;
use uuid::Uuid;

use crate::auth::middleware::AuthUser;
use crate::db;
use crate::error::AppError;
use crate::models::attachment::Attachment;
use crate::models::message::{
    EditMessageRequest, MessageQuery, MessageResponse, SendMessageRequest,
};
use crate::models::permissions::perms;
use crate::config::Config;
use crate::routes::upload::{delete_pending_upload_file, PendingUploads};
use crate::ws::gateway::{Gateway, GatewayEvent};

/// GET /api/channels/:channel_id/messages
pub async fn list_messages(
    State(pool): State<PgPool>,
    auth: AuthUser,
    Path(channel_id): Path<Uuid>,
    Query(query): Query<MessageQuery>,
) -> Result<Json<Vec<MessageResponse>>, AppError> {
    let channel = db::get_channel_by_id(&pool, channel_id)
        .await?
        .ok_or(AppError::NotFound("Channel not found".into()))?;

    require_member(&pool, channel.server_id, auth.user_id).await?;

    let messages = db::get_messages(&pool, channel_id, &query).await?;

    // Batch-load attachments for all messages
    let msg_ids: Vec<Uuid> = messages.iter().map(|m| m.id).collect();
    let all_attachments = db::get_attachments_for_messages(&pool, &msg_ids).await?;

    // Group attachments by message_id
    let responses = messages
        .into_iter()
        .map(|msg| {
            let attachments: Vec<Attachment> = all_attachments
                .iter()
                .filter(|a| a.message_id == msg.id)
                .cloned()
                .collect();
            MessageResponse::from_msg(msg, attachments)
        })
        .collect();

    Ok(Json(responses))
}

/// POST /api/channels/:channel_id/messages
pub async fn send_message(
    State(pool): State<PgPool>,
    State(config): State<Config>,
    State(gateway): State<Gateway>,
    State(pending): State<PendingUploads>,
    auth: AuthUser,
    Path(channel_id): Path<Uuid>,
    Json(req): Json<SendMessageRequest>,
) -> Result<Json<MessageResponse>, AppError> {
    let channel = db::get_channel_by_id(&pool, channel_id)
        .await?
        .ok_or(AppError::NotFound("Channel not found".into()))?;

    require_permission(&pool, channel.server_id, auth.user_id, perms::SEND_MESSAGES).await?;

    let content = req.content.as_deref().unwrap_or("").trim();
    let has_attachments = req
        .attachment_ids
        .as_ref()
        .is_some_and(|ids| !ids.is_empty());

    if content.is_empty() && !has_attachments {
        return Err(AppError::BadRequest(
            "Message must have content or attachments".into(),
        ));
    }
    if content.len() > 4000 {
        return Err(AppError::BadRequest(
            "Message too long (max 4000 chars)".into(),
        ));
    }

    // Check ATTACH_FILES permission if attaching files
    if has_attachments {
        require_permission(&pool, channel.server_id, auth.user_id, perms::ATTACH_FILES).await?;
    }

    // Use empty string for content-less messages (attachment-only)
    let msg_content = if content.is_empty() { "" } else { content };
    let msg = db::create_message(&pool, channel_id, auth.user_id, msg_content).await?;

    // Link pending uploads to this message
    let mut attachments = Vec::new();
    if let Some(upload_ids) = req.attachment_ids {
        for upload_id in upload_ids {
            let Some(pending_upload) = pending.get(&upload_id).map(|entry| entry.clone()) else {
                continue;
            };

            // Verify the uploader owns this upload
            if pending_upload.uploader_id != auth.user_id {
                continue;
            }

            let attachment = db::create_attachment(
                &pool,
                msg.id,
                &pending_upload.filename,
                &pending_upload.url,
                pending_upload.content_type.as_deref(),
                Some(pending_upload.size_bytes),
            )
            .await?;

            if let Some((_, stored_upload)) = pending.remove(&upload_id) {
                if stored_upload.uploader_id != auth.user_id {
                    delete_pending_upload_file(&config, &stored_upload).await;
                    continue;
                }
            }

            attachments.push(attachment);
        }
    }

    // Broadcast to channel subscribers
    gateway.broadcast_to_channel(
        channel_id,
        GatewayEvent::MessageCreate {
            id: msg.id,
            channel_id: msg.channel_id,
            author_id: msg.author_id,
            author_username: msg.author_username.clone(),
            author_display_name: msg.author_display_name.clone(),
            author_avatar_url: msg.author_avatar_url.clone(),
            content: msg.content.clone(),
            attachments: attachments.clone(),
            created_at: msg.created_at.to_rfc3339(),
        },
    );

    Ok(Json(MessageResponse::from_msg(msg, attachments)))
}

/// PATCH /api/channels/:channel_id/messages/:message_id
pub async fn edit_message(
    State(pool): State<PgPool>,
    State(gateway): State<Gateway>,
    auth: AuthUser,
    Path((channel_id, message_id)): Path<(Uuid, Uuid)>,
    Json(req): Json<EditMessageRequest>,
) -> Result<Json<MessageResponse>, AppError> {
    let existing = db::get_message_by_id(&pool, message_id)
        .await?
        .ok_or(AppError::NotFound("Message not found".into()))?;

    if existing.author_id != auth.user_id {
        return Err(AppError::Forbidden);
    }

    let content = req.content.trim();
    if content.is_empty() {
        return Err(AppError::BadRequest("Message cannot be empty".into()));
    }

    let msg = db::update_message(&pool, message_id, content).await?;
    let attachments = db::get_attachments_for_message(&pool, message_id).await?;

    gateway.broadcast_to_channel(
        channel_id,
        GatewayEvent::MessageUpdate {
            id: msg.id,
            channel_id,
            content: msg.content.clone(),
            edited_at: msg.edited_at.map(|t| t.to_rfc3339()).unwrap_or_default(),
        },
    );

    Ok(Json(MessageResponse::from_msg(msg, attachments)))
}

/// DELETE /api/channels/:channel_id/messages/:message_id
pub async fn delete_message(
    State(pool): State<PgPool>,
    State(gateway): State<Gateway>,
    auth: AuthUser,
    Path((channel_id, message_id)): Path<(Uuid, Uuid)>,
) -> Result<Json<serde_json::Value>, AppError> {
    let existing = db::get_message_by_id(&pool, message_id)
        .await?
        .ok_or(AppError::NotFound("Message not found".into()))?;

    // Author can delete own messages, or user with MANAGE_MESSAGES
    if existing.author_id != auth.user_id {
        let channel = db::get_channel_by_id(&pool, channel_id)
            .await?
            .ok_or(AppError::NotFound("Channel not found".into()))?;
        require_permission(&pool, channel.server_id, auth.user_id, perms::MANAGE_MESSAGES).await?;
    }

    // Attachments cascade-deleted by DB foreign key
    db::delete_message(&pool, message_id).await?;

    gateway.broadcast_to_channel(
        channel_id,
        GatewayEvent::MessageDelete {
            id: message_id,
            channel_id,
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
