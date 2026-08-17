//! Internal bridge endpoints for acknowledging externally-created messages.
//! Secured via a dedicated bridge secret passed as a Bearer token.

use std::collections::HashSet;

use axum::{
    Json,
    extract::{FromRef, FromRequestParts, State},
    http::{HeaderMap, StatusCode, request::Parts},
};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use sqlx::{PgPool, Postgres, Transaction};
use uuid::Uuid;

use crate::config::Config;
use crate::db;
use crate::error::AppError;
use crate::models::agent_control::MAX_AGENT_SEATS;
use crate::models::leadership::RoomFence;

/// HMAC type used to compare bridge credentials through fixed-size tags.
type HmacSha256 = Hmac<Sha256>;

/// Header carrying the positive managed-room leadership generation.
pub const FENCE_EPOCH_HEADER: &str = "x-henosis-fence-epoch";

/// Header carrying the opaque managed-room leadership lease identifier.
pub const FENCE_LEASE_HEADER: &str = "x-henosis-fence-lease";

/// Header carrying the managed room bound to a private-route capability.
pub const FENCE_SERVER_HEADER: &str = "x-henosis-fence-server";

/// Proof that the dedicated bridge bearer was validated before body extraction.
pub struct BridgeAuthorization;

/// Parsed optional leadership capability supplied with a bridge-only request.
pub struct BridgeFenceHeaders(Option<ParsedBridgeFence>);

/// Complete managed-room fence carried by a private request.
#[derive(Clone, Copy)]
struct ParsedBridgeFence {
    /// Room identifier cryptographically bound into the presented bearer.
    server_id: Uuid,
    /// Positive durable generation number.
    epoch: i64,
    /// Opaque durable lease identifier.
    lease_id: Uuid,
}

/// Converts parsed bridge headers into a capability scoped to one route target.
impl BridgeFenceHeaders {
    /// Require any presented capability to name the route's independently resolved server.
    fn for_server(&self, server_id: Uuid) -> Result<Option<RoomFence>, AppError> {
        let Some(fence) = self.0 else {
            return Ok(None);
        };
        if fence.server_id != server_id {
            return Err(AppError::stale_leadership_fence());
        }
        Ok(Some(RoomFence {
            server_id: fence.server_id,
            epoch: fence.epoch,
            lease_id: fence.lease_id,
        }))
    }
}

/// Authenticate bridge-only requests while only request headers are available.
impl<S> FromRequestParts<S> for BridgeAuthorization
where
    S: Send + Sync,
    Config: FromRef<S>,
{
    /// Authentication failures returned before a request body is consumed.
    type Rejection = AppError;

    /// Validate the bridge bearer against configuration derived from router state.
    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let config = Config::from_ref(state);
        if bridge_authorized(&parts.headers, &config) {
            Ok(Self)
        } else {
            Err(AppError::Unauthorized)
        }
    }
}

/// Extract a complete optional leadership capability before any request body is consumed.
impl<S> FromRequestParts<S> for BridgeFenceHeaders
where
    S: Send + Sync,
{
    /// Fence-header failures returned before JSON extraction can allocate a body.
    type Rejection = AppError;

    /// Reject partial, malformed, or duplicated leadership header triples.
    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        Ok(Self(parse_bridge_fence_headers(&parts.headers)?))
    }
}

/// Parse an all-or-nothing triple of optional leadership headers.
fn parse_bridge_fence_headers(headers: &HeaderMap) -> Result<Option<ParsedBridgeFence>, AppError> {
    let servers = headers.get_all(FENCE_SERVER_HEADER);
    let epochs = headers.get_all(FENCE_EPOCH_HEADER);
    let leases = headers.get_all(FENCE_LEASE_HEADER);
    let server_count = servers.iter().count();
    let epoch_count = epochs.iter().count();
    let lease_count = leases.iter().count();
    if server_count == 0 && epoch_count == 0 && lease_count == 0 {
        return Ok(None);
    }
    if server_count != 1 || epoch_count != 1 || lease_count != 1 {
        return Err(AppError::BadRequest(
            "leadership fence headers must be supplied exactly once as a complete triple"
                .to_string(),
        ));
    }
    let server_id = servers
        .iter()
        .next()
        .and_then(|value| value.to_str().ok())
        .and_then(|value| Uuid::parse_str(value).ok())
        .filter(|server_id| !server_id.is_nil())
        .ok_or_else(|| AppError::BadRequest("invalid leadership fence headers".to_string()))?;
    let epoch = epochs
        .iter()
        .next()
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<i64>().ok())
        .filter(|epoch| *epoch > 0)
        .ok_or_else(|| AppError::BadRequest("invalid leadership fence headers".to_string()))?;
    let lease_id = leases
        .iter()
        .next()
        .and_then(|value| value.to_str().ok())
        .and_then(|value| Uuid::parse_str(value).ok())
        .filter(|lease_id| !lease_id.is_nil())
        .ok_or_else(|| AppError::BadRequest("invalid leadership fence headers".to_string()))?;
    Ok(Some(ParsedBridgeFence {
        server_id,
        epoch,
        lease_id,
    }))
}

/// Require a route-targeted fence only when the target room opted into managed fencing.
pub(crate) async fn require_private_route_fence(
    pool: &PgPool,
    server_id: Uuid,
    headers: &BridgeFenceHeaders,
) -> Result<(), AppError> {
    let fence = headers.for_server(server_id)?;
    db::agent_control::require_room_fence(pool, server_id, fence.as_ref())
        .await
        .map_err(|error| map_private_route_fence_error(server_id, error))
}

/// Begin one repeatable-read private-route transaction holding the target fence share lock.
pub(crate) async fn begin_private_route_fence_transaction<'a>(
    pool: &'a PgPool,
    server_id: Uuid,
    headers: &BridgeFenceHeaders,
) -> Result<Transaction<'a, Postgres>, AppError> {
    let fence = headers.for_server(server_id)?;
    let mut transaction = pool.begin().await.map_err(|error| {
        tracing::error!(%server_id, %error, "managed room fence transaction could not begin");
        AppError::leadership_fence_unavailable()
    })?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")
        .execute(&mut *transaction)
        .await
        .map_err(|error| {
            tracing::error!(%server_id, %error, "managed room fence transaction isolation could not be established");
            AppError::leadership_fence_unavailable()
        })?;
    db::agent_control::require_room_fence_locked(&mut transaction, server_id, fence.as_ref())
        .await
        .map_err(|error| map_private_route_fence_error(server_id, error))?;
    Ok(transaction)
}

/// Convert a room-fence authority failure into Rift's stable private-route response.
fn map_private_route_fence_error(
    server_id: Uuid,
    error: db::agent_control::RoomFenceError,
) -> AppError {
    match error {
        db::agent_control::RoomFenceError::Stale => AppError::stale_leadership_fence(),
        db::agent_control::RoomFenceError::Database(error) => {
            tracing::error!(%server_id, %error, "managed room fence lookup failed");
            AppError::leadership_fence_unavailable()
        }
    }
}

/// Run one synchronous private-route effect while the target room fence row stays share-locked.
async fn with_private_route_fence_lock<T, F>(
    pool: &PgPool,
    server_id: Uuid,
    headers: &BridgeFenceHeaders,
    effect: F,
) -> Result<T, AppError>
where
    F: FnOnce() -> T,
{
    let transaction = begin_private_route_fence_transaction(pool, server_id, headers).await?;
    let output = effect();
    if let Err(error) = transaction.commit().await {
        // The in-memory effect already happened while authorization was locked.
        // Returning success avoids encouraging a duplicate notification retry.
        tracing::error!(%server_id, %error, "managed room fence transaction release failed after notify");
    }
    Ok(output)
}

/// Build the canonical account email for an agent username.
pub(crate) fn agent_email(username: &str) -> String {
    format!("{username}{}", db::CLAIMABLE_AGENT_EMAIL_SUFFIX)
}

/// Return whether an existing identity was already created through an agent-only path.
#[cfg(test)]
fn existing_identity_is_provisionable(is_agent: bool) -> bool {
    is_agent
}

/// Check a request's Bearer token against its standalone or managed authority branch.
///
/// Compares in constant time so the secret cannot be recovered a byte at a time
/// by timing repeated requests. Managed requests accept only the child key
/// derived for their exact room generation; no fallback to the standalone
/// bridge secret is permitted when fence headers are present.
pub(crate) fn bridge_authorized(headers: &HeaderMap, config: &Config) -> bool {
    let authorization = headers.get_all(axum::http::header::AUTHORIZATION);
    if authorization.iter().count() != 1 {
        return false;
    }
    let Some(token) = authorization
        .iter()
        .next()
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
    else {
        return false;
    };

    match parse_bridge_fence_headers(headers) {
        Ok(Some(parsed)) => {
            let fence = RoomFence {
                server_id: parsed.server_id,
                epoch: parsed.epoch,
                lease_id: parsed.lease_id,
            };
            let Ok(expected) =
                crate::auth::jwt::derive_managed_bridge_route_secret(&config.jwt_secret, &fence)
            else {
                return false;
            };
            bridge_secret_matches(token, &expected)
        }
        Ok(None) => bridge_secret_matches(token, &config.bridge_secret),
        Err(_) => false,
    }
}

/// Compare two bridge credentials through fixed-size HMAC tags.
fn bridge_secret_matches(presented: &str, expected: &str) -> bool {
    let mut presented_mac =
        HmacSha256::new_from_slice(presented.as_bytes()).expect("HMAC accepts any key length");
    presented_mac.update(b"henosis-rift-bridge-route-secret");
    let presented_tag = presented_mac.finalize().into_bytes();

    let mut expected_mac =
        HmacSha256::new_from_slice(expected.as_bytes()).expect("HMAC accepts any key length");
    expected_mac.update(b"henosis-rift-bridge-route-secret");
    expected_mac.verify_slice(&presented_tag).is_ok()
}

/// Request body for POST /api/bridge/notify.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NotifyRequest {
    /// Channel whose subscribers should receive the notification.
    pub channel_id: Uuid,
    /// Persisted message Rift must load and verify against the channel.
    pub message_id: Uuid,
}

/// One agent the bridge wants present and joined.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProvisionAgent {
    /// Rift username, unique across users.
    pub username: String,
    /// Display name shown in the UI.
    pub display_name: Option<String>,
}

/// Request body for POST /api/bridge/provision.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
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
/// Called by the bridge after its message transaction has already enqueued a durable event.
///
/// The route validates the message-to-channel binding and current managed-room
/// fence, but the supervised outbox dispatcher remains the sole publisher.
pub async fn notify_message(
    _authorization: BridgeAuthorization,
    fence_headers: BridgeFenceHeaders,
    State(pool): State<PgPool>,
    Json(req): Json<NotifyRequest>,
) -> Result<StatusCode, AppError> {
    let channel = db::get_channel_by_id(&pool, req.channel_id)
        .await?
        .ok_or_else(|| AppError::NotFound("channel".to_string()))?;
    // Reject a capability for another room, or a standalone bearer targeting
    // a managed room, before reading any message or attachment data.
    require_private_route_fence(&pool, channel.server_id, &fence_headers).await?;
    // Fetch the full message with author info
    let msg = match db::get_message_by_id(&pool, req.message_id).await {
        Ok(Some(m)) => m,
        Ok(None) => return Ok(StatusCode::NOT_FOUND),
        Err(error) => return Err(error.into()),
    };

    // The message and the broadcast target are two independent caller-supplied
    // fields. Without binding them, this route would relay any message -- including
    // a private DM -- to the subscribers of any other channel.
    if msg.channel_id != req.channel_id {
        return Ok(StatusCode::NOT_FOUND);
    }

    // Hold the authoritative row lock through acknowledgement so a successor
    // fence cannot commit between validation and the successful response.
    with_private_route_fence_lock(&pool, channel.server_id, &fence_headers, || ()).await?;

    Ok(StatusCode::OK)
}

/// POST /api/bridge/provision
///
/// Converges the bridge's configured agent roster: ensures every listed agent
/// exists as an agent user and is a member of the target server. Called on
/// every bridge boot and safe to repeat -- without it the agents exist but are
/// not members, and the gateway refuses their channel subscription, which
/// leaves the room silently deaf.
pub async fn provision_agents(
    _authorization: BridgeAuthorization,
    fence_headers: BridgeFenceHeaders,
    State(pool): State<PgPool>,
    Json(req): Json<ProvisionRequest>,
) -> Result<Json<ProvisionResponse>, AppError> {
    validate_provision_request(&req)?;
    let fence = fence_headers.for_server(req.server_id)?;

    // Fail loudly on a bad server_id. Joining agents to a server that does not
    // exist would otherwise "succeed" and strand the room with no members.
    if db::get_server_by_id(&pool, req.server_id).await?.is_none() {
        return Err(AppError::NotFound(format!(
            "Server {} not found",
            req.server_id
        )));
    }

    require_private_route_fence(&pool, req.server_id, &fence_headers).await?;

    let mut provisioned = Vec::with_capacity(req.agents.len());

    for agent in &req.agents {
        // Agents authenticate only via bridge-minted tokens. Hashing an
        // unguessable random password keeps the password login path
        // fail-closed rather than relying on a sentinel hash value.
        let password_hash = super::auth::hash_password_bounded(random_password()).await?;
        let (user, retyped) = match db::provision_agent_with_fence(
            &pool,
            req.server_id,
            fence.as_ref(),
            &agent.username,
            &agent_email(&agent.username),
            &password_hash,
            agent.display_name.as_deref(),
        )
        .await
        {
            Ok(result) => result,
            Err(db::ProvisionAgentError::HumanUsername) => {
                return Err(AppError::Conflict(format!(
                    "username '{}' belongs to a human account and will not be promoted to an agent",
                    agent.username
                )));
            }
            Err(db::ProvisionAgentError::StaleLeadership) => {
                return Err(AppError::stale_leadership_fence());
            }
            Err(db::ProvisionAgentError::Database(error)) => return Err(error.into()),
        };

        // Converge history on every boot: migration 004 only retypes rows
        // whose author was already flagged is_agent when it ran, while a
        // pre-stamping server build could still have written 'user'. Idempotent.
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

/// Bound one bridge provisioning request to the room roster and database contracts.
fn validate_provision_request(request: &ProvisionRequest) -> Result<(), AppError> {
    if request.agents.len() > MAX_AGENT_SEATS {
        return Err(AppError::BadRequest(format!(
            "a bridge may provision at most {MAX_AGENT_SEATS} agents"
        )));
    }
    let mut usernames = HashSet::with_capacity(request.agents.len());
    for agent in &request.agents {
        if agent.username.trim() != agent.username || !(3..=32).contains(&agent.username.len()) {
            return Err(AppError::BadRequest(
                "agent usernames must contain 3 through 32 bytes without surrounding whitespace"
                    .to_string(),
            ));
        }
        if agent
            .display_name
            .as_deref()
            .is_some_and(|name| name.chars().count() > 64)
        {
            return Err(AppError::BadRequest(
                "agent display names must contain at most 64 characters".to_string(),
            ));
        }
        if !usernames.insert(&agent.username) {
            return Err(AppError::BadRequest(
                "agent usernames must be unique within a provisioning request".to_string(),
            ));
        }
    }
    Ok(())
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
    use axum::response::IntoResponse;
    use sqlx::postgres::PgPoolOptions;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    use std::time::Duration;

    /// A database failure before the notify fence lock prevents acknowledgement.
    #[tokio::test]
    async fn notify_fence_unavailable_fails_before_acknowledgement() {
        let pool = PgPoolOptions::new()
            .acquire_timeout(Duration::from_millis(100))
            .connect_lazy("postgresql://localhost:1/rift_notify_fence_must_not_connect")
            .expect("static unavailable database URL must parse");
        let acknowledged = Arc::new(AtomicBool::new(false));
        let observed = acknowledged.clone();
        let error = with_private_route_fence_lock(
            &pool,
            Uuid::new_v4(),
            &BridgeFenceHeaders(None),
            move || observed.store(true, Ordering::SeqCst),
        )
        .await
        .expect_err("unavailable fence authority must reject notify");
        assert!(!acknowledged.load(Ordering::SeqCst));

        let response = error.into_response();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .expect("coded fence error body must be readable");
        let body: serde_json::Value =
            serde_json::from_slice(&body).expect("coded fence error body must be JSON");
        assert_eq!(body["code"], "leadership_fence_unavailable");
    }

    /// Build a Config carrying just the secret these tests exercise.
    fn config_with_secret(secret: &str) -> Config {
        Config {
            database_url: String::new(),
            jwt_secret: "different-jwt-signing-secret".to_string(),
            agent_jwt_secret: "different-agent-jwt-signing-secret".to_string(),
            bridge_secret: secret.to_string(),
            listen_addr: String::new(),
            bridge_listen_addr: String::new(),
            allow_remote_listen: false,
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

    /// Build a managed private-route request carrying one exact room fence.
    fn headers_with_managed_auth(value: &str, fence: &RoomFence) -> HeaderMap {
        let mut headers = headers_with_auth(value);
        headers.insert(
            "x-henosis-fence-server",
            HeaderValue::from_str(&fence.server_id.to_string()).unwrap(),
        );
        headers.insert(
            FENCE_EPOCH_HEADER,
            HeaderValue::from_str(&fence.epoch.to_string()).unwrap(),
        );
        headers.insert(
            FENCE_LEASE_HEADER,
            HeaderValue::from_str(&fence.lease_id.to_string()).unwrap(),
        );
        headers
    }

    /// Connect to the opt-in PostgreSQL test database for fence serialization checks.
    async fn live_bridge_test_pool() -> Option<PgPool> {
        let Some(database_url) = std::env::var_os("HENOSIS_RIFT_TEST_DATABASE_URL") else {
            eprintln!("skipping live notify fence test: HENOSIS_RIFT_TEST_DATABASE_URL is unset");
            return None;
        };
        let pool = PgPoolOptions::new()
            .max_connections(4)
            .connect(&database_url.to_string_lossy())
            .await
            .expect("test database must be reachable");
        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .expect("test database migrations must apply");
        Some(pool)
    }

    /// Live PostgreSQL proves a successor cannot advance during notify acknowledgement.
    #[tokio::test]
    async fn live_notify_acknowledgement_holds_fence_until_return() {
        let Some(pool) = live_bridge_test_pool().await else {
            return;
        };
        let suffix = Uuid::new_v4().simple().to_string();
        let suffix = &suffix[..12];
        let owner = db::create_user(
            &pool,
            &format!("notify_owner_{suffix}"),
            &format!("notify-owner-{suffix}@example.invalid"),
            "test-hash",
            None,
        )
        .await
        .expect("notify owner must be created");
        let server = db::create_server(&pool, &format!("notify-{suffix}"), None, owner.id)
            .await
            .expect("notify server must be created");
        sqlx::query(
            r#"INSERT INTO bridge_server_state (server_id, fencing_required)
               VALUES ($1, TRUE)"#,
        )
        .bind(server.id)
        .execute(&pool)
        .await
        .expect("managed notify state must be created");
        let current = db::agent_control::acquire_room_fence(&pool, server.id)
            .await
            .expect("notify fence must be acquired");
        let headers = BridgeFenceHeaders(Some(ParsedBridgeFence {
            server_id: current.server_id,
            epoch: current.epoch,
            lease_id: current.lease_id,
        }));
        let database_url = std::env::var("HENOSIS_RIFT_TEST_DATABASE_URL")
            .expect("live notify test database URL must remain available");
        let (start_tx, start_rx) = std::sync::mpsc::channel();
        let (lock_result_tx, lock_result_rx) = std::sync::mpsc::channel();
        let server_id = server.id;
        let successor_thread = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("successor test runtime must build");
            let lock_code = runtime.block_on(async move {
                let successor_pool = PgPoolOptions::new()
                    .max_connections(1)
                    .connect(&database_url)
                    .await
                    .expect("successor test database must be reachable");
                start_rx
                    .recv()
                    .expect("notify effect must start the successor probe");
                let mut connection = successor_pool
                    .acquire()
                    .await
                    .expect("successor test connection must be available");
                sqlx::query("SET lock_timeout = '100ms'")
                    .execute(&mut *connection)
                    .await
                    .expect("successor test connection must accept a bounded lock wait");
                let result = sqlx::query(
                    r#"UPDATE bridge_server_state
                       SET fencing_epoch = fencing_epoch + 1,
                           fencing_lease_id = $2
                       WHERE server_id = $1"#,
                )
                .bind(server_id)
                .bind(Uuid::new_v4())
                .execute(&mut *connection)
                .await;
                result.err().and_then(|error| {
                    error
                        .as_database_error()
                        .and_then(|database_error| database_error.code())
                        .map(|code| code.into_owned())
                })
            });
            lock_result_tx
                .send(lock_code)
                .expect("notify effect must await the successor lock result");
        });
        let effect_ran = Arc::new(AtomicBool::new(false));
        let observed_effect = effect_ran.clone();
        with_private_route_fence_lock(&pool, server.id, &headers, move || {
            start_tx
                .send(())
                .expect("successor probe must still be waiting for notify entry");
            let lock_code = lock_result_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("successor update must resolve through the database lock timeout");
            assert_eq!(lock_code.as_deref(), Some("55P03"));
            observed_effect.store(true, Ordering::SeqCst);
        })
        .await
        .expect("current fence must authorize notify effect");
        successor_thread
            .join()
            .expect("successor lock-probe thread must join");
        let successor_fence = tokio::time::timeout(
            Duration::from_secs(2),
            db::agent_control::acquire_room_fence(&pool, server.id),
        )
        .await
        .expect("successor must advance after notify releases its lock")
        .expect("successor fence acquisition must succeed");
        assert_eq!(successor_fence.epoch, current.epoch + 1);
        assert!(effect_ran.load(Ordering::SeqCst));
    }

    /// Notify acknowledges an already durable event without inserting or publishing another.
    #[tokio::test]
    async fn live_notify_preserves_the_single_durable_message_event() {
        let Some(pool) = live_bridge_test_pool().await else {
            return;
        };
        let suffix = Uuid::new_v4().simple().to_string();
        let suffix = &suffix[..12];
        let owner = db::create_user(
            &pool,
            &format!("notify_once_{suffix}"),
            &format!("notify-once-{suffix}@example.invalid"),
            "test-hash",
            None,
        )
        .await
        .expect("notify test owner must be created");
        let server = db::create_server(&pool, &format!("notify-once-{suffix}"), None, owner.id)
            .await
            .expect("notify test server must be created");
        let channel = db::create_channel(&pool, server.id, "notify", None, "text")
            .await
            .expect("notify test channel must be created");
        let message = db::create_message(&pool, channel.id, owner.id, "once", "user")
            .await
            .expect("notify test message and event must commit");
        let before = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM event_outbox WHERE payload #>> '{data,id}' = $1",
        )
        .bind(message.id.to_string())
        .fetch_one(&pool)
        .await
        .expect("notify test outbox count must be readable");
        assert_eq!(before, 1);

        let status = notify_message(
            BridgeAuthorization,
            BridgeFenceHeaders(None),
            State(pool.clone()),
            Json(NotifyRequest {
                channel_id: channel.id,
                message_id: message.id,
            }),
        )
        .await
        .expect("notify must acknowledge the committed message");
        assert_eq!(status, StatusCode::OK);
        let after = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM event_outbox WHERE payload #>> '{data,id}' = $1",
        )
        .bind(message.id.to_string())
        .fetch_one(&pool)
        .await
        .expect("post-notify outbox count must be readable");
        assert_eq!(after, 1, "notify must not create a second event identity");
    }

    /// Provisioning revalidates a managed fence transactionally after password hashing.
    #[tokio::test]
    async fn live_provision_revalidates_fence_after_password_hashing() {
        let Some(pool) = live_bridge_test_pool().await else {
            return;
        };
        let suffix = Uuid::new_v4().simple().to_string();
        let suffix = &suffix[..12];
        let owner = db::create_user(
            &pool,
            &format!("provision_owner_{suffix}"),
            &format!("provision-owner-{suffix}@example.invalid"),
            "test-hash",
            None,
        )
        .await
        .expect("provision owner must be created");
        let server = db::create_server(&pool, &format!("provision-{suffix}"), None, owner.id)
            .await
            .expect("provision server must be created");
        sqlx::query(
            r#"INSERT INTO bridge_server_state (server_id, fencing_required)
               VALUES ($1, TRUE)"#,
        )
        .bind(server.id)
        .execute(&pool)
        .await
        .expect("managed provision state must be created");
        let stale = db::agent_control::acquire_room_fence(&pool, server.id)
            .await
            .expect("initial provision fence must be acquired");
        let password_hash = super::super::auth::hash_password_bounded(random_password())
            .await
            .expect("agent password preparation must succeed");
        let successor = db::agent_control::acquire_room_fence(&pool, server.id)
            .await
            .expect("successor provision fence must advance");
        assert!(successor.epoch > stale.epoch);
        let username = format!("stale_agent_{suffix}");

        let error = db::provision_agent_with_fence(
            &pool,
            server.id,
            Some(&stale),
            &username,
            &agent_email(&username),
            &password_hash,
            None,
        )
        .await
        .expect_err("stale authority must fail at the mutation transaction");

        assert!(matches!(error, db::ProvisionAgentError::StaleLeadership));
        assert!(
            db::get_user_by_username(&pool, &username)
                .await
                .expect("postcondition lookup must succeed")
                .is_none(),
            "stale provisioning must not create an agent"
        );
    }

    /// Fence headers must arrive as one valid, non-duplicated capability triple.
    #[test]
    fn fence_headers_reject_partial_malformed_and_duplicate_values() {
        let server = "X-Henosis-Fence-Server";
        let epoch = "X-Henosis-Fence-Epoch";
        let lease = "X-Henosis-Fence-Lease";
        let server_id = Uuid::new_v4();
        let lease_id = Uuid::new_v4();

        let mut valid = HeaderMap::new();
        valid.insert(
            server,
            HeaderValue::from_str(&server_id.to_string()).unwrap(),
        );
        valid.insert(epoch, HeaderValue::from_static("7"));
        valid.insert(lease, HeaderValue::from_str(&lease_id.to_string()).unwrap());
        assert!(parse_bridge_fence_headers(&valid).is_ok());

        let mut partial = HeaderMap::new();
        partial.insert(epoch, HeaderValue::from_static("7"));
        assert!(parse_bridge_fence_headers(&partial).is_err());

        let mut missing_server = HeaderMap::new();
        missing_server.insert(epoch, HeaderValue::from_static("7"));
        missing_server.insert(lease, HeaderValue::from_str(&lease_id.to_string()).unwrap());
        assert!(parse_bridge_fence_headers(&missing_server).is_err());

        let mut malformed = HeaderMap::new();
        malformed.insert(
            server,
            HeaderValue::from_str(&server_id.to_string()).unwrap(),
        );
        malformed.insert(epoch, HeaderValue::from_static("not-an-epoch"));
        malformed.insert(lease, HeaderValue::from_str(&lease_id.to_string()).unwrap());
        assert!(parse_bridge_fence_headers(&malformed).is_err());

        let mut duplicate = valid.clone();
        duplicate.append(epoch, HeaderValue::from_static("8"));
        assert!(parse_bridge_fence_headers(&duplicate).is_err());
    }

    /// The matching shared secret is accepted.
    #[test]
    fn correct_secret_is_authorized() {
        let config = config_with_secret("super-secret");
        let headers = headers_with_auth("Bearer super-secret");
        assert!(bridge_authorized(&headers, &config));
    }

    /// Managed private authorization accepts only the exact fence-derived bearer branch.
    #[test]
    fn managed_bridge_bearer_cannot_escape_its_room_generation() {
        let root = "different-jwt-signing-secret";
        let config = config_with_secret("global-standalone-bridge-secret");
        let fence = RoomFence {
            server_id: Uuid::new_v4(),
            epoch: 7,
            lease_id: Uuid::new_v4(),
        };
        let derived = crate::auth::jwt::derive_managed_bridge_route_secret(root, &fence)
            .expect("derive managed bridge route bearer");

        assert!(bridge_authorized(
            &headers_with_managed_auth(&format!("Bearer {derived}"), &fence),
            &config
        ));
        assert!(!bridge_authorized(
            &headers_with_managed_auth("Bearer global-standalone-bridge-secret", &fence),
            &config
        ));
        assert!(!bridge_authorized(
            &headers_with_auth(&format!("Bearer {derived}")),
            &config
        ));

        let other_fence = RoomFence {
            server_id: Uuid::new_v4(),
            ..fence
        };
        assert!(!bridge_authorized(
            &headers_with_managed_auth(&format!("Bearer {derived}"), &other_fence),
            &config
        ));

        let parsed = BridgeFenceHeaders(
            parse_bridge_fence_headers(&headers_with_managed_auth(
                &format!("Bearer {derived}"),
                &fence,
            ))
            .expect("managed fence headers must parse"),
        );
        assert!(parsed.for_server(other_fence.server_id).is_err());
    }

    /// Ambiguous duplicate authorization values fail closed at the private boundary.
    #[test]
    fn duplicate_authorization_values_are_rejected() {
        let config = config_with_secret("super-secret");
        let mut headers = headers_with_auth("Bearer super-secret");
        headers.append("authorization", HeaderValue::from_static("Bearer attacker"));

        assert!(!bridge_authorized(&headers, &config));
    }

    /// A wrong secret of the same length is rejected by fixed-size tag verification.
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

    /// Provisioning payloads are bounded and reject malformed or duplicate identities.
    #[test]
    fn provision_request_shape_is_bounded() {
        let valid = ProvisionRequest {
            server_id: Uuid::new_v4(),
            agents: vec![ProvisionAgent {
                username: "agent-one".to_string(),
                display_name: Some("Agent One".to_string()),
            }],
        };
        assert!(validate_provision_request(&valid).is_ok());

        let oversized = ProvisionRequest {
            server_id: Uuid::new_v4(),
            agents: (0..=MAX_AGENT_SEATS)
                .map(|index| ProvisionAgent {
                    username: format!("agent-{index}"),
                    display_name: None,
                })
                .collect(),
        };
        assert!(validate_provision_request(&oversized).is_err());

        let duplicate = ProvisionRequest {
            server_id: Uuid::new_v4(),
            agents: vec![
                ProvisionAgent {
                    username: "duplicate".to_string(),
                    display_name: None,
                },
                ProvisionAgent {
                    username: "duplicate".to_string(),
                    display_name: None,
                },
            ],
        };
        assert!(validate_provision_request(&duplicate).is_err());

        let invalid_name = ProvisionRequest {
            server_id: Uuid::new_v4(),
            agents: vec![ProvisionAgent {
                username: " agent-one".to_string(),
                display_name: Some("x".repeat(65)),
            }],
        };
        assert!(validate_provision_request(&invalid_name).is_err());
    }

    /// Agent emails are stamped with the agent domain, which is the marker that
    /// is reserved for identities created directly through an agent-only path.
    #[test]
    fn agent_email_uses_the_agent_domain() {
        assert_eq!(agent_email("vera"), "vera@agent.local");
        assert_ne!(agent_email("vera"), "vera@example.com");
    }

    /// Provisioning never promotes an existing human even when its email uses the agent domain.
    #[test]
    fn existing_human_identity_is_never_provisionable() {
        assert!(existing_identity_is_provisionable(true));
        assert!(!existing_identity_is_provisionable(false));
    }

    /// Two provisioning runs must never mint the same agent password.
    #[test]
    fn random_password_is_not_constant() {
        assert_ne!(random_password(), random_password());
        assert_eq!(random_password().len(), 64);
    }
}
