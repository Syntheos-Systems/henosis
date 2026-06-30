//! JWT session claims, signing, decoding, and operator error types.
//!
//! This module owns the HS256 token lifecycle for the operator API.
//! [`OperatorClaims`] carries the principal, org, and role encoded in each
//! session JWT. [`sign`] mints a token; [`decode`] verifies and extracts it.
//! [`OperatorError`] is the single error type for the operator surface, with
//! an [`axum::response::IntoResponse`] impl that maps variants to HTTP status codes.

use axum::extract::{Json, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use henosis_plutus::{OrgStatus, PolicyBackend, Role};
use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Header, Validation};
use serde::{Deserialize, Serialize};
use syntheos_contracts::{PrincipalId, TenantId};
use syntheos_identity::SqliteDirectory;

use super::OperatorState;

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

/// The resolved outcome of a successful operator login.
///
/// Produced by [`resolve_login`] after verifying credentials, resolving the
/// principal's org membership, confirming the org is active, and looking up the
/// RBAC role. Consumed by the `login` handler to mint a JWT.
#[derive(Debug)]
pub struct SessionGrant {
    /// The authenticated principal.
    pub principal: PrincipalId,
    /// The tenant (org) the principal belongs to.
    pub org: TenantId,
    /// The principal's RBAC role within that org.
    pub role: Role,
}

/// Request body for `POST /api/auth/login`.
#[derive(Debug, Deserialize)]
pub struct LoginBody {
    /// The operator's email address. Compared case-insensitively against stored accounts.
    pub email: String,
    /// The plaintext password to verify against the stored Argon2id hash.
    pub password: String,
}

/// Response body for a successful login or token refresh.
#[derive(Debug, Serialize)]
pub struct LoginResponse {
    /// The signed HS256 JWT for use as a Bearer token.
    pub token: String,
    /// The authenticated principal's UUID string.
    pub principal_id: String,
    /// The tenant (org) UUID string the session is scoped to.
    pub org: String,
    /// The principal's role string within the org.
    pub role: String,
    /// Unix timestamp (seconds) at which the token expires.
    pub expires_at: i64,
}

/// Core login logic as a pure, PG-free-testable function.
///
/// Steps:
/// 1. Verify credentials via [`SqliteDirectory::verify_login`]; uniform 401 on failure.
/// 2. Resolve the principal's org via [`PolicyBackend::tenant_for_principal`]; 403 on None.
/// 3. Confirm org status is [`OrgStatus::Active`]; 403 otherwise.
/// 4. Resolve the RBAC role via [`PolicyBackend::member_role`]; 403 on None.
///
/// The `policy` parameter accepts `&dyn PolicyBackend` so tests can pass a
/// [`henosis_plutus::MockPolicyBackend`] without a live Postgres connection; the
/// `login` handler passes `&*state.plutus` which coerces to `&dyn PolicyBackend`.
pub async fn resolve_login(
    accounts: &SqliteDirectory,
    policy: &dyn PolicyBackend,
    email: &str,
    password: &str,
) -> Result<SessionGrant, OperatorError> {
    // Step 1: verify credentials. Uniform "invalid credentials" message -- no user enumeration.
    let principal = accounts
        .verify_login(email, password)
        .map_err(|e| OperatorError::Backend(e.to_string()))?
        .ok_or_else(|| OperatorError::Auth("invalid credentials".into()))?;

    // Step 2: resolve the tenant from the principal's membership row.
    let org = policy
        .tenant_for_principal(principal)
        .await
        .map_err(|e| OperatorError::Backend(e.to_string()))?
        .ok_or_else(|| OperatorError::Forbidden("principal has no org membership".into()))?;

    // Step 3: the org must be active (suspended or deleted orgs cannot log in).
    let status = policy
        .org_status(org)
        .await
        .map_err(|e| OperatorError::Backend(e.to_string()))?
        .ok_or_else(|| OperatorError::Forbidden("org not found".into()))?;
    if status != OrgStatus::Active {
        return Err(OperatorError::Forbidden(format!(
            "org is {status} -- only active orgs may log in"
        )));
    }

    // Step 4: resolve the role within the org.
    let role = policy
        .member_role(org, principal)
        .await
        .map_err(|e| OperatorError::Backend(e.to_string()))?
        .ok_or_else(|| OperatorError::Forbidden("no role in org".into()))?;

    Ok(SessionGrant { principal, org, role })
}

/// Extract a Bearer token from the `Authorization` header.
///
/// Returns the token string (without the `Bearer ` prefix) or an [`OperatorError::Auth`]
/// when the header is missing or does not start with `Bearer `.
fn bearer_token(headers: &HeaderMap) -> Result<&str, OperatorError> {
    let auth = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| OperatorError::Auth("missing Authorization header".into()))?;
    auth.strip_prefix("Bearer ")
        .ok_or_else(|| OperatorError::Auth("expected Bearer token in Authorization header".into()))
}

/// Handle `POST /api/auth/login`: verify credentials and issue a 24-hour JWT.
///
/// Delegates credential verification and org/role resolution to [`resolve_login`],
/// then mints an [`OperatorClaims`] with the current wall-clock time as `iat` and
/// a 24-hour TTL. Returns a [`LoginResponse`] with the signed token and metadata.
pub async fn login(
    State(state): State<OperatorState>,
    Json(body): Json<LoginBody>,
) -> Result<Json<LoginResponse>, OperatorError> {
    let grant =
        resolve_login(&state.accounts, &*state.plutus, &body.email, &body.password).await?;

    // Wall-clock iat is acceptable here; the unit tests for resolve_login are clock-free.
    let iat = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    let claims = OperatorClaims::new(
        &grant.principal.to_string(),
        &grant.org.to_string(),
        grant.role.as_str(),
        iat,
        86_400, // 24 hours
    );
    let token = sign(&claims, &state.jwt_secret)?;
    Ok(Json(LoginResponse {
        token,
        principal_id: grant.principal.to_string(),
        org: grant.org.to_string(),
        role: grant.role.to_string(),
        expires_at: claims.exp,
    }))
}

/// Handle `GET /api/auth/session`: decode the Bearer token and return the embedded claims.
///
/// Returns 401 when the token is missing, malformed, signed with the wrong key, or expired.
pub async fn session(
    State(state): State<OperatorState>,
    headers: HeaderMap,
) -> Result<Json<OperatorClaims>, OperatorError> {
    let token = bearer_token(&headers)?;
    let claims = decode(token, &state.jwt_secret)?;
    Ok(Json(claims))
}

/// Handle `POST /api/auth/refresh`: re-sign a valid token for another 24 hours.
///
/// Decodes the existing Bearer token (401 on any failure), then issues a fresh
/// [`OperatorClaims`] with the same `sub`/`org`/`role` and a new `iat`/`exp`.
/// Expired tokens cannot be refreshed -- the caller must log in again.
pub async fn refresh(
    State(state): State<OperatorState>,
    headers: HeaderMap,
) -> Result<Json<LoginResponse>, OperatorError> {
    let token = bearer_token(&headers)?;
    let claims = decode(token, &state.jwt_secret)?;
    let iat = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    let new_claims = OperatorClaims::new(&claims.sub, &claims.org, &claims.role, iat, 86_400);
    let new_token = sign(&new_claims, &state.jwt_secret)?;
    Ok(Json(LoginResponse {
        token: new_token,
        principal_id: claims.sub,
        org: claims.org,
        role: claims.role,
        expires_at: new_claims.exp,
    }))
}

/// Handle `POST /api/auth/logout`: invalidate the session on the client side.
///
/// The operator API uses stateless JWTs so the server has no token store to purge.
/// Returns 200 to signal that the client should discard the token.
pub async fn logout() -> StatusCode {
    StatusCode::OK
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

    /// `resolve_login` returns a `SessionGrant` with correct org and role for a valid member.
    ///
    /// Uses an in-memory `SqliteDirectory` (no disk I/O) and `MockPolicyBackend::allow_all()`
    /// (no Postgres connection) so this test is fully self-contained.
    #[tokio::test]
    async fn resolve_login_grants_session_for_member() {
        use henosis_plutus::MockPolicyBackend;
        use syntheos_contracts::PrincipalId;
        use syntheos_identity::SqliteDirectory;

        let accounts = SqliteDirectory::open_in_memory().expect("open in-memory directory");
        let principal = PrincipalId::new();
        accounts
            .create_account("operator@example.com", "hunter2", principal)
            .expect("create account");

        let policy = MockPolicyBackend::allow_all();
        // Capture the expected tenant before consuming the mock.
        let expected_tenant = policy.tenant.expect("allow_all sets a tenant");

        let grant = resolve_login(&accounts, &policy, "operator@example.com", "hunter2")
            .await
            .expect("resolve_login must succeed for valid credentials + active org member");

        assert_eq!(grant.principal, principal, "principal must match the enrolled account");
        assert_eq!(grant.org, expected_tenant, "org must match the mock tenant");
        assert_eq!(
            grant.role.as_str(),
            "member",
            "role must match MockPolicyBackend::allow_all() which returns Member"
        );
    }

    /// `resolve_login` returns `Auth` for a wrong password (no user enumeration in message).
    #[tokio::test]
    async fn resolve_login_rejects_wrong_password() {
        use henosis_plutus::MockPolicyBackend;
        use syntheos_contracts::PrincipalId;
        use syntheos_identity::SqliteDirectory;

        let accounts = SqliteDirectory::open_in_memory().expect("open");
        let principal = PrincipalId::new();
        accounts
            .create_account("op@example.com", "correct-password", principal)
            .expect("create");

        let policy = MockPolicyBackend::allow_all();
        let result = resolve_login(&accounts, &policy, "op@example.com", "wrong-password").await;

        assert!(
            matches!(result, Err(OperatorError::Auth(_))),
            "wrong password must produce Auth(401), got: {result:?}"
        );
    }

    /// `resolve_login` returns `Forbidden` for a principal with no org membership.
    ///
    /// Uses `MockPolicyBackend::deny_no_member()` which sets `tenant = None` so
    /// `tenant_for_principal` returns `None` -- the second step in the login flow.
    #[tokio::test]
    async fn resolve_login_forbids_principal_without_membership() {
        use henosis_plutus::MockPolicyBackend;
        use syntheos_contracts::PrincipalId;
        use syntheos_identity::SqliteDirectory;

        let accounts = SqliteDirectory::open_in_memory().expect("open");
        let principal = PrincipalId::new();
        accounts
            .create_account("op@example.com", "secret", principal)
            .expect("create");

        // deny_no_member: tenant = None, role = None -- no membership row.
        let policy = MockPolicyBackend::deny_no_member();
        let result = resolve_login(&accounts, &policy, "op@example.com", "secret").await;

        assert!(
            matches!(result, Err(OperatorError::Forbidden(_))),
            "missing membership must produce Forbidden(403), got: {result:?}"
        );
    }
}
