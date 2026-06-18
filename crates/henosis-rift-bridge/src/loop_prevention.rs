//! Turn budgets, agreement signals, hard ceiling.
//!
//! Prevents conversation loops by tracking per-agent turn budgets,
//! thread-wide ceilings, and consensus signals.

use std::collections::{HashMap, HashSet};

use crate::types::AgentId;

/// Tracks turn budgets, thread ceilings, and agreement signals.
pub struct LoopGuard {
    /// Per-agent turn count in the current topic.
    agent_turns: HashMap<AgentId, u32>,
    /// Maximum turns per agent per topic.
    turn_budget: u32,
    /// Total turns in the current thread.
    total_turns: u32,
    /// Hard ceiling on total thread turns.
    thread_ceiling: u32,
    /// Set of all registered agents (for consensus checking).
    registered_agents: HashSet<AgentId>,
    /// Agents that have signaled agreement.
    agreed_agents: HashSet<AgentId>,
}

impl LoopGuard {
    /// Create a new loop guard with the given per-agent budget and thread ceiling.
    pub fn new(turn_budget: u32, thread_ceiling: u32) -> Self {
        Self {
            agent_turns: HashMap::new(),
            turn_budget,
            total_turns: 0,
            thread_ceiling,
            registered_agents: HashSet::new(),
            agreed_agents: HashSet::new(),
        }
    }

    /// Register an agent for consensus tracking.
    pub fn register_agent(&mut self, id: AgentId) {
        self.registered_agents.insert(id);
    }

    /// Check if an agent can contribute (budget and ceiling check).
    pub fn can_contribute(&self, agent_id: AgentId) -> bool {
        if self.total_turns >= self.thread_ceiling {
            return false;
        }
        let turns = self.agent_turns.get(&agent_id).copied().unwrap_or(0);
        turns < self.turn_budget
    }

    /// Record that an agent contributed a turn.
    pub fn record_contribution(&mut self, agent_id: AgentId) {
        *self.agent_turns.entry(agent_id).or_insert(0) += 1;
        self.total_turns += 1;
    }

    /// Record that an agent agrees with the current consensus.
    pub fn record_agreement(&mut self, agent_id: AgentId) {
        self.agreed_agents.insert(agent_id);
    }

    /// Check if all registered agents have signaled agreement.
    pub fn has_consensus(&self) -> bool {
        if self.registered_agents.is_empty() {
            return false;
        }
        self.registered_agents
            .iter()
            .all(|id| self.agreed_agents.contains(id))
    }

    /// Check if a response is a pass signal.
    pub fn is_pass(&self, content: &str) -> bool {
        content.contains("[PASS]")
    }

    /// Check if a response contains an agreement signal.
    pub fn contains_agreement(&self, content: &str) -> bool {
        content.contains("[AGREE]")
    }

    /// Reset for a new topic/thread.
    pub fn reset(&mut self) {
        self.agent_turns.clear();
        self.total_turns = 0;
        self.agreed_agents.clear();
    }

    /// Get current total turn count.
    pub fn total_turns(&self) -> u32 {
        self.total_turns
    }
}
