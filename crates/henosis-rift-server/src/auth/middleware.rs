use axum::{
    extract::FromRequestParts,
    http::request::Parts,
};
use uuid::Uuid;

use crate::config::Config;
use crate::error::AppError;
use super::jwt;

/// Extractor that validates the Authorization header and provides the authenticated user's ID and username.
#[derive(Debug, Clone)]
pub struct AuthUser {
    pub user_id: Uuid,
    pub username: String,
}

impl<S> FromRequestParts<S> for AuthUser
where
    S: Send + Sync,
{
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        let config = parts
            .extensions
            .get::<Config>()
            .ok_or(AppError::Internal("Config not found in extensions".into()))?;

        let auth_header = parts
            .headers
            .get("Authorization")
            .and_then(|v| v.to_str().ok())
            .ok_or(AppError::Unauthorized)?;

        let token = auth_header
            .strip_prefix("Bearer ")
            .ok_or(AppError::Unauthorized)?;

        let claims = jwt::validate_token(token, &config.jwt_secret)?;

        Ok(AuthUser {
            user_id: claims.sub,
            username: claims.username,
        })
    }
}
