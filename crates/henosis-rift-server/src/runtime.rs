//! Reusable Rift HTTP and WebSocket server lifecycle.

use axum::{
    Router,
    extract::{DefaultBodyLimit, State, ws::WebSocketUpgrade},
    http::{HeaderValue, Method, header},
    response::IntoResponse,
    routing::{delete, get, patch, post, put},
};
use sqlx::postgres::PgPoolOptions;
use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tower::ServiceBuilder;
use tower_http::cors::{AllowOrigin, Any, CorsLayer};
use tower_http::services::ServeDir;
use tower_http::set_header::SetResponseHeaderLayer;
use tower_http::trace::TraceLayer;

use crate::{
    agent_control::ManagedAgentControlRegistry, config, outbox::OutboxDispatcher, routes, ws,
};

use config::{Config, ConfigError};
use routes::upload::PendingUploads;
use ws::gateway::Gateway;

/// Maximum accepted WebSocket message and frame size.
const MAX_WEBSOCKET_BYTES: usize = 64 * 1024;

/// Maximum public WebSocket connections admitted by one Rift process.
const MAX_WEBSOCKET_CONNECTIONS: usize = 512;

/// Maximum public TCP connections accepted by one Rift process.
const MAX_PUBLIC_TCP_CONNECTIONS: usize = 1024;

/// Maximum public TCP connections retained concurrently for one canonical peer IP.
const MAX_PUBLIC_TCP_CONNECTIONS_PER_IP: usize = 64;

/// Maximum loopback bridge TCP connections accepted by one Rift process.
const MAX_BRIDGE_TCP_CONNECTIONS: usize = 128;

/// Maximum time standalone components receive for a coordinated stop.
const RUNTIME_COMPONENT_STOP_TIMEOUT: Duration = Duration::from_secs(5);

/// Maximum JSON body accepted by any public authentication route.
const MAX_AUTH_REQUEST_BYTES: usize = 4 * 1024;

/// Multipart envelope allowance above the accepted file payload.
const MULTIPART_OVERHEAD_BYTES: usize = 64 * 1024;

/// Maximum avatar payload accepted by the avatar route.
const MAX_AVATAR_BYTES: usize = 5 * 1024 * 1024;

/// Maximum JSON payload accepted by any loopback-only bridge route.
const MAX_BRIDGE_REQUEST_BYTES: usize = 64 * 1024;

/// Failures returned while initializing or serving the Rift runtime.
#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    /// Environment-backed Rift configuration was missing or invalid.
    #[error("Rift configuration failed: {0}")]
    Config(#[from] crate::config::ConfigError),
    /// PostgreSQL connection or migration failed.
    #[error("Rift database initialization failed: {0}")]
    Database(#[from] sqlx::Error),
    /// An embedded schema migration could not be applied.
    #[error("Rift schema migration failed: {0}")]
    Migration(#[from] sqlx::migrate::MigrateError),
    /// Filesystem setup, socket binding, or HTTP serving failed.
    #[error("Rift I/O operation failed: {0}")]
    Io(#[from] std::io::Error),
    /// One named Rift listener could not be bound during atomic startup.
    #[error("{component} bind failed: {source}")]
    ListenerBind {
        /// Stable listener component name.
        component: &'static str,
        /// Underlying socket bind failure.
        #[source]
        source: std::io::Error,
    },
    /// One listener stopped before its supervising stop signal resolved.
    #[error("{component} stopped unexpectedly: {detail}")]
    ComponentStopped {
        /// Stable listener component name.
        component: &'static str,
        /// Listener completion or serving error.
        detail: String,
    },
}

/// Shared application state.
#[derive(Clone)]
struct AppState {
    pool: sqlx::PgPool,
    config: Config,
    gateway: Gateway,
    pending_uploads: PendingUploads,
    agent_control: ManagedAgentControlRegistry,
    websocket_admission: WebSocketAdmission,
}

/// Shared fail-fast capacity gate for public WebSocket connection lifetimes.
#[derive(Clone)]
struct WebSocketAdmission {
    permits: Arc<Semaphore>,
}

/// Implements bounded admission for public WebSocket upgrades.
impl WebSocketAdmission {
    /// Construct a gate with one permit per allowed live WebSocket.
    fn new(max_connections: usize) -> Self {
        Self {
            permits: Arc::new(Semaphore::new(max_connections)),
        }
    }

    /// Reserve one live-connection slot without queueing an unauthenticated client.
    fn try_admit(&self) -> Result<OwnedSemaphorePermit, ()> {
        self.permits.clone().try_acquire_owned().map_err(|_| ())
    }
}

/// Listener wrapper that reserves process and optional peer capacity before serving a socket.
pub struct LimitedTcpListener {
    inner: tokio::net::TcpListener,
    permits: Arc<Semaphore>,
    peer_admission: Option<PeerConnectionAdmission>,
}

/// Constructs bounded TCP listeners for public and bridge ingress.
impl LimitedTcpListener {
    /// Wrap an already-bound listener with only a process-wide connection bound.
    fn new(inner: tokio::net::TcpListener, max_connections: usize) -> Self {
        Self {
            inner,
            permits: Arc::new(Semaphore::new(max_connections)),
            peer_admission: None,
        }
    }

    /// Wrap an already-bound listener with process-wide and canonical peer-IP bounds.
    fn with_peer_limit(
        inner: tokio::net::TcpListener,
        max_connections: usize,
        max_connections_per_peer: usize,
    ) -> Self {
        Self {
            inner,
            permits: Arc::new(Semaphore::new(max_connections)),
            peer_admission: Some(PeerConnectionAdmission::new(max_connections_per_peer)),
        }
    }

    /// Apply Rift's source-fair public connection policy to an already-bound socket.
    pub fn for_public(inner: tokio::net::TcpListener) -> Self {
        Self::with_peer_limit(
            inner,
            MAX_PUBLIC_TCP_CONNECTIONS,
            MAX_PUBLIC_TCP_CONNECTIONS_PER_IP,
        )
    }

    /// Apply Rift's process-wide private bridge connection policy to a bound loopback socket.
    pub fn for_bridge(inner: tokio::net::TcpListener) -> Self {
        Self::new(inner, MAX_BRIDGE_TCP_CONNECTIONS)
    }
}

/// Tracks live public connections per canonical network peer.
#[derive(Clone)]
struct PeerConnectionAdmission {
    /// Active connection count keyed by canonical peer address.
    active: Arc<Mutex<HashMap<IpAddr, usize>>>,
    /// Maximum simultaneous public connections retained for one peer.
    max_connections_per_peer: usize,
}

/// Implements fail-fast source fairness with synchronous RAII accounting.
impl PeerConnectionAdmission {
    /// Construct an empty peer ledger with one fixed per-address bound.
    fn new(max_connections_per_peer: usize) -> Self {
        Self {
            active: Arc::new(Mutex::new(HashMap::new())),
            max_connections_per_peer,
        }
    }

    /// Reserve one live connection for a peer without queueing a saturated source.
    fn try_admit(&self, peer_ip: IpAddr) -> Option<PeerConnectionPermit> {
        let peer_ip = canonical_peer_ip(peer_ip);
        let mut active = self
            .active
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let count = active.entry(peer_ip).or_default();
        if *count >= self.max_connections_per_peer {
            return None;
        }
        *count += 1;
        Some(PeerConnectionPermit {
            peer_ip,
            active: Arc::clone(&self.active),
        })
    }

    /// Return the number of peers currently retained by test-owned guards.
    #[cfg(test)]
    fn active_peers(&self) -> usize {
        self.active
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .len()
    }
}

/// Releases one peer's connection count when every connection owner is gone.
struct PeerConnectionPermit {
    /// Canonical peer whose active count this guard owns.
    peer_ip: IpAddr,
    /// Shared bounded peer ledger updated on teardown.
    active: Arc<Mutex<HashMap<IpAddr, usize>>>,
}

/// Removes stale peer entries synchronously at connection teardown.
impl Drop for PeerConnectionPermit {
    /// Decrement the owned count and erase the key after its final live connection.
    fn drop(&mut self) {
        let mut active = self
            .active
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let remove = match active.get_mut(&self.peer_ip) {
            Some(count) if *count > 1 => {
                *count -= 1;
                false
            }
            Some(_) => true,
            None => false,
        };
        if remove {
            active.remove(&self.peer_ip);
        }
    }
}

/// Normalize IPv4-mapped IPv6 addresses so one network peer cannot split its quota.
fn canonical_peer_ip(peer_ip: IpAddr) -> IpAddr {
    match peer_ip {
        IpAddr::V6(address) => address
            .to_ipv4_mapped()
            .map(IpAddr::V4)
            .unwrap_or(IpAddr::V6(address)),
        address => address,
    }
}

/// Connection metadata whose shared permit lives with Axum's per-connection service.
#[derive(Clone)]
pub struct LimitedConnectionInfo {
    peer_addr: SocketAddr,
    _process_permit: Option<Arc<OwnedSemaphorePermit>>,
    _peer_permit: Option<Arc<PeerConnectionPermit>>,
}

/// Formats bounded connection metadata without exposing synchronization internals.
impl fmt::Debug for LimitedConnectionInfo {
    /// Render only the peer address used by Axum connection diagnostics.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LimitedConnectionInfo")
            .field("peer_addr", &self.peer_addr)
            .finish_non_exhaustive()
    }
}

/// Supplies bounded connection metadata to Axum's per-connection extension service.
impl axum::extract::connect_info::Connected<axum::serve::IncomingStream<'_, LimitedTcpListener>>
    for LimitedConnectionInfo
{
    /// Clone the peer address and permit into the connection-owned service.
    fn connect_info(stream: axum::serve::IncomingStream<'_, LimitedTcpListener>) -> Self {
        stream.remote_addr().clone()
    }
}

/// Accepts TCP connections only after process-wide and optional peer slots are available.
impl axum::serve::Listener for LimitedTcpListener {
    /// Raw TCP stream accepted by the bounded listener.
    type Io = tokio::net::TcpStream;
    /// Connection metadata retaining the admission permit through socket teardown.
    type Addr = LimitedConnectionInfo;

    /// Wait for capacity before accepting a connection from the kernel backlog.
    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            let process_permit = self
                .permits
                .clone()
                .acquire_owned()
                .await
                .expect("bounded listener semaphore is never closed");
            let (stream, peer_addr) = axum::serve::Listener::accept(&mut self.inner).await;
            let peer_permit = match &self.peer_admission {
                Some(admission) => match admission.try_admit(peer_addr.ip()) {
                    Some(permit) => Some(Arc::new(permit)),
                    None => {
                        tracing::trace!(
                            peer_ip = %canonical_peer_ip(peer_addr.ip()),
                            "closed public TCP connection from saturated peer"
                        );
                        drop(stream);
                        drop(process_permit);
                        tokio::task::yield_now().await;
                        continue;
                    }
                },
                None => None,
            };
            return (
                stream,
                LimitedConnectionInfo {
                    peer_addr,
                    _process_permit: Some(Arc::new(process_permit)),
                    _peer_permit: peer_permit,
                },
            );
        }
    }

    /// Return the socket address of the wrapped listener.
    fn local_addr(&self) -> std::io::Result<Self::Addr> {
        self.inner
            .local_addr()
            .map(|peer_addr| LimitedConnectionInfo {
                peer_addr,
                _process_permit: None,
                _peer_permit: None,
            })
    }
}

/// Initialized Rift resources that Henosis may inspect before binding the listener.
pub struct InitializedRuntime {
    /// Public HTTP and WebSocket router over the initialized persistence layer.
    public_app: Router,
    /// Loopback-only bridge-secret router over the same application state.
    bridge_app: Router,
    /// Shared PostgreSQL pool used by routes and room bootstrap.
    pool: sqlx::PgPool,
    /// Unstarted durable event publisher owned by the eventual runtime supervisor.
    outbox_dispatcher: OutboxDispatcher,
    /// Validated public listener address retained from configuration.
    public_listen_addr: SocketAddr,
    /// Validated loopback bridge listener address retained from configuration.
    bridge_listen_addr: SocketAddr,
}

/// Named initialized resources that prevent public and bridge boundaries from being swapped.
pub struct InitializedRuntimeParts {
    /// Validated public listener address.
    pub public_listen_addr: SocketAddr,
    /// Public human, agent, upload, and WebSocket router.
    pub public_app: Router,
    /// Validated loopback-only bridge listener address.
    pub bridge_listen_addr: SocketAddr,
    /// Private bridge-secret router.
    pub bridge_app: Router,
    /// Durable event publisher that must be supervised with both route surfaces.
    pub outbox_dispatcher: OutboxDispatcher,
}

/// Independently assembled Rift route surfaces sharing one application state.
pub struct RiftRouters {
    /// Public human, agent, upload, and WebSocket router.
    pub public: Router,
    /// Private bridge-secret router.
    pub bridge: Router,
}

/// Exposes initialized Rift resources to the unified Henosis supervisor.
impl InitializedRuntime {
    /// Borrow the PostgreSQL pool for idempotent room bootstrap.
    pub fn pool(&self) -> &sqlx::PgPool {
        &self.pool
    }

    /// Consume the initialized runtime into named public and bridge resources.
    pub fn into_parts(self) -> InitializedRuntimeParts {
        InitializedRuntimeParts {
            public_listen_addr: self.public_listen_addr,
            public_app: self.public_app,
            bridge_listen_addr: self.bridge_listen_addr,
            bridge_app: self.bridge_app,
            outbox_dispatcher: self.outbox_dispatcher,
        }
    }

    /// Consume the initialized runtime and return only its public router.
    pub fn into_router(self) -> Router {
        self.public_app
    }
}

/// Extracts the shared PostgreSQL pool from the Rift application state.
impl axum::extract::FromRef<AppState> for sqlx::PgPool {
    /// Clones the pooled database handle for an Axum request extractor.
    fn from_ref(state: &AppState) -> Self {
        state.pool.clone()
    }
}

/// Extracts runtime configuration from the Rift application state.
impl axum::extract::FromRef<AppState> for Config {
    /// Clones the runtime configuration for an Axum request extractor.
    fn from_ref(state: &AppState) -> Self {
        state.config.clone()
    }
}

/// Extracts the WebSocket gateway from the Rift application state.
impl axum::extract::FromRef<AppState> for Gateway {
    /// Clones the gateway handle for an Axum request extractor.
    fn from_ref(state: &AppState) -> Self {
        state.gateway.clone()
    }
}

/// Extracts pending upload state from the Rift application state.
impl axum::extract::FromRef<AppState> for PendingUploads {
    /// Clones the pending upload registry for an Axum request extractor.
    fn from_ref(state: &AppState) -> Self {
        state.pending_uploads.clone()
    }
}

/// Extracts managed execution control from the Rift application state.
impl axum::extract::FromRef<AppState> for ManagedAgentControlRegistry {
    /// Clones the one-time controller registry for an Axum request extractor.
    fn from_ref(state: &AppState) -> Self {
        state.agent_control.clone()
    }
}

/// Initialize Rift persistence and construct its complete HTTP router.
pub async fn build_router(config: Config) -> Result<Router, RuntimeError> {
    Ok(initialize(config).await?.into_router())
}

/// Initialize Rift persistence and retain the pool for unified room bootstrap.
pub async fn initialize(config: Config) -> Result<InitializedRuntime, RuntimeError> {
    initialize_with_control_registry(config, ManagedAgentControlRegistry::default()).await
}

/// Initialize Rift with a registry that Henosis may populate before serving.
pub async fn initialize_with_control_registry(
    config: Config,
    agent_control: ManagedAgentControlRegistry,
) -> Result<InitializedRuntime, RuntimeError> {
    let (public_listen_addr, bridge_listen_addr) = config.validate_runtime()?;
    let pool = PgPoolOptions::new()
        .max_connections(20)
        .connect(&config.database_url)
        .await?;

    tracing::info!("Connected to database");

    // Apply embedded migrations on boot. The standalone rift-server applied
    // migrations externally; in the henosis workspace the binary self-migrates
    // so a fresh deploy converges without an out-of-band `sqlx migrate run`.
    sqlx::migrate!("./migrations").run(&pool).await?;

    tracing::info!("Migrations applied");

    tokio::fs::create_dir_all(&config.upload_dir).await?;

    let gateway = Gateway::new();
    let apps = routers_with_control_registry_and_gateway(
        config,
        pool.clone(),
        agent_control,
        gateway.clone(),
        WebSocketAdmission::new(MAX_WEBSOCKET_CONNECTIONS),
    )?;
    Ok(InitializedRuntime {
        public_app: apps.public,
        bridge_app: apps.bridge,
        outbox_dispatcher: OutboxDispatcher::new(pool.clone(), gateway),
        pool,
        public_listen_addr,
        bridge_listen_addr,
    })
}

/// Construct Rift's public router over an initialized PostgreSQL pool.
pub fn router(config: Config, pool: sqlx::PgPool) -> Result<Router, ConfigError> {
    router_with_control_registry(config, pool, ManagedAgentControlRegistry::default())
}

/// Construct both least-privilege routers over an initialized PostgreSQL pool.
pub fn routers(config: Config, pool: sqlx::PgPool) -> Result<RiftRouters, ConfigError> {
    routers_with_control_registry(config, pool, ManagedAgentControlRegistry::default())
}

/// Construct Rift's public router with a shared managed execution controller registry.
pub fn router_with_control_registry(
    config: Config,
    pool: sqlx::PgPool,
    agent_control: ManagedAgentControlRegistry,
) -> Result<Router, ConfigError> {
    Ok(routers_with_control_registry(config, pool, agent_control)?.public)
}

/// Construct both route surfaces with one gateway and managed control registry.
pub fn routers_with_control_registry(
    config: Config,
    pool: sqlx::PgPool,
    agent_control: ManagedAgentControlRegistry,
) -> Result<RiftRouters, ConfigError> {
    routers_with_control_registry_and_websocket_admission(
        config,
        pool,
        agent_control,
        WebSocketAdmission::new(MAX_WEBSOCKET_CONNECTIONS),
    )
}

/// Construct both route surfaces with an explicit WebSocket capacity gate.
fn routers_with_control_registry_and_websocket_admission(
    config: Config,
    pool: sqlx::PgPool,
    agent_control: ManagedAgentControlRegistry,
    websocket_admission: WebSocketAdmission,
) -> Result<RiftRouters, ConfigError> {
    let gateway = Gateway::new();
    routers_with_control_registry_and_gateway(
        config,
        pool,
        agent_control,
        gateway,
        websocket_admission,
    )
}

/// Construct both route surfaces over an explicitly shared local gateway.
fn routers_with_control_registry_and_gateway(
    config: Config,
    pool: sqlx::PgPool,
    agent_control: ManagedAgentControlRegistry,
    gateway: Gateway,
    websocket_admission: WebSocketAdmission,
) -> Result<RiftRouters, ConfigError> {
    config.validate_runtime()?;
    let pending_uploads: PendingUploads = std::sync::Arc::new(dashmap::DashMap::new());

    let upload_dir = config.upload_dir.clone();
    let attachment_body_limit = config
        .max_upload_bytes
        .saturating_add(MULTIPART_OVERHEAD_BYTES);
    let avatar_body_limit = MAX_AVATAR_BYTES + MULTIPART_OVERHEAD_BYTES;

    let state = AppState {
        pool,
        config: config.clone(),
        gateway,
        pending_uploads,
        agent_control,
        websocket_admission,
    };

    let cors = CorsLayer::new()
        .allow_origin(AllowOrigin::list(config.cors_origins.clone()))
        .allow_methods([
            Method::GET,
            Method::POST,
            Method::PUT,
            Method::PATCH,
            Method::DELETE,
            Method::OPTIONS,
        ])
        .allow_headers(Any);

    let uploads = ServiceBuilder::new()
        .layer(SetResponseHeaderLayer::overriding(
            header::X_CONTENT_TYPE_OPTIONS,
            HeaderValue::from_static("nosniff"),
        ))
        .service(ServeDir::new(upload_dir));

    let public = Router::new()
        // Auth
        .route(
            "/api/auth/register",
            post(routes::auth::register).layer(DefaultBodyLimit::max(MAX_AUTH_REQUEST_BYTES)),
        )
        .route(
            "/api/auth/login",
            post(routes::auth::login).layer(DefaultBodyLimit::max(MAX_AUTH_REQUEST_BYTES)),
        )
        .route(
            "/api/auth/refresh",
            post(routes::auth::refresh).layer(DefaultBodyLimit::max(MAX_AUTH_REQUEST_BYTES)),
        )
        .route(
            "/api/auth/logout",
            post(routes::auth::logout).layer(DefaultBodyLimit::max(MAX_AUTH_REQUEST_BYTES)),
        )
        // Users
        .route(
            "/api/users/@me",
            get(routes::users::get_me).patch(routes::users::update_me),
        )
        .route(
            "/api/users/@me/avatar",
            post(routes::users::upload_avatar).layer(DefaultBodyLimit::max(avatar_body_limit)),
        )
        .route(
            "/api/users/@me/password",
            post(routes::users::change_password),
        )
        .route(
            "/api/users/@me/agents",
            get(routes::agent_identities::list_owned_agents)
                .post(routes::agent_identities::create_owned_agent),
        )
        .route(
            "/api/agents/{agent_id}/claim",
            post(routes::agent_identities::claim_agent),
        )
        .route(
            "/api/users/@me/dms",
            get(routes::users::list_dms).post(routes::users::create_dm),
        )
        .route("/api/users/{user_id}", get(routes::users::get_user))
        // Servers
        .route(
            "/api/servers",
            get(routes::servers::list_servers).post(routes::servers::create_server),
        )
        .route(
            "/api/servers/{server_id}",
            get(routes::servers::get_server)
                .patch(routes::servers::update_server)
                .delete(routes::servers::delete_server),
        )
        .route(
            "/api/servers/{server_id}/permissions/@me",
            get(routes::servers::current_user_permissions),
        )
        .route(
            "/api/servers/{server_id}/members",
            get(routes::servers::list_members),
        )
        .route(
            "/api/servers/{server_id}/members/{user_id}",
            delete(routes::servers::remove_member),
        )
        .route(
            "/api/servers/{server_id}/invites",
            get(routes::servers::list_invites).post(routes::servers::create_invite),
        )
        .route(
            "/api/servers/{server_id}/invites/{code}",
            delete(routes::servers::delete_invite),
        )
        .route(
            "/api/invites/{code}/join",
            post(routes::servers::join_via_invite),
        )
        // Roles
        .route(
            "/api/servers/{server_id}/roles",
            get(routes::roles::list_roles).post(routes::roles::create_role),
        )
        .route(
            "/api/servers/{server_id}/roles/{role_id}",
            patch(routes::roles::update_role).delete(routes::roles::delete_role),
        )
        .route(
            "/api/servers/{server_id}/members/{user_id}/roles/{role_id}",
            put(routes::roles::assign_role).delete(routes::roles::remove_role),
        )
        .route(
            "/api/servers/{server_id}/members/{user_id}/roles",
            get(routes::roles::get_member_roles),
        )
        // Channels
        .route(
            "/api/servers/{server_id}/channels",
            get(routes::channels::list_channels).post(routes::channels::create_channel),
        )
        .route(
            "/api/channels/{channel_id}",
            patch(routes::channels::update_channel).delete(routes::channels::delete_channel),
        )
        // Messages
        .route(
            "/api/channels/{channel_id}/messages",
            get(routes::messages::list_messages).post(routes::messages::send_message),
        )
        .route(
            "/api/channels/{channel_id}/messages/{message_id}",
            patch(routes::messages::edit_message).delete(routes::messages::delete_message),
        )
        // File uploads
        .route(
            "/api/upload",
            post(routes::upload::upload_files).layer(DefaultBodyLimit::max(attachment_body_limit)),
        )
        // Human bridge and desired-state control
        .route(
            "/api/servers/{server_id}/bridge/pause",
            post(routes::bridge_control::pause_bridge),
        )
        .route(
            "/api/servers/{server_id}/bridge/resume",
            post(routes::bridge_control::resume_bridge),
        )
        .route(
            "/api/servers/{server_id}/bridge/status",
            get(routes::bridge_control::bridge_status),
        )
        .route(
            "/api/servers/{server_id}/agent-roster",
            get(routes::agent_control::get_agent_roster)
                .put(routes::agent_control::put_agent_roster),
        )
        .route(
            "/api/servers/{server_id}/agent-capabilities",
            get(routes::agent_control::get_agent_capabilities),
        )
        .route(
            "/api/servers/{server_id}/bridge/reconcile",
            post(routes::agent_control::reconcile_agent_roster),
        )
        // DMs
        .route(
            "/api/dms/{dm_channel_id}/messages",
            get(routes::users::list_dm_messages).post(routes::users::send_dm_message),
        )
        // Static file serving for uploads
        .nest_service("/uploads", uploads)
        // WebSocket
        .route("/ws", get(ws_handler))
        // Middleware
        .layer(cors)
        .layer(TraceLayer::new_for_http())
        .layer(axum::Extension(config))
        .with_state(state.clone());

    let bridge = Router::new()
        .route("/api/bridge/notify", post(routes::bridge::notify_message))
        .route(
            "/api/bridge/provision",
            post(routes::bridge::provision_agents),
        )
        .route(
            "/api/bridge/servers/{server_id}/status",
            get(routes::bridge_control::daemon_bridge_status),
        )
        .layer(DefaultBodyLimit::max(MAX_BRIDGE_REQUEST_BYTES))
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    Ok(RiftRouters { public, bridge })
}

/// Serve Rift until the process or supervising runtime stops it.
pub async fn serve(config: Config) -> Result<(), RuntimeError> {
    serve_until(config, std::future::pending()).await
}

/// Serve Rift until the supplied stop signal resolves.
pub async fn serve_until<F>(config: Config, stop: F) -> Result<(), RuntimeError>
where
    F: Future<Output = ()> + Send + 'static,
{
    let parts = initialize(config).await?.into_parts();
    serve_initialized_until(parts, stop).await
}

/// Bind and supervise an initialized pair of Rift routers as one lifecycle unit.
async fn serve_initialized_until<F>(
    parts: InitializedRuntimeParts,
    stop: F,
) -> Result<(), RuntimeError>
where
    F: Future<Output = ()> + Send + 'static,
{
    let public_listener = tokio::net::TcpListener::bind(parts.public_listen_addr)
        .await
        .map_err(|source| RuntimeError::ListenerBind {
            component: "Rift public listener",
            source,
        })?;
    let bridge_listener = tokio::net::TcpListener::bind(parts.bridge_listen_addr)
        .await
        .map_err(|source| RuntimeError::ListenerBind {
            component: "Rift bridge listener",
            source,
        })?;
    let public_addr = parts.public_listen_addr;
    let bridge_addr = parts.bridge_listen_addr;
    let public_listener = LimitedTcpListener::for_public(public_listener);
    let bridge_listener = LimitedTcpListener::for_bridge(bridge_listener);
    let (listener_stop_tx, listener_stop_rx) = tokio::sync::watch::channel(false);
    let public_stop = listener_stop_rx.clone();
    let bridge_stop = listener_stop_rx.clone();
    let dispatcher_stop = listener_stop_rx;
    let public_server = async move {
        axum::serve(
            public_listener,
            parts
                .public_app
                .into_make_service_with_connect_info::<LimitedConnectionInfo>(),
        )
        .with_graceful_shutdown(wait_for_listener_stop(public_stop))
        .await
    };
    let bridge_server = async move {
        axum::serve(
            bridge_listener,
            parts
                .bridge_app
                .into_make_service_with_connect_info::<LimitedConnectionInfo>(),
        )
        .with_graceful_shutdown(wait_for_listener_stop(bridge_stop))
        .await
    };
    let outbox_dispatcher = parts.outbox_dispatcher.run(dispatcher_stop);
    tokio::pin!(public_server, bridge_server, outbox_dispatcher, stop);

    tracing::info!("Rift public listener active on {public_addr}");
    tracing::info!("Rift bridge listener active on {bridge_addr}");

    tokio::select! {
        biased;
        _ = &mut stop => {
            let _ = listener_stop_tx.send(true);
            let (public_result, bridge_result, dispatcher_result) =
                complete_runtime_stop_before(async {
                    tokio::join!(
                        &mut public_server,
                        &mut bridge_server,
                        &mut outbox_dispatcher,
                    )
                }, RUNTIME_COMPONENT_STOP_TIMEOUT).await?;
            public_result?;
            bridge_result?;
            if let Err(error) = dispatcher_result {
                return Err(RuntimeError::ComponentStopped {
                    component: "Rift event outbox dispatcher",
                    detail: error.to_string(),
                });
            }
            Ok(())
        }
        result = &mut public_server => {
            let _ = listener_stop_tx.send(true);
            Err(RuntimeError::ComponentStopped {
                component: "Rift public listener",
                detail: listener_result_detail(result),
            })
        }
        result = &mut bridge_server => {
            let _ = listener_stop_tx.send(true);
            Err(RuntimeError::ComponentStopped {
                component: "Rift bridge listener",
                detail: listener_result_detail(result),
            })
        }
        result = &mut outbox_dispatcher => {
            let _ = listener_stop_tx.send(true);
            Err(RuntimeError::ComponentStopped {
                component: "Rift event outbox dispatcher",
                detail: outbox_result_detail(result),
            })
        }
    }
}

/// Bound one coordinated standalone stop so stalled I/O cannot retain listeners forever.
async fn complete_runtime_stop_before<F>(
    completion: F,
    timeout: Duration,
) -> Result<F::Output, RuntimeError>
where
    F: Future,
{
    tokio::time::timeout(timeout, completion)
        .await
        .map_err(|_| RuntimeError::ComponentStopped {
            component: "Rift coordinated stop",
            detail: format!("component stop exceeded {} seconds", timeout.as_secs()),
        })
}

/// Wait until a shared listener stop signal is true or every sender is gone.
async fn wait_for_listener_stop(mut receiver: tokio::sync::watch::Receiver<bool>) {
    if *receiver.borrow() {
        return;
    }
    while receiver.changed().await.is_ok() {
        if *receiver.borrow() {
            return;
        }
    }
}

/// Render a listener completion without losing an underlying I/O error.
fn listener_result_detail(result: Result<(), std::io::Error>) -> String {
    match result {
        Ok(()) => "completed successfully".to_string(),
        Err(error) => error.to_string(),
    }
}

/// Render an outbox completion without losing its durable-publication failure.
fn outbox_result_detail(result: Result<(), crate::outbox::OutboxDispatchError>) -> String {
    match result {
        Ok(()) => "completed successfully".to_string(),
        Err(error) => error.to_string(),
    }
}

/// Upgrade an authenticated Rift WebSocket connection into the shared gateway.
async fn ws_handler(ws: WebSocketUpgrade, State(state): State<AppState>) -> impl IntoResponse {
    let Ok(connection_permit) = state.websocket_admission.try_admit() else {
        return axum::http::StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    let human_jwt_secret = state.config.jwt_secret.clone();
    let agent_jwt_secret = state.config.agent_jwt_secret.clone();
    let gateway = state.gateway.clone();
    let pool = state.pool.clone();
    ws.max_message_size(MAX_WEBSOCKET_BYTES)
        .max_frame_size(MAX_WEBSOCKET_BYTES)
        .on_upgrade(move |socket| async move {
            let _connection_permit = connection_permit;
            gateway
                .handle_connection(socket, human_jwt_secret, agent_jwt_secret, pool)
                .await;
        })
        .into_response()
}

/// Exercises security properties of routes that must remain authenticated.
#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr};
    use std::time::Duration;

    use axum::{
        body::Body,
        http::{Method, Request, StatusCode, header},
    };
    use sqlx::postgres::PgPoolOptions;
    use tower::ServiceExt;
    use uuid::Uuid;

    use super::{
        Config, Gateway, InitializedRuntimeParts, LimitedTcpListener, OutboxDispatcher,
        PeerConnectionAdmission, RuntimeError, WebSocketAdmission, canonical_peer_ip,
        complete_runtime_stop_before, router, routers,
        routers_with_control_registry_and_websocket_admission, serve_initialized_until,
    };

    /// Construct a secret-safe configuration for routes that reject before I/O.
    fn test_config() -> Config {
        Config {
            database_url: "postgresql://rift.invalid/henosis_test".to_string(),
            jwt_secret: "test-jwt-secret-not-for-production".to_string(),
            agent_jwt_secret: "test-agent-jwt-secret-not-for-prod".to_string(),
            bridge_secret: "test-bridge-secret-not-for-production".to_string(),
            listen_addr: "127.0.0.1:3200".to_string(),
            bridge_listen_addr: "127.0.0.1:3201".to_string(),
            allow_remote_listen: false,
            cors_origins: vec![
                "http://localhost:5173"
                    .parse()
                    .expect("test origin must parse"),
            ],
            upload_dir: "uploads".to_string(),
            max_upload_bytes: 1024,
        }
    }

    /// Construct a database-free dispatcher that still exercises supervisor ownership.
    fn idle_outbox_dispatcher() -> OutboxDispatcher {
        let pool = PgPoolOptions::new()
            .connect_lazy("postgresql://rift.invalid/henosis_test")
            .expect("test database URL must parse");
        OutboxDispatcher::idle_for_test(pool, Gateway::new())
    }

    /// Direct router construction cannot bypass runtime configuration validation.
    #[tokio::test]
    async fn router_rejects_invalid_programmatic_config() {
        let pool = PgPoolOptions::new()
            .connect_lazy("postgresql://rift.invalid/henosis_test")
            .expect("test database URL must parse");
        let mut config = test_config();
        config.bridge_secret = "short".to_string();

        assert!(routers(config, pool).is_err());
    }

    /// Build one request with an optional bridge bearer and JSON body.
    fn request(method: Method, uri: &str, body: &str, bearer: Option<&str>) -> Request<Body> {
        let mut builder = Request::builder()
            .method(method)
            .uri(uri)
            .header(header::CONTENT_TYPE, "application/json");
        if let Some(bearer) = bearer {
            builder = builder.header(header::AUTHORIZATION, format!("Bearer {bearer}"));
        }
        builder
            .body(Body::from(body.to_string()))
            .expect("test request must build")
    }

    /// Reserve and release one loopback address for an immediate listener test.
    async fn available_address() -> std::net::SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("reserve listener address");
        listener.local_addr().expect("read listener address")
    }

    /// Reserve two distinct loopback addresses before releasing either one.
    async fn available_address_pair() -> (std::net::SocketAddr, std::net::SocketAddr) {
        let public = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("reserve public listener address");
        let bridge = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("reserve bridge listener address");
        (
            public.local_addr().expect("read public listener address"),
            bridge.local_addr().expect("read bridge listener address"),
        )
    }

    /// Wait until a listener accepts connections within the test deadline.
    async fn wait_until_connectable(address: std::net::SocketAddr) {
        tokio::time::timeout(Duration::from_secs(2), async move {
            loop {
                if tokio::net::TcpStream::connect(address).await.is_ok() {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("listener must become connectable");
    }

    /// Connect one test client from an explicit IPv4 loopback source address.
    #[cfg(target_os = "linux")]
    async fn connect_from_ipv4(
        source: Ipv4Addr,
        destination: std::net::SocketAddr,
    ) -> tokio::net::TcpStream {
        let socket = tokio::net::TcpSocket::new_v4().expect("create IPv4 test socket");
        socket
            .bind(std::net::SocketAddr::new(IpAddr::V4(source), 0))
            .expect("bind explicit loopback source");
        socket
            .connect(destination)
            .await
            .expect("connect explicit loopback source")
    }

    /// Peer admission isolates a saturated source and releases its slot on guard drop.
    #[test]
    fn peer_connection_admission_is_source_fair_and_drop_safe() {
        let admission = PeerConnectionAdmission::new(1);
        let first_ip = IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1));
        let second_ip = IpAddr::V4(Ipv4Addr::new(127, 0, 0, 2));

        let first = admission
            .try_admit(first_ip)
            .expect("first source receives its connection slot");
        assert!(admission.try_admit(first_ip).is_none());
        let second = admission
            .try_admit(second_ip)
            .expect("an unrelated source keeps independent capacity");
        assert_eq!(admission.active_peers(), 2);

        drop(first);
        assert!(admission.try_admit(first_ip).is_some());
        drop(second);
    }

    /// IPv4-mapped IPv6 peers share the same canonical admission identity as IPv4.
    #[test]
    fn peer_connection_admission_canonicalizes_mapped_ipv6() {
        let ipv4 = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 8));
        let mapped = "::ffff:192.0.2.8"
            .parse::<IpAddr>()
            .expect("mapped IPv6 test address parses");
        assert_eq!(canonical_peer_ip(mapped), ipv4);
    }

    /// The listener skips a saturated peer and accepts a queued unrelated source.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn public_tcp_peer_limit_preserves_unrelated_source_progress() {
        let listener = tokio::net::TcpListener::bind("0.0.0.0:0")
            .await
            .expect("bind source-fair admission listener");
        let port = listener
            .local_addr()
            .expect("read source-fair listener address")
            .port();
        let destination = std::net::SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);
        let mut limited = LimitedTcpListener::with_peer_limit(listener, 2, 1);

        let first_client = connect_from_ipv4(Ipv4Addr::LOCALHOST, destination);
        let (first_client, first_accept) = tokio::join!(first_client, async {
            axum::serve::Listener::accept(&mut limited).await
        });
        let first_client = first_client;
        let (first_server, first_connection) = first_accept;
        assert_eq!(
            first_connection.peer_addr.ip(),
            IpAddr::V4(Ipv4Addr::LOCALHOST)
        );

        let saturated_client = connect_from_ipv4(Ipv4Addr::LOCALHOST, destination).await;
        let unrelated_ip = Ipv4Addr::new(127, 0, 0, 2);
        let unrelated_client = connect_from_ipv4(unrelated_ip, destination).await;
        let (unrelated_server, unrelated_connection) = tokio::time::timeout(
            Duration::from_secs(1),
            axum::serve::Listener::accept(&mut limited),
        )
        .await
        .expect("unrelated queued source must progress past saturated source");
        assert_eq!(
            unrelated_connection.peer_addr.ip(),
            IpAddr::V4(unrelated_ip)
        );

        drop(first_server);
        drop(first_connection);
        let replacement_client = connect_from_ipv4(Ipv4Addr::LOCALHOST, destination).await;
        let (_replacement_server, replacement_connection) = tokio::time::timeout(
            Duration::from_secs(1),
            axum::serve::Listener::accept(&mut limited),
        )
        .await
        .expect("dropping the first source guard must restore its peer slot");
        assert_eq!(
            replacement_connection.peer_addr.ip(),
            IpAddr::V4(Ipv4Addr::LOCALHOST)
        );

        drop(first_client);
        drop(saturated_client);
        drop(unrelated_client);
        drop(unrelated_server);
        drop(unrelated_connection);
        drop(replacement_client);
    }

    /// Public TCP admission blocks a second accept until the first stream closes.
    #[tokio::test]
    async fn public_tcp_capacity_precedes_http_parsing() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind admission test listener");
        let address = listener.local_addr().expect("read admission test address");
        let mut limited = LimitedTcpListener::new(listener, 1);
        let first_client = tokio::net::TcpStream::connect(address);
        let (first_client, first_accept) = tokio::join!(first_client, async {
            axum::serve::Listener::accept(&mut limited).await
        });
        let first_client = first_client.expect("connect first silent client");
        let (first_server, first_connection) = first_accept;

        let second_client = tokio::net::TcpStream::connect(address)
            .await
            .expect("kernel may queue a second client");
        let mut second_accept = std::pin::pin!(axum::serve::Listener::accept(&mut limited));
        assert!(
            tokio::time::timeout(Duration::from_millis(50), &mut second_accept)
                .await
                .is_err(),
            "capacity must block acceptance before any HTTP bytes arrive"
        );

        drop(first_server);
        drop(first_connection);
        tokio::time::timeout(Duration::from_secs(1), &mut second_accept)
            .await
            .expect("closing the first server stream must release capacity");
        drop(first_client);
        drop(second_client);
    }

    /// A stalled standalone component cannot retain the coordinated stop forever.
    #[tokio::test]
    async fn standalone_component_stop_has_a_hard_deadline() {
        let error =
            complete_runtime_stop_before(std::future::pending::<()>(), Duration::from_millis(10))
                .await
                .expect_err("a stalled component must exceed the bounded stop interval");

        assert!(matches!(
            error,
            RuntimeError::ComponentStopped {
                component: "Rift coordinated stop",
                ..
            }
        ));
    }

    /// An occupied WebSocket slot rejects excess upgrades and is reusable after disconnect.
    #[tokio::test]
    async fn websocket_capacity_is_enforced_before_upgrade() {
        let pool = PgPoolOptions::new()
            .connect_lazy("postgresql://rift.invalid/henosis_test")
            .expect("test database URL must parse");
        let public = routers_with_control_registry_and_websocket_admission(
            test_config(),
            pool,
            crate::agent_control::ManagedAgentControlRegistry::default(),
            WebSocketAdmission::new(1),
        )
        .expect("test runtime config must be valid")
        .public;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind WebSocket test listener");
        let address = listener.local_addr().expect("read test listener address");
        let server = tokio::spawn(async move {
            axum::serve(listener, public)
                .await
                .expect("serve WebSocket test router");
        });
        let url = format!("ws://{address}/ws");

        let (mut first, _) = tokio_tungstenite::connect_async(&url)
            .await
            .expect("first WebSocket must consume the only slot");
        let error = tokio_tungstenite::connect_async(&url)
            .await
            .expect_err("second WebSocket must be rejected at capacity");
        let tokio_tungstenite::tungstenite::Error::Http(response) = error else {
            panic!("capacity rejection must be an HTTP response");
        };
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);

        first.close(None).await.expect("close first WebSocket");
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Ok((mut replacement, _)) = tokio_tungstenite::connect_async(&url).await {
                    replacement
                        .close(None)
                        .await
                        .expect("close replacement WebSocket");
                    return;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("a disconnected WebSocket must release its slot");
        server.abort();
        let _ = server.await;
    }

    /// Both standalone sockets serve together and close together on coordinated stop.
    #[tokio::test]
    async fn standalone_runtime_owns_both_listener_lifecycles() {
        let (public_addr, bridge_addr) = available_address_pair().await;
        let parts = InitializedRuntimeParts {
            public_listen_addr: public_addr,
            public_app: axum::Router::new(),
            bridge_listen_addr: bridge_addr,
            bridge_app: axum::Router::new(),
            outbox_dispatcher: idle_outbox_dispatcher(),
        };
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
        let task = tokio::spawn(serve_initialized_until(parts, async move {
            let _ = stop_rx.await;
        }));

        wait_until_connectable(public_addr).await;
        wait_until_connectable(bridge_addr).await;
        stop_tx.send(()).expect("request coordinated stop");
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .expect("runtime must stop promptly")
            .expect("runtime task must not panic")
            .expect("coordinated stop must succeed");

        tokio::net::TcpListener::bind(public_addr)
            .await
            .expect("public address must be released");
        tokio::net::TcpListener::bind(bridge_addr)
            .await
            .expect("bridge address must be released");
    }

    /// A standalone dispatcher exit is fatal to both Rift listener boundaries.
    #[tokio::test]
    async fn standalone_runtime_treats_outbox_exit_as_fatal() {
        let (public_addr, bridge_addr) = available_address_pair().await;
        let pool = PgPoolOptions::new()
            .connect_lazy("postgresql://rift.invalid/henosis_test")
            .expect("test database URL must parse");
        pool.close().await;
        let parts = InitializedRuntimeParts {
            public_listen_addr: public_addr,
            public_app: axum::Router::new(),
            bridge_listen_addr: bridge_addr,
            bridge_app: axum::Router::new(),
            outbox_dispatcher: OutboxDispatcher::new(pool, Gateway::new()),
        };

        let error = tokio::time::timeout(
            Duration::from_secs(2),
            serve_initialized_until(parts, std::future::pending()),
        )
        .await
        .expect("closed outbox pool must fail promptly")
        .expect_err("dispatcher completion must be fatal");
        match error {
            RuntimeError::ComponentStopped { component, detail } => {
                assert_eq!(component, "Rift event outbox dispatcher");
                assert!(
                    detail.contains("closed pool"),
                    "unexpected detail: {detail}"
                );
            }
            other => panic!("unexpected runtime failure: {other}"),
        }
    }

    /// A bridge bind failure releases the public socket and names the failed boundary.
    #[tokio::test]
    async fn standalone_bridge_bind_failure_is_atomic() {
        let public_addr = available_address().await;
        let occupied_bridge = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("occupy bridge address");
        let bridge_addr = occupied_bridge
            .local_addr()
            .expect("read occupied bridge address");
        let parts = InitializedRuntimeParts {
            public_listen_addr: public_addr,
            public_app: axum::Router::new(),
            bridge_listen_addr: bridge_addr,
            bridge_app: axum::Router::new(),
            outbox_dispatcher: idle_outbox_dispatcher(),
        };

        let error = serve_initialized_until(parts, std::future::pending())
            .await
            .expect_err("occupied bridge socket must fail startup");
        match error {
            RuntimeError::ListenerBind { component, .. } => {
                assert_eq!(component, "Rift bridge listener");
            }
            other => panic!("unexpected bind failure: {other}"),
        }
        tokio::net::TcpListener::bind(public_addr)
            .await
            .expect("public socket must be released after bridge bind failure");
    }

    /// The current-user permission contract is mounted behind bearer authentication.
    #[tokio::test]
    async fn current_user_permissions_route_requires_authentication() {
        let pool = PgPoolOptions::new()
            .connect_lazy("postgresql://rift.invalid/henosis_test")
            .expect("test database URL must parse");
        let response = router(test_config(), pool)
            .expect("test runtime config must be valid")
            .oneshot(
                Request::builder()
                    .uri(format!("/api/servers/{}/permissions/@me", Uuid::new_v4()))
                    .body(Body::empty())
                    .expect("test request must build"),
            )
            .await
            .expect("router must answer");
        assert_eq!(response.status(), axum::http::StatusCode::UNAUTHORIZED);
    }

    /// Authentication bodies are rejected before oversized JSON reaches route handlers.
    #[tokio::test]
    async fn public_authentication_routes_have_a_narrow_body_limit() {
        let pool = PgPoolOptions::new()
            .connect_lazy("postgresql://rift.invalid/henosis_test")
            .expect("test database URL must parse");
        let oversized_password = "x".repeat(5 * 1024);
        let body = serde_json::json!({
            "username": "bounded-user",
            "password": oversized_password,
        })
        .to_string();
        let response = router(test_config(), pool)
            .expect("test runtime config must be valid")
            .oneshot(request(Method::POST, "/api/auth/login", &body, None))
            .await
            .expect("router must answer");

        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    /// Correct bridge credentials cannot make a private route exist on public ingress.
    #[tokio::test]
    async fn public_router_has_no_bridge_secret_routes() {
        let pool = PgPoolOptions::new()
            .connect_lazy("postgresql://rift.invalid/henosis_test")
            .expect("test database URL must parse");
        let config = test_config();
        let bridge_secret = config.bridge_secret.clone();
        let public = routers(config, pool)
            .expect("test runtime config must be valid")
            .public;
        let server_id = Uuid::new_v4();
        let cases = [
            (Method::POST, "/api/bridge/notify".to_string(), "{}"),
            (Method::POST, "/api/bridge/provision".to_string(), "{}"),
            (
                Method::GET,
                format!("/api/bridge/servers/{server_id}/status"),
                "",
            ),
        ];

        for (method, uri, body) in cases {
            let response = public
                .clone()
                .oneshot(request(method, &uri, body, Some(&bridge_secret)))
                .await
                .expect("public router must answer");
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "{uri}");
        }
    }

    /// Private ingress recognizes bridge routes but keeps their bearer gate intact.
    #[tokio::test]
    async fn bridge_router_requires_bridge_authorization() {
        let pool = PgPoolOptions::new()
            .connect_lazy("postgresql://rift.invalid/henosis_test")
            .expect("test database URL must parse");
        let bridge = routers(test_config(), pool)
            .expect("test runtime config must be valid")
            .bridge;
        let server_id = Uuid::new_v4();
        let channel_id = Uuid::new_v4();
        let message_id = Uuid::new_v4();
        let cases = [
            (
                Method::POST,
                "/api/bridge/notify".to_string(),
                format!(r#"{{"channel_id":"{channel_id}","message_id":"{message_id}"}}"#),
            ),
            (
                Method::POST,
                "/api/bridge/provision".to_string(),
                format!(r#"{{"server_id":"{server_id}","agents":[]}}"#),
            ),
            (
                Method::GET,
                format!("/api/bridge/servers/{server_id}/status"),
                String::new(),
            ),
        ];

        for (method, uri, body) in cases {
            let response = bridge
                .clone()
                .oneshot(request(method, &uri, &body, None))
                .await
                .expect("bridge router must answer");
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{uri}");
        }
    }

    /// Unauthorized bridge requests are rejected before malformed JSON is parsed.
    #[tokio::test]
    async fn bridge_router_authenticates_before_reading_the_body() {
        let pool = PgPoolOptions::new()
            .connect_lazy("postgresql://rift.invalid/henosis_test")
            .expect("test database URL must parse");
        let bridge = routers(test_config(), pool)
            .expect("test runtime config must be valid")
            .bridge;
        let response = bridge
            .oneshot(request(
                Method::POST,
                "/api/bridge/notify",
                "not-json",
                None,
            ))
            .await
            .expect("bridge router must answer");

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    /// Authenticated bridge bodies are bounded below Axum's broad default ceiling.
    #[tokio::test]
    async fn bridge_router_rejects_oversized_bodies() {
        let pool = PgPoolOptions::new()
            .connect_lazy("postgresql://rift.invalid/henosis_test")
            .expect("test database URL must parse");
        let config = test_config();
        let bridge_secret = config.bridge_secret.clone();
        let bridge = routers(config, pool)
            .expect("test runtime config must be valid")
            .bridge;
        let oversized = "x".repeat((64 * 1024) + 1);
        let response = bridge
            .oneshot(request(
                Method::POST,
                "/api/bridge/notify",
                &oversized,
                Some(&bridge_secret),
            ))
            .await
            .expect("bridge router must answer");

        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    /// Human, upload, and WebSocket routes are absent from private ingress.
    #[tokio::test]
    async fn bridge_router_has_no_public_routes() {
        let pool = PgPoolOptions::new()
            .connect_lazy("postgresql://rift.invalid/henosis_test")
            .expect("test database URL must parse");
        let bridge = routers(test_config(), pool)
            .expect("test runtime config must be valid")
            .bridge;
        let server_id = Uuid::new_v4();
        let cases = [
            (Method::POST, "/api/auth/login".to_string()),
            (Method::POST, "/api/upload".to_string()),
            (Method::GET, "/ws".to_string()),
            (
                Method::POST,
                format!("/api/servers/{server_id}/bridge/pause"),
            ),
        ];

        for (method, uri) in cases {
            let response = bridge
                .clone()
                .oneshot(request(method, &uri, "{}", None))
                .await
                .expect("bridge router must answer");
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "{uri}");
        }
    }
}
