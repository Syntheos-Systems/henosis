//! Reusable Rift agent bridge and Synapse room lifecycle.
//!
//! Connects to the Rift server via WebSocket, listens for messages,
//! and dispatches agent responses through the room state machine.

use std::future::Future;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinSet;

use crate::auth::AgentAuthManager;
use crate::capability::{PistisOracle, StaticAllowlistOracle};
use crate::config::{BridgeConfig, EmbeddingConfig};
#[cfg(feature = "cognition")]
use crate::embedding::CognitionEmbedder;
use crate::embedding::{Embedder, OpenAiEmbedder};
use crate::execution::approval::{decide_drain_action, ApprovalRegistry, DrainAction};
use crate::execution::sandbox::SandboxManager;
use crate::stimulus::{
    AxonEventSource, ChiasmTaskSource, GitHeadSource, LoomRunSource, ReflectionSource, Stimulus,
    StimulusInjector, StimulusSource,
};

/// Project scope for all bridge Kleos operations (tasks, activity, stimuli).
const KLEOS_PROJECT: &str = "rift";

/// Embedding backend selected from explicit HTTP configuration and the
/// compile-time/runtime in-process cognition switches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EmbeddingBackendChoice {
    /// Existing OpenAI-compatible HTTP provider.
    Http,
    /// Vendored Kleos ONNX provider shared inside the Henosis process.
    #[cfg(feature = "cognition")]
    Cognition,
    /// Token-overlap-only behavior with no semantic provider.
    Disabled,
}

/// Fully constructed embedding dependencies shared by cognition and the room.
struct EmbeddingRuntime {
    /// Rift-facing semantic embedder.
    room: Option<Arc<dyn Embedder>>,
    /// Threshold and reignition settings paired with the room embedder.
    config: Option<EmbeddingConfig>,
    /// Vendored provider cloned into the cognition memory facade.
    #[cfg(feature = "cognition")]
    cognition: Option<Arc<dyn henosis_cognition::EmbeddingProvider>>,
}

use crate::rift_client::{ws_listen, RiftRestClient, RiftWsEvent};
use crate::room::Room;

/// Run the complete Rift bridge until its process is terminated.
pub async fn run(config: BridgeConfig) -> anyhow::Result<()> {
    run_until(config, std::future::pending()).await
}

/// Run the complete Rift bridge until the supplied stop signal resolves.
pub async fn run_until<F>(config: BridgeConfig, stop: F) -> anyhow::Result<()>
where
    F: Future<Output = ()> + Send + 'static,
{
    tracing::info!("loaded config with {} agents", config.agents.len());
    let mut tasks = JoinSet::new();
    tokio::pin!(stop);

    // Build embeddings before either consumer so cognition and Rift receive
    // clones of the same provider Arc in the in-process configuration.
    let embedding_runtime = build_embedding_runtime(&config).await?;

    // Create auth manager and REST client.
    let auth = AgentAuthManager::new(
        config.rift.jwt_secret.clone(),
        config.rift.bridge_secret.clone(),
    );
    let rift = Arc::new(RiftRestClient::new(config.rift.api_url.clone(), auth));
    let kleos = build_kleos_client(&config, &embedding_runtime).await?;

    // Wire execution-mode dependencies from config.
    let oracle: Arc<dyn crate::capability::CapabilityOracle> = match &config.pistis {
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

    // Channel carrying control-server approvals to the drain task.
    let (approved_tx, approved_rx) = mpsc::channel::<crate::execution::PendingProposal>(64);

    // Optionally spawn the HTTP control server (shares the approval registry).
    if let Some(control_config) = config.control {
        let reg = approval_registry.clone();
        let tx = approved_tx.clone();
        tasks.spawn(async move {
            if let Err(e) = crate::control::serve(control_config, reg, tx).await {
                tracing::error!("control server stopped: {e}");
            }
        });
    }
    // Keep the original sender alive so `approved_rx` never sees all senders dropped.
    let _approved_tx = approved_tx;

    // Stimulus wiring needs pieces that move into the room below.
    let stimulus_settings = config.stimulus.clone();
    let workspace_paths: Vec<(String, PathBuf)> = config
        .workspaces
        .iter()
        .map(|w| (w.name.clone(), w.path.clone()))
        .collect();
    // Reporters whose Axon activity must never wake the room: the bridge's
    // own reporting identity (room.rs reports as "rift-bridge") plus every
    // roster agent -- otherwise the room's own wake reports and its agents'
    // execution-task updates would feed back into fresh stimuli.
    let stimulus_exclude_agents: Vec<String> = std::iter::once("rift-bridge".to_string())
        .chain(config.agents.iter().map(|a| a.username.clone()))
        .collect();

    // Create room (provisions agent users in Rift).
    let mut room = Room::new(
        &config.agents,
        config.bridge,
        rift.clone(),
        kleos.clone(),
        KLEOS_PROJECT.to_string(),
        config.rift.server_id,
        config.rift.channel_id,
        oracle,
        approval_registry.clone(),
        config.workspaces,
        sandbox_manager,
        max_concurrent,
        config.personas,
        embedding_runtime.room,
        embedding_runtime.config,
    )
    .await?;

    tracing::info!("room initialized, starting WebSocket listener");

    // Mint a WS connection token using the first agent's credentials.
    let ws_auth = AgentAuthManager::new(
        config.rift.jwt_secret.clone(),
        config.rift.bridge_secret.clone(),
    );
    let first_agent = room
        .roster_ref()
        .all()
        .next()
        .ok_or_else(|| anyhow::anyhow!("no agents configured"))?;
    let ws_token = ws_auth.issue_token(first_agent.rift_user_id, &first_agent.username)?;

    // WebSocket event channel.
    let (event_tx, mut event_rx) = mpsc::channel::<RiftWsEvent>(256);

    // Spawn WebSocket listener task.
    let ws_url = config.rift.ws_url.clone();
    let server_ids = vec![config.rift.server_id];
    tasks.spawn(async move {
        ws_listen(ws_url, ws_token, server_ids, event_tx).await;
    });

    // Pause state as a watch channel: one poller task owns the HTTP check;
    // the event loop, cascades, approval drain, and stimulus injector all
    // read (and react to) the same state without polling Rift themselves.
    let pause_interval = std::time::Duration::from_secs(config.rift.pause_poll_secs.unwrap_or(5));
    let pause_server_id = config.rift.server_id;
    let (pause_tx, pause_rx) = watch::channel(false);
    {
        let rift = rift.clone();
        tasks.spawn(async move {
            loop {
                tokio::time::sleep(pause_interval).await;
                match rift.is_paused(pause_server_id).await {
                    Ok(p) => {
                        // Send only on transitions: watch::Sender::send marks
                        // the value changed unconditionally, and an
                        // every-poll send would wake every changed() waiter
                        // (slot waits, the approvals drain) each cycle.
                        if *pause_tx.borrow() != p {
                            tracing::info!("bridge paused state changed: {p}");
                            if pause_tx.send(p).is_err() {
                                return;
                            }
                        }
                    }
                    Err(e) => tracing::warn!("failed to check pause status: {e}"),
                }
            }
        });
    }

    let dispatcher = room.dispatcher();

    // Approvals drain task: dispatches control-server approvals the moment
    // they arrive, cascade or no cascade. While paused, approvals are HELD --
    // and the hold lives in the ApprovalRegistry, not in a private queue here.
    // A held proposal therefore stays visible to /control/approvals (tagged
    // approved_held) and stays rejectable, instead of vanishing until someone
    // unpaused. Registry state is still in-memory, so a bridge restart loses
    // pending AND held approvals alike; durability is a separate concern.
    {
        let dispatcher = dispatcher.clone();
        let registry = approval_registry.clone();
        let mut pause_rx = pause_rx.clone();
        let mut approved_rx = approved_rx;
        tasks.spawn(async move {
            loop {
                tokio::select! {
                    maybe = approved_rx.recv() => match maybe {
                        Some(proposal) => {
                            let id = proposal.id;
                            let paused = *pause_rx.borrow();
                            match decide_drain_action(paused, &registry, id) {
                                // Leave it in the registry as Approved; the
                                // unpause branch below claims it.
                                DrainAction::Hold => {
                                    tracing::info!("bridge paused, holding approval {id}");
                                }
                                DrainAction::Dispatch => dispatcher.execute_approved(proposal),
                                // Already claimed by the unpause flush, or
                                // rejected while held. Dispatching here would
                                // run the task a second time.
                                DrainAction::Skip => {
                                    tracing::debug!("approval {id} already handled, skipping dispatch");
                                }
                            }
                        }
                        None => return,
                    },
                    changed = pause_rx.changed() => {
                        if changed.is_err() {
                            return;
                        }
                        if !*pause_rx.borrow() {
                            for proposal in registry.take_approved() {
                                tracing::info!("dispatching held approval {}", proposal.id);
                                dispatcher.execute_approved(proposal);
                            }
                        }
                    }
                }
            }
        });
    }

    // Expired-approval sweep task (was inline in the old event loop).
    {
        let dispatcher = dispatcher.clone();
        tasks.spawn(async move {
            loop {
                tokio::time::sleep(pause_interval).await;
                dispatcher.sweep_expired_approvals().await;
            }
        });
    }

    // Room-activity watch feeding the stimulus injector's reflection timer.
    let (activity_tx, activity_rx) = watch::channel(std::time::Instant::now());

    // Stimulus channel into the event loop. The keepalive clone means recv()
    // never yields None when the injector is disabled or stops.
    let (stim_tx, mut stim_rx) = mpsc::channel::<Stimulus>(16);
    let _stim_keepalive = stim_tx.clone();
    if let Some(settings) = stimulus_settings.filter(|s| s.enabled) {
        let mut sources: Vec<Box<dyn StimulusSource>> = vec![
            Box::new(ReflectionSource::new(std::time::Duration::from_secs(
                settings.reflection_after_secs,
            ))),
            Box::new(ChiasmTaskSource::new(
                kleos.clone(),
                KLEOS_PROJECT.to_string(),
            )),
            Box::new(AxonEventSource::new(
                kleos.clone(),
                KLEOS_PROJECT.to_string(),
                stimulus_exclude_agents,
            )),
            Box::new(LoomRunSource::new(kleos.clone())),
        ];
        if !workspace_paths.is_empty() {
            sources.push(Box::new(GitHeadSource::new(workspace_paths)));
        }
        let injector =
            StimulusInjector::new(&settings, sources, stim_tx, activity_rx, pause_rx.clone());
        tasks.spawn(injector.run());
        tracing::info!("stimulus injector running");
    }

    // Cascades borrow their own pause receiver so slot waits can watch it.
    let mut cascade_pause_rx = pause_rx.clone();

    tracing::info!("bridge is running");

    // Main event loop: WS events and stimuli. Approvals, pause polling, and
    // expiry sweeps live on their own tasks, so a long conversation cascade
    // no longer delays any of them; cascades themselves stay responsive via
    // the event receiver and pause watch passed into handle_message.
    loop {
        tokio::select! {
            _ = &mut stop => {
                tracing::info!("bridge stop requested");
                break;
            }
            Some(event) = event_rx.recv() => {
                match event {
                    RiftWsEvent::Ready => {
                        tracing::info!("WebSocket ready");
                    }
                    RiftWsEvent::MessageCreate(msg) => {
                        let _ = activity_tx.send(std::time::Instant::now());
                        if *pause_rx.borrow() {
                            tracing::debug!("bridge paused, ignoring message");
                            continue;
                        }
                        if let Err(e) = room
                            .handle_message(msg, &mut event_rx, &mut cascade_pause_rx)
                            .await
                        {
                            tracing::error!("error handling message: {e}");
                        }
                        // A cascade consumes events internally for its whole
                        // duration; stamp activity again at its end so the
                        // reflection idle timer never runs on a timestamp
                        // from before a long conversation.
                        let _ = activity_tx.send(std::time::Instant::now());
                    }
                    RiftWsEvent::Disconnected => {
                        tracing::warn!("WebSocket disconnected, will reconnect");
                    }
                }
            }
            Some(stim) = stim_rx.recv() => {
                if *pause_rx.borrow() {
                    tracing::debug!("bridge paused, dropping stimulus");
                    continue;
                }
                tracing::info!("injecting {} stimulus", stim.kind.as_str());
                let _ = activity_tx.send(std::time::Instant::now());
                if let Err(e) = room
                    .handle_stimulus(stim, &mut event_rx, &mut cascade_pause_rx)
                    .await
                {
                    tracing::error!("error handling stimulus: {e}");
                }
                // Same end-of-cascade stamp as the message arm.
                let _ = activity_tx.send(std::time::Instant::now());
            }
        }
    }

    tasks.abort_all();
    while tasks.join_next().await.is_some() {}
    Ok(())
}

/// Choose the provider without constructing model state, keeping precedence
/// deterministic and independently testable.
fn select_embedding_backend(has_http_url: bool, in_process: bool) -> EmbeddingBackendChoice {
    if has_http_url {
        return EmbeddingBackendChoice::Http;
    }
    #[cfg(feature = "cognition")]
    if in_process {
        return EmbeddingBackendChoice::Cognition;
    }
    #[cfg(not(feature = "cognition"))]
    let _ = in_process;
    EmbeddingBackendChoice::Disabled
}

/// Construct the selected provider and retain the exact shared `Arc` needed by
/// both the cognition facade and Rift's semantic adapter.
async fn build_embedding_runtime(config: &BridgeConfig) -> anyhow::Result<EmbeddingRuntime> {
    let embedding_config = config.embedding.clone();
    let has_http_url = embedding_config
        .as_ref()
        .and_then(|settings| settings.url.as_deref())
        .is_some();
    let in_process = config
        .kleos
        .as_ref()
        .map(|settings| settings.in_process)
        .unwrap_or(false);

    match select_embedding_backend(has_http_url, in_process) {
        EmbeddingBackendChoice::Http => {
            let cfg = embedding_config.expect("HTTP choice requires embedding config");
            let url = cfg.url.clone().expect("HTTP choice requires embedding URL");
            let api_key = match &cfg.api_key_env {
                Some(var) => match std::env::var(var) {
                    Ok(v) => Some(v),
                    Err(_) => {
                        tracing::warn!(
                            "embedding api_key_env {var} is not set; calling endpoint without auth"
                        );
                        None
                    }
                },
                None => None,
            };
            tracing::info!(
                "semantic echo/loop detection enabled via {} ({})",
                url,
                cfg.model
            );
            Ok(EmbeddingRuntime {
                room: Some(Arc::new(OpenAiEmbedder::new(
                    url,
                    cfg.model.clone(),
                    api_key,
                ))),
                config: Some(cfg),
                #[cfg(feature = "cognition")]
                cognition: None,
            })
        }
        #[cfg(feature = "cognition")]
        EmbeddingBackendChoice::Cognition => {
            let provider = henosis_cognition::load_embedding_provider_from_env().await?;
            let room: Arc<dyn Embedder> = Arc::new(CognitionEmbedder::new(Arc::clone(&provider)));
            tracing::info!(
                "semantic echo/loop detection and cognition share the in-process bge-m3 provider"
            );
            Ok(EmbeddingRuntime {
                room: Some(room),
                config: Some(embedding_config.unwrap_or_default()),
                cognition: Some(provider),
            })
        }
        EmbeddingBackendChoice::Disabled => {
            if embedding_config.is_some() {
                tracing::warn!(
                    "embedding config has no URL and in-process cognition is unavailable; using token-overlap only"
                );
            } else {
                tracing::info!(
                    "no embedding provider configured; echo detection is token-overlap only"
                );
            }
            Ok(EmbeddingRuntime {
                room: None,
                config: None,
                #[cfg(feature = "cognition")]
                cognition: None,
            })
        }
    }
}

/// Select the bridge's Kleos backend from config: HTTP standalone (default) or
/// the in-process henosis kernel stores.
async fn build_kleos_client(
    config: &BridgeConfig,
    embedding_runtime: &EmbeddingRuntime,
) -> anyhow::Result<Arc<dyn crate::kleos::KleosClient>> {
    use crate::kleos::{HttpKleosClient, InProcessKleosClient};

    let in_process = config.kleos.as_ref().map(|k| k.in_process).unwrap_or(false);

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

    let memory = build_memory_backend(config, embedding_runtime).await?;

    let tenant = crate::identity::bridge_tenant();
    let principal = crate::identity::principal_for_agent("rift-bridge");

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
/// upstream Kleos over HTTP when the local cognition feature is disabled.
#[cfg(feature = "cognition")]
async fn build_memory_backend(
    config: &BridgeConfig,
    embedding_runtime: &EmbeddingRuntime,
) -> anyhow::Result<Arc<dyn crate::kleos::BridgeMemory>> {
    use crate::kleos::CognitionMemoryBackend;

    let cognition = match config.kleos.as_ref().and_then(|k| k.db_dir.clone()) {
        Some(dir) => {
            std::fs::create_dir_all(&dir)?;
            let path = dir.join("cognition.db");
            let path = path
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("cognition db path is not valid UTF-8"))?;
            let cog = henosis_cognition::Cognition::open_path(path).await?;
            tracing::info!(
                db = path,
                "kleos memory backend: in-process cognition store (persistent)"
            );
            cog
        }
        None => {
            let cog = henosis_cognition::Cognition::open_in_memory().await?;
            tracing::info!(
                "kleos memory backend: in-process cognition store (in-memory, volatile)"
            );
            cog
        }
    };
    let cognition = match &embedding_runtime.cognition {
        Some(provider) => cognition.with_embedder(Arc::clone(provider)),
        None => cognition,
    };
    Ok(Arc::new(CognitionMemoryBackend::new(Arc::new(cognition))))
}

/// Build the in-process client's memory backend over upstream Kleos via HTTP.
/// The default (no `cognition` feature): the two memory ops have no in-process
/// store and route to `:4200`.
#[cfg(not(feature = "cognition"))]
async fn build_memory_backend(
    _config: &BridgeConfig,
    _embedding_runtime: &EmbeddingRuntime,
) -> anyhow::Result<Arc<dyn crate::kleos::BridgeMemory>> {
    use crate::kleos::HttpMemoryBackend;
    use henosis_memory_client::Client as MemoryClient;

    let kleos_url =
        std::env::var("KLEOS_URL").unwrap_or_else(|_| "http://127.0.0.1:4200".to_string());
    let api_key = std::env::var("KLEOS_API_KEY")
        .or_else(|_| std::env::var("KLEOS_KEY"))
        .ok();
    tracing::info!("kleos memory backend: upstream Kleos over HTTP (cognition feature off)");
    Ok(Arc::new(HttpMemoryBackend::new(Arc::new(
        MemoryClient::new(kleos_url, api_key, None),
    ))))
}

/// Resolve a `namespace/key` cred reference into the bearer token string it stores.
fn load_cred_secret(reference: &str) -> anyhow::Result<String> {
    let (namespace, key) = reference.split_once('/').ok_or_else(|| {
        anyhow::anyhow!("invalid cred reference '{reference}', expected namespace/key")
    })?;
    let output = Command::new("cred")
        .args(["get", namespace, key])
        .output()?;
    if !output.status.success() {
        anyhow::bail!(
            "cred get {} {} failed with status {}",
            namespace,
            key,
            output.status
        );
    }
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

/// Unit tests for provider selection precedence without constructing a real
/// ONNX session or contacting an HTTP endpoint.
#[cfg(test)]
mod tests {
    use super::{select_embedding_backend, EmbeddingBackendChoice};

    /// Verifies an explicit URL always preserves the HTTP override.
    #[test]
    fn test_explicit_url_selects_http() {
        assert_eq!(
            select_embedding_backend(true, true),
            EmbeddingBackendChoice::Http
        );
    }

    /// Verifies an in-process cognition build selects its local provider when
    /// no external URL overrides it.
    #[cfg(feature = "cognition")]
    #[test]
    fn test_in_process_cognition_selects_shared_provider() {
        assert_eq!(
            select_embedding_backend(false, true),
            EmbeddingBackendChoice::Cognition
        );
    }

    /// Verifies configurations without either provider preserve token-only
    /// behavior.
    #[test]
    fn test_missing_provider_disables_semantic_embedding() {
        assert_eq!(
            select_embedding_backend(false, false),
            EmbeddingBackendChoice::Disabled
        );
    }
}
