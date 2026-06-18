//! Integration test: proposal -> approval -> supervised execution with fakes.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use henosis_rift_bridge::capability::{CapabilityDecision, CapabilityOracle};
use henosis_rift_bridge::config::WorkspaceConfig;
use henosis_rift_bridge::error::BridgeError;
use henosis_rift_bridge::execution::approval::ApprovalRegistry;
use henosis_rift_bridge::execution::coordinator::{CoordinatorOutcome, ProposalCoordinator};
use henosis_rift_bridge::execution::supervisor::{ExecutionSupervisor, SupervisedTask};
use henosis_rift_bridge::execution::RoomNotifier;
use henosis_rift_bridge::executor::{
    AgentExecutor, AgentResponse, Capability, DiscussionContext, ExecutionProposal,
    ExecutionResult, ExecutionSandbox, HealthStatus, ProgressUpdate, TaskContext,
};
use henosis_rift_bridge::kleos::KleosClient;

/// Oracle granting everything requested.
struct GrantAll;
#[async_trait]
impl CapabilityOracle for GrantAll {
    async fn check(&self, _a: &str, req: &[Capability]) -> Result<CapabilityDecision, BridgeError> {
        Ok(CapabilityDecision::Granted(req.to_vec()))
    }
}

/// Kleos fake returning a fixed task id and recording status updates.
struct FakeKleos {
    /// Captured (task_id, status) updates.
    updates: Arc<Mutex<Vec<(String, String)>>>,
}
#[async_trait]
impl KleosClient for FakeKleos {
    async fn search_memories(
        &self,
        _: &str,
        _: &str,
        _: &[(String, String)],
        _: usize,
    ) -> Result<Vec<String>, BridgeError> {
        Ok(vec![])
    }
    async fn active_tasks_summary(&self, _: &str, _: usize) -> Result<Option<String>, BridgeError> {
        Ok(None)
    }
    async fn report_activity(
        &self,
        _: &str,
        _: &str,
        _: &str,
        _: &str,
        _: serde_json::Value,
    ) -> Result<(), BridgeError> {
        Ok(())
    }
    async fn store_consensus_memory(
        &self,
        _: &str,
        _: &str,
        _: &[String],
    ) -> Result<(), BridgeError> {
        Ok(())
    }
    async fn create_draft_task(
        &self,
        _: &str,
        _: &str,
        _: &str,
        _: &str,
    ) -> Result<(), BridgeError> {
        Ok(())
    }
    async fn create_execution_task(
        &self,
        _: &str,
        _: &str,
        _: &str,
        _: &str,
    ) -> Result<String, BridgeError> {
        Ok("777".into())
    }
    async fn update_task_status(
        &self,
        task_id: &str,
        status: &str,
        _: &str,
    ) -> Result<(), BridgeError> {
        self.updates
            .lock()
            .unwrap()
            .push((task_id.into(), status.into()));
        Ok(())
    }
}

/// Notifier recording every post.
struct RecordingNotifier {
    /// Captured posts.
    posts: Arc<Mutex<Vec<String>>>,
}
#[async_trait]
impl RoomNotifier for RecordingNotifier {
    async fn notify(&self, content: &str) -> Result<(), BridgeError> {
        self.posts.lock().unwrap().push(content.to_string());
        Ok(())
    }
}

/// Executor that succeeds immediately.
struct OkExecutor;
#[async_trait]
impl AgentExecutor for OkExecutor {
    fn required_capabilities(&self) -> Vec<Capability> {
        vec![Capability::new(Capability::BASH)]
    }
    fn sandbox(&self) -> ExecutionSandbox {
        ExecutionSandbox {
            branch: "agent/a/task-777".into(),
            working_dir: PathBuf::from("/tmp"),
            max_runtime_secs: 0,
        }
    }
    async fn discuss(&self, _c: DiscussionContext) -> anyhow::Result<Option<AgentResponse>> {
        Ok(None)
    }
    async fn execute(
        &self,
        _t: TaskContext,
        tx: tokio::sync::mpsc::Sender<ProgressUpdate>,
    ) -> anyhow::Result<ExecutionResult> {
        let _ = tx.send(ProgressUpdate::Done).await;
        Ok(ExecutionResult::Success {
            summary: "implemented".into(),
            commit_hash: Some("deadbeef".into()),
            evidence: None,
        })
    }
    async fn health_check(&self) -> anyhow::Result<HealthStatus> {
        Ok(HealthStatus::Ready)
    }
}

/// Full flow: granted proposal registers, approval pops it, supervisor runs it,
/// and the Chiasm task ends up completed.
#[tokio::test]
async fn test_full_propose_approve_execute_flow() {
    let registry = ApprovalRegistry::new(1800);
    let posts = Arc::new(Mutex::new(Vec::new()));
    let updates = Arc::new(Mutex::new(Vec::new()));

    let notifier: Arc<dyn RoomNotifier> = Arc::new(RecordingNotifier {
        posts: posts.clone(),
    });
    let kleos: Arc<dyn KleosClient> = Arc::new(FakeKleos {
        updates: updates.clone(),
    });
    let workspaces = vec![WorkspaceConfig {
        name: "rift".into(),
        path: PathBuf::from("/tmp/rift"),
        cargo_target_dir: None,
    }];

    let coordinator = ProposalCoordinator::new(
        Arc::new(GrantAll),
        kleos.clone(),
        notifier.clone(),
        registry.clone(),
        "rift".to_string(),
        workspaces,
    );

    let proposal = ExecutionProposal {
        scope_summary: "Implement the feature".into(),
        estimated_effort: None,
        required_capabilities: vec![Capability::new(Capability::BASH)],
    };

    // Propose.
    let outcome = coordinator
        .handle_proposal("architect", &proposal)
        .await
        .unwrap();
    let id = match outcome {
        CoordinatorOutcome::AwaitingApproval(id) => id,
        other => panic!("expected awaiting approval, got {other:?}"),
    };
    assert_eq!(registry.list().len(), 1);

    // Approve.
    let pending = registry.approve(id).expect("proposal should be approvable");
    assert_eq!(pending.task_id, "777");

    // Supervise (skip real git sandbox; build one inline).
    let supervisor = ExecutionSupervisor::new(notifier.clone());
    let task = SupervisedTask {
        executor: Arc::new(OkExecutor),
        task_id: pending.task_id.clone(),
        description: pending.scope_summary.clone(),
        sandbox: ExecutionSandbox {
            branch: "agent/architect/task-777".into(),
            working_dir: PathBuf::from("/tmp"),
            max_runtime_secs: 0,
        },
        granted_capabilities: pending.granted_capabilities.clone(),
    };
    let result = supervisor.run(task).await;
    assert!(matches!(result, ExecutionResult::Success { .. }));

    // Finalize task status the way the room would.
    kleos
        .update_task_status(&pending.task_id, "completed", "implemented")
        .await
        .unwrap();

    // Assertions on the recorded side effects.
    assert!(posts.lock().unwrap().iter().any(|p| p.contains("!approve")));
    assert!(posts
        .lock()
        .unwrap()
        .iter()
        .any(|p| p.contains("success: implemented")));
    assert!(updates
        .lock()
        .unwrap()
        .iter()
        .any(|(t, s)| t == "777" && s == "completed"));
}
