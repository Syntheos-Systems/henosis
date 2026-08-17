//! Approval dispatch decoupled from the room's mutable state.
//!
//! Executing an approved proposal only ever touched the room's Arc-shared
//! execution fields, yet it lived on `Room`, so control-server approvals had
//! to wait for the event loop -- and the event loop spends minutes inside
//! conversation cascades (2026-07-17 review finding 3, accepted then as
//! bounded latency). Extracting the dispatcher removes the coupling: the
//! approvals drain task and the in-cascade control-command path both dispatch
//! immediately, and the whole execution pipeline runs on a spawned task so no
//! caller stalls on health checks or worktree creation.
//!
//! Leadership cancellation drops each local stage future, but it is not a
//! rollback boundary. Arbitrary process descendants still require OS process
//! groups or cgroups, and remote provider effects require downstream
//! cancellation or idempotency.

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;

use crate::config::WorkspaceConfig;
use crate::execution::approval::ApprovalRegistry;
use crate::execution::preflight::{apply_runtime_policy, health_preflight, Preflight};
use crate::execution::sandbox::{resolve_workspace, SandboxManager};
use crate::execution::supervisor::{ExecutionSupervisor, SupervisedTask};
use crate::execution::PendingProposal;
use crate::executor::AgentExecutor;
use crate::kleos::KleosClient;
use crate::leadership::{wait_for_revocation, LeadershipGuard};

/// Dispatches human-approved execution proposals into supervised sandbox
/// sessions. Every field is shared ownership, so the dispatcher clones
/// cheaply into the control server drain task, the sweep task, and the room.
#[derive(Clone)]
pub struct ApprovalDispatcher {
    /// Executors keyed by agent username (proposals carry usernames).
    executors_by_username: HashMap<String, Arc<dyn AgentExecutor>>,
    /// Declared workspaces an approved task may execute against.
    workspaces: Arc<Vec<WorkspaceConfig>>,
    /// Creates per-task git worktrees.
    sandbox_manager: Arc<SandboxManager>,
    /// Supervises approved execution sessions.
    supervisor: Arc<ExecutionSupervisor>,
    /// Bounds simultaneous execution sessions.
    exec_semaphore: Arc<tokio::sync::Semaphore>,
    /// Kleos client for task status writes and result memories.
    kleos: Arc<dyn KleosClient>,
    /// Project name used for Kleos scoping.
    project_name: String,
    /// Shared approval registry (for expiry sweeps).
    approval_registry: ApprovalRegistry,
    /// Monotonic managed-room leadership gate for dispatch effects.
    leadership: Arc<LeadershipGuard>,
}

/// Construction, dispatch, and expiry sweeping.
impl ApprovalDispatcher {
    /// Build a dispatcher over the shared execution machinery.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        executors_by_username: HashMap<String, Arc<dyn AgentExecutor>>,
        workspaces: Arc<Vec<WorkspaceConfig>>,
        sandbox_manager: Arc<SandboxManager>,
        supervisor: Arc<ExecutionSupervisor>,
        exec_semaphore: Arc<tokio::sync::Semaphore>,
        kleos: Arc<dyn KleosClient>,
        project_name: String,
        approval_registry: ApprovalRegistry,
    ) -> Self {
        Self {
            executors_by_username,
            workspaces,
            sandbox_manager,
            supervisor,
            exec_semaphore,
            kleos,
            project_name,
            approval_registry,
            leadership: Arc::new(LeadershipGuard::unmanaged()),
        }
    }

    /// Replace the standalone guard with the managed room's shared guard.
    pub fn with_leadership_guard(mut self, leadership: Arc<LeadershipGuard>) -> Self {
        self.leadership = leadership;
        self
    }

    /// Dispatch an approved proposal. Returns immediately: preflight, sandbox
    /// creation, and the supervised run all happen on a spawned task, so a
    /// caller inside a cascade slot wait (or the drain task) never stalls.
    pub fn execute_approved(&self, proposal: PendingProposal) {
        let this = self.clone();
        tokio::spawn(async move {
            this.dispatch_inner(proposal).await;
        });
    }

    /// The full dispatch pipeline: health preflight, workspace resolution,
    /// sandbox creation, then a supervised run. The concurrency permit is
    /// acquired FIRST so `max_concurrent_executions` bounds the whole
    /// pipeline including worktree creation.
    async fn dispatch_inner(&self, proposal: PendingProposal) {
        if !self.require_current_leadership("before permit").await {
            return;
        }
        let Some(permit_result) = self
            .await_while_current(
                "execution permit acquisition",
                self.exec_semaphore.clone().acquire_owned(),
            )
            .await
        else {
            return;
        };
        let _permit = match permit_result {
            Ok(p) => p,
            Err(_) => return,
        };
        if !self.require_current_leadership("after permit").await {
            return;
        }

        let executor = match self.executors_by_username.get(&proposal.agent) {
            Some(e) => e.clone(),
            None => {
                tracing::error!("no executor for proposing agent {}", proposal.agent);
                return;
            }
        };
        if !self.require_current_leadership("before health check").await {
            return;
        }

        // Health preflight before creating any worktree: the executor's runtime
        // must be ready, or the task is blocked rather than spawned into a dead
        // runtime (AgentExecutor contract: health_check is called before spawn).
        let Some(health_result) = self
            .await_while_current("executor health check", executor.health_check())
            .await
        else {
            return;
        };
        if !self.require_current_leadership("after health check").await {
            return;
        }
        let health = match health_result {
            Ok(status) => status,
            Err(e) => {
                let note = format!("health check failed: {e}");
                let Some(_) = self
                    .await_while_current(
                        "health failure task status writeback",
                        self.kleos
                            .update_task_status(&proposal.task_id, "blocked", &note),
                    )
                    .await
                else {
                    return;
                };
                return;
            }
        };
        match health_preflight(health) {
            Preflight::Proceed => {}
            Preflight::ProceedDegraded(reason) => {
                tracing::warn!(
                    "executor for {} is degraded but proceeding: {reason}",
                    proposal.agent
                );
            }
            Preflight::Block(reason) => {
                tracing::error!(
                    "executor for {} unavailable, blocking task {}: {reason}",
                    proposal.agent,
                    proposal.task_id
                );
                let note = format!("executor unavailable: {reason}");
                let Some(_) = self
                    .await_while_current(
                        "unavailable executor task status writeback",
                        self.kleos
                            .update_task_status(&proposal.task_id, "blocked", &note),
                    )
                    .await
                else {
                    return;
                };
                return;
            }
        }

        let workspace = match resolve_workspace(&self.workspaces, &proposal.workspace) {
            Some(w) => w,
            None => {
                tracing::error!(
                    "workspace {} not found for approved task",
                    proposal.workspace
                );
                return;
            }
        };

        if !self
            .require_current_leadership("before sandbox creation")
            .await
        {
            return;
        }
        // Create the branch-isolated sandbox before spawning.
        let Some(sandbox_result) = self
            .await_while_current(
                "sandbox creation",
                self.sandbox_manager
                    .create(workspace, &proposal.agent, &proposal.task_id),
            )
            .await
        else {
            return;
        };
        if !self
            .require_current_leadership("after sandbox creation")
            .await
        {
            return;
        }
        let sandbox = match sandbox_result {
            Ok(s) => s,
            Err(e) => {
                tracing::error!("sandbox creation failed: {e}");
                let note = format!("sandbox failed: {e}");
                let Some(_) = self
                    .await_while_current(
                        "sandbox failure task status writeback",
                        self.kleos
                            .update_task_status(&proposal.task_id, "blocked", &note),
                    )
                    .await
                else {
                    return;
                };
                return;
            }
        };

        // Clamp the worktree's wall-clock limit to the executor's declared
        // runtime policy: the bridge owns branch/path, the executor owns its
        // ceiling (never above the operator-configured limit).
        let sandbox = apply_runtime_policy(sandbox, &executor.sandbox());

        let task = SupervisedTask {
            executor,
            task_id: proposal.task_id.clone(),
            description: proposal.scope_summary.clone(),
            sandbox,
            granted_capabilities: proposal.granted_capabilities.clone(),
            // First attempt against a fresh worktree: no prior work to resume. The
            // crash-recovery path will source a partial-work summary here.
            prior_context: None,
        };

        if !self
            .require_current_leadership("before supervisor start")
            .await
        {
            return;
        }
        let Some(result) = self
            .await_while_current("supervisor completion", self.supervisor.run(task))
            .await
        else {
            return;
        };
        if !self
            .require_current_leadership("after supervisor completion")
            .await
        {
            return;
        }
        let (status, note) = match &result {
            crate::executor::ExecutionResult::Success { summary, .. } => {
                ("completed", summary.clone())
            }
            crate::executor::ExecutionResult::Failed { reason, .. } => ("blocked", reason.clone()),
        };

        // Durable execution-result memory (best-effort).
        let tags = vec![
            format!("project:{}", self.project_name),
            "kind:execution-result".to_string(),
        ];
        let memory = format!(
            "Execution result for task {} ({}): {}",
            proposal.task_id, status, note
        );
        let Some(memory_result) = self
            .await_while_current(
                "execution result memory write",
                self.kleos
                    .store_consensus_memory(&self.project_name, &memory, &tags),
            )
            .await
        else {
            return;
        };
        if let Err(e) = memory_result {
            tracing::warn!("kleos execution-result memory store failed: {e}");
        }

        if !self
            .require_current_leadership("before task status writeback")
            .await
        {
            return;
        }
        let Some(status_result) = self
            .await_while_current(
                "terminal task status writeback",
                self.kleos
                    .update_task_status(&proposal.task_id, status, &note),
            )
            .await
        else {
            return;
        };
        if let Err(e) = status_result {
            tracing::warn!("kleos task status update failed: {e}");
        }
    }

    /// Sweep expired approvals and mark their tasks blocked in Kleos.
    pub async fn sweep_expired_approvals(&self) {
        for expired in self.approval_registry.sweep_expired() {
            if !self
                .require_current_leadership("before approval expiry writeback")
                .await
            {
                return;
            }
            tracing::info!("approval {} expired", expired.id);
            let Some(_) = self
                .await_while_current(
                    "approval expiry task status writeback",
                    self.kleos
                        .update_task_status(&expired.task_id, "blocked", "approval expired"),
                )
                .await
            else {
                return;
            };
        }
    }

    /// Verify the shared generation before one dispatch effect boundary.
    async fn require_current_leadership(&self, stage: &str) -> bool {
        let Some(result) = self
            .await_while_current(stage, self.leadership.require_current())
            .await
        else {
            return false;
        };
        match result {
            Ok(_) => true,
            Err(error) => {
                tracing::error!(%error, stage, "approved execution rejected by leadership guard");
                false
            }
        }
    }

    /// Race one dispatch stage against monotonic leadership revocation.
    async fn await_while_current<F>(&self, stage: &str, future: F) -> Option<F::Output>
    where
        F: Future,
    {
        let revoked = self.leadership.subscribe();
        if self.leadership.is_revoked() {
            tracing::error!(
                stage,
                "approved execution cancelled after leadership revocation"
            );
            return None;
        }
        let revoked = wait_for_revocation(revoked);
        tokio::pin!(revoked);
        tokio::pin!(future);

        let output = tokio::select! {
            biased;
            _ = &mut revoked => {
                tracing::error!(stage, "approved execution cancelled after leadership revocation");
                return None;
            }
            output = &mut future => output,
        };
        if self.leadership.is_revoked() {
            tracing::error!(
                stage,
                "approved execution cancelled after leadership revocation"
            );
            None
        } else {
            Some(output)
        }
    }
}

#[cfg(test)]
/// Proves leadership revocation interrupts dispatch at blocked await points.
mod tests {
    use std::collections::HashMap;
    use std::future::{pending, poll_fn, Future};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::task::Poll;
    use std::time::Duration;

    use async_trait::async_trait;

    use super::ApprovalDispatcher;
    use crate::config::WorkspaceConfig;
    use crate::error::BridgeError;
    use crate::execution::approval::ApprovalRegistry;
    use crate::execution::sandbox::SandboxManager;
    use crate::execution::supervisor::ExecutionSupervisor;
    use crate::execution::{PendingProposal, ProposalId, RoomNotifier};
    use crate::executor::{
        AgentExecutor, AgentResponse, Capability, DiscussionContext, ExecutionResult,
        ExecutionSandbox, HealthStatus, ProgressUpdate, TaskContext,
    };
    use crate::kleos::KleosClient;
    use crate::leadership::LeadershipGuard;

    /// Records whether a pending test future was destroyed by cancellation.
    struct DropProbe {
        /// Shared cancellation observation.
        dropped: Arc<AtomicBool>,
    }

    /// Marks the instant a pending test future is destroyed.
    impl Drop for DropProbe {
        /// Records destruction using sequentially consistent ordering for the test.
        fn drop(&mut self) {
            self.dropped.store(true, Ordering::SeqCst);
        }
    }

    /// Executor whose health check remains pending until dispatch cancels it.
    struct BlockingHealthExecutor {
        /// Whether the health-check future was first polled.
        entered: Arc<AtomicBool>,
        /// Whether cancellation destroyed the health-check future.
        dropped: Arc<AtomicBool>,
    }

    /// Supplies a cancellation-observable health check for dispatch tests.
    #[async_trait]
    impl AgentExecutor for BlockingHealthExecutor {
        /// Declares no capabilities because dispatch never reaches execution.
        fn required_capabilities(&self) -> Vec<Capability> {
            Vec::new()
        }

        /// Returns an inert sandbox policy because dispatch never reaches execution.
        fn sandbox(&self) -> ExecutionSandbox {
            ExecutionSandbox {
                branch: "agent/test/task-test".into(),
                working_dir: PathBuf::from("/tmp"),
                max_runtime_secs: 0,
                cargo_target_dir: None,
            }
        }

        /// Produces no discussion response in dispatch cancellation tests.
        async fn discuss(
            &self,
            _context: DiscussionContext,
        ) -> anyhow::Result<Option<AgentResponse>> {
            Ok(None)
        }

        /// Fails if a cancellation test unexpectedly reaches execution.
        async fn execute(
            &self,
            _task: TaskContext,
            _progress_tx: tokio::sync::mpsc::Sender<ProgressUpdate>,
        ) -> anyhow::Result<ExecutionResult> {
            panic!("dispatch cancellation test must not reach execution")
        }

        /// Signals entry and remains pending until leadership cancellation drops it.
        async fn health_check(&self) -> anyhow::Result<HealthStatus> {
            let _probe = DropProbe {
                dropped: self.dropped.clone(),
            };
            self.entered.store(true, Ordering::SeqCst);
            pending().await
        }
    }

    /// Kleos stub used only before any dispatch writeback can occur.
    struct NoopKleos;

    /// Supplies inert Kleos behavior for cancellation-focused dispatch tests.
    #[async_trait]
    impl KleosClient for NoopKleos {
        /// Returns no memories.
        async fn search_memories(
            &self,
            _project: &str,
            _channel: &str,
            _recent_messages: &[(String, String)],
            _limit: usize,
        ) -> Result<Vec<String>, BridgeError> {
            Ok(Vec::new())
        }

        /// Returns no active task summary.
        async fn active_tasks_summary(
            &self,
            _project: &str,
            _limit: usize,
        ) -> Result<Option<String>, BridgeError> {
            Ok(None)
        }

        /// Accepts an inert activity report.
        async fn report_activity(
            &self,
            _project: &str,
            _agent: &str,
            _action: &str,
            _summary: &str,
            _metadata: serde_json::Value,
        ) -> Result<(), BridgeError> {
            Ok(())
        }

        /// Accepts an inert consensus memory.
        async fn store_consensus_memory(
            &self,
            _project: &str,
            _content: &str,
            _tags: &[String],
        ) -> Result<(), BridgeError> {
            Ok(())
        }

        /// Accepts an inert draft task.
        async fn create_draft_task(
            &self,
            _project: &str,
            _agent: &str,
            _title: &str,
            _summary: &str,
        ) -> Result<(), BridgeError> {
            Ok(())
        }

        /// Returns a fixed id for an inert execution task.
        async fn create_execution_task(
            &self,
            _project: &str,
            _agent: &str,
            _title: &str,
            _description: &str,
        ) -> Result<String, BridgeError> {
            Ok("test-task".into())
        }

        /// Accepts an inert task status update.
        async fn update_task_status(
            &self,
            _task_id: &str,
            _status: &str,
            _note: &str,
        ) -> Result<(), BridgeError> {
            Ok(())
        }
    }

    /// Room notifier stub that accepts any message.
    struct NoopNotifier;

    /// Supplies inert room notification behavior for dispatcher construction.
    #[async_trait]
    impl RoomNotifier for NoopNotifier {
        /// Accepts an inert notification.
        async fn notify(&self, _content: &str) -> Result<(), BridgeError> {
            Ok(())
        }
    }

    /// Builds a dispatcher whose health stage can be observed and blocked.
    fn dispatcher(
        semaphore: Arc<tokio::sync::Semaphore>,
        leadership: Arc<LeadershipGuard>,
        entered: Arc<AtomicBool>,
        dropped: Arc<AtomicBool>,
    ) -> ApprovalDispatcher {
        let executor: Arc<dyn AgentExecutor> =
            Arc::new(BlockingHealthExecutor { entered, dropped });
        let executors_by_username = HashMap::from([("tester".to_string(), executor)]);
        let workspaces = Arc::new(vec![WorkspaceConfig {
            name: "test".into(),
            path: PathBuf::from("/tmp"),
            cargo_target_dir: None,
        }]);
        let supervisor = Arc::new(
            ExecutionSupervisor::new(Arc::new(NoopNotifier))
                .with_leadership_guard(leadership.clone()),
        );

        ApprovalDispatcher::new(
            executors_by_username,
            workspaces,
            Arc::new(SandboxManager::new(PathBuf::from("/tmp"), 0)),
            supervisor,
            semaphore,
            Arc::new(NoopKleos),
            "test".into(),
            ApprovalRegistry::new(60),
        )
        .with_leadership_guard(leadership)
    }

    /// Builds the approved proposal consumed by cancellation tests.
    fn proposal() -> PendingProposal {
        PendingProposal {
            id: ProposalId(1),
            agent: "tester".into(),
            task_id: "test-task".into(),
            scope_summary: "test cancellation".into(),
            granted_capabilities: Vec::new(),
            workspace: "test".into(),
        }
    }

    /// Revocation resolves dispatch while its execution permit remains unavailable.
    #[tokio::test]
    async fn revocation_cancels_blocked_permit_wait() {
        let semaphore = Arc::new(tokio::sync::Semaphore::new(1));
        let held_permit = semaphore
            .clone()
            .acquire_owned()
            .await
            .expect("test semaphore remains open");
        let leadership = Arc::new(LeadershipGuard::unmanaged());
        let health_entered = Arc::new(AtomicBool::new(false));
        let health_dropped = Arc::new(AtomicBool::new(false));
        let dispatcher = dispatcher(
            semaphore,
            leadership.clone(),
            health_entered.clone(),
            health_dropped,
        );
        let mut dispatch = Box::pin(dispatcher.dispatch_inner(proposal()));

        poll_fn(|context| match dispatch.as_mut().poll(context) {
            Poll::Pending => Poll::Ready(()),
            Poll::Ready(()) => panic!("dispatch completed despite the held permit"),
        })
        .await;
        assert!(!health_entered.load(Ordering::SeqCst));

        leadership.revoke();

        tokio::time::timeout(Duration::from_millis(100), dispatch)
            .await
            .expect("revocation must cancel a blocked permit wait");
        assert!(!health_entered.load(Ordering::SeqCst));
        drop(held_permit);
    }

    /// Revocation drops a health-check future that has already become pending.
    #[tokio::test]
    async fn revocation_cancels_blocked_health_check() {
        let leadership = Arc::new(LeadershipGuard::unmanaged());
        let health_entered = Arc::new(AtomicBool::new(false));
        let health_dropped = Arc::new(AtomicBool::new(false));
        let dispatcher = dispatcher(
            Arc::new(tokio::sync::Semaphore::new(1)),
            leadership.clone(),
            health_entered.clone(),
            health_dropped.clone(),
        );
        let mut dispatch = Box::pin(dispatcher.dispatch_inner(proposal()));

        poll_fn(|context| match dispatch.as_mut().poll(context) {
            Poll::Pending => Poll::Ready(()),
            Poll::Ready(()) => panic!("dispatch completed despite the pending health check"),
        })
        .await;
        assert!(health_entered.load(Ordering::SeqCst));
        assert!(!health_dropped.load(Ordering::SeqCst));

        leadership.revoke();

        tokio::time::timeout(Duration::from_millis(100), dispatch)
            .await
            .expect("revocation must cancel a blocked health check");
        assert!(health_dropped.load(Ordering::SeqCst));
    }
}
