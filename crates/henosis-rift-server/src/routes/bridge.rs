//! Internal bridge endpoints for notifying the gateway about externally-created messages.
//! Secured via a dedicated bridge secret passed as a Bearer token.

use axum::{
    Json,
    extract::State,
    http::{HeaderMap, StatusCode},
};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

use crate::config::Config;
use crate::db;
use crate::error::AppError;
use crate::ws::gateway::{Gateway, GatewayEvent};

/// Email domain stamped on every bridge-provisioned agent account.
///
/// Doubles as the discriminator that tells a bridge agent apart from a human
/// who merely happens to hold the same username, since `is_agent` is FALSE on
/// both a human account and an agent provisioned before that flag was set.
const AGENT_EMAIL_DOMAIN: &str = "agent.local";

/// Build the canonical account email for an agent username.
pub(crate) fn agent_email(username: &str) -> String {
    format!("{username}@{AGENT_EMAIL_DOMAIN}")
}

/// Check a request's Bearer token against the shared bridge secret.
///
/// Compares in constant time so the secret cannot be recovered a byte at a time
/// by timing repeated requests. Human credentials are JWTs and can never equal
/// the dedicated bridge secret, which keeps bridge-only routes isolated from
/// the user JWT signing key.
pub(crate) fn bridge_authorized(headers: &HeaderMap, config: &Config) -> bool {
    let Some(token) = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
    else {
        return false;
    };

    let (token, secret) = (token.as_bytes(), config.bridge_secret.as_bytes());
    if token.len() != secret.len() {
        return false;
    }
    token
        .iter()
        .zip(secret)
        .fold(0u8, |acc, (a, b)| acc | (a ^ b))
        == 0
}

/// Request body for POST /api/bridge/notify.
#[derive(Deserialize)]
pub struct NotifyRequest {
    pub channel_id: Uuid,
    pub message_id: Uuid,
}

/// One agent the bridge wants present and joined.
#[derive(Deserialize)]
pub struct ProvisionAgent {
    /// Rift username, unique across users.
    pub username: String,
    /// Display name shown in the UI.
    pub display_name: Option<String>,
}

/// Request body for POST /api/bridge/provision.
#[derive(Deserialize)]
pub struct ProvisionRequest {
    /// Server every listed agent must end up a member of.
    pub server_id: Uuid,
    /// Roster to converge. May be empty.
    pub agents: Vec<ProvisionAgent>,
}

/// A single provisioned agent as reported back to the bridge.
#[derive(Serialize)]
pub struct ProvisionedAgent {
    /// Rift user ID the bridge should mint tokens for.
    pub id: Uuid,
    /// Rift username.
    pub username: String,
    /// Always true once provisioning succeeds.
    pub is_agent: bool,
}

/// Response body for POST /api/bridge/provision.
#[derive(Serialize)]
pub struct ProvisionResponse {
    /// Provisioned agents, in request order.
    pub agents: Vec<ProvisionedAgent>,
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
    if !bridge_authorized(&headers, &config) {
        return StatusCode::UNAUTHORIZED;
    }

    // Fetch the full message with author info
    let msg = match db::get_message_by_id(&pool, req.message_id).await {
        Ok(Some(m)) => m,
        _ => return StatusCode::NOT_FOUND,
    };

    // The message and the broadcast target are two independent caller-supplied
    // fields. Without binding them, this route would relay any message -- including
    // a private DM -- to the subscribers of any other channel.
    if msg.channel_id != req.channel_id {
        return StatusCode::NOT_FOUND;
    }

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

/// POST /api/bridge/provision
///
/// Converges the bridge's configured agent roster: ensures every listed agent
/// exists as an agent user and is a member of the target server. Called on
/// every bridge boot and safe to repeat -- without it the agents exist but are
/// not members, and the gateway refuses their channel subscription, which
/// leaves the room silently deaf.
pub async fn provision_agents(
    State(pool): State<PgPool>,
    State(config): State<Config>,
    headers: HeaderMap,
    Json(req): Json<ProvisionRequest>,
) -> Result<Json<ProvisionResponse>, AppError> {
    if !bridge_authorized(&headers, &config) {
        return Err(AppError::Unauthorized);
    }

    // Fail loudly on a bad server_id. Joining agents to a server that does not
    // exist would otherwise "succeed" and strand the room with no members.
    if db::get_server_by_id(&pool, req.server_id).await?.is_none() {
        return Err(AppError::NotFound(format!(
            "Server {} not found",
            req.server_id
        )));
    }

    let mut provisioned = Vec::with_capacity(req.agents.len());

    for agent in &req.agents {
        let user = match db::get_user_by_username(&pool, &agent.username).await? {
            Some(existing) if existing.is_agent => existing,
            // An agent provisioned before is_agent was set correctly. The
            // agent-domain email is what proves it is ours to promote.
            Some(existing) if existing.email == agent_email(&agent.username) => {
                db::mark_user_as_agent(&pool, existing.id).await?
            }
            // A human already holds this username. Promoting it would hand the
            // bridge control of a person's account, so refuse instead.
            Some(_) => {
                return Err(AppError::Conflict(format!(
                    "username '{}' belongs to a human account and will not be promoted to an agent",
                    agent.username
                )));
            }
            None => {
                // Agents authenticate only via bridge-minted tokens. Hashing an
                // unguessable random password keeps the password login path
                // fail-closed rather than relying on a sentinel hash value.
                let password_hash = super::auth::hash_password(&random_password())?;
                db::create_agent_user(
                    &pool,
                    &agent.username,
                    &agent_email(&agent.username),
                    &password_hash,
                    agent.display_name.as_deref(),
                )
                .await?
            }
        };

        db::add_member(&pool, req.server_id, user.id).await?;

        // Converge history on every boot: migration 004 only retypes rows
        // whose author was already flagged is_agent when it ran, so messages
        // by accounts promoted just now (and rows written by a pre-stamping
        // server build) still sit at the 'user' default. Idempotent.
        let retyped = db::retype_agent_messages(&pool, user.id).await?;
        if retyped > 0 {
            tracing::info!(
                user_id = %user.id,
                retyped,
                "retyped agent's historic messages left at the 'user' default"
            );
        }

        tracing::info!(
            server_id = %req.server_id,
            user_id = %user.id,
            username = %user.username,
            "provisioned bridge agent and joined server"
        );

        provisioned.push(ProvisionedAgent {
            id: user.id,
            username: user.username,
            is_agent: true,
        });
    }

    Ok(Json(ProvisionResponse {
        agents: provisioned,
    }))
}

/// Generate an unguessable password for an agent account nobody logs into.
pub(crate) fn random_password() -> String {
    use rand::Rng;
    let bytes: [u8; 32] = rand::rng().random();
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Covers the bridge-only auth gate and the agent account conventions that
/// keep human accounts out of provisioning.
#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    /// Build a Config carrying just the secret these tests exercise.
    fn config_with_secret(secret: &str) -> Config {
        Config {
            database_url: String::new(),
            jwt_secret: "different-jwt-signing-secret".to_string(),
            bridge_secret: secret.to_string(),
            listen_addr: String::new(),
            cors_origins: Vec::new(),
            upload_dir: String::new(),
            max_upload_bytes: 0,
        }
    }

    /// Build headers carrying the given Authorization value.
    fn headers_with_auth(value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert("authorization", HeaderValue::from_str(value).unwrap());
        headers
    }

    /// The matching shared secret is accepted.
    #[test]
    fn correct_secret_is_authorized() {
        let config = config_with_secret("super-secret");
        let headers = headers_with_auth("Bearer super-secret");
        assert!(bridge_authorized(&headers, &config));
    }

    /// A wrong secret of the SAME length is rejected -- guards the constant-time
    /// comparison, whose early-exit-free loop only runs on equal lengths.
    #[test]
    fn wrong_secret_of_equal_length_is_rejected() {
        let config = config_with_secret("super-secret");
        let headers = headers_with_auth("Bearer super-secreT");
        assert!(!bridge_authorized(&headers, &config));
    }

    /// A prefix of the secret must not pass; length is part of the comparison.
    #[test]
    fn secret_prefix_is_rejected() {
        let config = config_with_secret("super-secret");
        let headers = headers_with_auth("Bearer super");
        assert!(!bridge_authorized(&headers, &config));
    }

    /// A JWT is rejected. This is the property that keeps bridge-only routes
    /// closed to ordinary human login tokens: they are never the raw secret.
    #[test]
    fn human_jwt_is_rejected() {
        let config = config_with_secret("super-secret");
        let token =
            crate::auth::jwt::create_access_token(uuid::Uuid::new_v4(), "alice", "super-secret")
                .expect("encode");
        let headers = headers_with_auth(&format!("Bearer {token}"));
        assert!(!bridge_authorized(&headers, &config));
    }

    /// A missing or non-Bearer Authorization header is rejected.
    #[test]
    fn missing_and_malformed_headers_are_rejected() {
        let config = config_with_secret("super-secret");
        assert!(!bridge_authorized(&HeaderMap::new(), &config));
        assert!(!bridge_authorized(
            &headers_with_auth("super-secret"),
            &config
        ));
        assert!(!bridge_authorized(
            &headers_with_auth("Basic super-secret"),
            &config
        ));
    }

    /// Agent emails are stamped with the agent domain, which is the marker that
    /// distinguishes a promotable agent account from a human's.
    #[test]
    fn agent_email_uses_the_agent_domain() {
        assert_eq!(agent_email("vera"), "vera@agent.local");
        assert_ne!(agent_email("vera"), "vera@example.com");
    }

    /// Two provisioning runs must never mint the same agent password.
    #[test]
    fn random_password_is_not_constant() {
        assert_ne!(random_password(), random_password());
        assert_eq!(random_password().len(), 64);
    }
}
