//! Public human authentication routes with bounded password-work admission.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};

use axum::{Json, extract::State, http::StatusCode};
use chrono::{Duration, Utc};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::auth::jwt;
use crate::auth::middleware::AuthUser;
use crate::config::Config;
use crate::db;
use crate::error::AppError;
use crate::models::user::{AuthResponse, LoginRequest, PublicUser, RegisterRequest};

/// Minimum encoded byte length accepted for human usernames.
const MIN_USERNAME_BYTES: usize = 3;
/// Maximum encoded byte length accepted for human usernames.
const MAX_USERNAME_BYTES: usize = 32;
/// Minimum encoded byte length accepted for human passwords.
const MIN_PASSWORD_BYTES: usize = 8;
/// Maximum encoded byte length accepted for human passwords.
const MAX_PASSWORD_BYTES: usize = 128;
/// Maximum encoded byte length accepted for public email addresses.
const MAX_EMAIL_BYTES: usize = 254;
/// Maximum Unicode scalar count accepted for optional display names.
const MAX_DISPLAY_NAME_CHARS: usize = 64;
/// Maximum encoded byte length possible for an accepted display name.
const MAX_DISPLAY_NAME_BYTES: usize = MAX_DISPLAY_NAME_CHARS * 4;
/// Exact encoded byte length of refresh tokens issued by Rift.
const REFRESH_TOKEN_BYTES: usize = 64;
/// Maximum number of Argon2 operations this process may run or queue at once.
const MAX_CONCURRENT_PASSWORD_JOBS: usize = 4;
/// Maximum number of login requests admitted from lookup through credential verification.
const MAX_CONCURRENT_LOGIN_REQUESTS: usize = 4;
/// Maximum number of registration Argon2 jobs admitted concurrently.
const MAX_CONCURRENT_REGISTRATION_JOBS: usize = 1;
/// Maximum failed credential attempts allowed per normalized username and window.
const LOGIN_RATE_LIMIT_ATTEMPTS: u32 = 10;
/// Fixed duration in seconds of one per-username login attempt window.
const LOGIN_RATE_LIMIT_WINDOW_SECS: i64 = 5 * 60;
/// Maximum number of privacy-preserving login keys retained in process memory.
const LOGIN_RATE_LIMIT_MAX_KEYS: usize = 4_096;
/// Fixed non-secret Argon2id hash used to equalize missing-account login work.
const DUMMY_PASSWORD_HASH: &str = "$argon2id$v=19$m=19456,t=2,p=1$aGVub3Npcy1hdXRoLWR1bW15LXYx$C/CKAoZmFHEali7vpLAX2z+aXmRa6HA0hy2V16TwVBM";

/// Process-wide admission budget for memory-hard public password operations.
static PASSWORD_WORK_BUDGET: LazyLock<Arc<Semaphore>> =
    LazyLock::new(|| Arc::new(Semaphore::new(MAX_CONCURRENT_PASSWORD_JOBS)));
/// Process-wide admission budget bounding login database and password work together.
static LOGIN_REQUEST_BUDGET: LazyLock<Arc<Semaphore>> =
    LazyLock::new(|| Arc::new(Semaphore::new(MAX_CONCURRENT_LOGIN_REQUESTS)));
/// Process-wide sub-budget that prevents registrations from consuming every password worker.
static REGISTRATION_WORK_BUDGET: LazyLock<Arc<Semaphore>> =
    LazyLock::new(|| Arc::new(Semaphore::new(MAX_CONCURRENT_REGISTRATION_JOBS)));
/// Process-wide bounded per-username login attempt ledger.
static LOGIN_RATE_LIMITER: LazyLock<Mutex<LoginRateLimiter>> =
    LazyLock::new(|| Mutex::new(LoginRateLimiter::default()));
/// Monotonic process-local epoch used so wall-clock adjustments cannot extend or reset windows.
static LOGIN_RATE_LIMIT_EPOCH: LazyLock<std::time::Instant> =
    LazyLock::new(std::time::Instant::now);

/// Tracks failed-login reservations for one fixed-size normalized username key.
#[derive(Debug, Clone, Copy)]
struct LoginAttempts {
    /// Process-local generation distinguishing replacement entries created in one second.
    generation: u64,
    /// Monotonic elapsed seconds marking the start of the current fixed window.
    window_started_at: i64,
    /// Number of provisionally charged attempts in the current window.
    attempts: u32,
}

/// Identifies a provisional attempt charge that a non-authentication result may refund.
#[derive(Debug)]
struct LoginAttemptReservation {
    /// Fixed-size normalized username digest charged by this reservation.
    key: String,
    /// Entry generation charged provisionally by this reservation.
    generation: u64,
}

/// Refunds one global login reservation unless the handler explicitly settles it.
struct LoginAttemptGuard {
    /// Provisional charge retained until an authentication outcome is classified.
    reservation: Option<LoginAttemptReservation>,
}

/// Makes provisional login accounting safe when a request future is canceled.
impl LoginAttemptGuard {
    /// Wrap one provisional charge in a cancel-safe lifetime guard.
    fn new(reservation: LoginAttemptReservation) -> Self {
        Self {
            reservation: Some(reservation),
        }
    }

    /// Commit invalid credentials or refund every other completed outcome.
    fn settle(&mut self, error: Option<&AppError>) {
        let Some(reservation) = self.reservation.take() else {
            return;
        };
        if !error.is_some_and(failed_credentials_consume_attempt) {
            refund_login_attempt(&reservation);
        }
    }
}

/// Restores provisional capacity when the request exits before classification.
impl Drop for LoginAttemptGuard {
    /// Refund the still-active reservation on cancellation, panic, or early return.
    fn drop(&mut self) {
        if let Some(reservation) = self.reservation.take() {
            refund_login_attempt(&reservation);
        }
    }
}

/// Maintains a bounded and prunable per-username login attempt map.
#[derive(Debug, Default)]
struct LoginRateLimiter {
    /// Attempt state indexed by SHA-256 digests rather than raw usernames.
    attempts: HashMap<String, LoginAttempts>,
    /// Next process-local generation assigned to a new attempt window.
    next_generation: u64,
    /// Earliest monotonic second at which a full-map expiry scan may run again.
    next_prune_at: i64,
}

/// Implements fixed-window login accounting without retaining attacker-controlled identities.
impl LoginRateLimiter {
    /// Reserve one attempt for `username`, returning a refundable charge when permitted.
    fn reserve(&mut self, username: &str, now: i64) -> Option<LoginAttemptReservation> {
        self.prune_if_due(now);

        let key = login_rate_limit_key(username);
        let key_expired = self.attempts.get(&key).is_some_and(|entry| {
            now.saturating_sub(entry.window_started_at) >= LOGIN_RATE_LIMIT_WINDOW_SECS
        });
        if key_expired {
            self.attempts.remove(&key);
        }
        if !self.attempts.contains_key(&key) && self.attempts.len() >= LOGIN_RATE_LIMIT_MAX_KEYS {
            return None;
        }
        if !self.attempts.contains_key(&key) {
            let generation = self.next_generation;
            self.next_generation = self.next_generation.wrapping_add(1);
            self.attempts.insert(
                key.clone(),
                LoginAttempts {
                    generation,
                    window_started_at: now,
                    attempts: 0,
                },
            );
        }
        let entry = self
            .attempts
            .get_mut(&key)
            .expect("login attempt entry was inserted before mutation");
        if entry.attempts >= LOGIN_RATE_LIMIT_ATTEMPTS {
            return None;
        }
        entry.attempts += 1;
        Some(LoginAttemptReservation {
            key,
            generation: entry.generation,
        })
    }

    /// Refund one provisional charge when authentication succeeded or did not complete.
    fn refund(&mut self, reservation: &LoginAttemptReservation) {
        let remove = match self.attempts.get_mut(&reservation.key) {
            Some(entry) if entry.generation == reservation.generation => {
                entry.attempts = entry.attempts.saturating_sub(1);
                entry.attempts == 0
            }
            _ => false,
        };
        if remove {
            self.attempts.remove(&reservation.key);
        }
    }

    /// Remove entries whose fixed windows have elapsed.
    fn prune(&mut self, now: i64) {
        self.attempts.retain(|_, entry| {
            now.saturating_sub(entry.window_started_at) < LOGIN_RATE_LIMIT_WINDOW_SECS
        });
    }

    /// Scan for expired entries at most once per fixed window under request pressure.
    fn prune_if_due(&mut self, now: i64) {
        if now < self.next_prune_at {
            return;
        }
        self.prune(now);
        self.next_prune_at = now.saturating_add(LOGIN_RATE_LIMIT_WINDOW_SECS);
    }
}

/// POST /api/auth/register
pub async fn register(
    State(pool): State<PgPool>,
    State(config): State<Config>,
    Json(req): Json<RegisterRequest>,
) -> Result<Json<AuthResponse>, AppError> {
    // Validate every attacker-controlled field before database or Argon2 work.
    let username = validate_username(&req.username)?;
    validate_password(&req.password)?;
    validate_human_email(&req.email)?;
    let email = req.email.trim();
    validate_display_name(req.display_name.as_deref())?;
    let registration_permit =
        reserve_registration_work_with_budget(Arc::clone(&REGISTRATION_WORK_BUDGET))?;

    // Check uniqueness
    if db::get_user_by_username(&pool, username).await?.is_some() {
        return Err(AppError::Conflict("Username already taken".into()));
    }
    if db::get_user_by_email(&pool, email).await?.is_some() {
        return Err(AppError::Conflict("Email already registered".into()));
    }

    // Hash password
    let (password_hash, _registration_permit) =
        hash_registration_password_bounded(req.password, registration_permit).await?;

    // Create user
    let user = db::create_user(
        &pool,
        username,
        email,
        &password_hash,
        req.display_name.as_deref(),
    )
    .await?;

    // Generate tokens
    let token = jwt::create_access_token(user.id, &user.username, &config.jwt_secret)?;
    let refresh = jwt::create_refresh_token();
    let refresh_hash = sha256_hex(&refresh);
    let expires = Utc::now() + Duration::days(30);
    db::store_refresh_token(&pool, user.id, &refresh_hash, expires).await?;

    Ok(Json(AuthResponse {
        token,
        refresh_token: refresh,
        user: PublicUser::from(user),
    }))
}

/// POST /api/auth/login
pub async fn login(
    State(pool): State<PgPool>,
    State(config): State<Config>,
    Json(req): Json<LoginRequest>,
) -> Result<Json<AuthResponse>, AppError> {
    // Bound credentials before allowing the request to reach PostgreSQL or Argon2.
    let username = validate_username(&req.username)?.to_owned();
    validate_password(&req.password)?;

    let mut attempt_guard = reserve_guarded_login_attempt(&username, login_rate_limit_now())
        .ok_or_else(|| AppError::Coded {
            status: StatusCode::TOO_MANY_REQUESTS,
            code: "login_rate_limited",
            message: "Too many login attempts; retry later".to_string(),
        })?;
    let _login_permit = reserve_login_work_with_budget(Arc::clone(&LOGIN_REQUEST_BUDGET))?;
    let result = authenticate_login(&pool, &config, &username, req.password).await;
    attempt_guard.settle(result.as_ref().err());
    result
}

/// Request body containing a refresh token to rotate.
#[derive(Deserialize)]
pub struct RefreshRequest {
    /// Opaque one-time token previously issued by Rift.
    pub refresh_token: String,
}

/// POST /api/auth/refresh
pub async fn refresh(
    State(pool): State<PgPool>,
    State(config): State<Config>,
    Json(req): Json<RefreshRequest>,
) -> Result<Json<AuthResponse>, AppError> {
    validate_refresh_token(&req.refresh_token)?;
    let hash = sha256_hex(&req.refresh_token);
    let user_id = db::consume_refresh_token(&pool, &hash)
        .await?
        .ok_or(AppError::Unauthorized)?;

    let user = db::get_user_by_id(&pool, user_id)
        .await?
        .ok_or(AppError::Unauthorized)?;
    require_human_login(user.is_agent)?;

    let token = jwt::create_access_token(user.id, &user.username, &config.jwt_secret)?;
    let new_refresh = jwt::create_refresh_token();
    let new_hash = sha256_hex(&new_refresh);
    let expires = Utc::now() + Duration::days(30);
    db::store_refresh_token(&pool, user.id, &new_hash, expires).await?;

    Ok(Json(AuthResponse {
        token,
        refresh_token: new_refresh,
        user: PublicUser::from(user),
    }))
}

/// POST /api/auth/logout
pub async fn logout(
    State(pool): State<PgPool>,
    auth: AuthUser,
) -> Result<Json<serde_json::Value>, AppError> {
    db::delete_user_refresh_tokens(&pool, auth.user_id).await?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

// ── Helpers ──

/// Normalize and validate a public username before database access.
fn validate_username(username: &str) -> Result<&str, AppError> {
    if username.len() > MAX_USERNAME_BYTES {
        return Err(AppError::BadRequest("Username must be 3-32 bytes".into()));
    }
    let username = username.trim();
    if !(MIN_USERNAME_BYTES..=MAX_USERNAME_BYTES).contains(&username.len()) {
        return Err(AppError::BadRequest("Username must be 3-32 bytes".into()));
    }
    Ok(username)
}

/// Derive a fixed-size, case-insensitive limiter key without retaining raw usernames.
fn login_rate_limit_key(username: &str) -> String {
    let normalized = username.trim().to_ascii_lowercase();
    sha256_hex(&normalized)
}

/// Reserve one bounded login attempt without holding the limiter lock during verification.
fn reserve_login_attempt(username: &str, now: i64) -> Option<LoginAttemptReservation> {
    let mut limiter = LOGIN_RATE_LIMITER
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    limiter.reserve(username, now)
}

/// Reserve one cancel-safe global login attempt for an in-flight request.
fn reserve_guarded_login_attempt(username: &str, now: i64) -> Option<LoginAttemptGuard> {
    reserve_login_attempt(username, now).map(LoginAttemptGuard::new)
}

/// Return monotonic elapsed seconds for process-local login windows.
fn login_rate_limit_now() -> i64 {
    i64::try_from(LOGIN_RATE_LIMIT_EPOCH.elapsed().as_secs()).unwrap_or(i64::MAX)
}

/// Refund a provisional attempt after success or a non-authentication failure.
fn refund_login_attempt(reservation: &LoginAttemptReservation) {
    let mut limiter = LOGIN_RATE_LIMITER
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    limiter.refund(reservation);
}

/// Return whether an authentication error represents completed invalid credentials.
fn failed_credentials_consume_attempt(error: &AppError) -> bool {
    matches!(error, AppError::Unauthorized)
}

/// Verify one reserved login and mint credentials without owning rate-limit accounting.
async fn authenticate_login(
    pool: &PgPool,
    config: &Config,
    username: &str,
    password: String,
) -> Result<Json<AuthResponse>, AppError> {
    let Some(mut user) = db::get_user_by_username(pool, username).await? else {
        return match verify_unknown_password(password).await {
            Ok(()) => Err(AppError::Unauthorized),
            Err(error) => Err(error),
        };
    };

    let password_hash = std::mem::take(&mut user.password_hash);
    verify_password_bounded(password, password_hash).await?;
    require_human_login(user.is_agent)?;

    let token = jwt::create_access_token(user.id, &user.username, &config.jwt_secret)?;
    let refresh = jwt::create_refresh_token();
    let refresh_hash = sha256_hex(&refresh);
    let expires = Utc::now() + Duration::days(30);
    db::store_refresh_token(pool, user.id, &refresh_hash, expires).await?;

    Ok(Json(AuthResponse {
        token,
        refresh_token: refresh,
        user: PublicUser::from(user),
    }))
}

/// Validate a public password before database or memory-hard work.
fn validate_password(password: &str) -> Result<(), AppError> {
    if !(MIN_PASSWORD_BYTES..=MAX_PASSWORD_BYTES).contains(&password.len()) {
        return Err(AppError::BadRequest("Password must be 8-128 bytes".into()));
    }
    Ok(())
}

/// Validate an optional display name against its database and allocation bounds.
fn validate_display_name(display_name: Option<&str>) -> Result<(), AppError> {
    if let Some(display_name) = display_name
        && (display_name.len() > MAX_DISPLAY_NAME_BYTES
            || display_name.chars().count() > MAX_DISPLAY_NAME_CHARS)
    {
        return Err(AppError::BadRequest(
            "Display name must be at most 64 characters".into(),
        ));
    }
    Ok(())
}

/// Validate a refresh token's exact issued representation before database access.
fn validate_refresh_token(refresh_token: &str) -> Result<(), AppError> {
    let is_lower_hex = refresh_token
        .bytes()
        .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'));
    if refresh_token.len() != REFRESH_TOKEN_BYTES || !is_lower_hex {
        return Err(AppError::BadRequest("Invalid refresh token".into()));
    }
    Ok(())
}

/// Hash a password on a bounded Tokio blocking worker.
pub(crate) async fn hash_password_bounded(password: String) -> Result<String, AppError> {
    run_password_job(move || hash_password(&password)).await
}

/// Hash a registration password and return its route-wide admission permit to the caller.
async fn hash_registration_password_bounded(
    password: String,
    registration_permit: OwnedSemaphorePermit,
) -> Result<(String, OwnedSemaphorePermit), AppError> {
    run_registration_password_job_with_budget(
        registration_permit,
        Arc::clone(&PASSWORD_WORK_BUDGET),
        move || hash_password(&password),
    )
    .await
}

/// Verify a password on a bounded Tokio blocking worker.
pub(crate) async fn verify_password_bounded(
    password: String,
    password_hash: String,
) -> Result<(), AppError> {
    run_password_job(move || verify_password(&password, &password_hash)).await
}

/// Spend bounded password work for a missing account without revealing its absence.
async fn verify_unknown_password(password: String) -> Result<(), AppError> {
    verify_unknown_password_with_budget(password, Arc::clone(&PASSWORD_WORK_BUDGET)).await
}

/// Verify a missing account against the fixed dummy hash under a supplied work budget.
async fn verify_unknown_password_with_budget(
    password: String,
    budget: Arc<Semaphore>,
) -> Result<(), AppError> {
    match run_password_job_with_budget(budget, move || {
        verify_password(&password, DUMMY_PASSWORD_HASH)
    })
    .await
    {
        Ok(()) | Err(AppError::Unauthorized) => Err(AppError::Unauthorized),
        Err(error) => Err(error),
    }
}

/// Run one public password operation under the process-wide admission budget.
async fn run_password_job<T, F>(job: F) -> Result<T, AppError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, AppError> + Send + 'static,
{
    run_password_job_with_budget(Arc::clone(&PASSWORD_WORK_BUDGET), job).await
}

/// Move an owned admission permit into a blocking job so cancellation cannot free it early.
async fn run_password_job_with_budget<T, F>(budget: Arc<Semaphore>, job: F) -> Result<T, AppError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, AppError> + Send + 'static,
{
    let permit = budget.try_acquire_owned().map_err(|_| AppError::Coded {
        status: StatusCode::TOO_MANY_REQUESTS,
        code: "authentication_busy",
        message: "Authentication is busy; retry shortly".to_string(),
    })?;

    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        job()
    })
    .await
    .map_err(|error| AppError::Internal(format!("Password worker failed: {error}")))?
}

/// Reserve one registration slot before any database or password work begins.
fn reserve_registration_work_with_budget(
    registration_budget: Arc<Semaphore>,
) -> Result<OwnedSemaphorePermit, AppError> {
    registration_budget
        .try_acquire_owned()
        .map_err(|_| AppError::Coded {
            status: StatusCode::TOO_MANY_REQUESTS,
            code: "registration_busy",
            message: "Registration is busy; retry shortly".to_string(),
        })
}

/// Reserve one login slot before its database lookup or password work begins.
fn reserve_login_work_with_budget(
    login_budget: Arc<Semaphore>,
) -> Result<OwnedSemaphorePermit, AppError> {
    login_budget
        .try_acquire_owned()
        .map_err(|_| AppError::Coded {
            status: StatusCode::TOO_MANY_REQUESTS,
            code: "authentication_busy",
            message: "Authentication is busy; retry shortly".to_string(),
        })
}

/// Run one registration password job while retaining its route permit after worker completion.
async fn run_registration_password_job_with_budget<T, F>(
    registration_permit: OwnedSemaphorePermit,
    password_budget: Arc<Semaphore>,
    job: F,
) -> Result<(T, OwnedSemaphorePermit), AppError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, AppError> + Send + 'static,
{
    let password_permit = password_budget
        .try_acquire_owned()
        .map_err(|_| AppError::Coded {
            status: StatusCode::TOO_MANY_REQUESTS,
            code: "authentication_busy",
            message: "Authentication is busy; retry shortly".to_string(),
        })?;

    tokio::task::spawn_blocking(move || {
        let result = job();
        drop(password_permit);
        result.map(|value| (value, registration_permit))
    })
    .await
    .map_err(|error| AppError::Internal(format!("Password worker failed: {error}")))?
}

/// Hash a password with Argon2 using a fresh random salt.
///
/// Shared with bridge agent provisioning, which hashes a throwaway random
/// password so agent accounts have no usable login.
pub(crate) fn hash_password(password: &str) -> Result<String, AppError> {
    use argon2::{
        Argon2, PasswordHasher,
        password_hash::{SaltString, rand_core::OsRng},
    };
    let salt = SaltString::generate(&mut OsRng);
    let argon2 = Argon2::default();
    argon2
        .hash_password(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| AppError::Internal(format!("Password hash error: {e}")))
}

/// Verifies a password against an Argon2 PHC hash.
fn verify_password(password: &str, hash: &str) -> Result<(), AppError> {
    use argon2::{Argon2, PasswordVerifier, password_hash::PasswordHash};
    let parsed =
        PasswordHash::new(hash).map_err(|e| AppError::Internal(format!("Invalid hash: {e}")))?;
    Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .map_err(|_| AppError::Unauthorized)
}

/// Hashes a refresh token into its lowercase SHA-256 storage representation.
fn sha256_hex(input: &str) -> String {
    let digest = Sha256::digest(input.as_bytes());
    format!("{digest:x}")
}

/// Validate and reserve the public email namespace for human accounts only.
pub(crate) fn validate_human_email(email: &str) -> Result<(), AppError> {
    if email.len() > MAX_EMAIL_BYTES {
        return Err(AppError::BadRequest("Invalid email".into()));
    }
    let email = email.trim();
    let Some((local, domain)) = email.split_once('@') else {
        return Err(AppError::BadRequest("Invalid email".into()));
    };
    if local.is_empty()
        || domain.is_empty()
        || domain.contains('@')
        || email.len() > MAX_EMAIL_BYTES
        || db::is_reserved_agent_email(email)
    {
        return Err(AppError::BadRequest("Invalid email".into()));
    }
    Ok(())
}

/// Refuse password and refresh sessions for bridge-controlled agent identities.
fn require_human_login(is_agent: bool) -> Result<(), AppError> {
    if is_agent {
        Err(AppError::Unauthorized)
    } else {
        Ok(())
    }
}

#[cfg(test)]
/// Tests for the human-account boundary on public authentication routes.
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    use axum::http::StatusCode;
    use tokio::sync::{Semaphore, oneshot};

    use super::*;

    /// Public authentication rejects oversized fields at its pure validation boundary.
    #[test]
    fn public_authentication_fields_have_strict_bounds() {
        assert!(validate_username("abc").is_ok());
        assert!(validate_username(&"u".repeat(32)).is_ok());
        assert!(validate_username("ab").is_err());
        assert!(validate_username(&"u".repeat(33)).is_err());

        assert!(validate_password(&"p".repeat(8)).is_ok());
        assert!(validate_password(&"p".repeat(128)).is_ok());
        assert!(validate_password("short").is_err());
        assert!(validate_password(&"p".repeat(129)).is_err());

        assert!(validate_human_email("a@example.com").is_ok());
        assert!(validate_human_email(&format!("{}@example.com", "e".repeat(242))).is_ok());
        assert!(validate_human_email(&format!("{}@example.com", "e".repeat(243))).is_err());

        assert!(validate_display_name(None).is_ok());
        assert!(validate_display_name(Some(&"d".repeat(64))).is_ok());
        assert!(validate_display_name(Some(&"d".repeat(65))).is_err());
    }

    /// Refresh rotation accepts only the exact lowercase hexadecimal token format Rift issues.
    #[test]
    fn refresh_tokens_are_validated_before_database_access() {
        assert!(validate_refresh_token(&"a".repeat(64)).is_ok());
        assert!(validate_refresh_token(&"a".repeat(63)).is_err());
        assert!(validate_refresh_token(&"a".repeat(65)).is_err());
        assert!(validate_refresh_token(&format!("{}G", "a".repeat(63))).is_err());
    }

    /// Saturated password work fails immediately and never starts another blocking job.
    #[tokio::test]
    async fn saturated_password_budget_rejects_without_running_work() {
        let budget = Arc::new(Semaphore::new(1));
        let _held = budget
            .clone()
            .try_acquire_owned()
            .expect("test must reserve the only password permit");
        let ran = Arc::new(AtomicBool::new(false));
        let ran_in_job = Arc::clone(&ran);

        let error = run_password_job_with_budget(budget, move || {
            ran_in_job.store(true, Ordering::SeqCst);
            Ok(())
        })
        .await
        .expect_err("saturated password work must fail closed");

        assert!(!ran.load(Ordering::SeqCst));
        assert!(matches!(
            error,
            AppError::Coded {
                status: StatusCode::TOO_MANY_REQUESTS,
                code: "authentication_busy",
                ..
            }
        ));
    }

    /// Bounded public helpers retain the normal Argon2 hash and verification contract.
    #[tokio::test]
    async fn bounded_password_helpers_accept_valid_credentials() {
        let password = "correct horse battery staple".to_string();
        let hash = hash_password_bounded(password.clone())
            .await
            .expect("bounded password hashing must succeed");

        verify_password_bounded(password, hash)
            .await
            .expect("bounded password verification must accept the original credential");
    }

    /// Unknown usernames consume the fixed dummy verifier and remain opaque to callers.
    #[tokio::test]
    async fn unknown_login_password_verification_is_opaque_and_bounded() {
        let parsed = argon2::password_hash::PasswordHash::new(DUMMY_PASSWORD_HASH);
        assert!(parsed.is_ok(), "dummy password hash must remain valid PHC");

        let error = verify_unknown_password("attacker supplied password".to_string())
            .await
            .expect_err("unknown usernames must always remain unauthorized");
        assert!(matches!(error, AppError::Unauthorized));

        let matching_error = verify_unknown_password("henosis fixed dummy password v1".to_string())
            .await
            .expect_err("even the dummy credential itself must remain unauthorized");
        assert!(matches!(matching_error, AppError::Unauthorized));

        let saturated = verify_unknown_password_with_budget(
            "attacker supplied password".to_string(),
            Arc::new(Semaphore::new(0)),
        )
        .await;
        assert!(matches!(
            saturated,
            Err(AppError::Coded {
                status: StatusCode::TOO_MANY_REQUESTS,
                code: "authentication_busy",
                ..
            })
        ));
    }

    /// Canceling a request cannot release capacity while its blocking password job still runs.
    #[tokio::test]
    async fn canceled_request_keeps_password_permit_until_job_finishes() {
        let budget = Arc::new(Semaphore::new(1));
        let task_budget = Arc::clone(&budget);
        let (started_tx, started_rx) = oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();

        let task = tokio::spawn(async move {
            run_password_job_with_budget(task_budget, move || {
                started_tx
                    .send(())
                    .expect("request task must observe job startup");
                release_rx
                    .recv_timeout(std::time::Duration::from_secs(2))
                    .expect("test must release the blocking password job");
                Ok(())
            })
            .await
        });

        started_rx
            .await
            .expect("blocking password job must report startup");
        task.abort();
        assert!(
            task.await
                .expect_err("request task must observe cancellation")
                .is_cancelled()
        );
        assert_eq!(budget.available_permits(), 0);

        let saturated = run_password_job_with_budget(Arc::clone(&budget), || Ok(())).await;
        assert!(matches!(
            saturated,
            Err(AppError::Coded {
                status: StatusCode::TOO_MANY_REQUESTS,
                code: "authentication_busy",
                ..
            })
        ));

        release_tx
            .send(())
            .expect("blocking password job must still receive its release");
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while budget.available_permits() == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("password capacity must return when the blocking job ends");
    }

    /// The login limiter enforces one fixed per-username window and prunes expired keys.
    #[test]
    fn login_rate_limiter_enforces_per_username_window_and_prunes() {
        let mut limiter = LoginRateLimiter::default();
        assert_eq!(login_rate_limit_key("Human"), login_rate_limit_key("human"));
        assert_ne!(login_rate_limit_key("human"), "human");
        for _ in 0..LOGIN_RATE_LIMIT_ATTEMPTS {
            assert!(limiter.reserve("human", 100).is_some());
        }
        assert!(limiter.reserve("human", 100).is_none());
        assert!(
            limiter
                .reserve("human", 100 + LOGIN_RATE_LIMIT_WINDOW_SECS)
                .is_some()
        );
        limiter.prune(100 + (2 * LOGIN_RATE_LIMIT_WINDOW_SECS));
        assert!(limiter.attempts.is_empty());
    }

    /// Rotating attacker-controlled usernames cannot consume an unrelated user's attempt budget.
    #[test]
    fn rotating_login_usernames_do_not_lock_out_an_unrelated_user() {
        let mut limiter = LoginRateLimiter::default();
        for index in 0..=300 {
            assert!(limiter.reserve(&format!("attacker-{index}"), 200).is_some());
        }
        assert!(limiter.reserve("human", 200).is_some());
    }

    /// A full limiter map preserves every charged window and rejects an unseen username.
    #[test]
    fn login_rate_limiter_preserves_charged_windows_at_key_capacity() {
        let mut limiter = LoginRateLimiter::default();
        for index in 0..LOGIN_RATE_LIMIT_MAX_KEYS {
            limiter.attempts.insert(
                login_rate_limit_key(&format!("human-{index}")),
                LoginAttempts {
                    generation: index as u64,
                    window_started_at: 200,
                    attempts: 0,
                },
            );
        }
        let protected_key = login_rate_limit_key("human-0");
        limiter
            .attempts
            .get_mut(&protected_key)
            .expect("protected login window must exist")
            .attempts = LOGIN_RATE_LIMIT_ATTEMPTS;

        assert!(limiter.reserve("new-human", 200).is_none());
        assert_eq!(limiter.attempts.len(), LOGIN_RATE_LIMIT_MAX_KEYS);
        assert_eq!(
            limiter
                .attempts
                .get(&protected_key)
                .expect("capacity pressure must not evict a charged identity")
                .attempts,
            LOGIN_RATE_LIMIT_ATTEMPTS
        );
    }

    /// Login admission saturation fails before a request can reach its database lookup.
    #[test]
    fn saturated_login_admission_budget_fails_closed() {
        let login_budget = Arc::new(Semaphore::new(1));
        let _held = login_budget
            .clone()
            .try_acquire_owned()
            .expect("test must reserve the only login slot");

        let error = reserve_login_work_with_budget(login_budget)
            .expect_err("saturated login admission must reject another lookup");
        assert!(matches!(
            error,
            AppError::Coded {
                status: StatusCode::TOO_MANY_REQUESTS,
                code: "authentication_busy",
                ..
            }
        ));
    }

    /// Refunding a non-authentication outcome restores the username's attempt budget.
    #[test]
    fn login_rate_limiter_refunds_non_authentication_outcomes() {
        let mut limiter = LoginRateLimiter::default();
        let reservation = limiter
            .reserve("human", 200)
            .expect("first reservation must succeed");
        limiter.refund(&reservation);
        for _ in 0..LOGIN_RATE_LIMIT_ATTEMPTS {
            assert!(limiter.reserve("human", 200).is_some());
        }
        assert!(limiter.reserve("human", 200).is_none());
    }

    /// Dropping an in-flight login request refunds its provisional identity charge.
    #[test]
    fn dropped_login_attempt_guard_refunds_canceled_request() {
        let username = format!("cancel-safety-{:p}", &*LOGIN_RATE_LIMITER);
        let key = login_rate_limit_key(&username);
        LOGIN_RATE_LIMITER
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .attempts
            .remove(&key);

        let guard = reserve_guarded_login_attempt(&username, login_rate_limit_now())
            .expect("a unique identity must receive a provisional reservation");
        assert!(
            LOGIN_RATE_LIMITER
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .attempts
                .contains_key(&key)
        );
        drop(guard);

        assert!(
            !LOGIN_RATE_LIMITER
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .attempts
                .contains_key(&key),
            "request cancellation must not leave a targeted lockout charge"
        );
    }

    /// A late refund cannot decrement a replacement entry created in the same clock second.
    #[test]
    fn stale_login_reservation_cannot_refund_a_replacement_generation() {
        let mut limiter = LoginRateLimiter::default();
        let stale = limiter
            .reserve("human", 200)
            .expect("initial reservation must succeed");
        limiter.attempts.remove(&stale.key);
        let replacement = limiter
            .reserve("human", 200)
            .expect("replacement reservation must succeed");

        limiter.refund(&stale);

        assert_ne!(stale.generation, replacement.generation);
        assert_eq!(
            limiter
                .attempts
                .get(&replacement.key)
                .expect("replacement entry must remain")
                .attempts,
            1
        );
    }

    /// Only completed invalid credentials consume a per-username attempt.
    #[test]
    fn login_attempt_classification_refunds_capacity_and_backend_failures() {
        assert!(failed_credentials_consume_attempt(&AppError::Unauthorized));
        assert!(!failed_credentials_consume_attempt(&AppError::Internal(
            "database unavailable".to_string()
        )));
        assert!(!failed_credentials_consume_attempt(&AppError::Coded {
            status: StatusCode::TOO_MANY_REQUESTS,
            code: "authentication_busy",
            message: "Authentication is busy; retry shortly".to_string(),
        }));
    }

    /// Registration saturation fails before consuming shared login capacity or starting work.
    #[tokio::test]
    async fn saturated_registration_budget_preserves_login_capacity() {
        let registration_budget = Arc::new(Semaphore::new(0));
        let password_budget = Arc::new(Semaphore::new(4));
        let error = reserve_registration_work_with_budget(registration_budget)
            .expect_err("saturated registration work must fail closed");

        assert_eq!(password_budget.available_permits(), 4);
        assert!(matches!(
            error,
            AppError::Coded {
                status: StatusCode::TOO_MANY_REQUESTS,
                code: "registration_busy",
                ..
            }
        ));

        let registration_budget = Arc::new(Semaphore::new(1));
        let password_budget = Arc::new(Semaphore::new(0));
        let registration_permit =
            reserve_registration_work_with_budget(Arc::clone(&registration_budget))
                .expect("test registration slot must be available");
        let ran = Arc::new(AtomicBool::new(false));
        let ran_in_job = Arc::clone(&ran);
        let error = run_registration_password_job_with_budget(
            registration_permit,
            password_budget,
            move || {
                ran_in_job.store(true, Ordering::SeqCst);
                Ok(())
            },
        )
        .await
        .expect_err("shared password saturation must reject registration");
        assert!(!ran.load(Ordering::SeqCst));
        assert_eq!(registration_budget.available_permits(), 1);
        assert!(matches!(
            error,
            AppError::Coded {
                status: StatusCode::TOO_MANY_REQUESTS,
                code: "authentication_busy",
                ..
            }
        ));

        let registration_budget = Arc::new(Semaphore::new(1));
        let registration_permit =
            reserve_registration_work_with_budget(Arc::clone(&registration_budget))
                .expect("test registration slot must be available");
        let (_, retained_permit) = run_registration_password_job_with_budget(
            registration_permit,
            Arc::new(Semaphore::new(1)),
            || Ok(()),
        )
        .await
        .expect("successful registration work must return its route permit");
        assert_eq!(registration_budget.available_permits(), 0);
        drop(retained_permit);
        assert_eq!(registration_budget.available_permits(), 1);
    }

    /// Public email writes reject every case variant of the bridge-only namespace.
    #[test]
    fn public_email_validation_reserves_the_agent_namespace() {
        assert!(validate_human_email("human@example.com").is_ok());
        assert!(validate_human_email("agent@agent.local").is_err());
        assert!(validate_human_email("agent@AGENT.LOCAL").is_err());
        assert!(validate_human_email(" agent@agent.local ").is_err());
    }

    /// Password and refresh authentication are available only to human accounts.
    #[test]
    fn public_password_sessions_reject_agent_accounts() {
        assert!(require_human_login(false).is_ok());
        assert!(matches!(
            require_human_login(true),
            Err(AppError::Unauthorized)
        ));
    }
}
