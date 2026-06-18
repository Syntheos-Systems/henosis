//! Bridge-controlled JWT issuance for agents.
//!
//! The bridge mints short-lived tokens on behalf of agent users.
//! Agents never authenticate directly -- the bridge manages their identity.

use chrono::Utc;
use jsonwebtoken::{encode, EncodingKey, Header};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::BridgeError;

/// JWT claims matching Rift server's expected format.
#[derive(Debug, Serialize, Deserialize)]
pub struct Claims {
    /// User ID (subject).
    pub sub: Uuid,
    /// Username.
    pub username: String,
    /// Issued-at timestamp.
    pub iat: i64,
    /// Expiration timestamp.
    pub exp: i64,
}

/// Issues short-lived JWTs for agent users.
pub struct AgentAuthManager {
    /// Shared JWT secret (must match Rift server).
    jwt_secret: String,
    /// Token TTL in seconds (default: 300 = 5 minutes).
    ttl_secs: i64,
}

impl AgentAuthManager {
    /// Create a new auth manager with the shared JWT secret.
    pub fn new(jwt_secret: String) -> Self {
        Self {
            jwt_secret,
            ttl_secs: 300,
        }
    }

    /// Issue a short-lived JWT for an agent.
    pub fn issue_token(&self, user_id: Uuid, username: &str) -> Result<String, BridgeError> {
        let now = Utc::now().timestamp();
        let claims = Claims {
            sub: user_id,
            username: username.to_string(),
            iat: now,
            exp: now + self.ttl_secs,
        };

        encode(
            &Header::default(),
            &claims,
            &EncodingKey::from_secret(self.jwt_secret.as_bytes()),
        )
        .map_err(|e| BridgeError::Auth(format!("failed to encode JWT: {e}")))
    }
}
