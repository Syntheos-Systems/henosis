//! JWT session claims, signing, decoding, and operator error types.
//!
//! This module owns the HS256 token lifecycle for the operator API.
//! [`OperatorClaims`] carries the principal, org, role, and durable session identifier encoded in each
//! session JWT. [`sign`] mints a token; [`decode`] verifies and extracts it.
//! [`OperatorError`] is the single error type for the operator surface, with
//! an [`axum::response::IntoResponse`] impl that maps variants to HTTP status codes.

use std::collections::HashMap;
use std::str::FromStr;
use std::sync::{Arc, Mutex, OnceLock};

use axum::extract::{Json, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use henosis_plutus::{OrgStatus, PolicyBackend, Role};
use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Header, Validation};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use syntheos_contracts::{PrincipalId, TenantId};
use syntheos_identity::{RefreshSessionIssued, SqliteDirectory};
use tokio::sync::Semaphore;
use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};

use super::OperatorState;

/// Limits access-token lifetime so an intercepted bearer credential has a short replay window.
const ACCESS_TOKEN_TTL_SECS: i64 = 15 * 60;

/// Bounds an operator refresh session to thirty days from its issuance or rotation.
const REFRESH_TOKEN_TTL_SECS: i64 = 30 * 24 * 60 * 60;

/// Caps failed and successful login attempts per identity key within one rate-limit window.
const LOGIN_RATE_LIMIT_ATTEMPTS: u32 = 10;

/// Caps all admitted login attempts in one process within one rate-limit window.
const LOGIN_GLOBAL_RATE_LIMIT_ATTEMPTS: u32 = 300;

/// Defines the fixed login rate-limit window in seconds.
const LOGIN_RATE_LIMIT_WINDOW_SECS: i64 = 5 * 60;

/// Caps the process-local login limiter's key count to prevent unbounded memory growth.
const LOGIN_RATE_LIMIT_MAX_KEYS: usize = 4_096;

/// Caps an operator email before normalization, hashing, or database access.
const LOGIN_EMAIL_MAX_BYTES: usize = 254;

/// Caps a login password before cloning it into the blocking verification worker.
const LOGIN_PASSWORD_MAX_BYTES: usize = 1_024;

/// Caps concurrent SQLite lookup and Argon2 verification jobs in this process.
const LOGIN_VERIFY_CONCURRENCY: usize = 4;

/// Stores the bounded process-local login rate limiter without changing `OperatorState` callers.
static LOGIN_RATE_LIMITER: OnceLock<Mutex<LoginRateLimiter>> = OnceLock::new();

/// Stores the process-wide concurrency gate for blocking login verification jobs.
static LOGIN_VERIFY_SEMAPHORE: OnceLock<Arc<Semaphore>> = OnceLock::new();

/// JWT claims embedded in every operator session token.
///
/// `sub` is the principal UUID; `org` is the tenant UUID; `role` is the
/// string representation of the operator's [`henosis_plutus::Role`]; `sid` is
/// the opaque durable refresh-session record identifier.
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
    /// The opaque durable refresh-session identifier bound to this access token.
    pub sid: String,
    /// Issued-at time as a Unix timestamp (seconds). Injected by the caller.
    pub iat: i64,
    /// Expiry time as a Unix timestamp (seconds). Computed as `iat + ttl_secs`.
    pub exp: i64,
}

/// Constructs deterministic operator JWT claims.
impl OperatorClaims {
    /// Construct a new set of claims with an explicitly injected issue time.
    ///
    /// `iat` is passed in rather than derived from the system clock so that
    /// the constructor is deterministic and usable in unit tests without a
    /// time dependency. `exp` is computed as `iat + ttl_secs`.
    pub fn new(sub: &str, org: &str, role: &str, sid: &str, iat: i64, ttl_secs: i64) -> Self {
        Self {
            sub: sub.to_owned(),
            org: org.to_owned(),
            role: role.to_owned(),
            sid: sid.to_owned(),
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
    /// carries only a generic error string.
    #[error("backend error: {0}")]
    Backend(String),
}

/// Maps operator API errors to HTTP responses.
impl IntoResponse for OperatorError {
    /// Convert an [`OperatorError`] into an HTTP response with the appropriate
    /// status code and the error message as the plain-text body.
    fn into_response(self) -> Response {
        let (status, msg) = match &self {
            OperatorError::Auth(m) => (StatusCode::UNAUTHORIZED, m.clone()),
            OperatorError::Forbidden(m) => (StatusCode::FORBIDDEN, m.clone()),
            OperatorError::Backend(m) => {
                tracing::error!(error = %m, "operator authentication backend failure");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal server error".into(),
                )
            }
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
        .map_err(|_| OperatorError::Auth("invalid session token".into()))
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
#[derive(Deserialize, Zeroize, ZeroizeOnDrop)]
pub struct LoginBody {
    /// The operator's email address. Compared case-insensitively against stored accounts.
    pub email: String,
    /// The plaintext password to verify against the stored Argon2id hash.
    pub password: String,
}

/// Response body for a successful login or token refresh.
#[derive(Serialize, Zeroize, ZeroizeOnDrop)]
pub struct LoginResponse {
    /// Compatibility alias for the signed short-lived access token.
    pub token: String,
    /// The signed short-lived HS256 JWT for use as a Bearer token.
    pub access_token: String,
    /// The opaque durable refresh credential, returned only at issue or rotation time.
    pub refresh_token: String,
    /// The authenticated principal's UUID string.
    pub principal_id: String,
    /// The tenant (org) UUID string the session is scoped to.
    pub org: String,
    /// The principal's role string within the org.
    pub role: String,
    /// Unix timestamp (seconds) at which the token expires.
    pub expires_at: i64,
}

/// Request body for refresh and logout operations using an opaque refresh credential.
#[derive(Deserialize, Zeroize, ZeroizeOnDrop)]
#[serde(deny_unknown_fields)]
pub struct RefreshBody {
    /// The opaque refresh credential returned by login or the most recent refresh.
    pub refresh_token: String,
}

/// Tracks attempts for one privacy-preserving login identity key.
#[derive(Debug, Clone, Copy)]
struct LoginAttempts {
    /// Unix timestamp marking the start of the current rate-limit window.
    window_started_at: i64,
    /// Number of attempts consumed inside the current window.
    attempts: u32,
}

/// Maintains a bounded, prunable login rate-limit map.
#[derive(Debug, Default)]
struct LoginRateLimiter {
    /// Per-identity rate-limit state indexed by SHA-256 email digests.
    attempts: HashMap<String, LoginAttempts>,
    /// Process-global admission state that cannot be bypassed with unique email values.
    global: Option<LoginAttempts>,
}

/// Implements bounded and prunable login-attempt accounting.
impl LoginRateLimiter {
    /// Consume one login attempt for `email`, returning whether it remains permitted.
    fn allow(&mut self, email: &str, now: i64) -> bool {
        if !Self::consume_window(&mut self.global, now, LOGIN_GLOBAL_RATE_LIMIT_ATTEMPTS) {
            return false;
        }
        self.prune(now);

        let key = login_rate_limit_key(email);
        if !self.attempts.contains_key(&key) && self.attempts.len() >= LOGIN_RATE_LIMIT_MAX_KEYS {
            return false;
        }
        let entry = self.attempts.entry(key).or_insert(LoginAttempts {
            window_started_at: now,
            attempts: 0,
        });
        if now.saturating_sub(entry.window_started_at) >= LOGIN_RATE_LIMIT_WINDOW_SECS {
            *entry = LoginAttempts {
                window_started_at: now,
                attempts: 0,
            };
        }
        if entry.attempts >= LOGIN_RATE_LIMIT_ATTEMPTS {
            return false;
        }
        entry.attempts += 1;
        true
    }

    /// Consume one attempt from an optional fixed-window counter.
    fn consume_window(window: &mut Option<LoginAttempts>, now: i64, limit: u32) -> bool {
        let entry = window.get_or_insert(LoginAttempts {
            window_started_at: now,
            attempts: 0,
        });
        if now.saturating_sub(entry.window_started_at) >= LOGIN_RATE_LIMIT_WINDOW_SECS {
            *entry = LoginAttempts {
                window_started_at: now,
                attempts: 0,
            };
        }
        if entry.attempts >= limit {
            return false;
        }
        entry.attempts += 1;
        true
    }

    /// Remove entries whose fixed attempt windows have elapsed.
    fn prune(&mut self, now: i64) {
        self.attempts.retain(|_, entry| {
            now.saturating_sub(entry.window_started_at) < LOGIN_RATE_LIMIT_WINDOW_SECS
        });
    }
}

/// Derive a non-reversible, case-insensitive limiter key without retaining raw email addresses.
fn login_rate_limit_key(email: &str) -> String {
    let normalized = email.trim().to_ascii_lowercase();
    let digest = Sha256::digest(normalized.as_bytes());
    hex::encode(digest)
}

/// Return whether attacker-controlled login fields fit their fixed byte budgets.
fn login_inputs_within_bounds(email: &str, password: &str) -> bool {
    email.len() <= LOGIN_EMAIL_MAX_BYTES && password.len() <= LOGIN_PASSWORD_MAX_BYTES
}

/// Return the current Unix timestamp or a safe backend error when the system clock is invalid.
fn unix_timestamp() -> Result<i64, OperatorError> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .map_err(|error| OperatorError::Backend(format!("system clock: {error}")))
}

/// Consume one bounded login attempt from the shared limiter without holding its lock during I/O.
fn allow_login_attempt(email: &str, now: i64) -> bool {
    let limiter = LOGIN_RATE_LIMITER.get_or_init(|| Mutex::new(LoginRateLimiter::default()));
    let mut limiter = limiter.lock().unwrap_or_else(|error| error.into_inner());
    limiter.allow(email, now)
}

/// Verify one credential pair inside the bounded blocking-work pool.
async fn verify_login_bounded(
    accounts: Arc<SqliteDirectory>,
    email: &str,
    password: &str,
) -> Result<Option<PrincipalId>, OperatorError> {
    let semaphore = LOGIN_VERIFY_SEMAPHORE
        .get_or_init(|| Arc::new(Semaphore::new(LOGIN_VERIFY_CONCURRENCY)))
        .clone();
    let permit = semaphore
        .try_acquire_owned()
        .map_err(|_| invalid_credentials())?;
    let email = Zeroizing::new(email.to_owned());
    let password = Zeroizing::new(password.to_owned());
    let result = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        accounts.verify_login(email.as_str(), password.as_str())
    })
    .await
    .map_err(|_| invalid_credentials())?;
    result.map_err(|error| OperatorError::Backend(error.to_string()))
}

/// Construct the uniform authentication failure returned for attacker-controlled login denials.
fn invalid_credentials() -> OperatorError {
    OperatorError::Auth("invalid credentials".into())
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
/// `login` handler clones its shared directory handle and passes its policy backend.
pub async fn resolve_login(
    accounts: Arc<SqliteDirectory>,
    policy: &dyn PolicyBackend,
    email: &str,
    password: &str,
) -> Result<SessionGrant, OperatorError> {
    // Step 1: verify credentials. Uniform "invalid credentials" message -- no user enumeration.
    let principal = verify_login_bounded(accounts, email, password)
        .await?
        .ok_or_else(invalid_credentials)?;

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

    Ok(SessionGrant {
        principal,
        org,
        role,
    })
}

/// Re-resolve a refresh session against current policy membership and role state.
///
/// The durable refresh record supplies the originally granted principal and tenant,
/// but refresh must not trust a stale role or a removed membership. Every denial is
/// deliberately an authentication failure so the refresh endpoint does not reveal
/// whether a credential was valid before policy evaluation.
async fn resolve_refresh_grant(
    policy: &dyn PolicyBackend,
    org: TenantId,
    principal: PrincipalId,
) -> Result<SessionGrant, OperatorError> {
    let current_org = policy
        .tenant_for_principal(principal)
        .await
        .map_err(|error| OperatorError::Backend(error.to_string()))?;
    if current_org != Some(org) {
        return Err(invalid_refresh_token());
    }

    let status = policy
        .org_status(org)
        .await
        .map_err(|error| OperatorError::Backend(error.to_string()))?;
    if status != Some(OrgStatus::Active) {
        return Err(invalid_refresh_token());
    }

    let role = policy
        .member_role(org, principal)
        .await
        .map_err(|error| OperatorError::Backend(error.to_string()))?
        .ok_or_else(invalid_refresh_token)?;

    Ok(SessionGrant {
        principal,
        org,
        role,
    })
}

/// Construct the uniform refresh credential failure returned for every invalid refresh state.
fn invalid_refresh_token() -> OperatorError {
    OperatorError::Auth("invalid refresh token".into())
}

/// Build an access-token response bound to a newly issued durable refresh session.
fn session_response(
    grant: &SessionGrant,
    mut refresh: RefreshSessionIssued,
    jwt_secret: &[u8],
    now: i64,
) -> Result<LoginResponse, OperatorError> {
    let claims = OperatorClaims::new(
        &grant.principal.to_string(),
        &grant.org.to_string(),
        grant.role.as_str(),
        &refresh.metadata.id.to_string(),
        now,
        ACCESS_TOKEN_TTL_SECS,
    );
    let access_token = sign(&claims, jwt_secret)?;
    Ok(LoginResponse {
        token: access_token.clone(),
        access_token,
        refresh_token: std::mem::take(&mut refresh.token),
        principal_id: grant.principal.to_string(),
        org: grant.org.to_string(),
        role: grant.role.to_string(),
        expires_at: claims.exp,
    })
}

/// Extract a Bearer token from the `Authorization` header.
///
/// Returns the token string (without the `Bearer ` prefix) or an [`OperatorError::Auth`]
/// when the header is missing or does not start with `Bearer `.
pub(super) fn bearer_token(headers: &HeaderMap) -> Result<&str, OperatorError> {
    let mut values = headers.get_all(axum::http::header::AUTHORIZATION).iter();
    let value = values
        .next()
        .ok_or_else(|| OperatorError::Auth("missing Authorization header".into()))?;
    if values.next().is_some() {
        return Err(OperatorError::Auth("ambiguous Authorization header".into()));
    }
    let auth = value
        .to_str()
        .map_err(|_| OperatorError::Auth("invalid Authorization header".into()))?;
    auth.strip_prefix("Bearer ")
        .filter(|token| !token.is_empty())
        .ok_or_else(|| OperatorError::Auth("expected Bearer token in Authorization header".into()))
}

/// Handle `POST /api/auth/login`: verify credentials and issue a bounded session pair.
///
/// Delegates credential verification and org/role resolution to [`resolve_login`],
/// then mints an [`OperatorClaims`] with the current wall-clock time as `iat` and
/// a 15-minute TTL. A durable, opaque refresh credential is persisted separately
/// through [`SqliteDirectory`] and returned only in the successful response.
pub async fn login(
    State(state): State<OperatorState>,
    Json(body): Json<LoginBody>,
) -> Result<Json<LoginResponse>, OperatorError> {
    if !login_inputs_within_bounds(&body.email, &body.password) {
        return Err(invalid_credentials());
    }
    let now = unix_timestamp()?;
    if !allow_login_attempt(&body.email, now) {
        return Err(invalid_credentials());
    }
    let grant = resolve_login(
        state.accounts.clone(),
        &*state.plutus,
        &body.email,
        &body.password,
    )
    .await?;
    let refresh = state
        .accounts
        .issue_operator_refresh(
            grant.org,
            grant.principal,
            Some(now + REFRESH_TOKEN_TTL_SECS),
            now,
        )
        .map_err(|error| OperatorError::Backend(error.to_string()))?;
    let refresh_family_id = refresh.metadata.family_id;
    let response = session_response(&grant, refresh, &state.jwt_secret, now);
    if response.is_err() {
        state
            .accounts
            .revoke_operator_refresh_family(grant.org, grant.principal, refresh_family_id, now)
            .map_err(|error| OperatorError::Backend(error.to_string()))?;
    }
    Ok(Json(response?))
}

/// Handle `GET /api/auth/session`: validate the Bearer token against current policy state.
///
/// Returns 401 when the token is missing, malformed, expired, or no longer backed by an active
/// tenant membership. The returned role is refreshed from policy rather than trusted from the JWT.
pub async fn session(
    State(state): State<OperatorState>,
    headers: HeaderMap,
) -> Result<Json<OperatorClaims>, OperatorError> {
    let token = bearer_token(&headers)?;
    let mut claims = decode(token, &state.jwt_secret)?;
    let principal = PrincipalId::from_str(&claims.sub)
        .map_err(|error| OperatorError::Auth(format!("invalid subject in JWT: {error}")))?;
    let org = TenantId::from_str(&claims.org)
        .map_err(|error| OperatorError::Auth(format!("invalid org in JWT: {error}")))?;
    let role = super::rbac::resolve_live_role(&*state.plutus, org, principal).await?;
    claims.role = role.to_string();
    Ok(Json(claims))
}

/// Handle `POST /api/auth/refresh`: atomically rotate a durable refresh credential.
///
/// The JSON request carries the opaque refresh credential, not an access JWT.
/// It is authenticated, checked against current membership and role state, then
/// atomically consumed and replaced by [`SqliteDirectory::rotate_operator_refresh`].
/// Any malformed, expired, revoked, stale, or raced refresh credential returns the
/// same 401 response.
pub async fn refresh(
    State(state): State<OperatorState>,
    Json(body): Json<RefreshBody>,
) -> Result<Json<LoginResponse>, OperatorError> {
    let now = unix_timestamp()?;
    let session = state
        .accounts
        .authenticate_operator_refresh(&body.refresh_token, now)
        .map_err(|error| OperatorError::Backend(error.to_string()))?
        .ok_or_else(invalid_refresh_token)?;
    let grant = match resolve_refresh_grant(&*state.plutus, session.tenant, session.principal).await
    {
        Ok(grant) => grant,
        Err(error @ OperatorError::Auth(_)) => {
            state
                .accounts
                .revoke_operator_refresh_family(
                    session.tenant,
                    session.principal,
                    session.family_id,
                    now,
                )
                .map_err(|revoke_error| OperatorError::Backend(revoke_error.to_string()))?;
            return Err(error);
        }
        Err(error) => return Err(error),
    };
    let refresh = state
        .accounts
        .rotate_operator_refresh(
            grant.org,
            grant.principal,
            &body.refresh_token,
            Some(now + REFRESH_TOKEN_TTL_SECS),
            now,
        )
        .map_err(|error| OperatorError::Backend(error.to_string()))?
        .ok_or_else(invalid_refresh_token)?;
    let refresh_family_id = refresh.metadata.family_id;
    let response = session_response(&grant, refresh, &state.jwt_secret, now);
    if response.is_err() {
        state
            .accounts
            .revoke_operator_refresh_family(grant.org, grant.principal, refresh_family_id, now)
            .map_err(|error| OperatorError::Backend(error.to_string()))?;
    }
    Ok(Json(response?))
}

/// Handle `POST /api/auth/logout`: revoke the durable family identified by a refresh credential.
///
/// Invalid refresh credentials still return success so logout does not disclose
/// refresh-session existence. The current access JWT remains usable only until
/// its short expiry, while the revoked family cannot mint a new refresh credential.
pub async fn logout(
    State(state): State<OperatorState>,
    Json(body): Json<RefreshBody>,
) -> Result<StatusCode, OperatorError> {
    let now = unix_timestamp()?;
    state
        .accounts
        .revoke_operator_refresh_family_by_token(&body.refresh_token, now)
        .map_err(|error| OperatorError::Backend(error.to_string()))?;
    Ok(StatusCode::NO_CONTENT)
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
            "33333333-3333-4333-8333-333333333333",
            iat,
            3600,
        );
        let tok = sign(&claims, secret).expect("sign");
        let back = decode(&tok, secret).expect("decode");
        assert_eq!(back.sub, claims.sub);
        assert_eq!(back.org, claims.org);
        assert_eq!(back.role, claims.role);
        assert_eq!(back.sid, claims.sid);
        assert_eq!(back.exp, claims.exp);
        // A different secret must not verify.
        assert!(decode(&tok, b"wrong-secret-wrong-secret-wrong!").is_err());
    }

    /// Claims minted for an operator access token always use the fifteen-minute lifetime.
    #[test]
    fn access_token_claims_have_short_ttl_and_session_id() {
        let claims = OperatorClaims::new(
            "principal",
            "tenant",
            "member",
            "session",
            1_000,
            ACCESS_TOKEN_TTL_SECS,
        );
        assert_eq!(claims.exp - claims.iat, ACCESS_TOKEN_TTL_SECS);
        assert_eq!(claims.sid, "session");
    }

    /// Bearer parsing rejects absent, empty, and duplicate authorization fields.
    #[test]
    fn bearer_token_requires_one_unambiguous_value() {
        let mut headers = HeaderMap::new();
        assert!(bearer_token(&headers).is_err());
        headers.append(
            axum::http::header::AUTHORIZATION,
            "Bearer valid-token".parse().expect("header"),
        );
        assert_eq!(bearer_token(&headers).expect("token"), "valid-token");
        headers.append(
            axum::http::header::AUTHORIZATION,
            "Bearer second-token".parse().expect("header"),
        );
        assert!(bearer_token(&headers).is_err());

        let mut empty = HeaderMap::new();
        empty.insert(
            axum::http::header::AUTHORIZATION,
            "Bearer ".parse().expect("header"),
        );
        assert!(bearer_token(&empty).is_err());
    }

    /// The login rate limiter prunes expired entries and enforces its per-key fixed window.
    #[test]
    fn login_rate_limiter_enforces_per_key_window_and_prunes() {
        let mut limiter = LoginRateLimiter::default();
        for _ in 0..LOGIN_RATE_LIMIT_ATTEMPTS {
            assert!(limiter.allow("operator@example.com", 100));
        }
        assert!(!limiter.allow("operator@example.com", 100));
        assert!(limiter.allow("operator@example.com", 100 + LOGIN_RATE_LIMIT_WINDOW_SECS));
        limiter.prune(100 + (2 * LOGIN_RATE_LIMIT_WINDOW_SECS));
        assert!(limiter.attempts.is_empty());
    }

    /// The global login budget cannot be bypassed by rotating unique email values.
    #[test]
    fn login_rate_limiter_enforces_global_window() {
        let mut limiter = LoginRateLimiter::default();
        for index in 0..LOGIN_GLOBAL_RATE_LIMIT_ATTEMPTS {
            assert!(limiter.allow(&format!("operator-{index}@example.com"), 200));
        }
        assert!(!limiter.allow("one-too-many@example.com", 200));
        assert!(limiter.allow("new-window@example.com", 200 + LOGIN_RATE_LIMIT_WINDOW_SECS));
    }

    /// A full active identity map rejects a new key rather than evicting and admitting it.
    #[test]
    fn login_rate_limiter_fails_closed_at_key_capacity() {
        let mut limiter = LoginRateLimiter::default();
        for index in 0..LOGIN_RATE_LIMIT_MAX_KEYS {
            limiter.attempts.insert(
                login_rate_limit_key(&format!("operator-{index}@example.com")),
                LoginAttempts {
                    window_started_at: 200,
                    attempts: 0,
                },
            );
        }
        assert!(!limiter.allow("unknown@example.com", 200));
        assert_eq!(limiter.attempts.len(), LOGIN_RATE_LIMIT_MAX_KEYS);
        assert!(limiter.allow("operator-0@example.com", 200));
    }

    /// Login field limits use encoded byte lengths and accept their exact boundaries.
    #[test]
    fn login_input_limits_are_inclusive_and_byte_based() {
        assert!(login_inputs_within_bounds(
            &"e".repeat(LOGIN_EMAIL_MAX_BYTES),
            &"p".repeat(LOGIN_PASSWORD_MAX_BYTES)
        ));
        assert!(!login_inputs_within_bounds(
            &"e".repeat(LOGIN_EMAIL_MAX_BYTES + 1),
            "password"
        ));
        assert!(!login_inputs_within_bounds(
            "operator@example.com",
            &"p".repeat(LOGIN_PASSWORD_MAX_BYTES + 1)
        ));
        assert!(!login_inputs_within_bounds(
            &"é".repeat((LOGIN_EMAIL_MAX_BYTES / 2) + 1),
            "password"
        ));
    }

    /// Refresh policy revalidation accepts the current tenant and role rather than stale claims.
    #[tokio::test]
    async fn resolve_refresh_grant_uses_current_membership_and_role() {
        use henosis_plutus::MockPolicyBackend;

        let policy = MockPolicyBackend::allow_all();
        let org = policy.tenant.expect("allow_all sets a tenant");
        let grant = resolve_refresh_grant(&policy, org, PrincipalId::new())
            .await
            .expect("current active membership must refresh");
        assert_eq!(grant.org, org);
        assert_eq!(grant.role, Role::Member);
    }

    /// Refresh policy revalidation rejects a session whose tenant no longer matches membership.
    #[tokio::test]
    async fn resolve_refresh_grant_rejects_stale_membership() {
        use henosis_plutus::MockPolicyBackend;

        let policy = MockPolicyBackend::allow_all();
        let result = resolve_refresh_grant(&policy, TenantId::new(), PrincipalId::new()).await;
        assert!(matches!(result, Err(OperatorError::Auth(_))));
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

        let accounts =
            Arc::new(SqliteDirectory::open_in_memory().expect("open in-memory directory"));
        let principal = PrincipalId::new();
        accounts
            .create_account("operator@example.com", "hunter2", principal)
            .expect("create account");

        let policy = MockPolicyBackend::allow_all();
        // Capture the expected tenant before consuming the mock.
        let expected_tenant = policy.tenant.expect("allow_all sets a tenant");

        let grant = resolve_login(accounts, &policy, "operator@example.com", "hunter2")
            .await
            .expect("resolve_login must succeed for valid credentials + active org member");

        assert_eq!(
            grant.principal, principal,
            "principal must match the enrolled account"
        );
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

        let accounts = Arc::new(SqliteDirectory::open_in_memory().expect("open"));
        let principal = PrincipalId::new();
        accounts
            .create_account("op@example.com", "correct-password", principal)
            .expect("create");

        let policy = MockPolicyBackend::allow_all();
        let result = resolve_login(accounts, &policy, "op@example.com", "wrong-password").await;

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

        let accounts = Arc::new(SqliteDirectory::open_in_memory().expect("open"));
        let principal = PrincipalId::new();
        accounts
            .create_account("op@example.com", "secret", principal)
            .expect("create");

        // deny_no_member: tenant = None, role = None -- no membership row.
        let policy = MockPolicyBackend::deny_no_member();
        let result = resolve_login(accounts, &policy, "op@example.com", "secret").await;

        assert!(
            matches!(result, Err(OperatorError::Forbidden(_))),
            "missing membership must produce Forbidden(403), got: {result:?}"
        );
    }
}
