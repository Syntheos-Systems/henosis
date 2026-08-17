use chrono::{Duration, Utc};
use hmac::{Hmac, Mac};
use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Header, Validation, decode, encode};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use uuid::Uuid;

use crate::error::AppError;
use crate::models::leadership::RoomFence;

/// Domain separator for managed-agent signing-key derivation.
const MANAGED_AGENT_KEY_DOMAIN: &[u8] = b"henosis:rift:managed-agent-jwt:v1\0";

/// Domain separator for managed bridge-only route bearer derivation.
const MANAGED_BRIDGE_ROUTE_KEY_DOMAIN: &[u8] = b"henosis:rift:managed-bridge-route:v1\0";

/// Cryptographic authority class carried by every Rift access token.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TokenKind {
    /// Interactive human session signed only by the human-session key.
    Human,
    /// Bridge-controlled agent session signed only by the agent-session key.
    Agent,
}

/// Claims carried by Rift access tokens.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Claims {
    /// Authenticated Rift user identifier.
    pub sub: Uuid,
    /// Authenticated Rift username.
    pub username: String,
    /// Required session authority class, cryptographically bound to its signing key.
    pub token_kind: TokenKind,
    /// Unix expiration timestamp.
    pub exp: i64,
    /// Unix issuance timestamp.
    pub iat: i64,
    /// Optional managed-room leadership capability, absent from human-issued tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub managed_fence: Option<RoomFence>,
}

/// Creates a signed 24-hour access token for a Rift user.
pub fn create_access_token(
    user_id: Uuid,
    username: &str,
    secret: &str,
) -> Result<String, AppError> {
    let now = Utc::now();
    let claims = Claims {
        sub: user_id,
        username: username.to_string(),
        token_kind: TokenKind::Human,
        iat: now.timestamp(),
        exp: (now + Duration::hours(24)).timestamp(),
        managed_fence: None,
    };
    encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(secret.as_bytes()),
    )
    .map_err(|e| AppError::Internal(format!("JWT encode error: {e}")))
}

/// Generates a cryptographically random refresh token.
pub fn create_refresh_token() -> String {
    use rand::Rng;
    let mut rng = rand::rng();
    let bytes: [u8; 32] = rng.random();
    hex::encode(&bytes)
}

// We don't have hex crate, use a manual approach
mod hex {
    /// Encodes bytes as lowercase hexadecimal text.
    pub fn encode(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }
}

/// Validate an access token against separate human and agent signing authorities.
pub fn validate_access_token(
    token: &str,
    human_secret: &str,
    agent_secret: &str,
) -> Result<Claims, AppError> {
    if human_secret == agent_secret {
        return Err(AppError::Unauthorized);
    }

    if let Ok(claims) = decode_with_secret(token, human_secret) {
        return (claims.token_kind == TokenKind::Human)
            .then_some(claims)
            .ok_or(AppError::Unauthorized);
    }
    if let Ok(claims) = decode_with_secret(token, agent_secret)
        && claims.token_kind == TokenKind::Agent
        && claims.managed_fence.is_none()
    {
        return Ok(claims);
    }

    // The unverified fence selects a candidate key only. No claim is trusted
    // until the complete token verifies under the key derived for that exact
    // fence, and the verified claims repeat the same capability.
    let unverified = jsonwebtoken::dangerous::insecure_decode::<Claims>(token)
        .map_err(|_| AppError::Unauthorized)?
        .claims;
    let fence = unverified
        .managed_fence
        .filter(|_| unverified.token_kind == TokenKind::Agent)
        .ok_or(AppError::Unauthorized)?;
    let managed_secret = derive_managed_agent_jwt_secret(human_secret, &fence)?;
    let claims = decode_with_secret(token, &managed_secret).map_err(|_| AppError::Unauthorized)?;
    (claims.token_kind == TokenKind::Agent && claims.managed_fence == Some(fence))
        .then_some(claims)
        .ok_or(AppError::Unauthorized)
}

/// Derive the signing key for one exact managed-room leadership generation.
///
/// The human-session secret remains server-side and acts as an existing
/// deployment root. HMAC domain separation prevents a leaked derived key from
/// signing human sessions or deriving another room or leadership generation.
pub fn derive_managed_agent_jwt_secret(
    root_secret: &str,
    fence: &RoomFence,
) -> Result<String, AppError> {
    derive_managed_room_secret(
        root_secret,
        fence,
        MANAGED_AGENT_KEY_DOMAIN,
        "managed agent key derivation failed",
    )
}

/// Derive a bridge-only bearer for one exact managed-room leadership generation.
///
/// This purpose-separated child key never grants human-session or agent-JWT
/// authority and cannot authenticate another room or successor generation.
pub fn derive_managed_bridge_route_secret(
    root_secret: &str,
    fence: &RoomFence,
) -> Result<String, AppError> {
    derive_managed_room_secret(
        root_secret,
        fence,
        MANAGED_BRIDGE_ROUTE_KEY_DOMAIN,
        "managed bridge route key derivation failed",
    )
}

/// Derive one fixed-width room-generation child key under an explicit purpose domain.
fn derive_managed_room_secret(
    root_secret: &str,
    fence: &RoomFence,
    domain: &[u8],
    failure: &'static str,
) -> Result<String, AppError> {
    let mut mac = Hmac::<Sha256>::new_from_slice(root_secret.as_bytes())
        .map_err(|_| AppError::Internal(failure.to_string()))?;
    mac.update(domain);
    mac.update(fence.server_id.as_bytes());
    mac.update(&fence.epoch.to_be_bytes());
    mac.update(fence.lease_id.as_bytes());
    Ok(hex::encode(&mac.finalize().into_bytes()))
}

/// Decode one HS256 token without assigning authority to the decoded claim class.
fn decode_with_secret(token: &str, secret: &str) -> Result<Claims, jsonwebtoken::errors::Error> {
    let validation = Validation::new(Algorithm::HS256);
    decode::<Claims>(
        token,
        &DecodingKey::from_secret(secret.as_bytes()),
        &validation,
    )
    .map(|data| data.claims)
}

/// Unit tests for access-token signing and validation.
#[cfg(test)]
mod tests {
    use super::*;

    /// Sign explicit claims with one test key so key/class binding can be attacked directly.
    fn sign_claims(claims: &Claims, secret: &str) -> String {
        encode(
            &Header::default(),
            claims,
            &EncodingKey::from_secret(secret.as_bytes()),
        )
        .expect("test claims must encode")
    }

    // Round-trips HS256 issuance through validation. Also exercises jsonwebtoken 10's runtime
    // CryptoProvider auto-registration via the `rust_crypto` backend feature -- if no provider were
    // selected, decode() would error at the verifier-factory call.
    #[test]
    fn access_token_round_trips() {
        let uid = Uuid::new_v4();
        let token = create_access_token(uid, "alice", "human-secret").expect("encode");
        let claims = validate_access_token(&token, "human-secret", "agent-secret")
            .expect("decode human token");
        assert_eq!(claims.sub, uid);
        assert_eq!(claims.username, "alice");
        assert_eq!(claims.token_kind, TokenKind::Human);
        assert_eq!(claims.managed_fence, None);
    }

    /// A token signed with neither configured authority must fail authentication.
    #[test]
    fn wrong_secret_is_rejected() {
        let token = create_access_token(Uuid::new_v4(), "bob", "secret-a").expect("encode");
        assert!(validate_access_token(&token, "secret-b", "secret-c").is_err());
    }

    /// A valid signature is insufficient when its key authority and declared class disagree.
    #[test]
    fn wrong_class_for_signing_key_is_rejected() {
        let now = Utc::now();
        let claims = Claims {
            sub: Uuid::new_v4(),
            username: "forged".to_string(),
            token_kind: TokenKind::Agent,
            iat: now.timestamp(),
            exp: (now + Duration::hours(1)).timestamp(),
            managed_fence: None,
        };
        let human_signed_agent = sign_claims(&claims, "human-secret");
        assert!(
            validate_access_token(&human_signed_agent, "human-secret", "agent-secret").is_err()
        );

        let claims = Claims {
            token_kind: TokenKind::Human,
            ..claims
        };
        let agent_signed_human = sign_claims(&claims, "agent-secret");
        assert!(
            validate_access_token(&agent_signed_human, "human-secret", "agent-secret").is_err()
        );
    }

    /// Fenced agent tokens validate only under the key derived for their exact leadership lease.
    #[test]
    fn managed_agent_tokens_require_the_exact_fence_derived_key() {
        let human_secret = "human-secret-root";
        let standalone_agent_secret = "standalone-agent-secret";
        let fence = RoomFence {
            server_id: Uuid::new_v4(),
            epoch: 7,
            lease_id: Uuid::new_v4(),
        };
        let now = Utc::now();
        let claims = Claims {
            sub: Uuid::new_v4(),
            username: "managed-agent".to_string(),
            token_kind: TokenKind::Agent,
            iat: now.timestamp(),
            exp: (now + Duration::hours(1)).timestamp(),
            managed_fence: Some(fence),
        };
        let derived = derive_managed_agent_jwt_secret(human_secret, &fence)
            .expect("derive managed agent key");
        let valid = sign_claims(&claims, &derived);
        assert!(
            validate_access_token(&valid, human_secret, standalone_agent_secret).is_ok(),
            "the exact per-leadership key must validate"
        );

        let globally_signed = sign_claims(&claims, standalone_agent_secret);
        assert!(
            validate_access_token(&globally_signed, human_secret, standalone_agent_secret).is_err(),
            "the standalone agent key must not authorize managed tokens"
        );

        let other_fence = RoomFence {
            lease_id: Uuid::new_v4(),
            ..fence
        };
        let other_claims = Claims {
            managed_fence: Some(other_fence),
            ..claims
        };
        let altered_fence = sign_claims(&other_claims, &derived);
        assert!(
            validate_access_token(&altered_fence, human_secret, standalone_agent_secret).is_err(),
            "an untrusted fence claim must select a different verification key"
        );
    }

    /// Fence derivation changes when any authority-bearing lease coordinate changes.
    #[test]
    fn managed_agent_key_derivation_binds_every_fence_coordinate() {
        let root = "human-secret-root";
        let fence = RoomFence {
            server_id: Uuid::new_v4(),
            epoch: 4,
            lease_id: Uuid::new_v4(),
        };
        let base = derive_managed_agent_jwt_secret(root, &fence).expect("derive base key");
        let changed_server = derive_managed_agent_jwt_secret(
            root,
            &RoomFence {
                server_id: Uuid::new_v4(),
                ..fence
            },
        )
        .expect("derive server-bound key");
        let changed_epoch = derive_managed_agent_jwt_secret(
            root,
            &RoomFence {
                epoch: fence.epoch + 1,
                ..fence
            },
        )
        .expect("derive epoch-bound key");
        let changed_lease = derive_managed_agent_jwt_secret(
            root,
            &RoomFence {
                lease_id: Uuid::new_v4(),
                ..fence
            },
        )
        .expect("derive lease-bound key");

        assert_ne!(base, changed_server);
        assert_ne!(base, changed_epoch);
        assert_ne!(base, changed_lease);
    }

    /// Managed private routes use a fence-bound key outside the agent JWT domain.
    #[test]
    fn managed_bridge_route_key_is_domain_separated_and_fence_scoped() {
        let root = "human-secret-root";
        let fence = RoomFence {
            server_id: Uuid::new_v4(),
            epoch: 4,
            lease_id: Uuid::new_v4(),
        };
        let agent = derive_managed_agent_jwt_secret(root, &fence).expect("derive agent key");
        let bridge =
            derive_managed_bridge_route_secret(root, &fence).expect("derive bridge route key");
        let successor = derive_managed_bridge_route_secret(
            root,
            &RoomFence {
                epoch: fence.epoch + 1,
                lease_id: Uuid::new_v4(),
                ..fence
            },
        )
        .expect("derive successor bridge route key");

        assert_ne!(bridge, agent);
        assert_ne!(bridge, successor);
    }

    /// Legacy tokens without an explicit authority class cannot cross the upgraded boundary.
    #[test]
    fn untyped_legacy_token_is_rejected() {
        /// Pre-split token shape that intentionally omits the required token kind.
        #[derive(Serialize)]
        struct LegacyClaims {
            /// Legacy subject identifier.
            sub: Uuid,
            /// Legacy caller-controlled username.
            username: String,
            /// Legacy issuance timestamp.
            iat: i64,
            /// Legacy expiration timestamp.
            exp: i64,
        }

        let now = Utc::now();
        let token = encode(
            &Header::default(),
            &LegacyClaims {
                sub: Uuid::new_v4(),
                username: "legacy".to_string(),
                iat: now.timestamp(),
                exp: (now + Duration::hours(1)).timestamp(),
            },
            &EncodingKey::from_secret(b"human-secret"),
        )
        .expect("legacy test token must encode");
        assert!(validate_access_token(&token, "human-secret", "agent-secret").is_err());
    }
}
