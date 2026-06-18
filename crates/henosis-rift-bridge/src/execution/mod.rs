//! Execution-mode orchestration: proposals, approval, sandboxing, supervision.

pub mod approval;
pub mod command;
pub mod coordinator;
pub mod sandbox;
pub mod supervisor;

use std::sync::Arc;

use async_trait::async_trait;
use uuid::Uuid;

use crate::error::BridgeError;
use crate::executor::Capability;
use crate::rift_client::RiftRestClient;

/// Opaque identifier for a pending execution proposal, shown to humans.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ProposalId(pub u64);

/// Renders the id as a short token for room display (e.g., "7").
impl std::fmt::Display for ProposalId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// A proposal that has passed capability checking and awaits human approval.
#[derive(Debug, Clone)]
pub struct PendingProposal {
    /// Human-facing approval id.
    pub id: ProposalId,
    /// Username of the proposing agent.
    pub agent: String,
    /// Claimed Chiasm task id.
    pub task_id: String,
    /// Human-readable scope of the proposed work.
    pub scope_summary: String,
    /// Capabilities granted for this task.
    pub granted_capabilities: Vec<Capability>,
    /// Resolved workspace name the task runs against.
    pub workspace: String,
}

/// Posts system notices to the room. Abstracted so the coordinator and
/// supervisor can be unit-tested without a live Rift server.
#[async_trait]
pub trait RoomNotifier: Send + Sync {
    /// Post a message to the room channel.
    async fn notify(&self, content: &str) -> Result<(), BridgeError>;
}

/// Real notifier backed by the Rift REST client, posting as a fixed identity.
pub struct RiftRoomNotifier {
    /// Shared Rift REST client.
    rift: Arc<RiftRestClient>,
    /// Rift user id the notice is posted as.
    user_id: Uuid,
    /// Username the notice is posted as.
    username: String,
    /// Target channel.
    channel_id: Uuid,
}

/// Construction for the Rift-backed room notifier.
impl RiftRoomNotifier {
    /// Build a notifier posting as the given agent identity into a channel.
    pub fn new(
        rift: Arc<RiftRestClient>,
        user_id: Uuid,
        username: String,
        channel_id: Uuid,
    ) -> Self {
        Self {
            rift,
            user_id,
            username,
            channel_id,
        }
    }
}

/// Posts notices through the Rift REST API.
#[async_trait]
impl RoomNotifier for RiftRoomNotifier {
    /// Send the content as a normal channel message.
    async fn notify(&self, content: &str) -> Result<(), BridgeError> {
        self.rift
            .send_message(self.user_id, &self.username, self.channel_id, content)
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{PendingProposal, ProposalId};
    use crate::executor::Capability;

    /// Verifies the proposal id renders as a bare number for room commands.
    #[test]
    fn test_proposal_id_display() {
        assert_eq!(ProposalId(7).to_string(), "7");
    }

    /// Verifies a pending proposal carries the fields the supervisor needs.
    #[test]
    fn test_pending_proposal_fields() {
        let p = PendingProposal {
            id: ProposalId(1),
            agent: "architect".into(),
            task_id: "42".into(),
            scope_summary: "Implement the thing".into(),
            granted_capabilities: vec![Capability::new(Capability::BASH)],
            workspace: "rift".into(),
        };
        assert_eq!(p.task_id, "42");
        assert_eq!(p.granted_capabilities.len(), 1);
    }
}
