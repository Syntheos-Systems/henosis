//! Authenticated Rift user-profile, password, avatar, and direct-message routes.

use axum::{
    Json,
    extract::{Multipart, Path, State},
};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

use crate::auth::middleware::AuthUser;
use crate::config::Config;
use crate::db;
use crate::error::AppError;
use crate::models::user::PublicUser;

/// Minimum encoded byte length accepted by the password-change route.
const MIN_PASSWORD_BYTES: usize = 8;
/// Maximum encoded byte length accepted by the password-change route.
const MAX_PASSWORD_BYTES: usize = 128;

#[derive(Deserialize)]
/// Fields an authenticated user may change on their own profile.
pub struct UpdateProfileRequest {
    pub display_name: Option<String>,
    pub about: Option<String>,
    pub email: Option<String>,
}

#[derive(Serialize)]
/// A public user record with private fields included for its owner.
pub struct UserProfile {
    #[serde(flatten)]
    pub user: PublicUser,
    pub email: Option<String>, // only included when viewing own profile
}

/// GET /api/users/@me
pub async fn get_me(
    State(pool): State<PgPool>,
    auth: AuthUser,
) -> Result<Json<UserProfile>, AppError> {
    let user = db::get_user_by_id(&pool, auth.user_id)
        .await?
        .ok_or(AppError::NotFound("User not found".into()))?;

    Ok(Json(UserProfile {
        email: Some(user.email.clone()),
        user: PublicUser::from(user),
    }))
}

/// PATCH /api/users/@me
pub async fn update_me(
    State(pool): State<PgPool>,
    auth: AuthUser,
    Json(req): Json<UpdateProfileRequest>,
) -> Result<Json<UserProfile>, AppError> {
    // If email is being changed, validate and check uniqueness
    if let Some(ref email) = req.email {
        let email = email.trim();
        super::auth::validate_human_email(email)?;
        if let Some(existing) = db::get_user_by_email(&pool, email).await?
            && existing.id != auth.user_id
        {
            return Err(AppError::Conflict("Email already registered".into()));
        }
        db::update_user_email(&pool, auth.user_id, email).await?;
    }

    let user = db::update_user_profile(
        &pool,
        auth.user_id,
        req.display_name.as_deref(),
        req.about.as_deref(),
        None, // avatar handled separately via upload
    )
    .await?;

    Ok(Json(UserProfile {
        email: Some(user.email.clone()),
        user: PublicUser::from(user),
    }))
}

/// Upload a new avatar image for the authenticated user.
pub async fn upload_avatar(
    State(pool): State<PgPool>,
    State(config): State<Config>,
    auth: AuthUser,
    mut multipart: Multipart,
) -> Result<Json<UserProfile>, AppError> {
    let field = multipart
        .next_field()
        .await
        .map_err(|e| AppError::BadRequest(format!("Multipart error: {e}")))?
        .ok_or_else(|| AppError::BadRequest("No file uploaded".into()))?;

    let content_type = field.content_type().map(|s| s.to_string());

    // Validate it's an image
    if let Some(ref ct) = content_type
        && !ct.starts_with("image/")
    {
        return Err(AppError::BadRequest("File must be an image".into()));
    }

    let data = field
        .bytes()
        .await
        .map_err(|e| AppError::BadRequest(format!("Failed to read file: {e}")))?;

    // Limit avatar to 5MB
    if data.len() > 5 * 1024 * 1024 {
        return Err(AppError::BadRequest("Avatar too large (max 5MB)".into()));
    }

    if data.is_empty() {
        return Err(AppError::BadRequest("Empty file".into()));
    }

    // Determine extension from content type
    let ext = match content_type.as_deref() {
        Some("image/png") => "png",
        Some("image/gif") => "gif",
        Some("image/webp") => "webp",
        _ => "jpg",
    };

    // Create avatars subdirectory
    let avatar_dir = std::path::Path::new(&config.upload_dir).join("avatars");
    tokio::fs::create_dir_all(&avatar_dir)
        .await
        .map_err(|e| AppError::Internal(format!("Failed to create avatar dir: {e}")))?;

    // Use user ID as filename so each user has exactly one avatar file
    let filename = format!("{}.{ext}", auth.user_id);
    let file_path = avatar_dir.join(&filename);
    tokio::fs::write(&file_path, &data)
        .await
        .map_err(|e| AppError::Internal(format!("Failed to save avatar: {e}")))?;

    // Add cache-busting timestamp to URL
    let ts = chrono::Utc::now().timestamp();
    let avatar_url = format!("/uploads/avatars/{filename}?t={ts}");

    let user = db::update_user_profile(&pool, auth.user_id, None, None, Some(&avatar_url)).await?;

    Ok(Json(UserProfile {
        email: Some(user.email.clone()),
        user: PublicUser::from(user),
    }))
}

#[derive(Deserialize)]
/// Current and replacement credentials for a password change.
pub struct ChangePasswordRequest {
    /// Existing credential that authorizes the password replacement.
    pub current_password: String,
    /// Replacement credential to hash and store.
    pub new_password: String,
}

/// Bound both password-change credentials before database or Argon2 work.
fn validate_password_change_request(request: &ChangePasswordRequest) -> Result<(), AppError> {
    if request.new_password.len() < MIN_PASSWORD_BYTES {
        return Err(AppError::BadRequest(
            "New password must be at least 8 characters".into(),
        ));
    }
    if request.new_password.len() > MAX_PASSWORD_BYTES {
        return Err(AppError::BadRequest(
            "New password must be at most 128 bytes".into(),
        ));
    }
    if !(MIN_PASSWORD_BYTES..=MAX_PASSWORD_BYTES).contains(&request.current_password.len()) {
        return Err(AppError::BadRequest(
            "Current password must be 8-128 bytes".into(),
        ));
    }
    Ok(())
}

/// Preserve opaque credential semantics when a concurrent password update wins.
fn require_password_update_applied(updated: bool) -> Result<(), AppError> {
    if updated {
        Ok(())
    } else {
        Err(AppError::Unauthorized)
    }
}

/// Change the authenticated user's password and revoke all refresh tokens.
pub async fn change_password(
    State(pool): State<PgPool>,
    auth: AuthUser,
    Json(req): Json<ChangePasswordRequest>,
) -> Result<Json<serde_json::Value>, AppError> {
    validate_password_change_request(&req)?;

    let user = db::get_user_by_id(&pool, auth.user_id)
        .await?
        .ok_or(AppError::NotFound("User not found".into()))?;

    // Retain the verified database value for the conditional update boundary.
    let expected_password_hash = user.password_hash;
    super::auth::verify_password_bounded(req.current_password, expected_password_hash.clone())
        .await?;

    // Hash new password
    let new_hash = super::auth::hash_password_bounded(req.new_password).await?;
    let updated =
        db::update_user_password(&pool, auth.user_id, &expected_password_hash, &new_hash).await?;
    require_password_update_applied(updated)?;

    Ok(Json(serde_json::json!({ "ok": true })))
}

/// GET /api/users/:user_id
pub async fn get_user(
    State(pool): State<PgPool>,
    _auth: AuthUser,
    Path(user_id): Path<Uuid>,
) -> Result<Json<PublicUser>, AppError> {
    let user = db::get_user_by_id(&pool, user_id)
        .await?
        .ok_or(AppError::NotFound("User not found".into()))?;

    Ok(Json(PublicUser::from(user)))
}

// ─── DMs ───

#[derive(Serialize)]
pub struct DmChannelInfo {
    pub id: Uuid,
    pub recipient: PublicUser,
}

#[derive(Deserialize)]
/// Identifies the recipient for a new direct-message channel.
pub struct CreateDmRequest {
    pub recipient_id: Uuid,
}

/// GET /api/users/@me/dms
pub async fn list_dms(
    State(pool): State<PgPool>,
    auth: AuthUser,
) -> Result<Json<Vec<DmChannelInfo>>, AppError> {
    let channels = db::get_user_dm_channels(&pool, auth.user_id).await?;
    let result: Vec<DmChannelInfo> = channels
        .into_iter()
        .map(
            |(dm_id, user_id, username, display_name, avatar_url, status, is_agent)| {
                DmChannelInfo {
                    id: dm_id,
                    recipient: PublicUser {
                        id: user_id,
                        username,
                        display_name,
                        avatar_url,
                        status,
                        about: None,
                        is_agent,
                    },
                }
            },
        )
        .collect();

    Ok(Json(result))
}

/// POST /api/users/@me/dms
pub async fn create_dm(
    State(pool): State<PgPool>,
    auth: AuthUser,
    Json(req): Json<CreateDmRequest>,
) -> Result<Json<DmChannelInfo>, AppError> {
    if req.recipient_id == auth.user_id {
        return Err(AppError::BadRequest("Cannot DM yourself".into()));
    }

    // Block agent-to-agent DMs -- agents communicate through room channels only.
    let initiator = db::get_user_by_id(&pool, auth.user_id)
        .await?
        .ok_or(AppError::NotFound("User not found".into()))?;
    let recipient = db::get_user_by_id(&pool, req.recipient_id)
        .await?
        .ok_or(AppError::NotFound("User not found".into()))?;

    if initiator.is_agent && recipient.is_agent {
        return Err(AppError::Forbidden);
    }

    let dm_id = db::get_or_create_dm_channel(&pool, auth.user_id, req.recipient_id).await?;

    Ok(Json(DmChannelInfo {
        id: dm_id,
        recipient: PublicUser::from(recipient),
    }))
}

/// GET /api/dms/:dm_channel_id/messages
pub async fn list_dm_messages(
    State(pool): State<PgPool>,
    auth: AuthUser,
    Path(dm_channel_id): Path<Uuid>,
    axum::extract::Query(query): axum::extract::Query<DmMessageQuery>,
) -> Result<Json<Vec<db::DmMessageWithAuthor>>, AppError> {
    let limit = crate::models::message::validated_message_page_limit(query.limit)
        .map_err(|message| AppError::BadRequest(message.to_string()))?;
    if !db::is_dm_participant(&pool, dm_channel_id, auth.user_id).await? {
        return Err(AppError::Forbidden);
    }

    let messages = db::get_dm_messages(&pool, dm_channel_id, limit, query.before).await?;
    Ok(Json(messages))
}

/// POST /api/dms/:dm_channel_id/messages
pub async fn send_dm_message(
    State(pool): State<PgPool>,
    auth: AuthUser,
    Path(dm_channel_id): Path<Uuid>,
    Json(req): Json<crate::models::message::SendMessageRequest>,
) -> Result<Json<db::DmMessageWithAuthor>, AppError> {
    if !db::is_dm_participant(&pool, dm_channel_id, auth.user_id).await? {
        return Err(AppError::Forbidden);
    }

    let content = req.content.as_deref().unwrap_or("").trim();
    if content.is_empty() {
        return Err(AppError::BadRequest("Message cannot be empty".into()));
    }

    let msg = db::create_dm_message(&pool, dm_channel_id, auth.user_id, content).await?;
    Ok(Json(msg))
}

#[derive(Deserialize)]
/// Pagination parameters for direct-message history.
pub struct DmMessageQuery {
    /// Return messages created before this direct-message identifier.
    pub before: Option<Uuid>,
    /// Maximum page size, constrained to the shared message-history boundary.
    pub limit: Option<i64>,
}

#[cfg(test)]
/// Exercises direct-message pagination validation at the route boundary.
mod tests {
    use std::time::Duration;

    use axum::{
        Json,
        extract::{Path, Query, State},
    };
    use chrono::{Duration as ChronoDuration, Utc};
    use sqlx::{PgPool, postgres::PgPoolOptions};
    use uuid::Uuid;

    use super::{
        ChangePasswordRequest, DmMessageQuery, change_password, list_dm_messages,
        require_password_update_applied, validate_password_change_request,
    };
    use crate::auth::middleware::AuthUser;
    use crate::db;
    use crate::error::AppError;

    /// Connect to the opt-in PostgreSQL database for password-update race tests.
    async fn live_users_test_pool() -> Option<PgPool> {
        let Some(database_url) = std::env::var_os("HENOSIS_RIFT_TEST_DATABASE_URL") else {
            eprintln!(
                "skipping live password-update race test: HENOSIS_RIFT_TEST_DATABASE_URL is unset"
            );
            return None;
        };
        let pool = PgPoolOptions::new()
            .max_connections(2)
            .connect(&database_url.to_string_lossy())
            .await
            .expect("test database must be reachable");
        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .expect("test database migrations must apply");
        Some(pool)
    }

    /// Invalid page sizes fail before participant lookup can touch the database.
    #[tokio::test]
    async fn list_dm_messages_rejects_invalid_limit_before_database_access() {
        let pool = PgPoolOptions::new()
            .connect_lazy("postgresql://localhost/rift_dm_pagination_must_not_connect")
            .expect("static database URL must parse");
        let result = list_dm_messages(
            State(pool),
            AuthUser {
                user_id: Uuid::new_v4(),
                username: "dm-pagination-test".to_string(),
                is_agent: false,
                managed_fence: None,
            },
            Path(Uuid::new_v4()),
            Query(DmMessageQuery {
                before: None,
                limit: Some(-1),
            }),
        )
        .await;
        assert!(matches!(result, Err(AppError::BadRequest(_))));
    }

    /// Both password-change fields enforce exact encoded-byte work bounds.
    #[test]
    fn password_change_fields_have_strict_bounds() {
        for (current_len, new_len) in [(8, 8), (128, 128)] {
            let request = ChangePasswordRequest {
                current_password: "c".repeat(current_len),
                new_password: "n".repeat(new_len),
            };
            assert!(validate_password_change_request(&request).is_ok());
        }

        for (current_len, new_len) in [(7, 8), (129, 8), (8, 7), (8, 129)] {
            let request = ChangePasswordRequest {
                current_password: "c".repeat(current_len),
                new_password: "n".repeat(new_len),
            };
            assert!(validate_password_change_request(&request).is_err());
        }
    }

    /// Invalid password sizes fail before the route can acquire a database connection.
    #[tokio::test]
    async fn invalid_password_change_fails_before_database_access() {
        let pool = PgPoolOptions::new()
            .acquire_timeout(Duration::from_millis(100))
            .connect_lazy("postgresql://localhost:1/rift_password_bounds_must_not_connect")
            .expect("static unavailable database URL must parse");
        let auth = AuthUser {
            user_id: Uuid::new_v4(),
            username: "password-bounds-test".to_string(),
            is_agent: false,
            managed_fence: None,
        };

        let current_too_long = change_password(
            State(pool.clone()),
            auth.clone(),
            Json(ChangePasswordRequest {
                current_password: "c".repeat(129),
                new_password: "n".repeat(8),
            }),
        )
        .await;
        assert!(matches!(current_too_long, Err(AppError::BadRequest(_))));

        let replacement_too_long = change_password(
            State(pool),
            auth,
            Json(ChangePasswordRequest {
                current_password: "c".repeat(8),
                new_password: "n".repeat(129),
            }),
        )
        .await;
        assert!(matches!(replacement_too_long, Err(AppError::BadRequest(_))));
    }

    /// A lost password-update race remains an opaque credential failure.
    #[test]
    fn stale_password_update_maps_to_unauthorized() {
        assert!(require_password_update_applied(true).is_ok());
        assert!(matches!(
            require_password_update_applied(false),
            Err(AppError::Unauthorized)
        ));
    }

    /// A stale verified hash cannot overwrite newer credentials or revoke their session.
    #[tokio::test]
    async fn live_stale_password_update_preserves_newer_credentials() {
        let Some(pool) = live_users_test_pool().await else {
            return;
        };
        let suffix = Uuid::new_v4().simple().to_string();
        let suffix = &suffix[..12];
        let user = db::create_user(
            &pool,
            &format!("password_owner_{suffix}"),
            &format!("password-owner-{suffix}@example.invalid"),
            "expected-hash",
            None,
        )
        .await
        .expect("password-race user must be created");
        let first_update =
            db::update_user_password(&pool, user.id, "expected-hash", "newer-password-hash")
                .await
                .expect("winning password update must execute");
        assert!(first_update);
        let refresh_hash = format!("post-race-refresh-{suffix}");
        db::store_refresh_token(
            &pool,
            user.id,
            &refresh_hash,
            Utc::now() + ChronoDuration::days(1),
        )
        .await
        .expect("newer credential session must be stored");

        let stale_update =
            db::update_user_password(&pool, user.id, "expected-hash", "stale-password-hash")
                .await
                .expect("stale conditional update must execute");

        assert!(!stale_update);
        let persisted = db::get_user_by_id(&pool, user.id)
            .await
            .expect("password postcondition lookup must succeed")
            .expect("password-race user must remain present");
        assert_eq!(persisted.password_hash, "newer-password-hash");
        assert_eq!(
            db::consume_refresh_token(&pool, &refresh_hash)
                .await
                .expect("post-race refresh lookup must succeed"),
            Some(user.id),
            "losing password update must not revoke the winning session"
        );
    }
}
