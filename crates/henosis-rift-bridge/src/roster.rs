//! Agent roster management.
//!
//! Provisions agent users in Rift from config and tracks their runtime state.

use std::collections::HashMap;
use std::time::Instant;
use uuid::Uuid;

use crate::config::AgentConfig;
use crate::error::BridgeError;
use crate::rift_client::RiftRestClient;
use crate::types::{AgentId, AgentState};

/// A registered agent with its Rift identity and runtime state.
#[derive(Debug)]
pub struct RegisteredAgent {
    /// Bridge-local agent identifier.
    pub id: AgentId,
    /// Rift server user ID for this agent.
    pub rift_user_id: Uuid,
    /// Rift username.
    pub username: String,
    /// Display name shown in the UI.
    pub display_name: String,
    /// Base response probability (0.0 to 1.0).
    pub base_chance: f64,
    /// System prompt preamble injected into every LLM call.
    pub system_prompt: String,
    /// Current runtime state.
    pub state: AgentState,
    /// Stable position in the config roster; derives this agent's compose
    /// slot so its timing window never overlaps another agent's (spec P1).
    pub slot_index: usize,
    /// Room turn at which this agent last posted (None = never posted).
    /// Feeds true turns-since-last-post recency (fixes finding N1).
    pub last_posted_turn: Option<u64>,
    /// Wall-clock instant of this agent's last post, for cooldown pacing.
    pub last_post_at: Option<Instant>,
}

/// Manages the pool of agent users in Rift.
pub struct AgentRoster {
    /// Map from bridge-local AgentId to registered agent state.
    agents: HashMap<AgentId, RegisteredAgent>,
}

/// Implements provisioning and lookup helpers for the bridge agent roster.
impl AgentRoster {
    /// Provision agent users in Rift from config entries.
    /// Registers each agent as a Rift user (or logs in if already registered).
    pub async fn provision(
        configs: &[AgentConfig],
        rift: &RiftRestClient,
    ) -> Result<Self, BridgeError> {
        let mut agents = HashMap::new();

        for (slot_index, config) in configs.iter().enumerate() {
            // Deterministic password derived from username.
            // Agents never authenticate themselves -- the bridge issues tokens.
            let password = format!("agent-internal-{}", config.username);

            let user = rift
                .register_agent(&config.username, &config.name, &password)
                .await?;

            tracing::info!(
                "provisioned agent: {} (rift_user_id: {})",
                config.name,
                user.id
            );

            let agent_id = AgentId(Uuid::new_v4());
            agents.insert(
                agent_id,
                RegisteredAgent {
                    id: agent_id,
                    rift_user_id: user.id,
                    username: config.username.clone(),
                    display_name: config.name.clone(),
                    base_chance: config.base_chance,
                    system_prompt: config.system_prompt.clone(),
                    state: AgentState::Idle,
                    slot_index,
                    last_posted_turn: None,
                    last_post_at: None,
                },
            );
        }

        Ok(Self { agents })
    }

    /// Test-only constructor building a roster from pre-made agents without
    /// touching the Rift network.
    #[cfg(test)]
    pub(crate) fn from_agents(agents: Vec<RegisteredAgent>) -> Self {
        Self {
            agents: agents.into_iter().map(|a| (a.id, a)).collect(),
        }
    }

    /// Iterate over all registered agents.
    pub fn all(&self) -> impl Iterator<Item = &RegisteredAgent> {
        self.agents.values()
    }

    /// Get a mutable reference to an agent by ID.
    pub fn get_mut(&mut self, id: &AgentId) -> Option<&mut RegisteredAgent> {
        self.agents.get_mut(id)
    }

    /// Find an agent by their Rift user ID.
    pub fn by_rift_user_id(&self, user_id: Uuid) -> Option<&RegisteredAgent> {
        self.agents.values().find(|a| a.rift_user_id == user_id)
    }

    /// Collect IDs of all agents currently in the Idle state.
    pub fn idle_agents(&self) -> Vec<AgentId> {
        self.agents
            .values()
            .filter(|a| a.state == AgentState::Idle)
            .map(|a| a.id)
            .collect()
    }

    /// Number of agents in the roster.
    pub fn len(&self) -> usize {
        self.agents.len()
    }

    /// Returns true when the roster has no registered agents.
    pub fn is_empty(&self) -> bool {
        self.agents.is_empty()
    }
}

/// Exercises roster queries without hitting the Rift network.
#[cfg(test)]
mod tests {
    use super::{AgentRoster, RegisteredAgent};
    use crate::types::{AgentId, AgentState};
    use std::collections::HashMap;
    use uuid::Uuid;

    /// Verifies the roster reports empty only when it contains no agents.
    #[test]
    fn test_is_empty_tracks_agent_count() {
        let mut roster = AgentRoster {
            agents: HashMap::new(),
        };

        assert!(roster.is_empty());
        assert_eq!(roster.len(), 0);

        let agent_id = AgentId(Uuid::new_v4());
        roster.agents.insert(
            agent_id,
            RegisteredAgent {
                id: agent_id,
                rift_user_id: Uuid::new_v4(),
                username: "agent-reviewer".to_string(),
                display_name: "Reviewer".to_string(),
                base_chance: 0.3,
                system_prompt: "You are a reviewer.".to_string(),
                state: AgentState::Idle,
                slot_index: 0,
                last_posted_turn: None,
                last_post_at: None,
            },
        );

        assert!(!roster.is_empty());
        assert_eq!(roster.len(), 1);
    }
}
