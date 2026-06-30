//! JWT session claims, signing, decoding, and operator error types.
//!
//! This module owns the HS256 token lifecycle for the operator API.
//! [`OperatorClaims`] carries the principal, org, and role encoded in each
//! session JWT. [`sign`] mints a token; [`decode`] verifies and extracts it.
//! [`OperatorError`] is the single error type for the operator surface, with
//! an [`axum::response::IntoResponse`] impl that maps variants to HTTP status codes.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Header, Validation};
use serde::{Deserialize, Serialize};

/// JWT claims embedded in every operator session token.
///
/// `sub` is the principal UUID; `org` is the tenant UUID; `role` is the
/// string representation of the operator's [`henosis_plutus::Role`].
/// `iat` and `exp` are Unix timestamps (seconds). The constructor takes an
/// explicit `iat` so callers -- including tests -- control the clock.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OperatorClaims {
    /// The authenticated principal's UUID (the `sub` claim in RFC 7519 terms).
    pub sub: String,
    /// The tenant (org) UUID this session is scoped to.
    pub org: String,
    /// The operator's role string (e.g. `"owner"`, `"admin"`, `"viewer"`).
    pub role: String,
    /// Issued-at time as a Unix timestamp (seconds). Injected by the caller.
    pub iat: i64,
    /// Expiry time as a Unix timestamp (seconds). Computed as `iat + ttl_secs`.
    pub exp: i64,
}

impl OperatorClaims {
    /// Construct a new set of claims with an explicitly injected issue time.
    ///
    /// `iat` is passed in rather than derived from the system clock so that
    /// the constructor is deterministic and usable in unit tests without a
    /// time dependency. `exp` is computed as `iat + ttl_secs`.
    pub fn new(sub: &str, org: &str, role: &str, iat: i64, ttl_secs: i64) -> Self {
        Self {
            sub: sub.to_owned(),
            org: org.to_owned(),
            role: role.to_owned(),
            iat,
            exp: iat + ttl_secs,
        }
    }
}

/// Structured errors for the operator API surface.
///
/// Variants map to HTTP status codes via the [`IntoResponse`] implementation:
/// - [`Auth`](OperatorError::Auth) -- 401 Unauthorized
/// - [`Forbidden`](OperatorError::Forbidden) -- 403 Forbidden
/// - [`Backend`](OperatorError::Backend) -- 500 Internal Server Error
#[derive(Debug, thiserror::Error)]
pub enum OperatorError {
    /// Authentication failed (bad credentials, invalid or expired JWT).
    /// Always maps to HTTP 401. Message is safe to surface to the caller.
    #[error("authentication failed: {0}")]
    Auth(String),

    /// The authenticated principal lacks the required permission.
    /// Maps to HTTP 403.
    #[error("forbidden: {0}")]
    Forbidden(String),

    /// An internal backend error (store failure, encoding failure, etc.).
    /// Maps to HTTP 500. Details are logged server-side; the response body
    /// carries only the message string (no stack trace).
    #[error("backend error: {0}")]
    Backend(String),
}

impl IntoResponse for OperatorError {
    /// Convert an [`OperatorError`] into an HTTP response with the appropriate
    /// status code and the error message as the plain-text body.
    fn into_response(self) -> Response {
        let (status, msg) = match &self {
            OperatorError::Auth(m) => (StatusCode::UNAUTHORIZED, m.clone()),
            OperatorError::Forbidden(m) => (StatusCode::FORBIDDEN, m.clone()),
            OperatorError::Backend(m) => (StatusCode::INTERNAL_SERVER_ERROR, m.clone()),
        };
        (status, msg).into_response()
    }
}

/// Sign a set of operator claims into a compact JWS string using HS256.
///
/// The resulting token is suitable for transmission in an `Authorization: Bearer`
/// header. The `secret` must be at least 32 bytes; shorter keys are accepted by
/// the library but weakly recommended against -- the operator surface enforces
/// >= 32 bytes at boot time.
pub fn sign(claims: &OperatorClaims, secret: &[u8]) -> Result<String, OperatorError> {
    jsonwebtoken::encode(
        &Header::new(Algorithm::HS256),
        claims,
        &EncodingKey::from_secret(secret),
    )
    .map_err(|e| OperatorError::Backend(format!("JWT sign: {e}")))
}

/// Decode and verify a compact JWS string, returning the embedded claims.
///
/// Verification checks:
/// - HS256 algorithm (rejects any other algorithm via `Validation`).
/// - Signature integrity (wrong `secret` -> `Auth` error).
/// - Expiry (`exp` claim; jsonwebtoken rejects expired tokens by default).
///
/// Any verification failure is mapped to [`OperatorError::Auth`] so callers
/// receive a uniform 401 for all invalid-token conditions.
pub fn decode(token: &str, secret: &[u8]) -> Result<OperatorClaims, OperatorError> {
    let validation = Validation::new(Algorithm::HS256);
    jsonwebtoken::decode::<OperatorClaims>(token, &DecodingKey::from_secret(secret), &validation)
        .map(|data| data.claims)
        .map_err(|e| OperatorError::Auth(format!("JWT decode: {e}")))
}

#[cfg(test)]
/// Unit tests for JWT claim round-trip and rejection behaviour.
mod tests {
    use super::*;

    /// A signed claim round-trips through encode/decode and a wrong secret is rejected.
    ///
    /// `iat` is set to a far-future value so the token does not expire during
    /// the test regardless of when it runs.
    #[test]
    fn jwt_round_trip_and_reject() {
        let secret = b"0123456789abcdef0123456789abcdef";
        // Use a far-future iat so exp = iat + 3600 is always in the future.
        let iat: i64 = 9_000_000_000;
        let claims = OperatorClaims::new(
            "11111111-1111-4111-8111-111111111111",
            "22222222-2222-4222-8222-222222222222",
            "owner",
            iat,
            3600,
        );
        let tok = sign(&claims, secret).expect("sign");
        let back = decode(&tok, secret).expect("decode");
        assert_eq!(back.sub, claims.sub);
        assert_eq!(back.org, claims.org);
        assert_eq!(back.role, claims.role);
        assert_eq!(back.exp, claims.exp);
        // A different secret must not verify.
        assert!(decode(&tok, b"wrong-secret-wrong-secret-wrong!").is_err());
    }
}
