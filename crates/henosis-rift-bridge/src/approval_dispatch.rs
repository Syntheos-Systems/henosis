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

use std::collections::HashMap;
use std::sync::Arc;

use crate::config::WorkspaceConfig;
use crate::execution::preflight::{apply_runtime_policy, health_preflight, Preflight};
use crate::execution::sandbox::{resolve_workspace, SandboxManager};
use crate::execution::supervisor::{ExecutionSupervisor, SupervisedTask};
use crate::execution::approval::ApprovalRegistry;
use crate::execution::PendingProposal;
use crate::executor::AgentExecutor;
use crate::kleos::KleosClient;

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
        }
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
    /// pipeline including worktree creation -- the pre-extraction code got
    /// this bound implicitly from running preflight inline in the single
    /// event loop (adversarial review finding).
    async fn dispatch_inner(&self, proposal: PendingProposal) {
        let _permit = match self.exec_semaphore.clone().acquire_owned().await {
            Ok(p) => p,
            Err(_) => return,
        };

        let executor = match self.executors_by_username.get(&proposal.agent) {
            Some(e) => e.clone(),
            None => {
                tracing::error!("no executor for proposing agent {}", proposal.agent);
                return;
            }
        };

        // Health preflight before creating any worktree: the executor's runtime
        // must be ready, or the task is blocked rather than spawned into a dead
        // runtime (AgentExecutor contract: health_check is called before spawn).
        let health = match executor.health_check().await {
            Ok(status) => status,
            Err(e) => {
                let _ = self
                    .kleos
                    .update_task_status(
                        &proposal.task_id,
                        "blocked",
                        &format!("health check failed: {e}"),
                    )
                    .await;
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
                let _ = self
                    .kleos
                    .update_task_status(
                        &proposal.task_id,
                        "blocked",
                        &format!("executor unavailable: {reason}"),
                    )
                    .await;
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

        // Create the branch-isolated sandbox before spawning.
        let sandbox = match self
            .sandbox_manager
            .create(workspace, &proposal.agent, &proposal.task_id)
            .await
        {
            Ok(s) => s,
            Err(e) => {
                tracing::error!("sandbox creation failed: {e}");
                let _ = self
                    .kleos
                    .update_task_status(
                        &proposal.task_id,
                        "blocked",
                        &format!("sandbox failed: {e}"),
                    )
                    .await;
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

        let result = self.supervisor.run(task).await;
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
        if let Err(e) = self
            .kleos
            .store_consensus_memory(&self.project_name, &memory, &tags)
            .await
        {
            tracing::warn!("kleos execution-result memory store failed: {e}");
        }

        if let Err(e) = self
            .kleos
            .update_task_status(&proposal.task_id, status, &note)
            .await
        {
            tracing::warn!("kleos task status update failed: {e}");
        }
    }

    /// Sweep expired approvals and mark their tasks blocked in Kleos.
    pub async fn sweep_expired_approvals(&self) {
        for expired in self.approval_registry.sweep_expired() {
            tracing::info!("approval {} expired", expired.id);
            let _ = self
                .kleos
                .update_task_status(&expired.task_id, "blocked", "approval expired")
                .await;
        }
    }
}
