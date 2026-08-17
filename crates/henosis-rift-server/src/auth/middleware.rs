use axum::{
    extract::{FromRef, FromRequestParts, MatchedPath, OptionalFromRequestParts},
    http::{Method, request::Parts},
};
use sqlx::PgPool;
use uuid::Uuid;

use super::jwt::{self, Claims, TokenKind};
use crate::config::Config;
use crate::db;
use crate::error::AppError;
use crate::models::leadership::RoomFence;

/// Extractor that validates the Authorization header and provides the authenticated user's ID and username.
#[derive(Debug, Clone)]
pub struct AuthUser {
    /// Stable identifier of the authenticated Rift account.
    pub user_id: Uuid,
    /// Displayed username of the authenticated Rift account.
    pub username: String,
    /// Server-truth account classification loaded from PostgreSQL.
    pub is_agent: bool,
    /// Optional managed-room leadership capability from a bridge-issued agent token.
    pub managed_fence: Option<RoomFence>,
}

/// Provides server-truth caller classification and target-scoped fence checks.
impl AuthUser {
    /// Return whether this caller is an interactive human account.
    pub fn is_human(&self) -> bool {
        !self.is_agent
    }

    /// Return whether this caller is an agent carrying a managed-room capability.
    pub fn is_managed_agent(&self) -> bool {
        self.is_agent && self.managed_fence.is_some()
    }

    /// Return whether this caller is an agent without a managed-room capability.
    pub fn is_standalone_agent(&self) -> bool {
        self.is_agent && self.managed_fence.is_none()
    }

    /// Require this authenticated agent capability to authorize one exact target server.
    ///
    /// Read and dispatch routes can use this boundary directly. Durable mutations must
    /// additionally hold the corresponding database fence lock through commit.
    pub async fn require_server_target_fence(
        &self,
        pool: &PgPool,
        server_id: Uuid,
    ) -> Result<(), AppError> {
        if !self.is_agent {
            return Ok(());
        }
        let authorized = match self.managed_fence.as_ref() {
            Some(fence) if fence.server_id == server_id => {
                db::agent_control::agent_room_fence_is_current(pool, self.user_id, fence).await
            }
            Some(_) => Ok(false),
            None => db::agent_control::agent_requires_room_fence(pool, self.user_id)
                .await
                .map(|required| !required),
        };
        map_fence_authorization(self.user_id, authorized)
    }
}

/// Extracts authenticated Rift user identity from bearer access tokens.
impl<S> FromRequestParts<S> for AuthUser
where
    S: Send + Sync,
    PgPool: FromRef<S>,
{
    /// Authentication failures returned by the extractor.
    type Rejection = AppError;

    /// Validate the bearer token, then bind its authority class to the canonical user row.
    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
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

        let claims =
            jwt::validate_access_token(token, &config.jwt_secret, &config.agent_jwt_secret)?;
        let pool = PgPool::from_ref(state);
        let user = db::get_user_by_id(&pool, claims.sub)
            .await?
            .ok_or(AppError::Unauthorized)?;
        let auth = bind_claims_to_account(claims, user.username, user.is_agent)?;
        let matched_path = parts
            .extensions
            .get::<MatchedPath>()
            .map(MatchedPath::as_str);
        require_agent_route_policy(auth.is_agent, &parts.method, matched_path)?;
        require_current_managed_identity(&pool, &auth).await?;
        Ok(auth)
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
    PgPool: FromRef<S>,
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

/// Reject missing, stale, or unverifiable capabilities for managed agent identities.
async fn require_current_managed_identity(pool: &PgPool, auth: &AuthUser) -> Result<(), AppError> {
    if !auth.is_agent {
        return Ok(());
    }
    let authorized = match auth.managed_fence.as_ref() {
        Some(fence) => {
            db::agent_control::agent_room_fence_is_current(pool, auth.user_id, fence).await
        }
        None => db::agent_control::agent_requires_room_fence(pool, auth.user_id)
            .await
            .map(|required| !required),
    };
    map_fence_authorization(auth.user_id, authorized)
}

/// Bind signed claims to a canonical database account and discard claimed display identity.
pub(crate) fn bind_claims_to_account(
    claims: Claims,
    canonical_username: String,
    is_agent: bool,
) -> Result<AuthUser, AppError> {
    let kind_matches = matches!(
        (claims.token_kind, is_agent),
        (TokenKind::Human, false) | (TokenKind::Agent, true)
    );
    if !kind_matches || (!is_agent && claims.managed_fence.is_some()) {
        return Err(AppError::Unauthorized);
    }
    Ok(AuthUser {
        user_id: claims.sub,
        username: canonical_username,
        is_agent,
        managed_fence: claims.managed_fence,
    })
}

/// Restrict agent-session credentials to the message operations needed by the bridge.
fn require_agent_route_policy(
    is_agent: bool,
    method: &Method,
    matched_path: Option<&str>,
) -> Result<(), AppError> {
    if !is_agent {
        return Ok(());
    }
    let is_message_collection = matched_path == Some("/api/channels/{channel_id}/messages");
    if is_message_collection && (method == Method::GET || method == Method::POST) {
        Ok(())
    } else {
        Err(AppError::Forbidden)
    }
}

/// Convert a fail-closed PostgreSQL fence verdict into the stable public API error.
fn map_fence_authorization(
    user_id: Uuid,
    authorized: Result<bool, sqlx::Error>,
) -> Result<(), AppError> {
    match authorized {
        Ok(true) => Ok(()),
        Ok(false) => Err(AppError::stale_leadership_fence()),
        Err(error) => {
            tracing::error!(%user_id, %error, "managed identity fence lookup failed");
            Err(AppError::leadership_fence_unavailable())
        }
    }
}

#[cfg(test)]
/// Exercises cryptographic token-class binding to canonical account rows.
mod tests {
    use chrono::{Duration, Utc};
    use sqlx::postgres::PgPoolOptions;

    use super::*;

    /// Build deterministic unexpired claims for account-binding unit tests.
    fn claims(token_kind: TokenKind, username: &str) -> Claims {
        let now = Utc::now();
        Claims {
            sub: Uuid::new_v4(),
            username: username.to_string(),
            token_kind,
            iat: now.timestamp(),
            exp: (now + Duration::hours(1)).timestamp(),
            managed_fence: None,
        }
    }

    /// Agent-authority tokens cannot be rebound to human database accounts.
    #[test]
    fn agent_token_for_human_subject_is_rejected() {
        let result = bind_claims_to_account(
            claims(TokenKind::Agent, "claimed-human"),
            "canonical-human".to_string(),
            false,
        );
        assert!(matches!(result, Err(AppError::Unauthorized)));
    }

    /// Human-authority tokens cannot be rebound to agent database accounts.
    #[test]
    fn human_token_for_agent_subject_is_rejected() {
        let result = bind_claims_to_account(
            claims(TokenKind::Human, "claimed-agent"),
            "canonical-agent".to_string(),
            true,
        );
        assert!(matches!(result, Err(AppError::Unauthorized)));
    }

    /// Successful authentication always uses the current database username.
    #[test]
    fn signed_username_is_replaced_with_canonical_database_username() {
        let auth = bind_claims_to_account(
            claims(TokenKind::Human, "forged-name"),
            "canonical-name".to_string(),
            false,
        )
        .expect("matching human token and account must bind");
        assert_eq!(auth.username, "canonical-name");
        assert!(auth.is_human());
        assert!(!auth.is_managed_agent());
        assert!(!auth.is_standalone_agent());
    }

    /// Agent sessions may read and append messages but cannot reach control-plane routes.
    #[test]
    fn agent_route_policy_is_least_privilege() {
        let messages = "/api/channels/{channel_id}/messages";
        assert!(require_agent_route_policy(true, &Method::GET, Some(messages)).is_ok());
        assert!(require_agent_route_policy(true, &Method::POST, Some(messages)).is_ok());

        let denied = [
            (Method::PATCH, Some(messages)),
            (Method::DELETE, Some(messages)),
            (
                Method::PATCH,
                Some("/api/channels/{channel_id}/messages/{message_id}"),
            ),
            (Method::GET, Some("/api/users/@me")),
            (Method::GET, Some("/api/servers/{server_id}")),
            (Method::POST, Some("/api/servers/{server_id}/roles")),
            (Method::POST, Some("/api/upload")),
            (Method::GET, None),
        ];
        for (method, path) in denied {
            assert!(
                matches!(
                    require_agent_route_policy(true, &method, path),
                    Err(AppError::Forbidden)
                ),
                "agent route unexpectedly allowed: {method} {path:?}"
            );
        }
        assert!(require_agent_route_policy(false, &Method::DELETE, None).is_ok());
    }

    /// A capability for room A cannot authorize room B, even when PostgreSQL is unavailable.
    #[tokio::test]
    async fn managed_agent_fence_is_bound_to_exact_target_server() {
        let fence_server = Uuid::new_v4();
        let target_server = Uuid::new_v4();
        let auth = AuthUser {
            user_id: Uuid::new_v4(),
            username: "managed-agent".to_string(),
            is_agent: true,
            managed_fence: Some(RoomFence {
                server_id: fence_server,
                epoch: 1,
                lease_id: Uuid::new_v4(),
            }),
        };
        let pool = PgPoolOptions::new()
            .connect_lazy("postgresql://rift.invalid/henosis_test")
            .expect("test database URL must parse");

        let result = auth.require_server_target_fence(&pool, target_server).await;
        assert!(matches!(
            result,
            Err(AppError::Coded {
                code: "stale_leadership_fence",
                ..
            })
        ));
    }
}
