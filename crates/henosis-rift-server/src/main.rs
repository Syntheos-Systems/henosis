use axum::{
    extract::{
        ws::WebSocketUpgrade,
        State,
    },
    http::{header, HeaderValue, Method},
    response::IntoResponse,
    routing::{delete, get, patch, post, put},
    Router,
};
use sqlx::postgres::PgPoolOptions;
use tower::ServiceBuilder;
use tower_http::cors::{AllowOrigin, Any, CorsLayer};
use tower_http::set_header::SetResponseHeaderLayer;
use tower_http::services::ServeDir;
use tower_http::trace::TraceLayer;

// Use the library target rather than re-declaring the module tree. Declaring
// `mod auth; mod db; ...` here compiled every module a second time into the
// binary, and that private copy is what produced the dead-code warnings: items
// reachable only from the library's public API looked unused in the bin's copy.
use henosis_rift_server::{config, routes, ws};

use config::Config;
use routes::upload::PendingUploads;
use ws::gateway::Gateway;

/// Shared application state
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

/// Start the standalone Rift server with its database, routes, and trust boundaries.
#[tokio::main]
async fn main() {
    dotenvy::dotenv().ok();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "henosis_rift_server=debug,tower_http=info".into()),
        )
        .init();

    let config = Config::from_env();
    let listen_addr = config.listen_addr.clone();

    // Database pool
    let pool = PgPoolOptions::new()
        .max_connections(20)
        .connect(&config.database_url)
        .await
        .expect("Failed to connect to database");

    tracing::info!("Connected to database");

    // Apply embedded migrations on boot. The standalone rift-server applied
    // migrations externally; in the henosis workspace the binary self-migrates
    // so a fresh deploy converges without an out-of-band `sqlx migrate run`.
    sqlx::migrate!("./migrations")
        .run(&pool)
        .await
        .expect("Failed to run migrations");

    tracing::info!("Migrations applied");

    // Create uploads directory
    tokio::fs::create_dir_all(&config.upload_dir)
        .await
        .expect("Failed to create upload dir");

    let gateway = Gateway::new();
    let pending_uploads: PendingUploads = std::sync::Arc::new(dashmap::DashMap::new());

    let upload_dir = config.upload_dir.clone();

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

    let app = Router::new()
        // Auth
        .route("/api/auth/register", post(routes::auth::register))
        .route("/api/auth/login", post(routes::auth::login))
        .route("/api/auth/refresh", post(routes::auth::refresh))
        .route("/api/auth/logout", post(routes::auth::logout))
        // Users
        .route("/api/users/@me", get(routes::users::get_me).patch(routes::users::update_me))
        .route("/api/users/@me/avatar", post(routes::users::upload_avatar))
        .route("/api/users/@me/password", post(routes::users::change_password))
        .route("/api/users/@me/dms", get(routes::users::list_dms).post(routes::users::create_dm))
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
        .route("/api/invites/{code}/join", post(routes::servers::join_via_invite))
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
        .route("/api/upload", post(routes::upload::upload_files))
        // Bridge internal
        .route("/api/bridge/notify", post(routes::bridge::notify_message))
        .route(
            "/api/bridge/provision",
            post(routes::bridge::provision_agents),
        )
        .route("/api/bridge/pause", post(routes::bridge_control::pause_bridge))
        .route("/api/bridge/resume", post(routes::bridge_control::resume_bridge))
        .route("/api/bridge/status", get(routes::bridge_control::bridge_status))
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
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(&listen_addr)
        .await
        .expect("Failed to bind");

    tracing::info!("Rift server listening on {listen_addr}");
    axum::serve(listener, app).await.expect("Server error");
}

/// Upgrade an authenticated Rift WebSocket connection into the shared gateway.
async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
) -> impl IntoResponse {
    let jwt_secret = state.config.jwt_secret.clone();
    let gateway = state.gateway.clone();
    let pool = state.pool.clone();
    ws.on_upgrade(move |socket| async move {
        gateway.handle_connection(socket, jwt_secret, pool).await;
    })
}
