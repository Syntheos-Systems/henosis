//! Proposal coordinator: capability check, task claim, pending-approval notice.

use std::sync::Arc;

use crate::capability::{CapabilityDecision, CapabilityOracle};
use crate::config::WorkspaceConfig;
use crate::error::BridgeError;
use crate::execution::approval::ApprovalRegistry;
use crate::execution::sandbox::resolve_workspace;
use crate::execution::{ProposalId, RoomNotifier};
use crate::executor::ExecutionProposal;
use crate::kleos::KleosClient;

/// What the coordinator did with a proposal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CoordinatorOutcome {
    /// Proposal was granted, a task claimed, and approval is pending.
    AwaitingApproval(ProposalId),
    /// Proposal was denied for missing capabilities.
    Denied,
    /// Proposal could not be handled (no workspace configured).
    Rejected(String),
}

/// Routes an `ExecutionProposal` through capability checking, task claiming,
/// and approval registration.
pub struct ProposalCoordinator {
    /// Capability trust boundary.
    oracle: Arc<dyn CapabilityOracle>,
    /// Kleos client for Chiasm task creation.
    kleos: Arc<dyn KleosClient>,
    /// Room notifier for approval and denial notices.
    notifier: Arc<dyn RoomNotifier>,
    /// Shared approval registry.
    registry: ApprovalRegistry,
    /// Project name for Kleos scoping and workspace resolution.
    project_name: String,
    /// Declared workspaces.
    workspaces: Vec<WorkspaceConfig>,
}

/// Coordinator construction and proposal handling.
impl ProposalCoordinator {
    /// Build a coordinator from its collaborators.
    pub fn new(
        oracle: Arc<dyn CapabilityOracle>,
        kleos: Arc<dyn KleosClient>,
        notifier: Arc<dyn RoomNotifier>,
        registry: ApprovalRegistry,
        project_name: String,
        workspaces: Vec<WorkspaceConfig>,
    ) -> Self {
        Self {
            oracle,
            kleos,
            notifier,
            registry,
            project_name,
            workspaces,
        }
    }

    /// Handle one execution proposal end-to-end up to the approval gate.
    pub async fn handle_proposal(
        &self,
        agent: &str,
        proposal: &ExecutionProposal,
    ) -> Result<CoordinatorOutcome, BridgeError> {
        // 1. Capability check.
        let granted = match self
            .oracle
            .check(agent, &proposal.required_capabilities)
            .await?
        {
            CapabilityDecision::Granted(caps) => caps,
            CapabilityDecision::Denied(missing) => {
                let names: Vec<String> = missing.iter().map(|c| c.to_string()).collect();
                let _ = self
                    .notifier
                    .notify(&format!(
                        "[EXEC] proposal by {agent} denied: missing capabilities {}",
                        names.join(", ")
                    ))
                    .await;
                return Ok(CoordinatorOutcome::Denied);
            }
        };

        // 2. Resolve workspace.
        let workspace = match resolve_workspace(&self.workspaces, &self.project_name) {
            Some(w) => w.name.clone(),
            None => {
                let _ = self
                    .notifier
                    .notify("[EXEC] proposal rejected: no workspace configured")
                    .await;
                return Ok(CoordinatorOutcome::Rejected(
                    "no workspace configured".into(),
                ));
            }
        };

        // 3. Claim a Chiasm task.
        let title = format!("[exec] {}", truncate(&proposal.scope_summary, 64));
        let task_id = self
            .kleos
            .create_execution_task(&self.project_name, agent, &title, &proposal.scope_summary)
            .await?;

        // 4. Register pending approval.
        let id = self.registry.register(
            agent.to_string(),
            task_id.clone(),
            proposal.scope_summary.clone(),
            granted,
            workspace,
        );

        // 5. Notify the room.
        let _ = self
            .notifier
            .notify(&format!(
                "[EXEC] {agent} proposes: {} (task {task_id}). Reply `!approve {id}` to run or `!reject {id}` to decline.",
                proposal.scope_summary
            ))
            .await;

        Ok(CoordinatorOutcome::AwaitingApproval(id))
    }
}

/// Truncate a string to at most `max` chars for compact titles.
fn truncate(s: &str, max: usize) -> String {
    s.trim().chars().take(max).collect()
}

#[cfg(test)]
mod tests {
    use super::{CoordinatorOutcome, ProposalCoordinator};
    use crate::capability::{CapabilityDecision, CapabilityOracle};
    use crate::config::WorkspaceConfig;
    use crate::error::BridgeError;
    use crate::execution::approval::ApprovalRegistry;
    use crate::execution::RoomNotifier;
    use crate::executor::{Capability, ExecutionProposal};
    use crate::kleos::KleosClient;
    use async_trait::async_trait;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    /// Oracle that grants everything.
    struct GrantAll;
    #[async_trait]
    impl CapabilityOracle for GrantAll {
        async fn check(
            &self,
            _a: &str,
            required: &[Capability],
        ) -> Result<CapabilityDecision, BridgeError> {
            Ok(CapabilityDecision::Granted(required.to_vec()))
        }
    }

    /// Oracle that denies everything.
    struct DenyAll;
    #[async_trait]
    impl CapabilityOracle for DenyAll {
        async fn check(
            &self,
            _a: &str,
            required: &[Capability],
        ) -> Result<CapabilityDecision, BridgeError> {
            Ok(CapabilityDecision::Denied(required.to_vec()))
        }
    }

    /// Kleos fake that records the created execution task and returns a fixed id.
    struct FakeKleos {
        /// Whether create_execution_task was called.
        created: Arc<Mutex<bool>>,
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
        async fn active_tasks_summary(
            &self,
            _: &str,
            _: usize,
        ) -> Result<Option<String>, BridgeError> {
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
            *self.created.lock().unwrap() = true;
            Ok("99".into())
        }
        async fn update_task_status(&self, _: &str, _: &str, _: &str) -> Result<(), BridgeError> {
            Ok(())
        }
    }

    /// Notifier that records posts.
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

    /// Single-workspace config for the coordinator.
    fn workspaces() -> Vec<WorkspaceConfig> {
        vec![WorkspaceConfig {
            name: "rift".into(),
            path: PathBuf::from("/tmp/rift"),
            cargo_target_dir: None,
        }]
    }

    /// A proposal needing bash.
    fn proposal() -> ExecutionProposal {
        ExecutionProposal {
            scope_summary: "Implement the widget".into(),
            estimated_effort: Some("1 hour".into()),
            required_capabilities: vec![Capability::new(Capability::BASH)],
        }
    }

    /// Verifies a granted proposal claims a task, registers, and notifies.
    #[tokio::test]
    async fn test_granted_proposal_registers_and_notifies() {
        let registry = ApprovalRegistry::new(1800);
        let posts = Arc::new(Mutex::new(Vec::new()));
        let created = Arc::new(Mutex::new(false));
        let coordinator = ProposalCoordinator::new(
            Arc::new(GrantAll),
            Arc::new(FakeKleos {
                created: created.clone(),
            }),
            Arc::new(RecordingNotifier {
                posts: posts.clone(),
            }),
            registry.clone(),
            "rift".to_string(),
            workspaces(),
        );

        let outcome = coordinator
            .handle_proposal("architect", &proposal())
            .await
            .unwrap();

        assert!(matches!(outcome, CoordinatorOutcome::AwaitingApproval(_)));
        assert!(*created.lock().unwrap());
        assert_eq!(registry.list().len(), 1);
        assert!(posts.lock().unwrap().iter().any(|p| p.contains("!approve")));
    }

    /// Verifies a denied proposal posts a denial and registers nothing.
    #[tokio::test]
    async fn test_denied_proposal_notifies_and_skips() {
        let registry = ApprovalRegistry::new(1800);
        let posts = Arc::new(Mutex::new(Vec::new()));
        let created = Arc::new(Mutex::new(false));
        let coordinator = ProposalCoordinator::new(
            Arc::new(DenyAll),
            Arc::new(FakeKleos {
                created: created.clone(),
            }),
            Arc::new(RecordingNotifier {
                posts: posts.clone(),
            }),
            registry.clone(),
            "rift".to_string(),
            workspaces(),
        );

        let outcome = coordinator
            .handle_proposal("architect", &proposal())
            .await
            .unwrap();

        assert!(matches!(outcome, CoordinatorOutcome::Denied));
        assert!(!*created.lock().unwrap());
        assert!(registry.list().is_empty());
        assert!(posts.lock().unwrap().iter().any(|p| p.contains("denied")));
    }
}
