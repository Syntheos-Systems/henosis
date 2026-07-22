//! Authenticated Rift user-profile, password, avatar, and direct-message routes.

use axum::{
    extract::{Multipart, Path, State},
    Json,
};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

use crate::auth::middleware::AuthUser;
use crate::config::Config;
use crate::db;
use crate::error::AppError;
use crate::models::user::PublicUser;

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
        if !email.contains('@') {
            return Err(AppError::BadRequest("Invalid email".into()));
        }
        if let Some(existing) = db::get_user_by_email(&pool, email).await?
            && existing.id != auth.user_id {
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
        && !ct.starts_with("image/") {
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
    pub current_password: String,
    pub new_password: String,
}

/// Change the authenticated user's password and revoke all refresh tokens.
pub async fn change_password(
    State(pool): State<PgPool>,
    auth: AuthUser,
    Json(req): Json<ChangePasswordRequest>,
) -> Result<Json<serde_json::Value>, AppError> {
    if req.new_password.len() < 8 {
        return Err(AppError::BadRequest(
            "New password must be at least 8 characters".into(),
        ));
    }

    let user = db::get_user_by_id(&pool, auth.user_id)
        .await?
        .ok_or(AppError::NotFound("User not found".into()))?;

    // Verify current password
    verify_password(&req.current_password, &user.password_hash)?;

    // Hash new password
    let new_hash = hash_password(&req.new_password)?;
    db::update_user_password(&pool, auth.user_id, &new_hash).await?;

    Ok(Json(serde_json::json!({ "ok": true })))
}

// ── Password helpers ──

fn hash_password(password: &str) -> Result<String, AppError> {
    use argon2::{
        password_hash::{rand_core::OsRng, SaltString},
        Argon2, PasswordHasher,
    };
    let salt = SaltString::generate(&mut OsRng);
    let argon2 = Argon2::default();
    argon2
        .hash_password(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| AppError::Internal(format!("Password hash error: {e}")))
}

/// Verify a password against its stored Argon2 hash.
fn verify_password(password: &str, hash: &str) -> Result<(), AppError> {
    use argon2::{password_hash::PasswordHash, Argon2, PasswordVerifier};
    let parsed =
        PasswordHash::new(hash).map_err(|e| AppError::Internal(format!("Invalid hash: {e}")))?;
    Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .map_err(|_| AppError::Unauthorized)
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
            |(dm_id, user_id, username, display_name, avatar_url, status, is_agent)| DmChannelInfo {
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
    if !db::is_dm_participant(&pool, dm_channel_id, auth.user_id).await? {
        return Err(AppError::Forbidden);
    }

    let messages =
        db::get_dm_messages(&pool, dm_channel_id, query.limit.unwrap_or(50), query.before).await?;
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
    pub before: Option<Uuid>,
    pub limit: Option<i64>,
}
