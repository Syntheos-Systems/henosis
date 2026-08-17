//! Bridge-controlled JWT issuance for agents.
//!
//! The bridge mints short-lived tokens on behalf of agent users.
//! Agents never authenticate directly -- the bridge manages their identity.

use chrono::Utc;
use henosis_rift_server::auth::jwt::{Claims, TokenKind};
use henosis_rift_server::models::leadership::RoomFence;
use jsonwebtoken::{encode, EncodingKey, Header};
use uuid::Uuid;

use crate::error::BridgeError;

/// Issues short-lived JWTs for agent users.
pub struct AgentAuthManager {
    /// Dedicated agent-token signing secret that cannot mint human sessions.
    agent_jwt_secret: String,
    /// Dedicated bearer secret for bridge-only server routes.
    bridge_secret: String,
    /// Token TTL in seconds (default: 300 = 5 minutes).
    ttl_secs: i64,
    /// Optional managed-room leadership capability copied into every issued agent token.
    managed_fence: Option<RoomFence>,
}

/// Implements bridge-side credential issuance for agent identities.
impl AgentAuthManager {
    /// Create an auth manager with independent agent-signing and bridge-route secrets.
    pub fn new(agent_jwt_secret: String, bridge_secret: String) -> Self {
        Self {
            agent_jwt_secret,
            bridge_secret,
            ttl_secs: 300,
            managed_fence: None,
        }
    }

    /// Attach an optional managed-room leadership capability to subsequently issued tokens.
    pub fn with_managed_fence(mut self, managed_fence: Option<RoomFence>) -> Self {
        self.managed_fence = managed_fence;
        self
    }

    /// Return the optional managed-room leadership capability for private Rift headers.
    pub fn managed_fence(&self) -> Option<&RoomFence> {
        self.managed_fence.as_ref()
    }

    /// The shared bridge secret, presented verbatim as the Bearer token on
    /// bridge-only server routes (`/api/bridge/*`).
    ///
    /// Those routes compare against the raw secret rather than validating a
    /// JWT, which is precisely what keeps them closed to human accounts: a
    /// login token is a JWT and can never equal the secret.
    pub fn bridge_secret(&self) -> &str {
        &self.bridge_secret
    }

    /// Issue a short-lived JWT for an agent.
    pub fn issue_token(&self, user_id: Uuid, username: &str) -> Result<String, BridgeError> {
        let now = Utc::now().timestamp();
        let claims = Claims {
            sub: user_id,
            username: username.to_string(),
            token_kind: TokenKind::Agent,
            iat: now,
            exp: now + self.ttl_secs,
            managed_fence: self.managed_fence,
        };

        encode(
            &Header::default(),
            &claims,
            &EncodingKey::from_secret(self.agent_jwt_secret.as_bytes()),
        )
        .map_err(|e| BridgeError::Auth(format!("failed to encode JWT: {e}")))
    }
}

/// Unit tests for independent bridge and JWT credential handling.
#[cfg(test)]
mod tests {
    use super::AgentAuthManager;
    use henosis_rift_server::auth::jwt::{self, TokenKind};
    use henosis_rift_server::models::leadership::RoomFence;
    use uuid::Uuid;

    /// Bridge-only requests use the dedicated secret rather than the JWT signing key.
    #[test]
    fn bridge_secret_is_independent() {
        let auth =
            AgentAuthManager::new("agent-jwt-secret".to_string(), "bridge-secret".to_string());
        assert_eq!(auth.bridge_secret(), "bridge-secret");
        assert_ne!(auth.bridge_secret(), "agent-jwt-secret");
    }

    /// Managed bridge tokens preserve their current room-leadership capability.
    #[test]
    fn managed_agent_token_carries_room_fence() {
        let human_secret = "human-jwt-secret";
        let standalone_agent_secret = "standalone-agent-jwt-secret";
        let fence = RoomFence {
            server_id: Uuid::new_v4(),
            epoch: 7,
            lease_id: Uuid::new_v4(),
        };
        let managed_key = jwt::derive_managed_agent_jwt_secret(human_secret, &fence)
            .expect("derive managed signing key");
        let auth = AgentAuthManager::new(managed_key, "bridge-secret".to_string())
            .with_managed_fence(Some(fence));

        assert_eq!(auth.managed_fence(), Some(&fence));

        let token = auth
            .issue_token(Uuid::new_v4(), "managed-agent")
            .expect("managed token");
        let claims = jwt::validate_access_token(&token, human_secret, standalone_agent_secret)
            .expect("Rift validates a lease-derived agent token");

        assert_eq!(claims.token_kind, TokenKind::Agent);
        assert_eq!(claims.managed_fence, Some(fence));
    }

    /// Bridge tokens validate only under the dedicated agent signing authority.
    #[test]
    fn issued_token_uses_agent_key_and_class() {
        let auth =
            AgentAuthManager::new("agent-jwt-secret".to_string(), "bridge-secret".to_string());
        let token = auth
            .issue_token(Uuid::new_v4(), "managed-agent")
            .expect("agent token");

        let claims = jwt::validate_access_token(&token, "human-jwt-secret", "agent-jwt-secret")
            .expect("dedicated agent key validates bridge token");
        assert_eq!(claims.token_kind, TokenKind::Agent);
        assert!(jwt::validate_access_token(
            &token,
            "human-jwt-secret",
            "different-agent-jwt-secret",
        )
        .is_err());
    }
}
