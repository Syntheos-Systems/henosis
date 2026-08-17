use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use sqlx::PgPool;
use std::collections::HashSet;
use uuid::Uuid;

use crate::auth::middleware::AuthUser;
use crate::db;
use crate::error::AppError;
use crate::models::attachment::Attachment;
use crate::models::message::{
    EditMessageRequest, MessageQuery, MessageResponse, SendMessageRequest,
    validated_message_page_limit,
};
use crate::models::permissions::perms;
use crate::routes::upload::{PendingUpload, PendingUploads};

/// Maximum number of staged attachments accepted by one message.
const MAX_MESSAGE_ATTACHMENTS: usize = 10;

/// Atomic, cancellation-safe ownership of staged uploads during message creation.
#[derive(Debug)]
struct PendingUploadClaims {
    /// Shared registry to restore into until the database transaction commits.
    pending: PendingUploads,
    /// Upload identifiers and metadata removed atomically from the registry.
    uploads: Vec<(Uuid, PendingUpload)>,
    /// True only after the database transaction has durably linked every upload.
    committed: bool,
}

/// Claim lifecycle that restores staged uploads whenever the request does not commit.
impl PendingUploadClaims {
    /// Iterate over claimed upload metadata while building database attachment rows.
    fn uploads(&self) -> impl Iterator<Item = &PendingUpload> {
        self.uploads.iter().map(|(_, upload)| upload)
    }

    /// Mark every claim consumed after the message and attachments commit together.
    fn commit(mut self) {
        self.committed = true;
    }
}

/// Restore atomically claimed uploads on error, panic, or request cancellation.
impl Drop for PendingUploadClaims {
    /// Return every uncommitted claim to the shared pending-upload registry.
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        for (upload_id, upload) in self.uploads.drain(..) {
            self.pending.insert(upload_id, upload);
        }
    }
}

/// Atomically claim a bounded, unique set of uploads owned by one authenticated user.
fn claim_pending_uploads(
    pending: &PendingUploads,
    uploader_id: Uuid,
    upload_ids: &[Uuid],
) -> Result<PendingUploadClaims, AppError> {
    if upload_ids.len() > MAX_MESSAGE_ATTACHMENTS {
        return Err(AppError::BadRequest(format!(
            "A message can include no more than {MAX_MESSAGE_ATTACHMENTS} attachments"
        )));
    }

    let mut unique_ids = HashSet::with_capacity(upload_ids.len());
    if upload_ids
        .iter()
        .any(|upload_id| !unique_ids.insert(*upload_id))
    {
        return Err(AppError::BadRequest(
            "Attachment identifiers must be unique".to_string(),
        ));
    }

    let mut claims = PendingUploadClaims {
        pending: pending.clone(),
        uploads: Vec::with_capacity(upload_ids.len()),
        committed: false,
    };
    for upload_id in upload_ids {
        let Some(claimed) =
            pending.remove_if(upload_id, |_, upload| upload.uploader_id == uploader_id)
        else {
            return Err(AppError::BadRequest(
                "One or more attachments are unavailable".to_string(),
            ));
        };
        claims.uploads.push(claimed);
    }
    Ok(claims)
}

/// GET /api/channels/:channel_id/messages
pub async fn list_messages(
    State(pool): State<PgPool>,
    auth: AuthUser,
    Path(channel_id): Path<Uuid>,
    Query(mut query): Query<MessageQuery>,
) -> Result<Json<Vec<MessageResponse>>, AppError> {
    query.limit = Some(
        validated_message_page_limit(query.limit)
            .map_err(|message| AppError::BadRequest(message.to_string()))?,
    );
    select_list_message_cursor(&query)?;
    let (messages, all_attachments) = db::list_channel_messages_authorized(
        &pool,
        channel_id,
        auth.user_id,
        auth.managed_fence.as_ref(),
        &query,
    )
    .await
    .map_err(map_list_messages_error)?;

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
    State(pending): State<PendingUploads>,
    auth: AuthUser,
    Path(channel_id): Path<Uuid>,
    Json(req): Json<SendMessageRequest>,
) -> Result<Json<MessageResponse>, AppError> {
    let channel = db::get_channel_by_id(&pool, channel_id)
        .await?
        .ok_or(AppError::NotFound("Channel not found".into()))?;

    auth.require_server_target_fence(&pool, channel.server_id)
        .await?;
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

    let upload_ids = req.attachment_ids.as_deref().unwrap_or_default();
    let claims = claim_pending_uploads(&pending, auth.user_id, upload_ids)?;
    let new_attachments = claims
        .uploads()
        .map(|upload| db::NewAttachment {
            filename: upload.filename.clone(),
            url: upload.url.clone(),
            content_type: upload.content_type.clone(),
            size_bytes: upload.size_bytes,
        })
        .collect::<Vec<_>>();

    // Use empty string for content-less messages (attachment-only).
    let msg_content = if content.is_empty() { "" } else { content };
    let write_authorization = if author.is_agent {
        db::MessageWriteAuthorization::Agent {
            server_id: channel.server_id,
            fence: auth.managed_fence.as_ref(),
            required_permissions: perms::SEND_MESSAGES
                | if has_attachments {
                    perms::ATTACH_FILES
                } else {
                    0
                },
        }
    } else {
        db::MessageWriteAuthorization::Human
    };
    let (msg, attachments) = db::create_message_with_attachments(
        &pool,
        channel_id,
        auth.user_id,
        msg_content,
        message_type,
        write_authorization,
        &new_attachments,
    )
    .await
    .map_err(map_message_create_error)?;
    claims.commit();

    Ok(Json(MessageResponse::from_msg(msg, attachments)))
}

/// PATCH /api/channels/:channel_id/messages/:message_id
pub async fn edit_message(
    State(pool): State<PgPool>,
    auth: AuthUser,
    Path((channel_id, message_id)): Path<(Uuid, Uuid)>,
    Json(req): Json<EditMessageRequest>,
) -> Result<Json<MessageResponse>, AppError> {
    let channel = db::get_channel_by_id(&pool, channel_id)
        .await?
        .ok_or(AppError::NotFound("Channel not found".into()))?;
    auth.require_server_target_fence(&pool, channel.server_id)
        .await?;
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

    let msg = db::update_message_with_fence(
        &pool,
        message_id,
        channel_id,
        auth.user_id,
        auth.managed_fence.as_ref(),
        content,
    )
    .await
    .map_err(map_message_mutation_error)?;
    let attachments = db::get_attachments_for_message(&pool, message_id).await?;

    Ok(Json(MessageResponse::from_msg(msg, attachments)))
}

/// DELETE /api/channels/:channel_id/messages/:message_id
pub async fn delete_message(
    State(pool): State<PgPool>,
    auth: AuthUser,
    Path((channel_id, message_id)): Path<(Uuid, Uuid)>,
) -> Result<Json<serde_json::Value>, AppError> {
    let channel = db::get_channel_by_id(&pool, channel_id)
        .await?
        .ok_or(AppError::NotFound("Channel not found".into()))?;
    auth.require_server_target_fence(&pool, channel.server_id)
        .await?;
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
    db::delete_message_with_fence(
        &pool,
        message_id,
        channel_id,
        auth.user_id,
        auth.managed_fence.as_ref(),
    )
    .await
    .map_err(map_message_mutation_error)?;

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

/// Map a fenced message transaction failure to a stable, non-disclosing API response.
fn map_message_create_error(error: db::CreateMessageError) -> AppError {
    match error {
        db::CreateMessageError::Forbidden => AppError::Forbidden,
        db::CreateMessageError::StaleLeadership => AppError::stale_leadership_fence(),
        db::CreateMessageError::Database(error) => error.into(),
    }
}

/// Map one transaction-scoped message-list failure to its stable API contract.
fn map_list_messages_error(error: db::ListMessagesError) -> AppError {
    match error {
        db::ListMessagesError::ChannelNotFound => {
            AppError::NotFound("Channel not found".to_string())
        }
        db::ListMessagesError::Forbidden => AppError::Forbidden,
        db::ListMessagesError::StaleLeadership => AppError::stale_leadership_fence(),
        db::ListMessagesError::InvalidCursor => invalid_message_cursor(),
        db::ListMessagesError::Database(error) => error.into(),
    }
}

/// Map a fenced edit or delete failure to a stable, non-disclosing API response.
fn map_message_mutation_error(error: db::MessageMutationError) -> AppError {
    match error {
        db::MessageMutationError::NotFound => AppError::NotFound("Message not found".to_string()),
        db::MessageMutationError::StaleLeadership => AppError::stale_leadership_fence(),
        db::MessageMutationError::Database(error) => error.into(),
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
#[cfg(test)]
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
#[cfg(test)]
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
        MAX_MESSAGE_ATTACHMENTS, claim_pending_uploads, list_messages, map_message_mutation_error,
        require_existing_message_cursor, require_message_channel, resolve_message_type,
        select_list_message_cursor,
    };
    use crate::auth::middleware::AuthUser;
    use crate::db;
    use crate::error::AppError;
    use crate::models::message::MessageQuery;
    use crate::models::permissions::perms;
    use crate::routes::upload::{PendingUpload, PendingUploads};
    use chrono::Utc;
    use uuid::Uuid;

    /// A stale transactional edit or delete uses the same stable revocation envelope as creation.
    #[tokio::test]
    async fn message_mutation_stale_fence_has_stable_conflict_code() {
        use axum::response::IntoResponse;

        let response =
            map_message_mutation_error(db::MessageMutationError::StaleLeadership).into_response();
        assert_eq!(response.status(), axum::http::StatusCode::CONFLICT);
        let body = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .expect("coded mutation error body must be readable");
        let body: serde_json::Value =
            serde_json::from_slice(&body).expect("coded mutation error body must be JSON");
        assert_eq!(body["code"], "stale_leadership_fence");
    }

    /// Build one pending upload owned by the requested user.
    fn pending_upload(uploader_id: Uuid) -> PendingUpload {
        PendingUpload {
            uploader_id,
            filename: "evidence.txt".to_string(),
            stored_filename: Uuid::new_v4().to_string(),
            url: "/uploads/evidence".to_string(),
            content_type: Some("text/plain".to_string()),
            size_bytes: 8,
            created_at: Utc::now(),
        }
    }

    /// Build an empty pending-upload registry for claim tests.
    fn pending_uploads() -> PendingUploads {
        std::sync::Arc::new(dashmap::DashMap::new())
    }

    /// Only one concurrent message may consume a staged upload identifier.
    #[test]
    fn pending_upload_claim_is_single_use_under_concurrency() {
        let pending = pending_uploads();
        let uploader_id = Uuid::new_v4();
        let upload_id = Uuid::new_v4();
        pending.insert(upload_id, pending_upload(uploader_id));
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));

        let claims = (0..2)
            .map(|_| {
                let pending = pending.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    claim_pending_uploads(&pending, uploader_id, &[upload_id])
                        .map(|claim| claim.commit())
                        .is_ok()
                })
            })
            .collect::<Vec<_>>();
        barrier.wait();

        let successes = claims
            .into_iter()
            .map(|claim| claim.join().expect("claim worker must not panic"))
            .filter(|succeeded| *succeeded)
            .count();
        assert_eq!(successes, 1);
        assert!(!pending.contains_key(&upload_id));
    }

    /// A failed partial claim restores earlier uploads and leaves foreign uploads untouched.
    #[test]
    fn pending_upload_claim_is_all_or_nothing() {
        let pending = pending_uploads();
        let uploader_id = Uuid::new_v4();
        let owned_id = Uuid::new_v4();
        let foreign_id = Uuid::new_v4();
        pending.insert(owned_id, pending_upload(uploader_id));
        pending.insert(foreign_id, pending_upload(Uuid::new_v4()));

        let error = claim_pending_uploads(&pending, uploader_id, &[owned_id, foreign_id])
            .expect_err("foreign upload must reject the entire claim");
        assert!(matches!(error, AppError::BadRequest(_)));
        assert!(pending.contains_key(&owned_id));
        assert!(pending.contains_key(&foreign_id));
    }

    /// Dropping an uncommitted claim restores it so cancellation cannot orphan the file.
    #[test]
    fn uncommitted_pending_upload_claim_restores_on_drop() {
        let pending = pending_uploads();
        let uploader_id = Uuid::new_v4();
        let upload_id = Uuid::new_v4();
        pending.insert(upload_id, pending_upload(uploader_id));

        let claim = claim_pending_uploads(&pending, uploader_id, &[upload_id])
            .expect("owned upload must be claimable");
        assert!(!pending.contains_key(&upload_id));
        drop(claim);
        assert!(pending.contains_key(&upload_id));
    }

    /// Duplicate or excessive identifiers are rejected before any upload is removed.
    #[test]
    fn pending_upload_claim_enforces_request_bounds() {
        let pending = pending_uploads();
        let uploader_id = Uuid::new_v4();
        let upload_id = Uuid::new_v4();
        pending.insert(upload_id, pending_upload(uploader_id));

        assert!(claim_pending_uploads(&pending, uploader_id, &[upload_id, upload_id]).is_err());
        assert!(pending.contains_key(&upload_id));

        let excessive = (0..=MAX_MESSAGE_ATTACHMENTS)
            .map(|_| Uuid::new_v4())
            .collect::<Vec<_>>();
        assert!(claim_pending_uploads(&pending, uploader_id, &excessive).is_err());
        assert!(pending.contains_key(&upload_id));
    }

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

    /// Live PostgreSQL proves one fenced list boundary covers membership, cursor, page, and files.
    #[tokio::test]
    async fn live_managed_message_list_is_authorized_as_one_database_read() {
        let Some(pool) = live_test_pool().await else {
            return;
        };
        let suffix = Uuid::new_v4().simple().to_string();
        let suffix = &suffix[..12];
        let owner = db::create_user(
            &pool,
            &format!("list_owner_{suffix}"),
            &format!("list-owner-{suffix}@example.invalid"),
            "test-hash",
            None,
        )
        .await
        .expect("list owner must be created");
        let agent = db::create_owned_agent_user(
            &pool,
            &format!("list_agent_{suffix}"),
            &format!("list-agent-{suffix}@agent.local"),
            "test-hash",
            None,
            owner.id,
        )
        .await
        .expect("list agent must be created");
        let server = db::create_server(&pool, &format!("list-{suffix}"), None, owner.id)
            .await
            .expect("list server must be created");
        sqlx::query(
            r#"INSERT INTO bridge_server_state (server_id, fencing_required)
               VALUES ($1, TRUE)"#,
        )
        .bind(server.id)
        .execute(&pool)
        .await
        .expect("managed list state must be created");
        db::add_member(&pool, server.id, agent.id)
            .await
            .expect("list agent must join the room");
        let channel = db::create_channel(&pool, server.id, "list", None, "text")
            .await
            .expect("list channel must be created");
        let message = db::create_message(&pool, channel.id, agent.id, "listed", "agent")
            .await
            .expect("listed message must be created");
        db::create_attachment(
            &pool,
            message.id,
            "evidence.txt",
            "/uploads/evidence",
            Some("text/plain"),
            Some(8),
        )
        .await
        .expect("listed attachment must be created");
        let stale = db::agent_control::acquire_room_fence(&pool, server.id)
            .await
            .expect("first list fence must be acquired");

        let (messages, attachments) = db::list_channel_messages_authorized(
            &pool,
            channel.id,
            agent.id,
            Some(&stale),
            &message_query(None, None),
        )
        .await
        .expect("current managed list must load");
        assert_eq!(
            messages.iter().map(|item| item.id).collect::<Vec<_>>(),
            [message.id]
        );
        assert_eq!(attachments.len(), 1);
        assert_eq!(attachments[0].message_id, message.id);

        let current = db::agent_control::acquire_room_fence(&pool, server.id)
            .await
            .expect("successor list fence must be acquired");
        assert!(matches!(
            db::list_channel_messages_authorized(
                &pool,
                channel.id,
                agent.id,
                Some(&stale),
                &message_query(None, None),
            )
            .await,
            Err(db::ListMessagesError::StaleLeadership)
        ));
        db::remove_member(&pool, server.id, agent.id)
            .await
            .expect("list membership must be removable");
        assert!(matches!(
            db::list_channel_messages_authorized(
                &pool,
                channel.id,
                agent.id,
                Some(&current),
                &message_query(None, None),
            )
            .await,
            Err(db::ListMessagesError::Forbidden)
        ));
    }

    /// Live PostgreSQL rejects managed inserts after membership or permission revocation.
    #[tokio::test]
    async fn live_managed_send_revalidates_authority_inside_message_transaction() {
        let Some(pool) = live_test_pool().await else {
            return;
        };
        let suffix = Uuid::new_v4().simple().to_string();
        let suffix = &suffix[..12];
        let owner = db::create_user(
            &pool,
            &format!("send_owner_{suffix}"),
            &format!("send-owner-{suffix}@example.invalid"),
            "test-hash",
            None,
        )
        .await
        .expect("send owner must be created");
        let agent = db::create_owned_agent_user(
            &pool,
            &format!("send_agent_{suffix}"),
            &format!("send-agent-{suffix}@agent.local"),
            "test-hash",
            None,
            owner.id,
        )
        .await
        .expect("send agent must be created");
        let server = db::create_server(&pool, &format!("send-{suffix}"), None, owner.id)
            .await
            .expect("send server must be created");
        sqlx::query(
            r#"INSERT INTO bridge_server_state (server_id, fencing_required)
               VALUES ($1, TRUE)"#,
        )
        .bind(server.id)
        .execute(&pool)
        .await
        .expect("managed send state must be created");
        db::add_member(&pool, server.id, agent.id)
            .await
            .expect("send agent must join the room");
        let role = db::create_role(&pool, server.id, "sender", 0, perms::SEND_MESSAGES)
            .await
            .expect("sender role must be created");
        db::assign_role(&pool, server.id, agent.id, role.id)
            .await
            .expect("sender role must be assigned");
        let channel = db::create_channel(&pool, server.id, "send", None, "text")
            .await
            .expect("send channel must be created");
        let fence = db::agent_control::acquire_room_fence(&pool, server.id)
            .await
            .expect("send fence must be acquired");

        db::create_message_with_attachments(
            &pool,
            channel.id,
            agent.id,
            "authorized",
            "agent",
            db::MessageWriteAuthorization::Agent {
                server_id: server.id,
                fence: Some(&fence),
                required_permissions: perms::SEND_MESSAGES,
            },
            &[],
        )
        .await
        .expect("current membership and permission must authorize send");

        assert!(matches!(
            db::create_message_with_attachments(
                &pool,
                channel.id,
                agent.id,
                "missing attachment permission",
                "agent",
                db::MessageWriteAuthorization::Agent {
                    server_id: server.id,
                    fence: Some(&fence),
                    required_permissions: perms::SEND_MESSAGES | perms::ATTACH_FILES,
                },
                &[],
            )
            .await,
            Err(db::CreateMessageError::Forbidden)
        ));
        let current_fence = db::agent_control::acquire_room_fence(&pool, server.id)
            .await
            .expect("successor send fence must be acquired");
        assert!(matches!(
            db::create_message_with_attachments(
                &pool,
                channel.id,
                agent.id,
                "stale",
                "agent",
                db::MessageWriteAuthorization::Agent {
                    server_id: server.id,
                    fence: Some(&fence),
                    required_permissions: perms::SEND_MESSAGES,
                },
                &[],
            )
            .await,
            Err(db::CreateMessageError::StaleLeadership)
        ));
        db::remove_role_from_member(&pool, server.id, agent.id, role.id)
            .await
            .expect("sender role must be removable");
        assert!(matches!(
            db::create_message_with_attachments(
                &pool,
                channel.id,
                agent.id,
                "revoked",
                "agent",
                db::MessageWriteAuthorization::Agent {
                    server_id: server.id,
                    fence: Some(&current_fence),
                    required_permissions: perms::SEND_MESSAGES,
                },
                &[],
            )
            .await,
            Err(db::CreateMessageError::Forbidden)
        ));
        db::assign_role(&pool, server.id, agent.id, role.id)
            .await
            .expect("sender role must be restorable");
        db::remove_member(&pool, server.id, agent.id)
            .await
            .expect("send membership must be removable");
        assert!(matches!(
            db::create_message_with_attachments(
                &pool,
                channel.id,
                agent.id,
                "nonmember",
                "agent",
                db::MessageWriteAuthorization::Agent {
                    server_id: server.id,
                    fence: Some(&current_fence),
                    required_permissions: perms::SEND_MESSAGES,
                },
                &[],
            )
            .await,
            Err(db::CreateMessageError::Forbidden)
        ));
    }

    /// Live PostgreSQL proves edit/delete fences are target-scoped and held by each mutation.
    #[tokio::test]
    async fn live_message_mutations_reject_stale_and_cross_room_fences() {
        let Some(pool) = live_test_pool().await else {
            return;
        };
        let suffix = Uuid::new_v4().simple().to_string();
        let suffix = &suffix[..12];
        let owner = db::create_user(
            &pool,
            &format!("mutation_owner_{suffix}"),
            &format!("mutation-owner-{suffix}@example.invalid"),
            "test-hash",
            None,
        )
        .await
        .expect("mutation owner must be created");
        let agent = db::create_owned_agent_user(
            &pool,
            &format!("mutation_agent_{suffix}"),
            &format!("mutation-agent-{suffix}@agent.local"),
            "test-hash",
            None,
            owner.id,
        )
        .await
        .expect("mutation agent must be created");
        let target_server =
            db::create_server(&pool, &format!("mutation-target-{suffix}"), None, owner.id)
                .await
                .expect("target server must be created");
        let other_server =
            db::create_server(&pool, &format!("mutation-other-{suffix}"), None, owner.id)
                .await
                .expect("other server must be created");
        for server_id in [target_server.id, other_server.id] {
            sqlx::query(
                r#"INSERT INTO bridge_server_state (server_id, fencing_required)
                   VALUES ($1, TRUE)"#,
            )
            .bind(server_id)
            .execute(&pool)
            .await
            .expect("managed room state must be created");
        }
        db::add_member(&pool, target_server.id, agent.id)
            .await
            .expect("agent must join target room");
        let channel = db::create_channel(&pool, target_server.id, "target", None, "text")
            .await
            .expect("target channel must be created");
        let message = db::create_message(&pool, channel.id, agent.id, "original", "agent")
            .await
            .expect("agent test message must be created");

        let stale_fence = db::agent_control::acquire_room_fence(&pool, target_server.id)
            .await
            .expect("first target fence must be acquired");
        let current_fence = db::agent_control::acquire_room_fence(&pool, target_server.id)
            .await
            .expect("second target fence must be acquired");
        let other_fence = db::agent_control::acquire_room_fence(&pool, other_server.id)
            .await
            .expect("other room fence must be acquired");

        let stale_edit = db::update_message_with_fence(
            &pool,
            message.id,
            channel.id,
            agent.id,
            Some(&stale_fence),
            "stale edit",
        )
        .await;
        assert!(matches!(
            stale_edit,
            Err(db::MessageMutationError::StaleLeadership)
        ));
        let cross_room_edit = db::update_message_with_fence(
            &pool,
            message.id,
            channel.id,
            agent.id,
            Some(&other_fence),
            "cross room edit",
        )
        .await;
        assert!(matches!(
            cross_room_edit,
            Err(db::MessageMutationError::StaleLeadership)
        ));
        let updated = db::update_message_with_fence(
            &pool,
            message.id,
            channel.id,
            agent.id,
            Some(&current_fence),
            "current edit",
        )
        .await
        .expect("current target fence must edit");
        assert_eq!(updated.content, "current edit");

        let stale_delete = db::delete_message_with_fence(
            &pool,
            message.id,
            channel.id,
            agent.id,
            Some(&stale_fence),
        )
        .await;
        assert!(matches!(
            stale_delete,
            Err(db::MessageMutationError::StaleLeadership)
        ));
        db::delete_message_with_fence(
            &pool,
            message.id,
            channel.id,
            agent.id,
            Some(&current_fence),
        )
        .await
        .expect("current target fence must delete");
        assert!(
            db::get_message_by_id(&pool, message.id)
                .await
                .expect("deleted message lookup must succeed")
                .is_none()
        );
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

    /// Invalid page sizes fail before an unavailable database can be queried.
    #[tokio::test]
    async fn list_messages_rejects_invalid_limit_before_database_access() {
        let pool = PgPoolOptions::new()
            .connect_lazy("postgresql://localhost/rift_pagination_must_not_connect")
            .expect("static database URL must parse");
        let mut query = message_query(None, None);
        query.limit = Some(-1);
        let result = list_messages(
            State(pool),
            AuthUser {
                user_id: Uuid::new_v4(),
                username: "pagination-test".to_string(),
                is_agent: false,
                managed_fence: None,
            },
            Path(Uuid::new_v4()),
            Query(query),
        )
        .await;
        assert!(matches!(result, Err(AppError::BadRequest(_))));
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
            is_agent: false,
            managed_fence: None,
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
