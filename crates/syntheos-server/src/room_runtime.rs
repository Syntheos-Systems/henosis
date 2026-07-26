//! Managed Rift room and Synapse bridge lifecycle.

use std::env;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;

use henosis_broca::BrocaStore;
use henosis_chiasm::ChiasmStore;
use henosis_cognition::Cognition;
use henosis_rift_bridge::config::BridgeConfig;
use henosis_rift_bridge::identity::{bridge_tenant, principal_for_agent};
use henosis_rift_bridge::kleos::{
    BridgeMemory, CognitionMemoryBackend, InProcessKleosClient, KleosClient,
};
use henosis_rift_bridge::runtime::{run_with_dependencies, RuntimeDependencies};
use henosis_rift_server::bootstrap::{bootstrap_managed_room, BootstrapError, ManagedRoomConfig};
use henosis_rift_server::config::{
    parse_cors_origins, parse_upload_limit, validate_secrets, Config as RiftConfig,
};
use henosis_rift_server::runtime::{initialize, InitializedRuntime, RuntimeError};
use synapse_cron::CronScheduler;
use tokio::sync::watch;

/// Environment value enabling the complete managed room.
pub(crate) const REQUIRED_MODE: &str = "required";

/// Environment value reserved for explicit developer-only room suppression.
pub(crate) const DISABLED_MODE: &str = "disabled";

/// Default browser clients allowed to connect to the local Rift API.
const DEFAULT_CORS_ORIGINS: &str = "http://localhost:5173,http://127.0.0.1:5173,tauri://localhost";

/// Default persistent directory for managed scheduled jobs and their results.
const DEFAULT_CRON_DIR: &str = "data/synapse-cron";

/// Complete room configuration after environment validation.
pub struct RoomRuntimeConfig {
    /// Rift HTTP server settings.
    rift: RiftConfig,
    /// Agent behavior configuration, excluding managed connection coordinates.
    bridge_config_path: PathBuf,
    /// Persistent room display settings.
    room: ManagedRoomConfig,
    /// Persistent Synapse cron state owned by this managed runtime.
    cron_dir: PathBuf,
}

/// Whether this Henosis process owns the full room stack.
pub enum RoomRuntimeSelection {
    /// Explicit developer-only mode without Rift or Synapse room services.
    Disabled,
    /// Required production room configuration.
    Required(Box<RoomRuntimeConfig>),
}

/// Prepared server, bridge configuration, and shared kernel dependencies.
pub struct PreparedRoomRuntime {
    /// Initialized Rift router and persistence.
    rift: InitializedRuntime,
    /// Bridge settings targeting the bootstrapped room.
    bridge: BridgeConfig,
    /// Shared kernel handles injected into the bridge.
    dependencies: RuntimeDependencies,
}

/// Failures emitted while configuring or supervising the room stack.
#[derive(Debug, thiserror::Error)]
pub enum RoomRuntimeError {
    /// A required environment variable was missing or invalid Unicode.
    #[error("{name}: {source}")]
    Environment {
        /// Name of the rejected environment variable.
        name: &'static str,
        /// Environment lookup failure.
        source: env::VarError,
    },
    /// A room environment setting violated its contract.
    #[error("invalid room configuration: {0}")]
    InvalidConfig(String),
    /// Rift persistence or router initialization failed.
    #[error(transparent)]
    Rift(#[from] RuntimeError),
    /// Managed room convergence failed.
    #[error(transparent)]
    Bootstrap(#[from] BootstrapError),
    /// Synapse bridge configuration could not be loaded.
    #[error("Synapse room configuration failed: {0}")]
    BridgeConfig(#[from] henosis_rift_bridge::error::BridgeError),
    /// Persistent scheduled-job state could not be opened.
    #[error("Synapse cron state at {} failed: {source}", path.display())]
    Cron {
        /// Scheduler directory that failed to open.
        path: PathBuf,
        /// Underlying filesystem or serialization failure.
        #[source]
        source: anyhow::Error,
    },
    /// Rift listener setup failed.
    #[error("Rift listener failed: {0}")]
    Listener(#[from] std::io::Error),
    /// A managed component stopped before Henosis requested it.
    #[error("{component} stopped unexpectedly: {detail}")]
    ComponentStopped {
        /// Stable component name.
        component: &'static str,
        /// Component result or task failure.
        detail: String,
    },
    /// The bridge failed while honoring a coordinated stop.
    #[error("Synapse bridge stop failed: {0}")]
    BridgeStop(#[source] anyhow::Error),
}

/// Load the room mode and all settings required by the selected mode.
pub fn room_runtime_from_environment() -> Result<RoomRuntimeSelection, RoomRuntimeError> {
    let mode = env::var("HENOSIS_ROOM_MODE").unwrap_or_else(|_| REQUIRED_MODE.to_string());
    match parse_room_mode(&mode)? {
        RoomMode::Disabled => Ok(RoomRuntimeSelection::Disabled),
        RoomMode::Required => Ok(RoomRuntimeSelection::Required(Box::new(
            RoomRuntimeConfig::from_environment()?,
        ))),
    }
}

/// Prepare the managed room over the exact kernel handles used by Henosis.
pub async fn prepare_room_runtime(
    config: RoomRuntimeConfig,
    chiasm: Arc<ChiasmStore>,
    broca: Arc<BrocaStore>,
    cognition: Arc<Cognition>,
) -> Result<PreparedRoomRuntime, RoomRuntimeError> {
    let jwt_secret = config.rift.jwt_secret.clone();
    let bridge_secret = config.rift.bridge_secret.clone();
    let (api_url, ws_url) = internal_urls(&config.rift.listen_addr)?;
    let rift = initialize(config.rift).await?;
    let room = bootstrap_managed_room(rift.pool(), config.room).await?;
    let bridge = BridgeConfig::load_for_managed_room(
        &config.bridge_config_path,
        api_url,
        ws_url,
        jwt_secret,
        bridge_secret,
        room.server_id,
        room.channel_id,
    )?;
    let cron_scheduler =
        CronScheduler::open(&config.cron_dir).map_err(|source| RoomRuntimeError::Cron {
            path: config.cron_dir.clone(),
            source,
        })?;
    let memory: Arc<dyn BridgeMemory> = Arc::new(CognitionMemoryBackend::new(cognition));
    let kleos: Arc<dyn KleosClient> = Arc::new(InProcessKleosClient::new(
        chiasm,
        broca,
        memory,
        bridge_tenant(),
        principal_for_agent("rift-bridge"),
    ));
    Ok(PreparedRoomRuntime {
        rift,
        bridge,
        dependencies: RuntimeDependencies {
            kleos: Some(kleos),
            cron_scheduler: Some(cron_scheduler),
        },
    })
}

/// Supervise Rift and the Synapse bridge until the parent requests a stop.
impl PreparedRoomRuntime {
    /// Bind Rift before the bridge provisions agents, then supervise both tasks.
    pub async fn run(self, mut stop: watch::Receiver<bool>) -> Result<(), RoomRuntimeError> {
        let (listen_addr, app) = self.rift.into_parts();
        let listener = tokio::net::TcpListener::bind(&listen_addr).await?;
        let (component_stop_tx, component_stop_rx) = watch::channel(false);
        let mut server_task = tokio::spawn(async move { axum::serve(listener, app).await });
        let mut bridge_task = tokio::spawn(run_with_dependencies(
            self.bridge,
            self.dependencies,
            wait_for_stop(component_stop_rx),
        ));

        tokio::select! {
            _ = wait_for_stop_ref(&mut stop) => {
                let _ = component_stop_tx.send(true);
                let bridge_result = bridge_task.await;
                server_task.abort();
                let _ = server_task.await;
                match bridge_result {
                    Ok(Ok(())) => Ok(()),
                    Ok(Err(error)) => Err(RoomRuntimeError::BridgeStop(error)),
                    Err(error) => Err(RoomRuntimeError::ComponentStopped {
                        component: "Synapse bridge",
                        detail: error.to_string(),
                    }),
                }
            }
            result = &mut server_task => {
                let _ = component_stop_tx.send(true);
                let _ = bridge_task.await;
                Err(RoomRuntimeError::ComponentStopped {
                    component: "Rift server",
                    detail: task_result_detail(result),
                })
            }
            result = &mut bridge_task => {
                server_task.abort();
                let _ = server_task.await;
                Err(RoomRuntimeError::ComponentStopped {
                    component: "Synapse bridge",
                    detail: task_result_detail(result),
                })
            }
        }
    }
}

/// Internal room mode after strict parsing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RoomMode {
    /// The complete room is required.
    Required,
    /// A developer explicitly suppressed room startup.
    Disabled,
}

/// Parse an exact room mode without silently accepting typos.
pub(crate) fn parse_room_mode(value: &str) -> Result<RoomMode, RoomRuntimeError> {
    match value {
        REQUIRED_MODE => Ok(RoomMode::Required),
        DISABLED_MODE => Ok(RoomMode::Disabled),
        other => Err(RoomRuntimeError::InvalidConfig(format!(
            "HENOSIS_ROOM_MODE must be {REQUIRED_MODE:?} or {DISABLED_MODE:?}, got {other:?}"
        ))),
    }
}

/// Load required environment state only after room mode selects it.
impl RoomRuntimeConfig {
    /// Build the server and managed room configuration.
    fn from_environment() -> Result<Self, RoomRuntimeError> {
        let jwt_secret = required_env("HENOSIS_RIFT_JWT_SECRET")?;
        let bridge_secret = required_env("HENOSIS_RIFT_BRIDGE_SECRET")?;
        validate_secrets(&jwt_secret, &bridge_secret).map_err(RoomRuntimeError::InvalidConfig)?;
        let listen_addr =
            env::var("HENOSIS_RIFT_ADDR").unwrap_or_else(|_| "127.0.0.1:3200".to_string());
        internal_urls(&listen_addr)?;
        let cors = env::var("HENOSIS_RIFT_CORS_ORIGINS")
            .unwrap_or_else(|_| DEFAULT_CORS_ORIGINS.to_string());
        let cors_origins = parse_cors_origins(&cors).map_err(RoomRuntimeError::InvalidConfig)?;
        let max_upload = env::var("HENOSIS_RIFT_MAX_UPLOAD_BYTES").ok();
        let max_upload_bytes =
            parse_upload_limit(max_upload.as_deref()).map_err(RoomRuntimeError::InvalidConfig)?;
        Ok(Self {
            rift: RiftConfig {
                database_url: required_env("HENOSIS_RIFT_DATABASE_URL")?,
                jwt_secret,
                bridge_secret,
                listen_addr,
                cors_origins,
                upload_dir: env::var("HENOSIS_RIFT_UPLOAD_DIR")
                    .unwrap_or_else(|_| "data/rift-uploads".to_string()),
                max_upload_bytes,
            },
            bridge_config_path: PathBuf::from(required_env("HENOSIS_RIFT_BRIDGE_CONFIG")?),
            room: ManagedRoomConfig {
                server_name: env::var("HENOSIS_RIFT_SERVER_NAME")
                    .unwrap_or_else(|_| "Henosis".to_string()),
                channel_name: env::var("HENOSIS_RIFT_CHANNEL_NAME")
                    .unwrap_or_else(|_| "general".to_string()),
            },
            cron_dir: PathBuf::from(env_or_default("HENOSIS_CRON_DIR", DEFAULT_CRON_DIR)?),
        })
    }
}

/// Read one optional Unicode environment setting with a deterministic default.
fn env_or_default(name: &'static str, default: &str) -> Result<String, RoomRuntimeError> {
    match env::var(name) {
        Ok(value) => Ok(value),
        Err(env::VarError::NotPresent) => Ok(default.to_string()),
        Err(source) => Err(RoomRuntimeError::Environment { name, source }),
    }
}

/// Read one mandatory Unicode environment setting.
fn required_env(name: &'static str) -> Result<String, RoomRuntimeError> {
    env::var(name).map_err(|source| RoomRuntimeError::Environment { name, source })
}

/// Derive loopback-safe internal HTTP and WebSocket endpoints from a listener.
fn internal_urls(listen_addr: &str) -> Result<(String, String), RoomRuntimeError> {
    let parsed = listen_addr.parse::<SocketAddr>().map_err(|error| {
        RoomRuntimeError::InvalidConfig(format!(
            "HENOSIS_RIFT_ADDR {listen_addr:?} is not a socket address: {error}"
        ))
    })?;
    let internal_ip = match parsed.ip() {
        IpAddr::V4(ip) if ip.is_unspecified() => IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
        IpAddr::V6(ip) if ip.is_unspecified() => IpAddr::V6(std::net::Ipv6Addr::LOCALHOST),
        ip => ip,
    };
    let internal = SocketAddr::new(internal_ip, parsed.port());
    Ok((format!("http://{internal}"), format!("ws://{internal}/ws")))
}

/// Wait until a component-local stop receiver becomes true or disconnects.
async fn wait_for_stop(mut receiver: watch::Receiver<bool>) {
    if *receiver.borrow() {
        return;
    }
    while receiver.changed().await.is_ok() {
        if *receiver.borrow() {
            return;
        }
    }
}

/// Wait on a borrowed parent stop receiver.
async fn wait_for_stop_ref(receiver: &mut watch::Receiver<bool>) {
    if *receiver.borrow() {
        return;
    }
    while receiver.changed().await.is_ok() {
        if *receiver.borrow() {
            return;
        }
    }
}

/// Render a nested task result without discarding either error layer.
fn task_result_detail<T, E>(result: Result<Result<T, E>, tokio::task::JoinError>) -> String
where
    E: std::fmt::Display,
{
    match result {
        Ok(Ok(_)) => "completed successfully".to_string(),
        Ok(Err(error)) => error.to_string(),
        Err(error) => error.to_string(),
    }
}

/// Pure configuration and endpoint tests.
#[cfg(test)]
mod tests {
    use super::{internal_urls, parse_room_mode, RoomMode, DEFAULT_CRON_DIR};

    /// Production room startup is the default contract value.
    #[test]
    fn required_mode_is_accepted() {
        assert_eq!(parse_room_mode("required").unwrap(), RoomMode::Required);
    }

    /// Suppression must be exact so a typo cannot remove the room.
    #[test]
    fn invalid_mode_fails_closed() {
        assert!(parse_room_mode("off").is_err());
        assert!(parse_room_mode("disabled ").is_err());
    }

    /// Managed cron state defaults beneath the runtime's existing data directory.
    #[test]
    fn cron_state_uses_runtime_local_default() {
        assert_eq!(DEFAULT_CRON_DIR, "data/synapse-cron");
    }

    /// Wildcard listeners produce loopback internal bridge endpoints.
    #[test]
    fn wildcard_listener_uses_loopback_internally() {
        let (api, ws) = internal_urls("0.0.0.0:3200").unwrap();
        assert_eq!(api, "http://127.0.0.1:3200");
        assert_eq!(ws, "ws://127.0.0.1:3200/ws");
    }
}
