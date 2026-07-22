use axum::{Json, extract::State};
use chrono::{Duration, Utc};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use sqlx::PgPool;

use crate::auth::jwt;
use crate::auth::middleware::AuthUser;
use crate::config::Config;
use crate::db;
use crate::error::AppError;
use crate::models::user::{AuthResponse, LoginRequest, PublicUser, RegisterRequest};

/// POST /api/auth/register
pub async fn register(
    State(pool): State<PgPool>,
    State(config): State<Config>,
    Json(req): Json<RegisterRequest>,
) -> Result<Json<AuthResponse>, AppError> {
    // Validate input
    let username = req.username.trim();
    if username.len() < 3 || username.len() > 32 {
        return Err(AppError::BadRequest(
            "Username must be 3-32 characters".into(),
        ));
    }
    if req.password.len() < 8 {
        return Err(AppError::BadRequest(
            "Password must be at least 8 characters".into(),
        ));
    }
    if !req.email.contains('@') {
        return Err(AppError::BadRequest("Invalid email".into()));
    }

    // Check uniqueness
    if db::get_user_by_username(&pool, username).await?.is_some() {
        return Err(AppError::Conflict("Username already taken".into()));
    }
    if db::get_user_by_email(&pool, &req.email).await?.is_some() {
        return Err(AppError::Conflict("Email already registered".into()));
    }

    // Hash password
    let password_hash = hash_password(&req.password)?;

    // Create user
    let user = db::create_user(
        &pool,
        username,
        &req.email,
        &password_hash,
        req.display_name.as_deref(),
    )
    .await?;

    // Generate tokens
    let token = jwt::create_access_token(user.id, &user.username, &config.jwt_secret)?;
    let refresh = jwt::create_refresh_token();
    let refresh_hash = sha256_hex(&refresh);
    let expires = Utc::now() + Duration::days(30);
    db::store_refresh_token(&pool, user.id, &refresh_hash, expires).await?;

    Ok(Json(AuthResponse {
        token,
        refresh_token: refresh,
        user: PublicUser::from(user),
    }))
}

/// POST /api/auth/login
pub async fn login(
    State(pool): State<PgPool>,
    State(config): State<Config>,
    Json(req): Json<LoginRequest>,
) -> Result<Json<AuthResponse>, AppError> {
    let user = db::get_user_by_username(&pool, &req.username)
        .await?
        .ok_or(AppError::Unauthorized)?;

    verify_password(&req.password, &user.password_hash)?;

    let token = jwt::create_access_token(user.id, &user.username, &config.jwt_secret)?;
    let refresh = jwt::create_refresh_token();
    let refresh_hash = sha256_hex(&refresh);
    let expires = Utc::now() + Duration::days(30);
    db::store_refresh_token(&pool, user.id, &refresh_hash, expires).await?;

    Ok(Json(AuthResponse {
        token,
        refresh_token: refresh,
        user: PublicUser::from(user),
    }))
}

/// Request body containing a refresh token to rotate.
#[derive(Deserialize)]
pub struct RefreshRequest {
    pub refresh_token: String,
}

/// POST /api/auth/refresh
pub async fn refresh(
    State(pool): State<PgPool>,
    State(config): State<Config>,
    Json(req): Json<RefreshRequest>,
) -> Result<Json<AuthResponse>, AppError> {
    let hash = sha256_hex(&req.refresh_token);
    let user_id = db::consume_refresh_token(&pool, &hash)
        .await?
        .ok_or(AppError::Unauthorized)?;

    let user = db::get_user_by_id(&pool, user_id)
        .await?
        .ok_or(AppError::Unauthorized)?;

    let token = jwt::create_access_token(user.id, &user.username, &config.jwt_secret)?;
    let new_refresh = jwt::create_refresh_token();
    let new_hash = sha256_hex(&new_refresh);
    let expires = Utc::now() + Duration::days(30);
    db::store_refresh_token(&pool, user.id, &new_hash, expires).await?;

    Ok(Json(AuthResponse {
        token,
        refresh_token: new_refresh,
        user: PublicUser::from(user),
    }))
}

/// POST /api/auth/logout
pub async fn logout(
    State(pool): State<PgPool>,
    auth: AuthUser,
) -> Result<Json<serde_json::Value>, AppError> {
    db::delete_user_refresh_tokens(&pool, auth.user_id).await?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

// ── Helpers ──

/// Hash a password with Argon2 using a fresh random salt.
///
/// Shared with bridge agent provisioning, which hashes a throwaway random
/// password so agent accounts have no usable login.
pub(crate) fn hash_password(password: &str) -> Result<String, AppError> {
    use argon2::{
        Argon2, PasswordHasher,
        password_hash::{SaltString, rand_core::OsRng},
    };
    let salt = SaltString::generate(&mut OsRng);
    let argon2 = Argon2::default();
    argon2
        .hash_password(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| AppError::Internal(format!("Password hash error: {e}")))
}

/// Verifies a password against an Argon2 PHC hash.
fn verify_password(password: &str, hash: &str) -> Result<(), AppError> {
    use argon2::{Argon2, PasswordVerifier, password_hash::PasswordHash};
    let parsed =
        PasswordHash::new(hash).map_err(|e| AppError::Internal(format!("Invalid hash: {e}")))?;
    Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .map_err(|_| AppError::Unauthorized)
}

/// Hashes a refresh token into its lowercase SHA-256 storage representation.
fn sha256_hex(input: &str) -> String {
    let digest = Sha256::digest(input.as_bytes());
    format!("{digest:x}")
}
