//! Desired-state reconciliation for one managed Rift room bridge.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use henosis_rift_bridge::catalog::discover_catalog;
use henosis_rift_bridge::config::{AgentConfig, BridgeConfig};
use henosis_rift_bridge::materialize::{
    materialize_error, materialize_loaded_revision, preflight_revision, validate_seats,
    CredentialBindingResolver, ManagedSeat, MaterializeError, ResolvedCredentialBinding,
};
use henosis_rift_bridge::runtime::{run_managed, BridgeReady, RuntimeDependencies};
use henosis_rift_server::agent_control::{ManagedAgentControl, ManagedAgentControlError};
use henosis_rift_server::db;
use henosis_rift_server::models::agent_control::{
    AgentSeatInput, AgentSeatView, ApplyState, ApplyStatusUpdate, ExecutionCapabilityCatalog,
    RoomAgentRoster,
};
use sqlx::PgPool;
use tokio::sync::{oneshot, watch};
use tokio::task::JoinHandle;
use uuid::Uuid;

use crate::credential_bindings::{FileCredentialBindingResolver, CREDENTIAL_BINDINGS_FILE_ENV};

/// Single-slot notification state; durable Rift state remains authoritative.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RevisionNotification {
    /// A new immutable desired revision committed.
    Desired(i64),
    /// A room manager requested another attempt for the same revision.
    Retry(i64),
}

/// Identity and ownership fields needed to materialize one persistent agent.
#[derive(Debug, Clone)]
struct RuntimeAgentIdentity {
    /// Stable Rift user identifier.
    id: Uuid,
    /// Unique Rift username.
    username: String,
    /// Display name used by the bridge roster.
    display_name: String,
    /// Human owner, or nil for an imported unclaimed identity.
    owner_user_id: Uuid,
}

/// Persistence boundary used by the reconciler and deterministic lifecycle tests.
#[async_trait]
trait RoomRevisionStore: Send + Sync {
    /// Read desired and applied state for the managed room.
    async fn current_roster(&self) -> Result<RoomAgentRoster, String>;

    /// Read one immutable revision, including current ownership metadata.
    async fn revision_seats(&self, revision: i64) -> Result<Vec<AgentSeatView>, String>;

    /// Resolve one persistent Rift agent identity, or `None` when absent.
    async fn agent_identity(
        &self,
        agent_user_id: Uuid,
    ) -> Result<Option<RuntimeAgentIdentity>, String>;

    /// Persist one observable runtime transition.
    async fn set_status(&self, status: ApplyStatusUpdate) -> Result<(), String>;
}

/// PostgreSQL implementation over the Rift-owned desired-state tables.
struct PostgresRoomRevisionStore {
    /// Shared Rift connection pool.
    pool: PgPool,
    /// Single server supervised by this reconciler.
    server_id: Uuid,
}

/// Reads Rift state without exposing database failures through public control routes.
#[async_trait]
impl RoomRevisionStore for PostgresRoomRevisionStore {
    /// Read the latest room roster and apply status.
    async fn current_roster(&self) -> Result<RoomAgentRoster, String> {
        db::agent_control::read_room_agent_roster(&self.pool, self.server_id)
            .await
            .map_err(|error| error.to_string())
    }

    /// Read a historical immutable roster snapshot.
    async fn revision_seats(&self, revision: i64) -> Result<Vec<AgentSeatView>, String> {
        db::agent_control::read_room_agent_revision(&self.pool, self.server_id, revision)
            .await
            .map_err(|error| error.to_string())
    }

    /// Resolve the current public identity plus current ownership record.
    ///
    /// Database failures are logged in full here and reduced to a stable
    /// phrase, because this path's message can reach API clients and durable
    /// dashboard state.
    async fn agent_identity(
        &self,
        agent_user_id: Uuid,
    ) -> Result<Option<RuntimeAgentIdentity>, String> {
        let Some(user) = db::get_user_by_id(&self.pool, agent_user_id)
            .await
            .map_err(|error| sanitized_store_failure("agent identity lookup", &error))?
            .filter(|user| user.is_agent)
        else {
            return Ok(None);
        };
        let owner_user_id = db::agent_control::owner_for_agent(&self.pool, agent_user_id)
            .await
            .map_err(|error| sanitized_store_failure("agent ownership lookup", &error))?
            .unwrap_or_else(Uuid::nil);
        Ok(Some(RuntimeAgentIdentity {
            id: user.id,
            display_name: user.display_name.unwrap_or_else(|| user.username.clone()),
            username: user.username,
            owner_user_id,
        }))
    }

    /// Write active, last-good, and failure state through Rift's repository.
    async fn set_status(&self, status: ApplyStatusUpdate) -> Result<(), String> {
        db::agent_control::set_room_apply_status(&self.pool, self.server_id, status)
            .await
            .map_err(|error| error.to_string())
    }
}

/// Process lifecycle boundary replaced by fakes in reconciler tests.
#[async_trait]
trait BridgeRunner: Send + Sync {
    /// Health-check a candidate without touching the current bridge.
    async fn preflight(&self, config: &BridgeConfig) -> Result<(), MaterializeError>;

    /// Spawn one bridge and return its pre-readiness lifecycle handles.
    fn spawn(&self, config: BridgeConfig, dependencies: RuntimeDependencies) -> SpawnedBridge;
}

/// Production bridge runner backed by the reusable Rift bridge runtime.
struct NativeBridgeRunner;

/// Starts real bridge tasks after applying the executor health preflight.
#[async_trait]
impl BridgeRunner for NativeBridgeRunner {
    /// Build and health-check every candidate executor.
    async fn preflight(&self, config: &BridgeConfig) -> Result<(), MaterializeError> {
        preflight_revision(config).await
    }

    /// Spawn a cancellable bridge with an explicit readiness receiver.
    fn spawn(&self, config: BridgeConfig, dependencies: RuntimeDependencies) -> SpawnedBridge {
        let (cancellation, cancellation_rx) = watch::channel(false);
        let (ready_tx, ready) = oneshot::channel();
        let task = tokio::spawn(run_managed(config, dependencies, cancellation_rx, ready_tx));
        SpawnedBridge {
            cancellation,
            ready,
            task,
        }
    }
}

/// Handles for one spawned bridge that has not yet proven readiness.
struct SpawnedBridge {
    /// Cooperative stop signal.
    cancellation: watch::Sender<bool>,
    /// One-shot proof of roster provisioning and Rift subscription.
    ready: oneshot::Receiver<BridgeReady>,
    /// Full bridge task result.
    task: JoinHandle<anyhow::Result<()>>,
}

/// Handles for one bridge past its explicit readiness boundary.
struct RunningBridge {
    /// Cooperative stop signal.
    cancellation: watch::Sender<bool>,
    /// Full bridge task result, polled to completion at most once.
    task: JoinHandle<anyhow::Result<()>>,
}

/// Readiness-wait resolution captured before any handle is consumed.
enum ReadyWait {
    /// Parent stop was requested first.
    Stopped,
    /// The bridge reported its readiness payload.
    Ready(BridgeReady),
    /// The runtime dropped its readiness sender, which only happens on exit.
    Closed,
    /// The startup deadline elapsed first.
    TimedOut,
}

/// Outcome of starting one bridge; every non-Ready case has collected the task.
enum StartupOutcome {
    /// The bridge reached its explicit readiness boundary and keeps running.
    Ready(RunningBridge, BridgeReady),
    /// Parent stop interrupted startup; the candidate was stopped.
    Cancelled,
    /// The bridge exited, closed readiness, or timed out; it was stopped.
    Failed(String),
}

/// Drives one spawned bridge to its readiness boundary exactly once.
impl SpawnedBridge {
    /// Wait for explicit Ready while remaining responsive to parent stop.
    async fn wait_ready(
        mut self,
        stop: &mut watch::Receiver<bool>,
        timeout: Duration,
    ) -> StartupOutcome {
        let wait = {
            let deadline = tokio::time::sleep(timeout);
            tokio::pin!(deadline);
            tokio::select! {
                _ = wait_for_stop(stop) => ReadyWait::Stopped,
                result = &mut self.ready => match result {
                    Ok(ready) => ReadyWait::Ready(ready),
                    Err(_) => ReadyWait::Closed,
                },
                _ = &mut deadline => ReadyWait::TimedOut,
            }
        };
        match wait {
            ReadyWait::Ready(ready) => StartupOutcome::Ready(
                RunningBridge {
                    cancellation: self.cancellation,
                    task: self.task,
                },
                ready,
            ),
            ReadyWait::Stopped => {
                if let Err(detail) = stop_bridge_task(self.cancellation, self.task, timeout).await {
                    tracing::warn!(%detail, "bridge stop after startup cancellation reported an error");
                }
                StartupOutcome::Cancelled
            }
            // The runtime drops its readiness sender only on exit, so the task
            // result is available promptly and is polled exactly once here.
            ReadyWait::Closed => StartupOutcome::Failed(task_result_detail(self.task.await)),
            ReadyWait::TimedOut => {
                let detail = match stop_bridge_task(self.cancellation, self.task, timeout).await {
                    Ok(()) => "bridge startup timed out".to_string(),
                    Err(stop_detail) => {
                        format!("bridge startup timed out; stop reported: {stop_detail}")
                    }
                };
                StartupOutcome::Failed(detail)
            }
        }
    }
}

/// Owns the lifecycle operations for one ready bridge.
impl RunningBridge {
    /// Report whether the bridge task has already exited.
    fn is_finished(&self) -> bool {
        self.task.is_finished()
    }

    /// Request cooperative stop, then abort only when the bridge exceeds its bound.
    async fn stop(self, timeout: Duration) -> Result<(), String> {
        stop_bridge_task(self.cancellation, self.task, timeout).await
    }

    /// Collect the result of a task already observed as finished.
    async fn finish(self) -> String {
        task_result_detail(self.task.await)
    }
}

/// Cooperatively stop one bridge task, aborting only past the lifecycle deadline.
async fn stop_bridge_task(
    cancellation: watch::Sender<bool>,
    mut task: JoinHandle<anyhow::Result<()>>,
    timeout: Duration,
) -> Result<(), String> {
    let _ = cancellation.send(true);
    match tokio::time::timeout(timeout, &mut task).await {
        Ok(Ok(Ok(()))) => Ok(()),
        Ok(Ok(Err(error))) => Err(error.to_string()),
        Ok(Err(error)) => Err(error.to_string()),
        Err(_) => {
            task.abort();
            let _ = task.await;
            Err("bridge did not stop before the lifecycle deadline".to_string())
        }
    }
}

/// Revision represented by the currently running bridge.
///
/// The exact configuration for restarts always lives in the supervisor's
/// last-known-good pair, which every path keeps equal to the running bridge.
struct ActiveBridge {
    /// Durable revision, or none for the initial TOML fallback.
    revision: Option<i64>,
    /// Live task handles.
    running: RunningBridge,
}

/// Shared validation state used by both Rift handlers and the supervisor loop.
struct RoomReconcilerCore {
    /// Single managed Rift server.
    server_id: Uuid,
    /// Deployment-owned base bridge configuration.
    base: BridgeConfig,
    /// Runtime-discovered, secret-free capabilities.
    catalog: ExecutionCapabilityCatalog,
    /// Rift desired-state repository.
    store: Arc<dyn RoomRevisionStore>,
    /// Opaque credential binding resolver.
    bindings: Arc<dyn CredentialBindingResolver>,
}

/// Cloneable Rift control implementation and coalesced reconciler notifier.
#[derive(Clone)]
pub struct RoomReconcilerHandle {
    /// Shared validation and persistence dependencies.
    core: Arc<RoomReconcilerCore>,
    /// Single-slot latest notification channel.
    notifications: watch::Sender<Option<RevisionNotification>>,
}

/// Long-lived desired-state supervisor for one room bridge.
pub struct RoomReconciler {
    /// Shared validation and persistence dependencies.
    core: Arc<RoomReconcilerCore>,
    /// Latest revision hint receiver.
    notifications: watch::Receiver<Option<RevisionNotification>>,
    /// Kernel services injected into every bridge generation.
    dependencies: RuntimeDependencies,
    /// Replaceable process lifecycle implementation.
    runner: Arc<dyn BridgeRunner>,
    /// Durable-state poll interval.
    poll_interval: Duration,
    /// Bound for both startup and graceful bridge stop.
    lifecycle_timeout: Duration,
}

/// Build the production handle and supervisor over an initialized Rift pool.
pub fn build_room_reconciler(
    pool: PgPool,
    base: BridgeConfig,
    dependencies: RuntimeDependencies,
    bindings: Arc<dyn CredentialBindingResolver>,
) -> (RoomReconcilerHandle, RoomReconciler) {
    let server_id = base.rift.server_id;
    let store: Arc<dyn RoomRevisionStore> = Arc::new(PostgresRoomRevisionStore { pool, server_id });
    build_room_reconciler_with_parts(
        base,
        dependencies,
        bindings,
        store,
        Arc::new(NativeBridgeRunner),
        Duration::from_secs(5),
        Duration::from_secs(30),
    )
}

/// Assemble a reconciler with replaceable persistence and runner boundaries.
fn build_room_reconciler_with_parts(
    base: BridgeConfig,
    dependencies: RuntimeDependencies,
    bindings: Arc<dyn CredentialBindingResolver>,
    store: Arc<dyn RoomRevisionStore>,
    runner: Arc<dyn BridgeRunner>,
    poll_interval: Duration,
    lifecycle_timeout: Duration,
) -> (RoomReconcilerHandle, RoomReconciler) {
    let core = Arc::new(RoomReconcilerCore {
        server_id: base.rift.server_id,
        catalog: discover_catalog(&base, Uuid::new_v4()),
        base,
        store,
        bindings,
    });
    let (notifications, notifications_rx) = watch::channel(None);
    (
        RoomReconcilerHandle {
            core: core.clone(),
            notifications,
        },
        RoomReconciler {
            core,
            notifications: notifications_rx,
            dependencies,
            runner,
            poll_interval,
            lifecycle_timeout,
        },
    )
}

/// Continue-or-stop signal returned by supervision passes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LoopControl {
    /// Keep supervising after this pass.
    Continue,
    /// Parent stop was observed during this pass.
    Stop,
}

/// Reason the supervisor woke from its event wait.
enum WakeEvent {
    /// A coalesced notification arrived from a Rift handler.
    Hint,
    /// Every hint sender is gone; only durable polling remains.
    HintsClosed,
    /// The active bridge task exited with the collected detail.
    BridgeExited(String),
    /// The periodic durable poll interval elapsed.
    Poll,
}

/// Mutable supervision state threaded through one reconciler lifetime.
struct Supervision {
    /// Currently running bridge, when one exists.
    active: Option<ActiveBridge>,
    /// Revision and exact configuration for last-known-good restarts.
    last_good: (Option<i64>, BridgeConfig),
    /// Failed desired revision waiting for an explicit retry hint.
    failed_revision: Option<i64>,
    /// Latest durable last-good revision observed, so failure writes never
    /// erase history proven good by a previous process lifetime.
    durable_last_good: Option<i64>,
    /// Whether the hint channel can still deliver wakeups.
    hints_open: bool,
}

/// Shared seat-resolution logic used by Rift handlers and the supervisor.
impl RoomReconcilerCore {
    /// Reject requests addressed to a server this reconciler does not manage.
    fn ensure_server(&self, server_id: Uuid) -> Result<(), ManagedAgentControlError> {
        if server_id == self.server_id {
            Ok(())
        } else {
            Err(ManagedAgentControlError::Internal(
                "request addressed an unmanaged server".to_string(),
            ))
        }
    }

    /// Resolve identities and behavior templates for the enabled subset of a seat list.
    ///
    /// Disabled seats are skipped entirely so a manager can park a seat whose
    /// harness disappeared from the host without failing the whole roster.
    async fn enabled_managed_seats(
        &self,
        seats: &[AgentSeatInput],
    ) -> Result<Vec<ManagedSeat>, MaterializeError> {
        let mut managed = Vec::new();
        for seat in seats.iter().filter(|seat| seat.enabled) {
            let identity = self
                .store
                .agent_identity(seat.agent_user_id)
                .await
                .map_err(|detail| materialize_error("store_unavailable", bounded_detail(&detail)))?
                .ok_or_else(|| {
                    materialize_error(
                        "agent_identity_unavailable",
                        "selected agent identity does not exist",
                    )
                })?;
            let template = behavior_template(&self.base.agents, &identity.username, seat.position)
                .ok_or_else(|| {
                    materialize_error(
                        "materialize_failed",
                        "the deployment configuration has no agent behavior templates",
                    )
                })?;
            managed.push(ManagedSeat {
                seat_id: seat.seat_id,
                agent_user_id: identity.id,
                owner_user_id: identity.owner_user_id,
                name: identity.display_name,
                username: identity.username,
                harness_id: seat.harness_id.clone(),
                model_id: seat.model_id.clone(),
                settings: seat.settings.clone(),
                credential_binding_id: seat.credential_binding_id,
                enabled: seat.enabled,
                position: seat.position,
                base_chance: template.base_chance,
                system_prompt: template.system_prompt.clone(),
            });
        }
        if managed.is_empty() {
            return Err(materialize_error(
                "empty_roster",
                "a room requires at least one enabled agent seat",
            ));
        }
        Ok(managed)
    }
}

/// Deterministically select the deployment behavior template for one seat.
///
/// Username matches preserve the exact behavior of rosters imported from the
/// deployment TOML. Seats created later fall back to the template at the
/// seat's position, then to the first configured template.
fn behavior_template<'a>(
    templates: &'a [AgentConfig],
    username: &str,
    position: i32,
) -> Option<&'a AgentConfig> {
    templates
        .iter()
        .find(|template| template.username == username)
        .or_else(|| {
            usize::try_from(position)
                .ok()
                .and_then(|index| templates.get(index))
        })
        .or_else(|| templates.first())
}

/// Map a materialization failure onto the sanitized managed-control categories.
fn control_error(error: MaterializeError) -> ManagedAgentControlError {
    if error.code.starts_with("credential_") {
        ManagedAgentControlError::CredentialNotReady(error.message)
    } else if matches!(error.code, "materialize_failed" | "store_unavailable") {
        ManagedAgentControlError::Internal(error.message)
    } else {
        ManagedAgentControlError::CapabilityUnavailable(error.message)
    }
}

/// Rift-facing control implementation backed by the shared reconciler state.
#[async_trait]
impl ManagedAgentControl for RoomReconcilerHandle {
    /// Return the host catalog discovered when the reconciler was built.
    async fn capabilities(
        &self,
        server_id: Uuid,
        _owner_user_id: Uuid,
    ) -> Result<ExecutionCapabilityCatalog, ManagedAgentControlError> {
        self.core.ensure_server(server_id)?;
        Ok(self.core.catalog.clone())
    }

    /// Validate the enabled seats of one submitted roster against host capabilities.
    async fn validate_revision(
        &self,
        server_id: Uuid,
        _owner_user_id: Uuid,
        seats: &[AgentSeatInput],
    ) -> Result<(), ManagedAgentControlError> {
        self.core.ensure_server(server_id)?;
        let managed = self
            .core
            .enabled_managed_seats(seats)
            .await
            .map_err(control_error)?;
        validate_seats(
            &self.core.base,
            &self.core.catalog,
            self.core.bindings.as_ref(),
            &managed,
        )
        .await
        .map_err(control_error)?;
        Ok(())
    }

    /// Coalesce a committed-revision hint; durable polling covers a lost hint.
    async fn revision_committed(
        &self,
        server_id: Uuid,
        revision: i64,
    ) -> Result<(), ManagedAgentControlError> {
        self.core.ensure_server(server_id)?;
        if self
            .notifications
            .send(Some(RevisionNotification::Desired(revision)))
            .is_err()
        {
            tracing::warn!(
                revision,
                "room reconciler is not running; durable polling will recover the revision"
            );
        }
        Ok(())
    }

    /// Coalesce an explicit retry hint for an already durable revision.
    async fn retry_revision(
        &self,
        server_id: Uuid,
        revision: i64,
    ) -> Result<(), ManagedAgentControlError> {
        self.core.ensure_server(server_id)?;
        if self
            .notifications
            .send(Some(RevisionNotification::Retry(revision)))
            .is_err()
        {
            tracing::warn!(
                revision,
                "room reconciler is not running; the retry hint was dropped"
            );
        }
        Ok(())
    }
}

/// Supervises one bridge process against durable Rift desired state.
impl RoomReconciler {
    /// Supervise the room bridge until the parent requests a stop.
    ///
    /// The deployment TOML roster starts first and must reach its explicit
    /// readiness boundary; durable desired revisions then converge on top of
    /// it. Rift is never aborted from here: an initial startup failure returns
    /// an error for the parent to treat as fatal, while later bridge failures
    /// are absorbed by fallback and restart.
    pub async fn run(mut self, mut stop: watch::Receiver<bool>) -> Result<(), String> {
        let base = self.core.base.clone();
        let active = match self.start_bridge(base.clone(), &mut stop).await {
            StartupOutcome::Ready(running, _ready) => Some(ActiveBridge {
                revision: None,
                running,
            }),
            StartupOutcome::Cancelled => return Ok(()),
            StartupOutcome::Failed(detail) => {
                return Err(format!("initial bridge startup failed: {detail}"));
            }
        };
        let mut state = Supervision {
            active,
            last_good: (None, base),
            failed_revision: None,
            durable_last_good: None,
            hints_open: true,
        };
        loop {
            if self.converge(&mut state, &mut stop).await == LoopControl::Stop {
                break;
            }
            if self.wait_for_event(&mut state, &mut stop).await == LoopControl::Stop {
                break;
            }
        }
        if let Some(bridge) = state.active.take() {
            if let Err(detail) = bridge.running.stop(self.lifecycle_timeout).await {
                tracing::warn!(%detail, "managed bridge stop reported an error during shutdown");
            }
        }
        Ok(())
    }

    /// Sleep until a hint, poll tick, parent stop, or bridge exit needs attention.
    async fn wait_for_event(
        &mut self,
        state: &mut Supervision,
        stop: &mut watch::Receiver<bool>,
    ) -> LoopControl {
        let poll_interval = self.poll_interval;
        let event = {
            let notifications = &mut self.notifications;
            // A closed hint channel must park instead of resolving instantly,
            // otherwise the supervisor would busy-loop; polling still converges.
            let hint_open = state.hints_open;
            let hint = async move {
                if hint_open {
                    notifications.changed().await
                } else {
                    std::future::pending().await
                }
            };
            let bridge_exit = async {
                match state.active.as_mut() {
                    Some(bridge) => task_result_detail((&mut bridge.running.task).await),
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                _ = wait_for_stop(stop) => return LoopControl::Stop,
                changed = hint => match changed {
                    Ok(()) => WakeEvent::Hint,
                    Err(_) => WakeEvent::HintsClosed,
                },
                detail = bridge_exit => WakeEvent::BridgeExited(detail),
                _ = tokio::time::sleep(poll_interval) => WakeEvent::Poll,
            }
        };
        match event {
            WakeEvent::Hint => {
                let latest = *self.notifications.borrow_and_update();
                if let Some(RevisionNotification::Retry(revision)) = latest {
                    if state.failed_revision == Some(revision) {
                        state.failed_revision = None;
                    }
                }
            }
            WakeEvent::HintsClosed => {
                // Park the hint branch permanently so the select cannot spin
                // on an instantly-erroring closed channel.
                state.hints_open = false;
            }
            WakeEvent::BridgeExited(detail) => {
                tracing::warn!(%detail, "managed bridge exited unexpectedly");
                // The exit arm polled the task to completion, so the handles
                // are spent; drop them and let convergence restart last-good.
                state.active = None;
            }
            WakeEvent::Poll => {}
        }
        LoopControl::Continue
    }

    /// Converge the running bridge toward durable desired state in one pass.
    async fn converge(
        &mut self,
        state: &mut Supervision,
        stop: &mut watch::Receiver<bool>,
    ) -> LoopControl {
        // Collect a bridge that crashed while this pass was not watching it.
        if state
            .active
            .as_ref()
            .is_some_and(|bridge| bridge.running.is_finished())
        {
            if let Some(bridge) = state.active.take() {
                let detail = bridge.running.finish().await;
                tracing::warn!(%detail, "managed bridge exited unexpectedly");
            }
        }
        // Restore supervision before consulting desired state.
        if state.active.is_none() {
            match self.start_bridge(state.last_good.1.clone(), stop).await {
                StartupOutcome::Ready(running, _ready) => {
                    state.active = Some(ActiveBridge {
                        revision: state.last_good.0,
                        running,
                    });
                }
                StartupOutcome::Cancelled => return LoopControl::Stop,
                StartupOutcome::Failed(detail) => {
                    tracing::error!(%detail, "restart of the last known good bridge failed");
                    self.persist_status(ApplyStatusUpdate {
                        active_revision: None,
                        last_good_revision: state.last_good.0.or(state.durable_last_good),
                        apply_state: ApplyState::Failed,
                        error_code: Some("bridge_unavailable".to_string()),
                        error_message: Some(bounded_detail(&detail)),
                    })
                    .await;
                    // The next poll tick retries the restart.
                    return LoopControl::Continue;
                }
            }
        }
        let roster = match self.core.store.current_roster().await {
            Ok(roster) => roster,
            Err(detail) => {
                tracing::warn!(%detail, "durable roster read failed; keeping the current bridge");
                return LoopControl::Continue;
            }
        };
        // Track history proven good by any process lifetime, so failure
        // writes after a restart cannot erase the durable pointer.
        state.durable_last_good = roster.last_good_revision.or(state.durable_last_good);
        let Some(desired) = roster.desired_revision else {
            return LoopControl::Continue;
        };
        let running_revision = state.active.as_ref().and_then(|bridge| bridge.revision);
        if running_revision == Some(desired) {
            // Heal durable status that lags the running bridge, for example
            // after a status write that failed right after a successful swap.
            if roster.active_revision != Some(desired) || roster.apply_state != ApplyState::Active {
                self.persist_status(ApplyStatusUpdate {
                    active_revision: Some(desired),
                    last_good_revision: Some(desired),
                    apply_state: ApplyState::Active,
                    error_code: None,
                    error_message: None,
                })
                .await;
            }
            return LoopControl::Continue;
        }
        if state.failed_revision == Some(desired) {
            // A failed revision waits for an explicit retry hint. A different
            // desired revision falls through and applies normally.
            return LoopControl::Continue;
        }
        self.apply_revision(desired, state, stop).await
    }

    /// Validate, preflight, and swap to one desired revision with fallback.
    async fn apply_revision(
        &mut self,
        revision: i64,
        state: &mut Supervision,
        stop: &mut watch::Receiver<bool>,
    ) -> LoopControl {
        let seats = match self.core.store.revision_seats(revision).await {
            Ok(views) => views.into_iter().map(|view| view.seat).collect::<Vec<_>>(),
            Err(detail) => {
                tracing::warn!(%detail, revision, "desired revision read failed; retrying on the next pass");
                return LoopControl::Continue;
            }
        };
        let running_revision = state.active.as_ref().and_then(|bridge| bridge.revision);
        let candidate = match self.materialize_candidate(&seats).await {
            Ok(config) => config,
            // A store outage during seat resolution is transient: retry on
            // the next pass instead of latching the revision as failed.
            Err(error) if error.code == "store_unavailable" => {
                tracing::warn!(
                    revision,
                    message = %error.message,
                    "seat resolution hit a store failure; retrying on the next pass"
                );
                return LoopControl::Continue;
            }
            Err(error) => {
                self.record_apply_failure(revision, state, running_revision, error)
                    .await;
                return LoopControl::Continue;
            }
        };
        if let Err(error) = self.runner.preflight(&candidate).await {
            self.record_apply_failure(revision, state, running_revision, error)
                .await;
            return LoopControl::Continue;
        }
        // The candidate passed every check that can run beside the live
        // bridge; replacement is the only remaining step.
        if let Some(bridge) = state.active.take() {
            if let Err(detail) = bridge.running.stop(self.lifecycle_timeout).await {
                tracing::warn!(%detail, "previous bridge stop reported an error during replacement");
            }
        }
        match self.start_bridge(candidate.clone(), stop).await {
            StartupOutcome::Ready(running, _ready) => {
                state.active = Some(ActiveBridge {
                    revision: Some(revision),
                    running,
                });
                state.last_good = (Some(revision), candidate);
                state.durable_last_good = Some(revision);
                state.failed_revision = None;
                self.persist_status(ApplyStatusUpdate {
                    active_revision: Some(revision),
                    last_good_revision: Some(revision),
                    apply_state: ApplyState::Active,
                    error_code: None,
                    error_message: None,
                })
                .await;
                LoopControl::Continue
            }
            StartupOutcome::Cancelled => LoopControl::Stop,
            StartupOutcome::Failed(detail) => {
                tracing::error!(%detail, revision, "candidate bridge failed to start; restarting last known good");
                match self.start_bridge(state.last_good.1.clone(), stop).await {
                    StartupOutcome::Ready(running, _ready) => {
                        state.active = Some(ActiveBridge {
                            revision: state.last_good.0,
                            running,
                        });
                    }
                    StartupOutcome::Cancelled => return LoopControl::Stop,
                    StartupOutcome::Failed(fallback_detail) => {
                        tracing::error!(
                            %fallback_detail,
                            "last known good restart also failed; retrying on the next pass"
                        );
                    }
                }
                let restored_revision = state.active.as_ref().and_then(|bridge| bridge.revision);
                self.record_apply_failure(
                    revision,
                    state,
                    restored_revision,
                    materialize_error("bridge_start_failed", detail),
                )
                .await;
                LoopControl::Continue
            }
        }
    }

    /// Build a preflight-ready configuration for the enabled seats of one revision.
    async fn materialize_candidate(
        &self,
        seats: &[AgentSeatInput],
    ) -> Result<BridgeConfig, MaterializeError> {
        let managed = self.core.enabled_managed_seats(seats).await?;
        let modes = validate_seats(
            &self.core.base,
            &self.core.catalog,
            self.core.bindings.as_ref(),
            &managed,
        )
        .await?;
        materialize_loaded_revision(self.core.base.clone(), managed, modes)
    }

    /// Spawn one bridge and drive it to its readiness boundary.
    async fn start_bridge(
        &self,
        config: BridgeConfig,
        stop: &mut watch::Receiver<bool>,
    ) -> StartupOutcome {
        let spawned = self.runner.spawn(config, self.dependencies.clone());
        spawned.wait_ready(stop, self.lifecycle_timeout).await
    }

    /// Latch one failed revision and persist its sanitized failure status.
    ///
    /// The durable last-good pointer is preserved even when this process has
    /// not proven any revision good yet, such as right after a restart.
    async fn record_apply_failure(
        &self,
        revision: i64,
        state: &mut Supervision,
        active_revision: Option<i64>,
        error: MaterializeError,
    ) {
        tracing::warn!(
            revision,
            code = error.code,
            message = %error.message,
            "desired revision failed to apply"
        );
        state.failed_revision = Some(revision);
        self.persist_status(ApplyStatusUpdate {
            active_revision,
            last_good_revision: state.last_good.0.or(state.durable_last_good),
            apply_state: ApplyState::Failed,
            error_code: Some(error.code.to_string()),
            error_message: Some(bounded_detail(&error.message)),
        })
        .await;
    }

    /// Persist one observable transition, tolerating a transient store failure.
    async fn persist_status(&self, status: ApplyStatusUpdate) {
        if let Err(detail) = self.core.store.set_status(status).await {
            tracing::warn!(%detail, "bridge status persistence failed; the poll loop will heal it");
        }
    }
}

/// Resolver used when the deployment declares no credential bindings.
pub struct EmptyCredentialBindingResolver;

/// Resolves every binding to absent so host-session seats keep working.
#[async_trait]
impl CredentialBindingResolver for EmptyCredentialBindingResolver {
    /// Report every opaque binding as not currently resolvable.
    async fn resolve_binding(
        &self,
        _binding_id: Uuid,
    ) -> Result<Option<ResolvedCredentialBinding>, MaterializeError> {
        Ok(None)
    }
}

/// Select the deployment credential resolver from process environment.
///
/// An absent or empty `HENOSIS_AGENT_CREDENTIAL_BINDINGS_FILE` keeps
/// host-session execution working through the empty resolver. A configured
/// but unusable file must fail room preparation instead of silently
/// downgrading credential-bound seats.
pub fn credential_binding_resolver_from_environment(
) -> Result<Arc<dyn CredentialBindingResolver>, MaterializeError> {
    match std::env::var_os(CREDENTIAL_BINDINGS_FILE_ENV) {
        Some(value) if !value.is_empty() => {
            Ok(Arc::new(FileCredentialBindingResolver::from_env()?))
        }
        _ => Ok(Arc::new(EmptyCredentialBindingResolver)),
    }
}

/// Wait until a parent stop receiver becomes true or disconnects.
async fn wait_for_stop(receiver: &mut watch::Receiver<bool>) {
    if *receiver.borrow() {
        return;
    }
    while receiver.changed().await.is_ok() {
        if *receiver.borrow() {
            return;
        }
    }
}

/// Render a nested bridge task result without discarding either error layer.
fn task_result_detail(result: Result<anyhow::Result<()>, tokio::task::JoinError>) -> String {
    match result {
        Ok(Ok(())) => "completed successfully".to_string(),
        Ok(Err(error)) => error.to_string(),
        Err(error) => error.to_string(),
    }
}

/// Log a database failure in full and return only a stable public phrase.
fn sanitized_store_failure(operation: &str, error: &sqlx::Error) -> String {
    tracing::warn!(%error, operation, "Rift store operation failed");
    format!("{operation} failed")
}

/// Bound one failure detail for durable dashboard-safe persistence.
fn bounded_detail(detail: &str) -> String {
    const MAX_DETAIL_BYTES: usize = 500;
    if detail.len() <= MAX_DETAIL_BYTES {
        return detail.to_string();
    }
    let mut end = MAX_DETAIL_BYTES;
    while !detail.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &detail[..end])
}

#[cfg(test)]
/// Deterministic lifecycle tests over a fake store and scripted bridge runner.
mod tests {
    use std::collections::{HashMap, VecDeque};
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::sync::Mutex;

    use henosis_rift_bridge::config::{BridgeDaemonConfig, ExecutorConfig, RiftConfig};
    use henosis_rift_bridge::materialize::ResolvedExecutionMode;
    use henosis_rift_server::models::agent_control::CredentialReadiness;
    use serde_json::json;
    use tokio::sync::Notify;

    use super::*;

    /// UUID-scoped executable fixture keeping catalog discovery deterministic.
    struct ExecutableFixture {
        /// Isolated fixture directory.
        root: PathBuf,
        /// Executable file inside the fixture directory.
        path: PathBuf,
    }

    /// Creates one harmless executable file and cleans it up after each test.
    impl ExecutableFixture {
        /// Build a fresh executable shell fixture.
        fn new(name: &str) -> Self {
            let root =
                std::env::temp_dir().join(format!("henosis-reconciler-{name}-{}", Uuid::new_v4()));
            fs::create_dir(&root).expect("create executable fixture directory");
            let path = root.join(name);
            fs::write(&path, "#!/bin/sh\nexit 0\n").expect("write executable fixture");
            #[cfg(unix)]
            {
                let mut permissions = fs::metadata(&path).expect("stat fixture").permissions();
                permissions.set_mode(0o700);
                fs::set_permissions(&path, permissions).expect("make fixture executable");
            }
            Self { root, path }
        }
    }

    /// Removes only the UUID-scoped fixture directory.
    impl Drop for ExecutableFixture {
        /// Clean up the isolated executable fixture.
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    /// Stable server identity shared by every test roster.
    fn server_id() -> Uuid {
        Uuid::from_u128(1)
    }

    /// Build the two-template base configuration around live fixture binaries.
    fn test_base(claude: &ExecutableFixture, codex: &ExecutableFixture) -> BridgeConfig {
        BridgeConfig {
            rift: RiftConfig {
                api_url: "http://127.0.0.1:3200".to_string(),
                ws_url: "ws://127.0.0.1:3200/ws".to_string(),
                jwt_secret: "jwt-secret-that-is-at-least-32-bytes".to_string(),
                bridge_secret: "bridge-secret-that-is-at-least-32-bytes".to_string(),
                server_id: server_id(),
                channel_id: Uuid::from_u128(2),
                pause_poll_secs: None,
            },
            bridge: BridgeDaemonConfig::default(),
            agents: vec![
                AgentConfig {
                    name: "Claude Template".to_string(),
                    username: "claude-template".to_string(),
                    executor: ExecutorConfig::ClaudeCode {
                        binary: claude.path.clone(),
                        model: Some("sonnet".to_string()),
                        max_tokens: Some(4096),
                    },
                    base_chance: 0.4,
                    system_prompt: "Claude template prompt".to_string(),
                    execution_mode: ResolvedExecutionMode::HostSession,
                },
                AgentConfig {
                    name: "Codex Template".to_string(),
                    username: "codex-template".to_string(),
                    executor: ExecutorConfig::Codex {
                        binary: codex.path.clone(),
                        model: "gpt-5.6-sol".to_string(),
                        reasoning_effort: Some("medium".to_string()),
                    },
                    base_chance: 0.5,
                    system_prompt: "Codex template prompt".to_string(),
                    execution_mode: ResolvedExecutionMode::HostSession,
                },
            ],
            capabilities: HashMap::new(),
            workspaces: Vec::new(),
            execution: None,
            pistis: None,
            control: None,
            personas: None,
            kleos: None,
            embedding: None,
            stimulus: None,
        }
    }

    /// Mutable durable-state fixture shared between the test and the reconciler.
    struct FakeStoreState {
        /// Roster status served to the reconciler.
        roster: RoomAgentRoster,
        /// Immutable revision snapshots by revision number.
        revisions: HashMap<i64, Vec<AgentSeatView>>,
        /// Persistent identities by agent user ID.
        identities: HashMap<Uuid, RuntimeAgentIdentity>,
        /// Every status write attempted by the reconciler.
        status_updates: Vec<ApplyStatusUpdate>,
        /// Scripted set_status failures consumed per call; Ok when exhausted.
        status_results: VecDeque<Result<(), String>>,
        /// Scripted current_roster failures consumed per call; Ok when exhausted.
        roster_results: VecDeque<Result<(), String>>,
        /// Scripted revision_seats failures consumed per call; Ok when exhausted.
        revision_results: VecDeque<Result<(), String>>,
        /// Revisions whose seats the reconciler read.
        revision_reads: Vec<i64>,
    }

    /// In-memory revision store recording every reconciler interaction.
    struct FakeStore {
        /// Lock-protected durable fixture state.
        state: Mutex<FakeStoreState>,
    }

    /// Builds and inspects fake durable state.
    impl FakeStore {
        /// Create a store with no desired revision.
        fn new() -> Arc<Self> {
            Arc::new(Self {
                state: Mutex::new(FakeStoreState {
                    roster: RoomAgentRoster {
                        server_id: server_id(),
                        desired_revision: None,
                        active_revision: None,
                        last_good_revision: None,
                        apply_state: ApplyState::Idle,
                        apply_error_code: None,
                        apply_error_message: None,
                        seats: Vec::new(),
                    },
                    revisions: HashMap::new(),
                    identities: HashMap::new(),
                    status_updates: Vec::new(),
                    status_results: VecDeque::new(),
                    roster_results: VecDeque::new(),
                    revision_results: VecDeque::new(),
                    revision_reads: Vec::new(),
                }),
            })
        }

        /// Install one desired revision holding a single enabled Claude seat.
        fn install_revision(&self, revision: i64) {
            let agent_user_id = Uuid::new_v4();
            let seat = AgentSeatInput {
                seat_id: Uuid::new_v4(),
                agent_user_id,
                harness_id: "claude-code".to_string(),
                model_id: "sonnet".to_string(),
                settings: json!({}),
                credential_binding_id: None,
                enabled: true,
                position: 0,
            };
            let mut state = self.state.lock().expect("fake store lock");
            state.identities.insert(
                agent_user_id,
                RuntimeAgentIdentity {
                    id: agent_user_id,
                    username: format!("agent-r{revision}"),
                    display_name: format!("Agent R{revision}"),
                    owner_user_id: Uuid::nil(),
                },
            );
            state.revisions.insert(
                revision,
                vec![AgentSeatView {
                    seat,
                    owner_user_id: None,
                    credential_readiness: CredentialReadiness::HostSession,
                }],
            );
            state.roster.desired_revision = Some(revision);
            state.roster.apply_state = ApplyState::Pending;
        }

        /// Script the next status write to fail once.
        fn fail_next_status_write(&self) {
            self.state
                .lock()
                .expect("fake store lock")
                .status_results
                .push_back(Err("scripted status write failure".to_string()));
        }

        /// Script the next roster read to fail once.
        fn fail_next_roster_read(&self) {
            self.state
                .lock()
                .expect("fake store lock")
                .roster_results
                .push_back(Err("scripted roster read failure".to_string()));
        }

        /// Script the next revision read to fail once.
        fn fail_next_revision_read(&self) {
            self.state
                .lock()
                .expect("fake store lock")
                .revision_results
                .push_back(Err("scripted revision read failure".to_string()));
        }

        /// Seed durable history left behind by a previous process lifetime.
        fn set_durable_history(&self, active: Option<i64>, last_good: Option<i64>) {
            let mut state = self.state.lock().expect("fake store lock");
            state.roster.active_revision = active;
            state.roster.last_good_revision = last_good;
        }

        /// Snapshot the current roster status.
        fn latest_roster(&self) -> RoomAgentRoster {
            self.state.lock().expect("fake store lock").roster.clone()
        }

        /// Snapshot every attempted status write.
        fn status_updates(&self) -> Vec<ApplyStatusUpdate> {
            self.state
                .lock()
                .expect("fake store lock")
                .status_updates
                .clone()
        }

        /// Snapshot the revisions whose seats were read.
        fn revision_reads(&self) -> Vec<i64> {
            self.state
                .lock()
                .expect("fake store lock")
                .revision_reads
                .clone()
        }

        /// Return the seat inputs stored for one revision.
        fn revision_seats_input(&self, revision: i64) -> Vec<AgentSeatInput> {
            self.state
                .lock()
                .expect("fake store lock")
                .revisions
                .get(&revision)
                .expect("test revision exists")
                .iter()
                .map(|view| view.seat.clone())
                .collect()
        }
    }

    /// Serves durable state from the shared fixture.
    #[async_trait]
    impl RoomRevisionStore for FakeStore {
        /// Read the current roster snapshot, honoring scripted failures.
        async fn current_roster(&self) -> Result<RoomAgentRoster, String> {
            let mut state = self.state.lock().expect("fake store lock");
            if let Some(result) = state.roster_results.pop_front() {
                result?;
            }
            Ok(state.roster.clone())
        }

        /// Read one immutable revision, recording the access attempt first.
        async fn revision_seats(&self, revision: i64) -> Result<Vec<AgentSeatView>, String> {
            let mut state = self.state.lock().expect("fake store lock");
            state.revision_reads.push(revision);
            if let Some(result) = state.revision_results.pop_front() {
                result?;
            }
            state
                .revisions
                .get(&revision)
                .cloned()
                .ok_or_else(|| "missing revision".to_string())
        }

        /// Resolve one persistent identity, absent when never installed.
        async fn agent_identity(
            &self,
            agent_user_id: Uuid,
        ) -> Result<Option<RuntimeAgentIdentity>, String> {
            Ok(self
                .state
                .lock()
                .expect("fake store lock")
                .identities
                .get(&agent_user_id)
                .cloned())
        }

        /// Record one status write, honoring scripted failures before mutation.
        async fn set_status(&self, status: ApplyStatusUpdate) -> Result<(), String> {
            let mut state = self.state.lock().expect("fake store lock");
            state.status_updates.push(status.clone());
            if let Some(result) = state.status_results.pop_front() {
                result?;
            }
            state.roster.active_revision = status.active_revision;
            state.roster.last_good_revision = status.last_good_revision;
            state.roster.apply_state = status.apply_state;
            state.roster.apply_error_code = status.error_code;
            state.roster.apply_error_message = status.error_message;
            Ok(())
        }
    }

    /// One scripted spawn behavior consumed per bridge start.
    #[derive(Clone, Copy)]
    enum SpawnScript {
        /// Report Ready immediately and run until cancellation or crash.
        Ready,
        /// Exit with an error before reporting Ready.
        FailStartup,
    }

    /// One observable fake bridge lifecycle transition.
    #[derive(Debug, Clone, PartialEq, Eq)]
    enum RunnerEvent {
        /// A bridge started with the given roster usernames.
        Spawned(Vec<String>),
        /// A bridge honored cooperative cancellation.
        Stopped(Vec<String>),
        /// A bridge crashed on the scripted trigger.
        Crashed(Vec<String>),
    }

    /// Scripted bridge runner recording lifecycle order without real processes.
    struct FakeRunner {
        /// Spawn behaviors consumed per call; Ready when exhausted.
        scripts: Mutex<VecDeque<SpawnScript>>,
        /// Preflight results consumed per call; Ok when exhausted.
        preflights: Mutex<VecDeque<Result<(), MaterializeError>>>,
        /// Ordered lifecycle observations.
        events: Arc<Mutex<Vec<RunnerEvent>>>,
        /// Crash triggers for spawned fake bridges, newest last.
        crash_triggers: Mutex<Vec<Arc<Notify>>>,
    }

    /// Builds and inspects the scripted runner.
    impl FakeRunner {
        /// Create a runner with scripted spawn and preflight behavior.
        fn new(
            scripts: Vec<SpawnScript>,
            preflights: Vec<Result<(), MaterializeError>>,
        ) -> Arc<Self> {
            Arc::new(Self {
                scripts: Mutex::new(scripts.into()),
                preflights: Mutex::new(preflights.into()),
                events: Arc::new(Mutex::new(Vec::new())),
                crash_triggers: Mutex::new(Vec::new()),
            })
        }

        /// Snapshot every lifecycle observation in order.
        fn events(&self) -> Vec<RunnerEvent> {
            self.events.lock().expect("fake events lock").clone()
        }

        /// Return the roster usernames of every spawn in order.
        fn spawned(&self) -> Vec<Vec<String>> {
            self.events()
                .into_iter()
                .filter_map(|event| match event {
                    RunnerEvent::Spawned(usernames) => Some(usernames),
                    _ => None,
                })
                .collect()
        }

        /// Crash the most recently spawned live fake bridge.
        fn crash_latest(&self) {
            self.crash_triggers
                .lock()
                .expect("fake crash lock")
                .last()
                .expect("a bridge was spawned")
                .notify_one();
        }
    }

    /// Serves scripted lifecycle behavior through the runner boundary.
    #[async_trait]
    impl BridgeRunner for FakeRunner {
        /// Pop one scripted preflight result, succeeding when exhausted.
        async fn preflight(&self, _config: &BridgeConfig) -> Result<(), MaterializeError> {
            self.preflights
                .lock()
                .expect("fake preflights lock")
                .pop_front()
                .unwrap_or(Ok(()))
        }

        /// Spawn one scripted fake bridge task.
        fn spawn(&self, config: BridgeConfig, _dependencies: RuntimeDependencies) -> SpawnedBridge {
            let usernames: Vec<String> = config
                .agents
                .iter()
                .map(|agent| agent.username.clone())
                .collect();
            let script = self
                .scripts
                .lock()
                .expect("fake scripts lock")
                .pop_front()
                .unwrap_or(SpawnScript::Ready);
            let events = self.events.clone();
            events
                .lock()
                .expect("fake events lock")
                .push(RunnerEvent::Spawned(usernames.clone()));
            let crash = Arc::new(Notify::new());
            self.crash_triggers
                .lock()
                .expect("fake crash lock")
                .push(crash.clone());
            let (cancellation, mut cancel_rx) = watch::channel(false);
            let (ready_tx, ready) = oneshot::channel();
            let task = tokio::spawn(async move {
                match script {
                    SpawnScript::FailStartup => {
                        drop(ready_tx);
                        Err(anyhow::anyhow!("scripted startup failure"))
                    }
                    SpawnScript::Ready => {
                        let _ = ready_tx.send(BridgeReady { roster: Vec::new() });
                        loop {
                            tokio::select! {
                                changed = cancel_rx.changed() => {
                                    if changed.is_err() || *cancel_rx.borrow() {
                                        // Delay the stop record so a reconciler
                                        // that returns without awaiting the stop
                                        // is caught: the record would then land
                                        // after the test resumes and assertions
                                        // on lifecycle order would fail.
                                        tokio::time::sleep(Duration::from_millis(10)).await;
                                        events
                                            .lock()
                                            .expect("fake events lock")
                                            .push(RunnerEvent::Stopped(usernames));
                                        return Ok(());
                                    }
                                }
                                _ = crash.notified() => {
                                    events
                                        .lock()
                                        .expect("fake events lock")
                                        .push(RunnerEvent::Crashed(usernames));
                                    return Err(anyhow::anyhow!("scripted bridge crash"));
                                }
                            }
                        }
                    }
                }
            });
            SpawnedBridge {
                cancellation,
                ready,
                task,
            }
        }
    }

    /// Live reconciler under test plus its collaborators and stop control.
    struct Harness {
        /// Rift-facing control handle.
        handle: RoomReconcilerHandle,
        /// Shared durable-state fixture.
        store: Arc<FakeStore>,
        /// Shared scripted runner.
        runner: Arc<FakeRunner>,
        /// Supervisor task under test.
        task: JoinHandle<Result<(), String>>,
        /// Parent stop signal.
        stop: watch::Sender<bool>,
        /// Executable fixtures kept alive for catalog availability.
        _fixtures: (ExecutableFixture, ExecutableFixture),
    }

    /// Start one reconciler over fresh fixtures and scripted behaviors.
    fn start_harness(
        store: Arc<FakeStore>,
        scripts: Vec<SpawnScript>,
        preflights: Vec<Result<(), MaterializeError>>,
    ) -> Harness {
        let claude = ExecutableFixture::new("claude");
        let codex = ExecutableFixture::new("codex");
        let base = test_base(&claude, &codex);
        let runner = FakeRunner::new(scripts, preflights);
        let (handle, reconciler) = build_room_reconciler_with_parts(
            base,
            RuntimeDependencies::default(),
            Arc::new(EmptyCredentialBindingResolver),
            store.clone(),
            runner.clone(),
            Duration::from_millis(50),
            Duration::from_secs(5),
        );
        let (stop, stop_rx) = watch::channel(false);
        let task = tokio::spawn(reconciler.run(stop_rx));
        Harness {
            handle,
            store,
            runner,
            task,
            stop,
            _fixtures: (claude, codex),
        }
    }

    /// Await one observable condition under paused time with a bounded budget.
    async fn wait_until(mut condition: impl FnMut() -> bool) {
        tokio::time::timeout(Duration::from_secs(120), async {
            while !condition() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("condition was not reached before the test budget");
    }

    /// Signal parent stop and join the reconciler, asserting a clean exit.
    async fn stop_and_join(harness: Harness) -> (Arc<FakeStore>, Arc<FakeRunner>) {
        let _ = harness.stop.send(true);
        harness
            .task
            .await
            .expect("reconciler task join")
            .expect("clean reconciler exit");
        (harness.store, harness.runner)
    }

    /// The deployment TOML bridge starts first and reaches explicit readiness.
    #[tokio::test(start_paused = true)]
    async fn initial_toml_bridge_reaches_ready() {
        let harness = start_harness(FakeStore::new(), vec![SpawnScript::Ready], Vec::new());
        wait_until(|| harness.runner.spawned().len() == 1).await;
        assert_eq!(
            harness.runner.spawned(),
            vec![vec![
                "claude-template".to_string(),
                "codex-template".to_string()
            ]]
        );
        let (store, runner) = stop_and_join(harness).await;
        assert!(store.status_updates().is_empty());
        assert!(runner
            .events()
            .iter()
            .any(|event| matches!(event, RunnerEvent::Stopped(_))));
    }

    /// A failed candidate preflight leaves the current bridge untouched.
    #[tokio::test(start_paused = true)]
    async fn preflight_failure_leaves_current_bridge_running() {
        let store = FakeStore::new();
        store.install_revision(2);
        let harness = start_harness(
            store,
            vec![SpawnScript::Ready],
            vec![Err(materialize_error(
                "executor_unavailable",
                "scripted preflight failure",
            ))],
        );
        wait_until(|| harness.store.latest_roster().apply_state == ApplyState::Failed).await;
        let roster = harness.store.latest_roster();
        assert_eq!(
            roster.apply_error_code.as_deref(),
            Some("executor_unavailable")
        );
        assert_eq!(roster.active_revision, None);
        assert_eq!(harness.runner.spawned().len(), 1);
        assert!(!harness
            .runner
            .events()
            .iter()
            .any(|event| matches!(event, RunnerEvent::Stopped(_) | RunnerEvent::Crashed(_))));

        // The failure latches: poll ticks must not reread or reapply it.
        let reads_before = harness.store.revision_reads().len();
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(harness.store.revision_reads().len(), reads_before);

        // An explicit retry hint clears the latch and the apply succeeds.
        harness
            .handle
            .retry_revision(server_id(), 2)
            .await
            .expect("retry accepted");
        wait_until(|| harness.store.latest_roster().apply_state == ApplyState::Active).await;
        assert_eq!(harness.store.latest_roster().active_revision, Some(2));
        stop_and_join(harness).await;
    }

    /// A candidate that dies before readiness restarts the last known good bridge.
    #[tokio::test(start_paused = true)]
    async fn candidate_startup_failure_restarts_last_known_good() {
        let store = FakeStore::new();
        store.install_revision(2);
        let harness = start_harness(
            store,
            vec![
                SpawnScript::Ready,
                SpawnScript::FailStartup,
                SpawnScript::Ready,
            ],
            Vec::new(),
        );
        wait_until(|| harness.runner.spawned().len() == 3).await;
        let spawned = harness.runner.spawned();
        assert_eq!(spawned[1], vec!["agent-r2".to_string()]);
        assert_eq!(spawned[2], spawned[0]);
        wait_until(|| harness.store.latest_roster().apply_state == ApplyState::Failed).await;
        let roster = harness.store.latest_roster();
        assert_eq!(
            roster.apply_error_code.as_deref(),
            Some("bridge_start_failed")
        );
        assert_eq!(roster.active_revision, None);
        assert_eq!(roster.last_good_revision, None);
        stop_and_join(harness).await;
    }

    /// Successful candidates advance both the active and last-good revisions.
    #[tokio::test(start_paused = true)]
    async fn successful_candidate_advances_active_and_last_good() {
        let store = FakeStore::new();
        store.install_revision(2);
        let harness = start_harness(store, Vec::new(), Vec::new());
        wait_until(|| {
            let roster = harness.store.latest_roster();
            roster.apply_state == ApplyState::Active && roster.active_revision == Some(2)
        })
        .await;
        assert_eq!(harness.store.latest_roster().last_good_revision, Some(2));

        harness.store.install_revision(3);
        harness
            .handle
            .revision_committed(server_id(), 3)
            .await
            .expect("commit hint accepted");
        wait_until(|| harness.store.latest_roster().active_revision == Some(3)).await;
        let roster = harness.store.latest_roster();
        assert_eq!(roster.apply_state, ApplyState::Active);
        assert_eq!(roster.last_good_revision, Some(3));

        // The revision 2 bridge stopped before its successor started.
        let events = harness.runner.events();
        let stop_r2 = events
            .iter()
            .position(|event| *event == RunnerEvent::Stopped(vec!["agent-r2".to_string()]))
            .expect("revision 2 bridge stopped");
        let spawn_r3 = events
            .iter()
            .position(|event| *event == RunnerEvent::Spawned(vec!["agent-r3".to_string()]))
            .expect("revision 3 bridge spawned");
        assert!(stop_r2 < spawn_r3);
        stop_and_join(harness).await;
    }

    /// Burst notifications coalesce so only the newest durable revision applies.
    #[tokio::test(start_paused = true)]
    async fn notifications_coalesce_to_newest_desired_revision() {
        let store = FakeStore::new();
        store.install_revision(2);
        store.install_revision(3);
        let harness = start_harness(store, Vec::new(), Vec::new());
        harness
            .handle
            .revision_committed(server_id(), 2)
            .await
            .expect("first hint");
        harness
            .handle
            .revision_committed(server_id(), 3)
            .await
            .expect("second hint");
        wait_until(|| harness.store.latest_roster().active_revision == Some(3)).await;
        assert!(harness
            .store
            .revision_reads()
            .iter()
            .all(|revision| *revision == 3));
        assert!(!harness
            .runner
            .spawned()
            .iter()
            .any(|usernames| usernames == &vec!["agent-r2".to_string()]));
        stop_and_join(harness).await;
    }

    /// Parent stop terminates the bridge before the reconciler returns.
    #[tokio::test(start_paused = true)]
    async fn parent_stop_stops_bridge_before_reconciler_returns() {
        let harness = start_harness(FakeStore::new(), Vec::new(), Vec::new());
        wait_until(|| harness.runner.spawned().len() == 1).await;
        let _ = harness.stop.send(true);
        harness
            .task
            .await
            .expect("reconciler task join")
            .expect("clean reconciler exit");
        let events = harness.runner.events();
        assert!(matches!(events.last(), Some(RunnerEvent::Stopped(_))));
    }

    /// A bridge already running the desired revision heals lagging durable status.
    #[tokio::test(start_paused = true)]
    async fn matching_active_revision_heals_pending_status_without_restart() {
        let store = FakeStore::new();
        store.install_revision(2);
        store.fail_next_status_write();
        let harness = start_harness(store, Vec::new(), Vec::new());
        wait_until(|| harness.store.latest_roster().apply_state == ApplyState::Active).await;
        // The first write failed; the poll loop healed durable state without
        // restarting the already-correct bridge.
        assert!(harness.store.status_updates().len() >= 2);
        assert_eq!(harness.store.latest_roster().active_revision, Some(2));
        assert_eq!(harness.runner.spawned().len(), 2);
        stop_and_join(harness).await;
    }

    /// An unexpected bridge crash restarts the last known good configuration.
    #[tokio::test(start_paused = true)]
    async fn crashed_bridge_restarts_last_known_good() {
        let harness = start_harness(FakeStore::new(), Vec::new(), Vec::new());
        wait_until(|| harness.runner.spawned().len() == 1).await;
        harness.runner.crash_latest();
        wait_until(|| harness.runner.spawned().len() == 2).await;
        let spawned = harness.runner.spawned();
        assert_eq!(spawned[1], spawned[0]);
        assert!(harness
            .runner
            .events()
            .iter()
            .any(|event| matches!(event, RunnerEvent::Crashed(_))));
        stop_and_join(harness).await;
    }

    /// A latched failure blocks only its own revision, never a newer one.
    #[tokio::test(start_paused = true)]
    async fn latched_failure_yields_to_new_desired_revision() {
        let store = FakeStore::new();
        store.install_revision(2);
        let harness = start_harness(
            store,
            Vec::new(),
            vec![Err(materialize_error(
                "executor_unavailable",
                "scripted preflight failure",
            ))],
        );
        wait_until(|| harness.store.latest_roster().apply_state == ApplyState::Failed).await;
        assert_eq!(harness.runner.spawned().len(), 1);

        // A NEW desired revision must apply without any retry hint.
        harness.store.install_revision(3);
        harness
            .handle
            .revision_committed(server_id(), 3)
            .await
            .expect("commit hint accepted");
        wait_until(|| harness.store.latest_roster().active_revision == Some(3)).await;
        assert_eq!(
            harness.store.latest_roster().apply_state,
            ApplyState::Active
        );
        // The latched revision 2 was read once for its failed apply and never again.
        assert_eq!(
            harness
                .store
                .revision_reads()
                .iter()
                .filter(|revision| **revision == 2)
                .count(),
            1
        );
        assert_eq!(harness.runner.spawned()[1], vec!["agent-r3".to_string()]);
        stop_and_join(harness).await;
    }

    /// Transient store read failures never kill the bridge or the supervisor.
    #[tokio::test(start_paused = true)]
    async fn store_read_failures_are_transient() {
        let store = FakeStore::new();
        store.install_revision(2);
        store.fail_next_roster_read();
        store.fail_next_revision_read();
        let harness = start_harness(store, Vec::new(), Vec::new());
        wait_until(|| harness.store.latest_roster().active_revision == Some(2)).await;
        // Neither read failure produced a status write or a bridge restart;
        // the only write is the successful activation.
        let updates = harness.store.status_updates();
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].apply_state, ApplyState::Active);
        assert_eq!(harness.runner.spawned().len(), 2);
        stop_and_join(harness).await;
    }

    /// Failure writes preserve durable last-good history across process restarts.
    #[tokio::test(start_paused = true)]
    async fn apply_failure_preserves_durable_last_good_after_restart() {
        let store = FakeStore::new();
        store.install_revision(4);
        // A previous process lifetime proved revision 3 good.
        store.set_durable_history(Some(3), Some(3));
        let harness = start_harness(
            store,
            Vec::new(),
            vec![Err(materialize_error(
                "executor_unavailable",
                "scripted preflight failure",
            ))],
        );
        wait_until(|| harness.store.latest_roster().apply_state == ApplyState::Failed).await;
        let roster = harness.store.latest_roster();
        // The candidate failure must not erase the durable last-good pointer,
        // while active honestly reflects the TOML bridge running here.
        assert_eq!(roster.last_good_revision, Some(3));
        assert_eq!(roster.active_revision, None);
        stop_and_join(harness).await;
    }

    /// Control-plane validation rejects rosters with stable error categories.
    #[tokio::test(start_paused = true)]
    async fn validate_revision_maps_stable_error_categories() {
        let store = FakeStore::new();
        store.install_revision(2);
        let harness = start_harness(store, Vec::new(), Vec::new());
        let seats = harness.store.revision_seats_input(2);

        // Wrong server routing is an internal failure.
        assert!(matches!(
            harness
                .handle
                .validate_revision(Uuid::from_u128(99), Uuid::nil(), &seats)
                .await,
            Err(ManagedAgentControlError::Internal(_))
        ));

        // A fully disabled roster cannot produce a runnable bridge.
        let mut disabled = seats.clone();
        for seat in &mut disabled {
            seat.enabled = false;
        }
        assert!(matches!(
            harness
                .handle
                .validate_revision(server_id(), Uuid::nil(), &disabled)
                .await,
            Err(ManagedAgentControlError::CapabilityUnavailable(_))
        ));

        // An unknown model is a capability failure.
        let mut unknown = seats.clone();
        unknown[0].model_id = "missing-model".to_string();
        assert!(matches!(
            harness
                .handle
                .validate_revision(server_id(), Uuid::nil(), &unknown)
                .await,
            Err(ManagedAgentControlError::CapabilityUnavailable(_))
        ));

        // An unresolvable binding is a credential failure.
        let mut bound = seats.clone();
        bound[0].credential_binding_id = Some(Uuid::new_v4());
        assert!(matches!(
            harness
                .handle
                .validate_revision(server_id(), Uuid::nil(), &bound)
                .await,
            Err(ManagedAgentControlError::CredentialNotReady(_))
        ));

        // An unknown agent identity is a capability failure with a safe message.
        let mut ghost = seats.clone();
        ghost[0].agent_user_id = Uuid::new_v4();
        assert!(matches!(
            harness
                .handle
                .validate_revision(server_id(), Uuid::nil(), &ghost)
                .await,
            Err(ManagedAgentControlError::CapabilityUnavailable(_))
        ));

        // The healthy roster validates cleanly.
        harness
            .handle
            .validate_revision(server_id(), Uuid::nil(), &seats)
            .await
            .expect("valid roster accepted");
        stop_and_join(harness).await;
    }
}
