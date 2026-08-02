//! Reusable Rift HTTP and WebSocket server lifecycle.

use axum::{
    Router,
    extract::{DefaultBodyLimit, State, ws::WebSocketUpgrade},
    http::{HeaderValue, Method, header},
    response::IntoResponse,
    routing::{delete, get, patch, post, put},
};
use sqlx::postgres::PgPoolOptions;
use std::future::Future;
use tower::ServiceBuilder;
use tower_http::cors::{AllowOrigin, Any, CorsLayer};
use tower_http::services::ServeDir;
use tower_http::set_header::SetResponseHeaderLayer;
use tower_http::trace::TraceLayer;

use crate::{agent_control::ManagedAgentControlRegistry, config, routes, ws};

use config::Config;
use routes::upload::PendingUploads;
use ws::gateway::Gateway;

/// Maximum accepted WebSocket message and frame size.
const MAX_WEBSOCKET_BYTES: usize = 64 * 1024;

/// Multipart envelope allowance above the accepted file payload.
const MULTIPART_OVERHEAD_BYTES: usize = 64 * 1024;

/// Maximum avatar payload accepted by the avatar route.
const MAX_AVATAR_BYTES: usize = 5 * 1024 * 1024;

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
}

/// Shared application state.
#[derive(Clone)]
struct AppState {
    pool: sqlx::PgPool,
    config: Config,
    gateway: Gateway,
    pending_uploads: PendingUploads,
    agent_control: ManagedAgentControlRegistry,
}

/// Initialized Rift resources that Henosis may inspect before binding the listener.
pub struct InitializedRuntime {
    /// Complete HTTP and WebSocket router over the initialized persistence layer.
    app: Router,
    /// Shared PostgreSQL pool used by routes and room bootstrap.
    pool: sqlx::PgPool,
    /// Validated listener address retained from configuration.
    listen_addr: String,
}

/// Exposes initialized Rift resources to the unified Henosis supervisor.
impl InitializedRuntime {
    /// Borrow the PostgreSQL pool for idempotent room bootstrap.
    pub fn pool(&self) -> &sqlx::PgPool {
        &self.pool
    }

    /// Consume the initialized runtime into the listener address and router.
    pub fn into_parts(self) -> (String, Router) {
        (self.listen_addr, self.app)
    }

    /// Consume the initialized runtime and return only its router.
    pub fn into_router(self) -> Router {
        self.app
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

    let listen_addr = config.listen_addr.clone();
    let app = router_with_control_registry(config, pool.clone(), agent_control);
    Ok(InitializedRuntime {
        app,
        pool,
        listen_addr,
    })
}

/// Construct Rift's complete router over an initialized PostgreSQL pool.
pub fn router(config: Config, pool: sqlx::PgPool) -> Router {
    router_with_control_registry(config, pool, ManagedAgentControlRegistry::default())
}

/// Construct Rift's router with a shared managed execution controller registry.
pub fn router_with_control_registry(
    config: Config,
    pool: sqlx::PgPool,
    agent_control: ManagedAgentControlRegistry,
) -> Router {
    let gateway = Gateway::new();
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

    Router::new()
        // Auth
        .route("/api/auth/register", post(routes::auth::register))
        .route("/api/auth/login", post(routes::auth::login))
        .route("/api/auth/refresh", post(routes::auth::refresh))
        .route("/api/auth/logout", post(routes::auth::logout))
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
        // Bridge internal
        .route("/api/bridge/notify", post(routes::bridge::notify_message))
        .route(
            "/api/bridge/provision",
            post(routes::bridge::provision_agents),
        )
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
            "/api/bridge/servers/{server_id}/status",
            get(routes::bridge_control::daemon_bridge_status),
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
        .with_state(state)
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
    let listen_addr = config.listen_addr.clone();
    let app = build_router(config).await?;
    let listener = tokio::net::TcpListener::bind(&listen_addr).await?;

    tracing::info!("Rift server listening on {listen_addr}");
    axum::serve(listener, app)
        .with_graceful_shutdown(stop)
        .await?;
    Ok(())
}

/// Upgrade an authenticated Rift WebSocket connection into the shared gateway.
async fn ws_handler(ws: WebSocketUpgrade, State(state): State<AppState>) -> impl IntoResponse {
    let jwt_secret = state.config.jwt_secret.clone();
    let gateway = state.gateway.clone();
    let pool = state.pool.clone();
    ws.max_message_size(MAX_WEBSOCKET_BYTES)
        .max_frame_size(MAX_WEBSOCKET_BYTES)
        .on_upgrade(move |socket| async move {
            gateway.handle_connection(socket, jwt_secret, pool).await;
        })
}

/// Exercises security properties of routes that must remain authenticated.
#[cfg(test)]
mod tests {
    use axum::{body::Body, http::Request};
    use sqlx::postgres::PgPoolOptions;
    use tower::ServiceExt;
    use uuid::Uuid;

    use super::{Config, router};

    /// Construct a secret-safe configuration for routes that reject before I/O.
    fn test_config() -> Config {
        Config {
            database_url: "postgresql://rift.invalid/henosis_test".to_string(),
            jwt_secret: "test-jwt-secret-not-for-production".to_string(),
            bridge_secret: "test-bridge-secret-not-for-production".to_string(),
            listen_addr: "127.0.0.1:0".to_string(),
            cors_origins: vec![
                "http://localhost:5173"
                    .parse()
                    .expect("test origin must parse"),
            ],
            upload_dir: "uploads".to_string(),
            max_upload_bytes: 1024,
        }
    }

    /// The current-user permission contract is mounted behind bearer authentication.
    #[tokio::test]
    async fn current_user_permissions_route_requires_authentication() {
        let pool = PgPoolOptions::new()
            .connect_lazy("postgresql://rift.invalid/henosis_test")
            .expect("test database URL must parse");
        let response = router(test_config(), pool)
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
}
