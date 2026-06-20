//! Rift agent bridge daemon.
//!
//! Connects to the Rift server via WebSocket, listens for messages,
//! and dispatches agent responses through the room state machine.

use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use tokio::sync::mpsc;

use henosis_rift_bridge::auth::AgentAuthManager;
use henosis_rift_bridge::capability::{PistisOracle, StaticAllowlistOracle};
use henosis_rift_bridge::config::BridgeConfig;
use henosis_rift_bridge::execution::approval::ApprovalRegistry;
use henosis_rift_bridge::execution::sandbox::SandboxManager;

use henosis_rift_bridge::rift_client::{ws_listen, RiftRestClient, RiftWsEvent};
use henosis_rift_bridge::room::Room;

/// Entry point: load config, provision agents, then run the event loop.
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "henosis_rift_bridge=info".into()),
        )
        .init();

    // Load config from command-line arg or default path.
    let config_path = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("config.toml"));

    let config = BridgeConfig::load(&config_path)?;
    tracing::info!("loaded config with {} agents", config.agents.len());

    // Create auth manager and REST client.
    let auth = AgentAuthManager::new(config.rift.jwt_secret.clone());
    let rift = Arc::new(RiftRestClient::new(config.rift.api_url.clone(), auth));
    let kleos = build_kleos_client(&config).await?;

    // Wire execution-mode dependencies from config.
    let oracle: Arc<dyn henosis_rift_bridge::capability::CapabilityOracle> = match &config.pistis {
        Some(pistis) if pistis.enabled => Arc::new(PistisOracle::new(
            pistis.orchestrator_url.clone(),
            load_cred_secret(&pistis.auth_token_cred)?,
            pistis.room.clone(),
        )),
        _ => Arc::new(StaticAllowlistOracle::new(config.capabilities.clone())),
    };
    let (approval_ttl_secs, max_concurrent, worktrees_root, max_runtime_secs) =
        match &config.execution {
            Some(e) => (
                e.approval_ttl_secs,
                e.max_concurrent_executions,
                e.worktrees_root.clone(),
                e.max_runtime_secs,
            ),
            None => (1800, 1, PathBuf::from("/tmp/rift-worktrees"), 1800),
        };
    let approval_registry = ApprovalRegistry::new(approval_ttl_secs);
    let sandbox_manager = Arc::new(SandboxManager::new(worktrees_root, max_runtime_secs));

    // Channel carrying control-server approvals to the event loop.
    let (approved_tx, mut approved_rx) =
        mpsc::channel::<henosis_rift_bridge::execution::PendingProposal>(64);

    // Optionally spawn the HTTP control server (shares the approval registry).
    if let Some(control_config) = config.control {
        let reg = approval_registry.clone();
        let tx = approved_tx.clone();
        tokio::spawn(async move {
            if let Err(e) = henosis_rift_bridge::control::serve(control_config, reg, tx).await {
                tracing::error!("control server stopped: {e}");
            }
        });
    }
    // Keep the original sender alive so `approved_rx` never sees all senders dropped.
    let _approved_tx = approved_tx;

    // Create room (provisions agent users in Rift).
    let mut room = Room::new(
        &config.agents,
        config.bridge,
        rift.clone(),
        kleos,
        "rift".to_string(),
        config.rift.channel_id,
        oracle,
        approval_registry.clone(),
        config.workspaces,
        sandbox_manager,
        max_concurrent,
        config.personas,
    )
    .await?;

    tracing::info!("room initialized, starting WebSocket listener");

    // Mint a WS connection token using the first agent's credentials.
    let ws_auth = AgentAuthManager::new(config.rift.jwt_secret.clone());
    let first_agent = room
        .roster_ref()
        .all()
        .next()
        .ok_or("no agents configured")?;
    let ws_token = ws_auth.issue_token(first_agent.rift_user_id, &first_agent.username)?;

    // WebSocket event channel.
    let (event_tx, mut event_rx) = mpsc::channel::<RiftWsEvent>(256);

    // Spawn WebSocket listener task.
    let ws_url = config.rift.ws_url.clone();
    let server_ids = vec![config.rift.server_id];
    tokio::spawn(async move {
        ws_listen(ws_url, ws_token, server_ids, event_tx).await;
    });

    // Pause check interval.
    let pause_interval = std::time::Duration::from_secs(config.rift.pause_poll_secs.unwrap_or(5));
    let mut paused = false;

    tracing::info!("bridge is running");

    // Main event loop: select between WS events and pause polling.
    loop {
        tokio::select! {
            Some(event) = event_rx.recv() => {
                match event {
                    RiftWsEvent::Ready => {
                        tracing::info!("WebSocket ready");
                    }
                    RiftWsEvent::MessageCreate(msg) => {
                        if paused {
                            tracing::debug!("bridge paused, ignoring message");
                            continue;
                        }
                        if let Err(e) = room.handle_message(msg).await {
                            tracing::error!("error handling message: {e}");
                        }
                    }
                    RiftWsEvent::Disconnected => {
                        tracing::warn!("WebSocket disconnected, will reconnect");
                    }
                }
            }
            Some(proposal) = approved_rx.recv() => {
                if !paused {
                    room.execute_approved(proposal).await;
                }
            }
            _ = tokio::time::sleep(pause_interval) => {
                room.sweep_expired_approvals().await;
                match rift.is_paused().await {
                    Ok(p) => {
                        if p != paused {
                            paused = p;
                            tracing::info!("bridge paused state changed: {paused}");
                        }
                    }
                    Err(e) => tracing::warn!("failed to check pause status: {e}"),
                }
            }
        }
    }
}

/// Select the bridge's Kleos backend from config: HTTP standalone (default) or
/// the in-process henosis kernel stores.
async fn build_kleos_client(
    config: &BridgeConfig,
) -> Result<Arc<dyn henosis_rift_bridge::kleos::KleosClient>, Box<dyn std::error::Error>> {
    use henosis_rift_bridge::kleos::{HttpKleosClient, InProcessKleosClient};

    let in_process = config
        .kleos
        .as_ref()
        .map(|k| k.in_process)
        .unwrap_or(false);

    if !in_process {
        return Ok(Arc::new(HttpKleosClient::from_env()?));
    }

    // In-process backend: open the henosis kernel stores (SQLite at db_dir, or
    // ephemeral in-memory) on a shared Axon bus. Memory goes through the
    // BridgeMemory seam (HTTP to upstream Kleos by default; the in-process
    // cognition store under the `cognition` feature -- see build_memory_backend).
    use henosis_broca::BrocaStore;
    use henosis_chiasm::ChiasmStore;
    use syntheos_axon::AxonBus;

    let bus = Arc::new(AxonBus::new());
    let (chiasm, broca) = match config.kleos.as_ref().and_then(|k| k.db_dir.clone()) {
        Some(dir) => {
            std::fs::create_dir_all(&dir)?;
            (
                ChiasmStore::open(dir.join("chiasm.db"), bus.clone())?,
                BrocaStore::open(dir.join("broca.db"), bus)?,
            )
        }
        None => (
            ChiasmStore::open_in_memory(bus.clone())?,
            BrocaStore::open_in_memory(bus)?,
        ),
    };

    let memory = build_memory_backend(config).await?;

    let tenant = henosis_rift_bridge::identity::bridge_tenant();
    let principal = henosis_rift_bridge::identity::principal_for_agent("rift-bridge");

    tracing::info!("kleos backend: in-process henosis kernel stores");
    Ok(Arc::new(InProcessKleosClient::new(
        Arc::new(chiasm),
        Arc::new(broca),
        memory,
        tenant,
        principal,
    )))
}

/// Build the in-process client's memory backend (the [`BridgeMemory`] seam).
///
/// Under the `cognition` feature, memory runs against a local kleos-lib store in
/// the bridge process: persistent at `db_dir/cognition.db` when `db_dir` is set,
/// else a volatile in-memory store. Without the feature, memory routes to
/// upstream Kleos over HTTP (the pre-Wave-3 behavior).
#[cfg(feature = "cognition")]
async fn build_memory_backend(
    config: &BridgeConfig,
) -> Result<
    Arc<dyn henosis_rift_bridge::kleos::BridgeMemory>,
    Box<dyn std::error::Error>,
> {
    use henosis_rift_bridge::kleos::CognitionMemoryBackend;

    let cognition = match config.kleos.as_ref().and_then(|k| k.db_dir.clone()) {
        Some(dir) => {
            std::fs::create_dir_all(&dir)?;
            let path = dir.join("cognition.db");
            let path = path
                .to_str()
                .ok_or("cognition db path is not valid UTF-8")?;
            let cog = henosis_cognition::Cognition::open_path(path).await?;
            tracing::info!(db = path, "kleos memory backend: in-process cognition store (persistent)");
            cog
        }
        None => {
            let cog = henosis_cognition::Cognition::open_in_memory().await?;
            tracing::info!("kleos memory backend: in-process cognition store (in-memory, volatile)");
            cog
        }
    };
    Ok(Arc::new(CognitionMemoryBackend::new(Arc::new(cognition))))
}

/// Build the in-process client's memory backend over upstream Kleos via HTTP.
/// The default (no `cognition` feature): the two memory ops have no in-process
/// store and route to `:4200`.
#[cfg(not(feature = "cognition"))]
async fn build_memory_backend(
    _config: &BridgeConfig,
) -> Result<
    Arc<dyn henosis_rift_bridge::kleos::BridgeMemory>,
    Box<dyn std::error::Error>,
> {
    use henosis_memory_client::Client as MemoryClient;
    use henosis_rift_bridge::kleos::HttpMemoryBackend;

    let kleos_url =
        std::env::var("KLEOS_URL").unwrap_or_else(|_| "http://127.0.0.1:4200".to_string());
    let api_key = std::env::var("KLEOS_API_KEY")
        .or_else(|_| std::env::var("KLEOS_KEY"))
        .ok();
    tracing::info!("kleos memory backend: upstream Kleos over HTTP (cognition feature off)");
    Ok(Arc::new(HttpMemoryBackend::new(Arc::new(MemoryClient::new(
        kleos_url, api_key, None,
    )))))
}

/// Resolve a `namespace/key` cred reference into the bearer token string it stores.
fn load_cred_secret(reference: &str) -> Result<String, Box<dyn std::error::Error>> {
    let (namespace, key) = reference
        .split_once('/')
        .ok_or_else(|| format!("invalid cred reference '{reference}', expected namespace/key"))?;
    let output = Command::new("cred")
        .args(["get", namespace, key])
        .output()?;
    if !output.status.success() {
        return Err(format!(
            "cred get {} {} failed with status {}",
            namespace, key, output.status
        )
        .into());
    }
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}
