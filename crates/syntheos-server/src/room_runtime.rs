//! Managed Rift room and Synapse bridge lifecycle.

use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use henosis_broca::BrocaStore;
use henosis_chiasm::ChiasmStore;
use henosis_cognition::Cognition;
use henosis_rift_bridge::config::BridgeConfig;
use henosis_rift_bridge::identity::{bridge_tenant, principal_for_agent};
use henosis_rift_bridge::kleos::{
    BridgeMemory, CognitionMemoryBackend, InProcessKleosClient, KleosClient,
};
use henosis_rift_bridge::runtime::RuntimeDependencies;
use henosis_rift_server::agent_control::ManagedAgentControlRegistry;
use henosis_rift_server::auth::jwt::{
    derive_managed_agent_jwt_secret, derive_managed_bridge_route_secret,
};
use henosis_rift_server::bootstrap::{bootstrap_managed_room, BootstrapError, ManagedRoomConfig};
use henosis_rift_server::config::{
    parse_cors_origins, parse_upload_limit, validate_listener_topology, validate_secrets,
    Config as RiftConfig,
};
use henosis_rift_server::outbox::{OutboxDispatchError, OutboxDispatcher};
use henosis_rift_server::runtime::{
    initialize_with_control_registry, InitializedRuntime, InitializedRuntimeParts,
    LimitedConnectionInfo, LimitedTcpListener, RuntimeError,
};
use synapse_cron::CronScheduler;
use tokio::sync::watch;
use uuid::Uuid;
use zeroize::Zeroizing;

use crate::room_reconciler::{
    build_room_reconciler, credential_binding_resolver_from_environment, RoomReconciler,
};

/// Environment value enabling the complete managed room.
pub(crate) const REQUIRED_MODE: &str = "required";

/// Environment value reserved for explicit developer-only room suppression.
pub(crate) const DISABLED_MODE: &str = "disabled";

/// Default browser clients allowed to connect to the local Rift API.
const DEFAULT_CORS_ORIGINS: &str = "http://localhost:5173,http://127.0.0.1:5173,tauri://localhost";

/// Default persistent directory for managed scheduled jobs and their results.
const DEFAULT_CRON_DIR: &str = "data/synapse-cron";

/// Default feature-gate value keeping deployment TOML authoritative.
const DEFAULT_AGENT_CONTROL: &str = "0";

/// Maximum time managed components receive for a coordinated stop before abort.
const MANAGED_COMPONENT_STOP_TIMEOUT: Duration = Duration::from_secs(5);

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
    /// Whether Rift desired-state writes and reconciliation are enabled.
    agent_control_enabled: bool,
}

/// Whether this Henosis process owns the full room stack.
pub enum RoomRuntimeSelection {
    /// Explicit developer-only mode without Rift or Synapse room services.
    Disabled,
    /// Required production room configuration.
    Required(Box<RoomRuntimeConfig>),
}

/// Prepared server and the desired-state supervisor owning the bridge.
pub struct PreparedRoomRuntime {
    /// Initialized Rift routers and persistence.
    rift: InitializedRuntime,
    /// Reconciler supervising the bridge against durable desired state.
    reconciler: RoomReconciler,
}

/// Failures emitted while configuring or supervising the room stack.
#[derive(Debug, thiserror::Error)]
pub enum RoomRuntimeError {
    /// A required environment variable was missing, conflicted, or not Unicode.
    #[error("{0}")]
    Environment(String),
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
    /// One named Rift listener could not be bound.
    #[error("{component} bind failed: {source}")]
    Listener {
        /// Stable listener component name.
        component: &'static str,
        /// Underlying socket bind failure.
        #[source]
        source: std::io::Error,
    },
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
    let mode = env_or_default("SYNTHEOS_ROOM_MODE", REQUIRED_MODE)?;
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
    let managed_authority_root = Zeroizing::new(config.rift.jwt_secret.clone());
    let (api_url, ws_url) = internal_urls(&config.rift.listen_addr)?;
    let (_, bridge_listen_addr) = validate_listener_topology(
        &config.rift.listen_addr,
        &config.rift.bridge_listen_addr,
        config.rift.allow_remote_listen,
    )
    .map_err(RoomRuntimeError::InvalidConfig)?;
    let bridge_api_url = format!("http://{bridge_listen_addr}");
    let agent_control = ManagedAgentControlRegistry::default();
    let rift = initialize_with_control_registry(config.rift, agent_control.clone()).await?;
    let room = bootstrap_managed_room(rift.pool(), config.room).await?;
    let bootstrap_fence = henosis_rift_server::models::leadership::RoomFence {
        server_id: room.server_id,
        epoch: 0,
        lease_id: Uuid::nil(),
    };
    let bootstrap_agent_signing_key = derive_managed_agent_jwt_secret(
        &managed_authority_root,
        &bootstrap_fence,
    )
    .map_err(|_| {
        RoomRuntimeError::InvalidConfig("managed agent signing key derivation failed".to_string())
    })?;
    let bootstrap_bridge_route_key = derive_managed_bridge_route_secret(
        &managed_authority_root,
        &bootstrap_fence,
    )
    .map_err(|_| {
        RoomRuntimeError::InvalidConfig("managed bridge route key derivation failed".to_string())
    })?;
    let bridge = BridgeConfig::load_for_managed_room(
        &config.bridge_config_path,
        api_url,
        bridge_api_url,
        ws_url,
        bootstrap_agent_signing_key,
        bootstrap_bridge_route_key,
        room.server_id,
        room.channel_id,
    )?;
    // Validate the durable scheduler store now so a misconfigured directory
    // fails preparation; each bridge generation re-opens it from disk.
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
    let bindings = credential_binding_resolver_from_environment().map_err(|error| {
        RoomRuntimeError::InvalidConfig(format!("credential binding configuration failed: {error}"))
    })?;
    let managed_control = config
        .agent_control_enabled
        .then_some((room.owner_id, agent_control));
    let reconciler = build_room_reconciler(
        rift.pool().clone(),
        bridge,
        RuntimeDependencies {
            kleos: Some(kleos),
            cron_dir: Some(config.cron_dir.clone()),
            managed_fence: None,
        },
        bindings,
        managed_control,
        managed_authority_root,
    );
    Ok(PreparedRoomRuntime { rift, reconciler })
}

/// Supervise Rift and the bridge reconciler until the parent requests a stop.
impl PreparedRoomRuntime {
    /// Bind both Rift trust boundaries before provisioning agents, then supervise all components.
    ///
    /// Rift exit stays fatal for the whole room runtime. Bridge failures are
    /// absorbed inside the reconciler and never abort Rift; only the
    /// reconciler itself exiting is fatal here.
    pub async fn run(self, mut stop: watch::Receiver<bool>) -> Result<(), RoomRuntimeError> {
        let bound = bind_rift_listeners(self.rift.into_parts()).await?;
        let (component_stop_tx, component_stop_rx) = watch::channel(false);
        let public_service = bound
            .public_app
            .into_make_service_with_connect_info::<LimitedConnectionInfo>();
        let bridge_service = bound
            .bridge_app
            .into_make_service_with_connect_info::<LimitedConnectionInfo>();
        let mut public_server_task =
            tokio::spawn(async move { axum::serve(bound.public_listener, public_service).await });
        let mut bridge_server_task =
            tokio::spawn(async move { axum::serve(bound.bridge_listener, bridge_service).await });
        let mut outbox_task = tokio::spawn(bound.outbox_dispatcher.run(component_stop_rx.clone()));
        let mut reconciler_task = tokio::spawn(self.reconciler.run(component_stop_rx));

        tokio::select! {
            _ = wait_for_stop_ref(&mut stop) => {
                // Stop the reconciler first so the bridge is fully down before
                // either Rift listener it talks to goes away.
                let _ = component_stop_tx.send(true);
                let (reconciler_result, outbox_result) =
                    tokio::join!(
                        await_task_until_or_abort(
                            &mut reconciler_task,
                            "room reconciler",
                            MANAGED_COMPONENT_STOP_TIMEOUT,
                        ),
                        await_task_until_or_abort(
                            &mut outbox_task,
                            "Rift event outbox dispatcher",
                            MANAGED_COMPONENT_STOP_TIMEOUT,
                        ),
                    );
                public_server_task.abort();
                bridge_server_task.abort();
                let _ = public_server_task.await;
                let _ = bridge_server_task.await;
                let outbox_result = outbox_result.ok_or_else(|| {
                    RoomRuntimeError::ComponentStopped {
                        component: "Rift event outbox dispatcher",
                        detail: "coordinated stop exceeded 5 seconds and was aborted".to_string(),
                    }
                })?;
                match outbox_result {
                    Ok(Ok(())) => {}
                    other => return Err(outbox_component_stopped(other)),
                }
                let reconciler_result = reconciler_result.ok_or_else(|| {
                    RoomRuntimeError::ComponentStopped {
                        component: "room reconciler",
                        detail: "coordinated stop exceeded 5 seconds and was aborted".to_string(),
                    }
                })?;
                match reconciler_result {
                    Ok(Ok(())) => Ok(()),
                    Ok(Err(detail)) => Err(RoomRuntimeError::BridgeStop(anyhow::anyhow!(detail))),
                    Err(error) => Err(RoomRuntimeError::ComponentStopped {
                        component: "room reconciler",
                        detail: error.to_string(),
                    }),
                }
            }
            result = &mut public_server_task => {
                // Either Rift boundary exiting is fatal; stop the bridge and
                // its peer listener before reporting the exact component.
                let _ = component_stop_tx.send(true);
                let _ = tokio::join!(
                    await_task_until_or_abort(
                        &mut reconciler_task,
                        "room reconciler",
                        MANAGED_COMPONENT_STOP_TIMEOUT,
                    ),
                    await_task_until_or_abort(
                        &mut outbox_task,
                        "Rift event outbox dispatcher",
                        MANAGED_COMPONENT_STOP_TIMEOUT,
                    ),
                );
                bridge_server_task.abort();
                let _ = bridge_server_task.await;
                Err(RoomRuntimeError::ComponentStopped {
                    component: "Rift public listener",
                    detail: task_result_detail(result),
                })
            }
            result = &mut bridge_server_task => {
                let _ = component_stop_tx.send(true);
                let _ = tokio::join!(
                    await_task_until_or_abort(
                        &mut reconciler_task,
                        "room reconciler",
                        MANAGED_COMPONENT_STOP_TIMEOUT,
                    ),
                    await_task_until_or_abort(
                        &mut outbox_task,
                        "Rift event outbox dispatcher",
                        MANAGED_COMPONENT_STOP_TIMEOUT,
                    ),
                );
                public_server_task.abort();
                let _ = public_server_task.await;
                Err(RoomRuntimeError::ComponentStopped {
                    component: "Rift bridge listener",
                    detail: task_result_detail(result),
                })
            }
            result = &mut reconciler_task => {
                let _ = component_stop_tx.send(true);
                let _ = await_task_until_or_abort(
                    &mut outbox_task,
                    "Rift event outbox dispatcher",
                    MANAGED_COMPONENT_STOP_TIMEOUT,
                ).await;
                public_server_task.abort();
                bridge_server_task.abort();
                let _ = public_server_task.await;
                let _ = bridge_server_task.await;
                Err(RoomRuntimeError::ComponentStopped {
                    component: "room reconciler",
                    detail: task_result_detail(result),
                })
            }
            result = &mut outbox_task => {
                let _ = component_stop_tx.send(true);
                let _ = await_task_until_or_abort(
                    &mut reconciler_task,
                    "room reconciler",
                    MANAGED_COMPONENT_STOP_TIMEOUT,
                ).await;
                public_server_task.abort();
                bridge_server_task.abort();
                let _ = public_server_task.await;
                let _ = bridge_server_task.await;
                Err(outbox_component_stopped(result))
            }
        }
    }
}

/// Two successfully bound Rift listeners paired with their least-privilege routers.
struct BoundRiftListeners {
    /// Public human, agent, upload, and WebSocket socket.
    public_listener: LimitedTcpListener,
    /// Public human, agent, upload, and WebSocket router.
    public_app: axum::Router,
    /// Loopback-only bridge-secret socket.
    bridge_listener: LimitedTcpListener,
    /// Private bridge-secret router.
    bridge_app: axum::Router,
    /// Durable message-event publisher supervised with the managed room.
    outbox_dispatcher: OutboxDispatcher,
}

/// Bind both Rift sockets before returning either router to a serving task.
async fn bind_rift_listeners(
    parts: InitializedRuntimeParts,
) -> Result<BoundRiftListeners, RoomRuntimeError> {
    let public_listener = tokio::net::TcpListener::bind(parts.public_listen_addr)
        .await
        .map_err(|source| RoomRuntimeError::Listener {
            component: "Rift public listener",
            source,
        })?;
    let bridge_listener = tokio::net::TcpListener::bind(parts.bridge_listen_addr)
        .await
        .map_err(|source| RoomRuntimeError::Listener {
            component: "Rift bridge listener",
            source,
        })?;
    Ok(BoundRiftListeners {
        public_listener: LimitedTcpListener::for_public(public_listener),
        public_app: parts.public_app,
        bridge_listener: LimitedTcpListener::for_bridge(bridge_listener),
        bridge_app: parts.bridge_app,
        outbox_dispatcher: parts.outbox_dispatcher,
    })
}

/// Convert a completed dispatcher task into the managed room's fatal component error.
fn outbox_component_stopped(
    result: Result<Result<(), OutboxDispatchError>, tokio::task::JoinError>,
) -> RoomRuntimeError {
    RoomRuntimeError::ComponentStopped {
        component: "Rift event outbox dispatcher",
        detail: task_result_detail(result),
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
            "SYNTHEOS_ROOM_MODE must be {REQUIRED_MODE:?} or {DISABLED_MODE:?}, got {other:?}"
        ))),
    }
}

/// Parse the exact opt-in managed-agent feature gate.
pub(crate) fn parse_agent_control(value: &str) -> Result<bool, RoomRuntimeError> {
    match value {
        "0" => Ok(false),
        "1" => Ok(true),
        other => Err(RoomRuntimeError::InvalidConfig(format!(
            "SYNTHEOS_ROOM_AGENT_CONTROL must be \"0\" or \"1\", got {other:?}"
        ))),
    }
}

/// Parse the exact managed acknowledgement required for remote public Rift ingress.
fn parse_remote_listen_ack(value: &str) -> Result<bool, RoomRuntimeError> {
    match value {
        "0" => Ok(false),
        "1" => Ok(true),
        other => Err(RoomRuntimeError::InvalidConfig(format!(
            "SYNTHEOS_RIFT_ALLOW_REMOTE_LISTEN must be \"0\" or \"1\", got {other:?}"
        ))),
    }
}

/// Load required environment state only after room mode selects it.
impl RoomRuntimeConfig {
    /// Build the server and managed room configuration.
    fn from_environment() -> Result<Self, RoomRuntimeError> {
        let jwt_secret = required_env("SYNTHEOS_RIFT_JWT_SECRET")?;
        let agent_jwt_secret = required_env("SYNTHEOS_RIFT_AGENT_JWT_SECRET")?;
        let bridge_secret = required_env("SYNTHEOS_RIFT_BRIDGE_SECRET")?;
        validate_managed_rift_secrets(&jwt_secret, &agent_jwt_secret, &bridge_secret)?;
        let listen_addr = env_or_default("SYNTHEOS_RIFT_ADDR", "127.0.0.1:3200")?;
        let bridge_listen_addr = env_or_default("SYNTHEOS_RIFT_BRIDGE_ADDR", "127.0.0.1:3201")?;
        let allow_remote_listen =
            parse_remote_listen_ack(&env_or_default("SYNTHEOS_RIFT_ALLOW_REMOTE_LISTEN", "0")?)?;
        validate_listener_topology(&listen_addr, &bridge_listen_addr, allow_remote_listen)
            .map_err(RoomRuntimeError::InvalidConfig)?;
        internal_urls(&listen_addr)?;
        let cors = env_or_default("SYNTHEOS_RIFT_CORS_ORIGINS", DEFAULT_CORS_ORIGINS)?;
        let cors_origins = parse_cors_origins(&cors).map_err(RoomRuntimeError::InvalidConfig)?;
        let max_upload = optional_env("SYNTHEOS_RIFT_MAX_UPLOAD_BYTES")?;
        let max_upload_bytes =
            parse_upload_limit(max_upload.as_deref()).map_err(RoomRuntimeError::InvalidConfig)?;
        let agent_control_enabled = parse_agent_control(&env_or_default(
            "SYNTHEOS_ROOM_AGENT_CONTROL",
            DEFAULT_AGENT_CONTROL,
        )?)?;
        Ok(Self {
            rift: RiftConfig {
                database_url: required_env("SYNTHEOS_RIFT_DATABASE_URL")?,
                jwt_secret,
                agent_jwt_secret,
                bridge_secret,
                listen_addr,
                bridge_listen_addr,
                allow_remote_listen,
                cors_origins,
                upload_dir: env_or_default("SYNTHEOS_RIFT_UPLOAD_DIR", "data/rift-uploads")?,
                max_upload_bytes,
            },
            bridge_config_path: PathBuf::from(required_env("SYNTHEOS_RIFT_BRIDGE_CONFIG")?),
            room: ManagedRoomConfig {
                server_name: env_or_default("SYNTHEOS_RIFT_SERVER_NAME", "Henosis")?,
                channel_name: env_or_default("SYNTHEOS_RIFT_CHANNEL_NAME", "general")?,
            },
            cron_dir: PathBuf::from(env_or_default("SYNTHEOS_CRON_DIR", DEFAULT_CRON_DIR)?),
            agent_control_enabled,
        })
    }
}

/// Enforce independent human-session, agent-session, and private-route authorities.
fn validate_managed_rift_secrets(
    jwt_secret: &str,
    agent_jwt_secret: &str,
    bridge_secret: &str,
) -> Result<(), RoomRuntimeError> {
    validate_secrets(jwt_secret, agent_jwt_secret, bridge_secret)
        .map_err(RoomRuntimeError::InvalidConfig)
}

/// Read one optional Unicode environment setting with a deterministic default.
fn env_or_default(name: &'static str, default: &str) -> Result<String, RoomRuntimeError> {
    Ok(optional_env(name)?.unwrap_or_else(|| default.to_string()))
}

/// Read one optional Unicode environment setting through the rename boundary,
/// where canonical `SYNTHEOS_*` keys honor their read-only legacy aliases.
fn optional_env(name: &'static str) -> Result<Option<String>, RoomRuntimeError> {
    syntheos_env::resolve_name(name)
        .map_err(|error| RoomRuntimeError::Environment(error.to_string()))
}

/// Read one mandatory Unicode environment setting through the rename boundary.
fn required_env(name: &'static str) -> Result<String, RoomRuntimeError> {
    optional_env(name)?.ok_or_else(|| RoomRuntimeError::Environment(format!("{name} is required")))
}

/// Derive loopback-safe internal HTTP and WebSocket endpoints from a listener.
fn internal_urls(listen_addr: &str) -> Result<(String, String), RoomRuntimeError> {
    let parsed = listen_addr.parse::<SocketAddr>().map_err(|error| {
        RoomRuntimeError::InvalidConfig(format!(
            "SYNTHEOS_RIFT_ADDR {listen_addr:?} is not a socket address: {error}"
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

/// Await one managed task for a bounded interval and abort it if it does not stop.
async fn await_task_until_or_abort<T>(
    task: &mut tokio::task::JoinHandle<T>,
    component: &'static str,
    timeout: Duration,
) -> Option<Result<T, tokio::task::JoinError>> {
    match tokio::time::timeout(timeout, &mut *task).await {
        Ok(result) => Some(result),
        Err(_) => {
            tracing::warn!(
                component,
                ?timeout,
                "managed component stop timed out; aborting task"
            );
            task.abort();
            let _ = task.await;
            None
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
    use std::time::Duration;

    use axum::Router;
    use henosis_rift_server::outbox::OutboxDispatcher;
    use henosis_rift_server::runtime::InitializedRuntimeParts;
    use henosis_rift_server::ws::gateway::Gateway;
    use sqlx::postgres::PgPoolOptions;
    use tokio::sync::watch;

    use super::{
        await_task_until_or_abort, bind_rift_listeners, internal_urls, outbox_component_stopped,
        parse_agent_control, parse_remote_listen_ack, parse_room_mode,
        validate_managed_rift_secrets, RoomMode, RoomRuntimeError, DEFAULT_AGENT_CONTROL,
        DEFAULT_CRON_DIR,
    };

    /// Construct a dispatcher backed by a lazy test-only PostgreSQL pool.
    fn test_outbox_dispatcher() -> OutboxDispatcher {
        let pool = PgPoolOptions::new()
            .connect_lazy("postgresql://rift.invalid/henosis_test")
            .expect("test database URL must parse");
        OutboxDispatcher::new(pool, Gateway::new())
    }

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

    /// Managed roster writes require the exact numeric opt-in value.
    #[test]
    fn agent_control_gate_accepts_only_zero_or_one() {
        assert!(!parse_agent_control("0").unwrap());
        assert!(parse_agent_control("1").unwrap());
        for invalid in ["", "true", "enabled", "2", "1 "] {
            assert!(parse_agent_control(invalid).is_err());
        }
    }

    /// Remote public ingress requires the exact documented acknowledgement.
    #[test]
    fn remote_listener_acknowledgement_is_exact() {
        assert!(!parse_remote_listen_ack("0").unwrap());
        assert!(parse_remote_listen_ack("1").unwrap());
        for invalid in ["", "true", "yes", "2", "1 "] {
            assert!(parse_remote_listen_ack(invalid).is_err());
        }
    }

    /// Managed startup rejects every possible reuse across its three Rift secrets.
    #[test]
    fn managed_rift_secrets_must_be_pairwise_distinct() {
        let human = "human-jwt-secret-that-is-at-least-32-bytes";
        let agent = "agent-jwt-secret-that-is-at-least-32-bytes";
        let bridge = "bridge-secret-that-is-at-least-32-bytes";
        assert!(validate_managed_rift_secrets(human, agent, bridge).is_ok());
        assert!(validate_managed_rift_secrets(human, human, bridge).is_err());
        assert!(validate_managed_rift_secrets(human, agent, human).is_err());
        assert!(validate_managed_rift_secrets(human, agent, agent).is_err());
    }

    /// An absent feature gate resolves to the disabled contract value.
    #[test]
    fn agent_control_defaults_to_disabled() {
        assert_eq!(DEFAULT_AGENT_CONTROL, "0");
        assert!(!parse_agent_control(DEFAULT_AGENT_CONTROL).unwrap());
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

    /// A failed private bind releases the public socket instead of leaving a partial runtime.
    #[tokio::test]
    async fn bridge_bind_failure_never_leaves_public_listener_bound() {
        let public_reservation = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("reserve public test address");
        let public_addr = public_reservation
            .local_addr()
            .expect("read public test address");
        drop(public_reservation);
        let occupied_bridge = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("occupy bridge test address");
        let bridge_addr = occupied_bridge
            .local_addr()
            .expect("read bridge test address");
        let result = bind_rift_listeners(InitializedRuntimeParts {
            public_listen_addr: public_addr,
            public_app: Router::new(),
            bridge_listen_addr: bridge_addr,
            bridge_app: Router::new(),
            outbox_dispatcher: test_outbox_dispatcher(),
        })
        .await;

        match result {
            Err(RoomRuntimeError::Listener { component, .. }) => {
                assert_eq!(component, "Rift bridge listener");
            }
            Err(other) => panic!("unexpected bind failure: {other}"),
            Ok(_) => panic!("occupied bridge address unexpectedly bound"),
        }
        tokio::net::TcpListener::bind(public_addr)
            .await
            .expect("public listener must be released after bridge bind failure");
    }

    /// A managed dispatcher exit is classified as a fatal room component failure.
    #[tokio::test]
    async fn managed_room_treats_outbox_exit_as_fatal() {
        let pool = PgPoolOptions::new()
            .connect_lazy("postgresql://rift.invalid/henosis_test")
            .expect("test database URL must parse");
        pool.close().await;
        let (_stop_tx, stop_rx) = watch::channel(false);
        let result = tokio::spawn(OutboxDispatcher::new(pool, Gateway::new()).run(stop_rx)).await;

        match outbox_component_stopped(result) {
            RoomRuntimeError::ComponentStopped { component, detail } => {
                assert_eq!(component, "Rift event outbox dispatcher");
                assert!(
                    detail.contains("closed pool"),
                    "unexpected detail: {detail}"
                );
            }
            other => panic!("unexpected managed runtime failure: {other}"),
        }
    }

    /// A stalled managed component is aborted within the caller's bounded stop interval.
    #[tokio::test]
    async fn managed_component_stop_aborts_a_stalled_task() {
        let mut task = tokio::spawn(std::future::pending::<()>());

        let result = await_task_until_or_abort(
            &mut task,
            "stalled test component",
            Duration::from_millis(10),
        )
        .await;

        assert!(result.is_none());
        assert!(
            task.is_finished(),
            "the overdue task must be reaped after abort"
        );
    }
}
