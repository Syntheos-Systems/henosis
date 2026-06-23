use chrono::{Duration, Utc};
use jsonwebtoken::{decode, encode, DecodingKey, EncodingKey, Header, Validation};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::AppError;

#[derive(Debug, Serialize, Deserialize)]
pub struct Claims {
    pub sub: Uuid, // user id
    pub username: String,
    pub exp: i64,
    pub iat: i64,
}

pub fn create_access_token(
    user_id: Uuid,
    username: &str,
    secret: &str,
) -> Result<String, AppError> {
    let now = Utc::now();
    let claims = Claims {
        sub: user_id,
        username: username.to_string(),
        iat: now.timestamp(),
        exp: (now + Duration::hours(24)).timestamp(),
    };
    encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(secret.as_bytes()),
    )
    .map_err(|e| AppError::Internal(format!("JWT encode error: {e}")))
}

pub fn create_refresh_token() -> String {
    use rand::Rng;
    let mut rng = rand::rng();
    let bytes: [u8; 32] = rng.random();
    hex::encode(&bytes)
}

// We don't have hex crate, use a manual approach
mod hex {
    pub fn encode(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }
}

pub fn validate_token(token: &str, secret: &str) -> Result<Claims, AppError> {
    let data = decode::<Claims>(
        token,
        &DecodingKey::from_secret(secret.as_bytes()),
        &Validation::default(),
    )
    .map_err(|_| AppError::Unauthorized)?;
    Ok(data.claims)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Round-trips HS256 issuance through validation. Also exercises jsonwebtoken 10's runtime
    // CryptoProvider auto-registration via the `rust_crypto` backend feature -- if no provider were
    // selected, decode() would error at the verifier-factory call.
    #[test]
    fn access_token_round_trips() {
        let uid = Uuid::new_v4();
        let token = create_access_token(uid, "alice", "shared-secret").expect("encode");
        let claims = validate_token(&token, "shared-secret").expect("decode");
        assert_eq!(claims.sub, uid);
        assert_eq!(claims.username, "alice");
    }

    // A token signed with one secret must not validate under another.
    #[test]
    fn wrong_secret_is_rejected() {
        let token = create_access_token(Uuid::new_v4(), "bob", "secret-a").expect("encode");
        assert!(validate_token(&token, "secret-b").is_err());
    }
}
