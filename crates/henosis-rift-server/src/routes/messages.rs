use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use sqlx::PgPool;
use uuid::Uuid;

use crate::auth::middleware::AuthUser;
use crate::config::Config;
use crate::db;
use crate::error::AppError;
use crate::models::attachment::Attachment;
use crate::models::message::{
    EditMessageRequest, MessageQuery, MessageResponse, SendMessageRequest,
};
use crate::models::permissions::perms;
use crate::routes::upload::{PendingUploads, delete_pending_upload_file};
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
    let cursor = select_list_message_cursor(&query)?;
    let messages = db::get_messages(&pool, channel_id, &query).await?;
    require_existing_message_cursor(cursor, channel_id, |cursor| {
        db::message_cursor_exists_in_channel(&pool, channel_id, cursor)
    })
    .await?;

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

    // The author row is server truth for is_agent; the JWT only carries the
    // user id, so typing a message requires this lookup.
    let author = db::get_user_by_id(&pool, auth.user_id)
        .await?
        .ok_or(AppError::Unauthorized)?;
    let message_type = resolve_message_type(req.message_type.as_deref(), author.is_agent)?;

    // Use empty string for content-less messages (attachment-only)
    let msg_content = if content.is_empty() { "" } else { content };
    let msg =
        db::create_message(&pool, channel_id, auth.user_id, msg_content, message_type).await?;

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

            if let Some((_, stored_upload)) = pending.remove(&upload_id)
                && stored_upload.uploader_id != auth.user_id
            {
                delete_pending_upload_file(&config, &stored_upload).await;
                continue;
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
            message_type: msg.message_type.clone(),
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
    let channel = db::get_channel_by_id(&pool, channel_id)
        .await?
        .ok_or(AppError::NotFound("Channel not found".into()))?;
    require_member(&pool, channel.server_id, auth.user_id).await?;

    let existing = db::get_message_by_id(&pool, message_id)
        .await?
        .ok_or(AppError::NotFound("Message not found".into()))?;
    require_message_channel(existing.channel_id, channel_id)?;

    if existing.author_id != auth.user_id {
        return Err(AppError::Forbidden);
    }

    let content = req.content.trim();
    if content.is_empty() {
        return Err(AppError::BadRequest("Message cannot be empty".into()));
    }
    if content.len() > 4000 {
        return Err(AppError::BadRequest(
            "Message too long (max 4000 chars)".into(),
        ));
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
    let channel = db::get_channel_by_id(&pool, channel_id)
        .await?
        .ok_or(AppError::NotFound("Channel not found".into()))?;
    require_member(&pool, channel.server_id, auth.user_id).await?;

    let existing = db::get_message_by_id(&pool, message_id)
        .await?
        .ok_or(AppError::NotFound("Message not found".into()))?;
    require_message_channel(existing.channel_id, channel_id)?;

    // Author can delete own messages, or user with MANAGE_MESSAGES
    if existing.author_id != auth.user_id {
        require_permission(
            &pool,
            channel.server_id,
            auth.user_id,
            perms::MANAGE_MESSAGES,
        )
        .await?;
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

/// Reject a message identifier that does not belong to the channel in the route path.
fn require_message_channel(
    message_channel_id: Uuid,
    path_channel_id: Uuid,
) -> Result<(), AppError> {
    if message_channel_id != path_channel_id {
        return Err(AppError::NotFound("Message not found".into()));
    }
    Ok(())
}

/// Resolve and authorize the stored message_type for a new message.
///
/// Absent means "infer from the author": agents post 'agent', humans post
/// 'user'. Explicit values are whitelisted and checked against the author's
/// is_agent flag, so a human cannot forge bridge machinery ('stimulus',
/// 'system') and an agent cannot pass itself off as a human ('user').
/// Whitelisting also guarantees the value fits the VARCHAR(16) column.
fn resolve_message_type(requested: Option<&str>, author_is_agent: bool) -> Result<&str, AppError> {
    let Some(requested) = requested else {
        return Ok(if author_is_agent { "agent" } else { "user" });
    };
    let allowed = match requested {
        "user" => !author_is_agent,
        "agent" | "stimulus" | "system" => author_is_agent,
        other => {
            return Err(AppError::BadRequest(format!(
                "Unknown message_type '{other}'"
            )));
        }
    };
    if allowed {
        Ok(requested)
    } else {
        Err(AppError::Forbidden)
    }
}

/// Direction-bearing message boundary selected from one unambiguous page query.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ListMessageCursor {
    /// Existing message before which older history is requested.
    Before(Uuid),
    /// Existing message, or the requested channel itself, after which history is requested.
    After(Uuid),
}

/// Return the opaque identifier carried by one selected page boundary.
impl ListMessageCursor {
    /// Extract the identifier without discarding its pagination direction.
    fn id(self) -> Uuid {
        match self {
            Self::Before(cursor) | Self::After(cursor) => cursor,
        }
    }
}

/// Select one pagination direction and reject ambiguous list queries.
fn select_list_message_cursor(query: &MessageQuery) -> Result<Option<ListMessageCursor>, AppError> {
    match (query.before, query.after) {
        (Some(_), Some(_)) => Err(AppError::BadRequest(
            "before and after cursors cannot be combined".to_string(),
        )),
        (Some(cursor), None) => Ok(Some(ListMessageCursor::Before(cursor))),
        (None, Some(cursor)) => Ok(Some(ListMessageCursor::After(cursor))),
        (None, None) => Ok(None),
    }
}

/// Confirm after the page read that its boundary exists or is the reserved beginning cursor.
async fn require_existing_message_cursor<F, Fut>(
    cursor: Option<ListMessageCursor>,
    channel_id: Uuid,
    cursor_exists: F,
) -> Result<(), AppError>
where
    F: FnOnce(Uuid) -> Fut,
    Fut: std::future::Future<Output = Result<bool, sqlx::Error>>,
{
    let Some(cursor) = cursor else {
        return Ok(());
    };
    if cursor == ListMessageCursor::After(channel_id) {
        return Ok(());
    }
    if !cursor_exists(cursor.id()).await? {
        return Err(invalid_message_cursor());
    }
    Ok(())
}

/// Construct the stable non-disclosing response for an invalid room cursor.
fn invalid_message_cursor() -> AppError {
    AppError::Coded {
        status: StatusCode::NOT_FOUND,
        code: "invalid_message_cursor",
        message: "Message cursor does not exist in this channel".to_string(),
    }
}

/// Reject callers that are not members of the server owning the channel.
async fn require_member(pool: &PgPool, server_id: Uuid, user_id: Uuid) -> Result<(), AppError> {
    if !db::is_member(pool, server_id, user_id).await? {
        return Err(AppError::Forbidden);
    }
    Ok(())
}

/// Reject members that lack the given permission bit in the server.
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

/// Covers message parent binding and message-type authorization rules.
#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use axum::extract::{Path, Query, State};
    use axum::response::IntoResponse;
    use sqlx::postgres::PgPoolOptions;

    use super::{
        list_messages, require_existing_message_cursor, require_message_channel,
        resolve_message_type, select_list_message_cursor,
    };
    use crate::auth::middleware::AuthUser;
    use crate::db;
    use crate::error::AppError;
    use crate::models::message::MessageQuery;
    use uuid::Uuid;

    /// Construct one list query around the requested before and after cursors.
    fn message_query(before: Option<Uuid>, after: Option<Uuid>) -> MessageQuery {
        MessageQuery {
            before,
            after,
            limit: Some(50),
        }
    }

    /// Connect to the opt-in PostgreSQL test database without exposing its URL.
    async fn live_test_pool() -> Option<sqlx::PgPool> {
        let Some(database_url) = std::env::var_os("HENOSIS_RIFT_TEST_DATABASE_URL") else {
            eprintln!("skipping live message cursor test: HENOSIS_RIFT_TEST_DATABASE_URL is unset");
            return None;
        };
        let pool = PgPoolOptions::new()
            .max_connections(4)
            .connect(&database_url.to_string_lossy())
            .await
            .expect("test database must be reachable");
        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .expect("test database migrations must apply");
        Some(pool)
    }

    /// Assert the concrete HTTP envelope for one invalid cursor error.
    async fn assert_invalid_message_cursor_error(error: AppError) {
        let response = error.into_response();
        assert_eq!(response.status(), axum::http::StatusCode::NOT_FOUND);
        let body = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .expect("coded error body must be readable");
        let body: serde_json::Value =
            serde_json::from_slice(&body).expect("coded error body must be JSON");
        assert_eq!(body["code"], "invalid_message_cursor");
    }

    /// Assert that one unavailable cursor maps to Rift's stable coded 404.
    async fn assert_invalid_message_cursor(query: MessageQuery) {
        let cursor = select_list_message_cursor(&query).expect("one cursor must be selectable");
        let channel_id = Uuid::nil();
        assert_ne!(cursor.expect("cursor must exist").id(), channel_id);
        let error = require_existing_message_cursor(cursor, channel_id, |_| async { Ok(false) })
            .await
            .expect_err("unavailable cursor must fail");
        assert_invalid_message_cursor_error(error).await;
    }

    /// An unknown after cursor returns the stable non-disclosing 404 contract.
    #[tokio::test]
    async fn list_messages_rejects_unknown_after_cursor() {
        assert_invalid_message_cursor(message_query(None, Some(Uuid::new_v4()))).await;
    }

    /// An unknown before cursor returns the stable non-disclosing 404 contract.
    #[tokio::test]
    async fn list_messages_rejects_unknown_before_cursor() {
        assert_invalid_message_cursor(message_query(Some(Uuid::new_v4()), None)).await;
    }

    /// A cursor rejected by the channel-scoped store uses the same opaque error.
    #[tokio::test]
    async fn list_messages_rejects_cursor_from_another_channel() {
        assert_invalid_message_cursor(message_query(Some(Uuid::new_v4()), None)).await;
    }

    /// A channel-owned cursor passes validation even when its page will be empty.
    #[tokio::test]
    async fn list_messages_accepts_valid_boundary_cursor() {
        let query = message_query(None, Some(Uuid::new_v4()));
        let cursor = select_list_message_cursor(&query).expect("one cursor must be selectable");
        require_existing_message_cursor(cursor, Uuid::new_v4(), |_| async { Ok(true) })
            .await
            .expect("channel-owned cursor must pass");
    }

    /// A channel's own identifier is accepted only as its beginning after cursor.
    #[tokio::test]
    async fn list_messages_accepts_room_scoped_beginning_after_cursor() {
        let channel_id = Uuid::new_v4();
        let after = select_list_message_cursor(&message_query(None, Some(channel_id)))
            .expect("beginning cursor must be selectable");
        let existence_checked = Cell::new(false);
        require_existing_message_cursor(after, channel_id, |_| {
            existence_checked.set(true);
            async { Ok(false) }
        })
        .await
        .expect("room-scoped beginning after cursor must pass");
        assert!(!existence_checked.get());

        let before = select_list_message_cursor(&message_query(Some(channel_id), None))
            .expect("before cursor must be selectable");
        let error = require_existing_message_cursor(before, channel_id, |_| async { Ok(false) })
            .await
            .expect_err("channel identifier is not a valid before cursor");
        assert_invalid_message_cursor_error(error).await;
    }

    /// Combining pagination directions remains a client error.
    #[tokio::test]
    async fn list_messages_rejects_combined_cursors() {
        let error =
            select_list_message_cursor(&message_query(Some(Uuid::new_v4()), Some(Uuid::new_v4())))
                .expect_err("combined cursors must fail");
        assert!(matches!(error, AppError::BadRequest(_)));
    }

    /// Live PostgreSQL proves cursor scope, stable ordering, and empty boundaries.
    #[tokio::test]
    async fn list_messages_enforce_live_channel_cursor_contracts() {
        let Some(pool) = live_test_pool().await else {
            return;
        };
        let suffix = Uuid::new_v4().simple().to_string();
        let suffix = &suffix[..12];
        let username = format!("cursor_{suffix}");
        let user = db::create_user(
            &pool,
            &username,
            &format!("cursor-{suffix}@example.invalid"),
            "test-hash",
            None,
        )
        .await
        .expect("test user must be created");
        let server = db::create_server(&pool, &format!("cursor-{suffix}"), None, user.id)
            .await
            .expect("test server must be created");
        db::add_member(&pool, server.id, user.id)
            .await
            .expect("test member must be created");
        let channel = db::create_channel(&pool, server.id, "target", None, "text")
            .await
            .expect("target channel must be created");
        let other_channel = db::create_channel(&pool, server.id, "other", None, "text")
            .await
            .expect("other channel must be created");
        let first = db::create_message(&pool, channel.id, user.id, "first", "user")
            .await
            .expect("first message must be created");
        let second = db::create_message(&pool, channel.id, user.id, "second", "user")
            .await
            .expect("second message must be created");
        let other = db::create_message(&pool, other_channel.id, user.id, "other", "user")
            .await
            .expect("other-channel message must be created");
        let equal_time_ids = vec![first.id, second.id];
        sqlx::query(
            "UPDATE messages SET created_at = TIMESTAMPTZ '2000-01-01 00:00:00+00' \
             WHERE id = ANY($1)",
        )
        .bind(&equal_time_ids)
        .execute(&pool)
        .await
        .expect("test messages must share one timestamp");

        let auth = AuthUser {
            user_id: user.id,
            username,
        };
        for query in [
            message_query(Some(Uuid::new_v4()), None),
            message_query(None, Some(Uuid::new_v4())),
            message_query(Some(other.id), None),
        ] {
            let error = match list_messages(
                State(pool.clone()),
                auth.clone(),
                Path(channel.id),
                Query(query),
            )
            .await
            {
                Ok(_) => panic!("invalid cursor must not produce a page"),
                Err(error) => error,
            };
            assert_invalid_message_cursor_error(error).await;
        }

        let (lower_id, higher_id) = if first.id < second.id {
            (first.id, second.id)
        } else {
            (second.id, first.id)
        };
        let from_start = list_messages(
            State(pool.clone()),
            auth.clone(),
            Path(channel.id),
            Query(message_query(None, Some(channel.id))),
        )
        .await
        .expect("room-scoped beginning cursor must return the oldest page");
        assert_eq!(
            from_start
                .0
                .iter()
                .map(|message| message.id)
                .collect::<Vec<_>>(),
            vec![lower_id, higher_id]
        );
        let before = db::get_messages(&pool, channel.id, &message_query(Some(higher_id), None))
            .await
            .expect("before page must load");
        assert_eq!(
            before.iter().map(|message| message.id).collect::<Vec<_>>(),
            vec![lower_id]
        );
        let after = db::get_messages(&pool, channel.id, &message_query(None, Some(lower_id)))
            .await
            .expect("after page must load");
        assert_eq!(
            after.iter().map(|message| message.id).collect::<Vec<_>>(),
            vec![higher_id]
        );

        let newest = db::create_message(&pool, channel.id, user.id, "newest", "user")
            .await
            .expect("newest message must be created");
        let after_equal_time =
            db::get_messages(&pool, channel.id, &message_query(None, Some(higher_id)))
                .await
                .expect("forward page must load");
        assert_eq!(
            after_equal_time
                .iter()
                .map(|message| message.id)
                .collect::<Vec<_>>(),
            vec![newest.id]
        );

        let (concurrent_one, concurrent_two) = tokio::join!(
            db::create_message(&pool, channel.id, user.id, "concurrent-one", "user"),
            db::create_message(&pool, channel.id, user.id, "concurrent-two", "user"),
        );
        let concurrent_one = concurrent_one.expect("first concurrent message must be created");
        let concurrent_two = concurrent_two.expect("second concurrent message must be created");
        let concurrent_page =
            db::get_messages(&pool, channel.id, &message_query(None, Some(newest.id)))
                .await
                .expect("concurrent forward page must load");
        let expected_concurrent_ids = if concurrent_one
            .created_at
            .cmp(&concurrent_two.created_at)
            .then(concurrent_one.id.cmp(&concurrent_two.id))
            .is_lt()
        {
            vec![concurrent_one.id, concurrent_two.id]
        } else {
            vec![concurrent_two.id, concurrent_one.id]
        };
        assert_eq!(
            concurrent_page
                .iter()
                .map(|message| message.id)
                .collect::<Vec<_>>(),
            expected_concurrent_ids
        );

        let newest_concurrent_id = concurrent_page
            .last()
            .expect("concurrent page must contain its newest boundary")
            .id;
        let boundary = list_messages(
            State(pool),
            auth,
            Path(channel.id),
            Query(message_query(None, Some(newest_concurrent_id))),
        )
        .await
        .expect("valid newest cursor must return an empty page");
        assert!(boundary.0.is_empty());
    }

    /// A message is accepted only under its authoritative channel identifier.
    #[test]
    fn message_parent_must_match_route_channel() {
        let channel_id = Uuid::new_v4();
        assert!(require_message_channel(channel_id, channel_id).is_ok());
        assert!(require_message_channel(channel_id, Uuid::new_v4()).is_err());
    }

    /// An absent type infers from the author: agents post 'agent', humans 'user'.
    #[test]
    fn test_absent_type_infers_from_author() {
        assert_eq!(resolve_message_type(None, true).unwrap(), "agent");
        assert_eq!(resolve_message_type(None, false).unwrap(), "user");
    }

    /// Agents may stamp the structural types the bridge machinery uses.
    #[test]
    fn test_agent_may_set_structural_types() {
        for t in ["agent", "stimulus", "system"] {
            assert_eq!(resolve_message_type(Some(t), true).unwrap(), t);
        }
    }

    /// A human explicitly asking for 'user' is redundant but valid.
    #[test]
    fn test_human_may_set_user() {
        assert_eq!(resolve_message_type(Some("user"), false).unwrap(), "user");
    }

    /// Humans cannot forge agent, stimulus, or system messages.
    #[test]
    fn test_human_cannot_forge_structural_types() {
        for t in ["agent", "stimulus", "system"] {
            assert!(matches!(
                resolve_message_type(Some(t), false),
                Err(AppError::Forbidden)
            ));
        }
    }

    /// An agent cannot pass itself off as a human author.
    #[test]
    fn test_agent_cannot_post_as_user() {
        assert!(matches!(
            resolve_message_type(Some("user"), true),
            Err(AppError::Forbidden)
        ));
    }

    /// Unknown discriminators are a client error, not a silent default.
    #[test]
    fn test_unknown_type_is_bad_request() {
        for requested in ["shout", "", "USER", "Agent"] {
            assert!(matches!(
                resolve_message_type(Some(requested), true),
                Err(AppError::BadRequest(_))
            ));
        }
    }
}
