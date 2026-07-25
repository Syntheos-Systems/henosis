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

use crate::{config, routes, ws};

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

/// Initialize Rift persistence and construct its complete HTTP router.
pub async fn build_router(config: Config) -> Result<Router, RuntimeError> {
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

    Ok(router(config, pool))
}

/// Construct Rift's complete router over an initialized PostgreSQL pool.
pub fn router(config: Config, pool: sqlx::PgPool) -> Router {
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
    };

    let cors = CorsLayer::new()
        .allow_origin(AllowOrigin::list(config.cors_origins.clone()))
        .allow_methods([
            Method::GET,
            Method::POST,
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
