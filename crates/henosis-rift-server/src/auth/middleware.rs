use axum::{
    extract::{FromRequestParts, OptionalFromRequestParts},
    http::request::Parts,
};
use uuid::Uuid;

use super::jwt;
use crate::config::Config;
use crate::error::AppError;

/// Extractor that validates the Authorization header and provides the authenticated user's ID and username.
#[derive(Debug, Clone)]
pub struct AuthUser {
    pub user_id: Uuid,
    pub username: String,
}

/// Extracts authenticated Rift user identity from bearer access tokens.
impl<S> FromRequestParts<S> for AuthUser
where
    S: Send + Sync,
{
    /// Authentication failures returned by the extractor.
    type Rejection = AppError;

    /// Validates the request bearer token against the configured JWT secret.
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

/// Extracts Rift user identity when present, without rejecting when it is absent.
///
/// Used by routes that accept more than one kind of caller, such as the bridge
/// status route, which admits either the bridge daemon's shared secret or a
/// human controller. A missing, malformed, or expired token yields `None`; the
/// route is then responsible for refusing the request through its other path.
impl<S> OptionalFromRequestParts<S> for AuthUser
where
    S: Send + Sync,
{
    /// Optional extraction never rejects on its own.
    type Rejection = AppError;

    /// Returns the authenticated user, or `None` when no valid bearer token is present.
    async fn from_request_parts(
        parts: &mut Parts,
        state: &S,
    ) -> Result<Option<Self>, Self::Rejection> {
        Ok(
            <AuthUser as FromRequestParts<S>>::from_request_parts(parts, state)
                .await
                .ok(),
        )
    }
}
