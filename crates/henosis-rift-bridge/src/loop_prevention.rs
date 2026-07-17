//! Turn budgets, agreement signals, hard ceiling.
//!
//! Prevents conversation loops by tracking per-agent turn budgets,
//! thread-wide ceilings, and consensus signals.
//!
//! HONESTY NOTE (2026-07-17 design spec, P4): the original design (memory
//! 27272) specified embedding-based loop detection. What is implemented here
//! is counters plus consensus markers; semantic "circling" detection is a
//! cheap token-overlap pass in [`crate::echo`], and embedding-based
//! detection remains future work. This gap is recorded deliberately instead
//! of silently drifting.
//!
//! Consensus markers are LINE-ANCHORED (spec P5): a signal counts only when
//! a line starts with it, so an agent *discussing* the protocol ("should I
//! emit [AGREE] here?") does not trip it (spec F6, same bug class as
//! botcore's isNoReply incident).

use std::collections::{HashMap, HashSet};

use crate::types::AgentId;

/// Marker an agent emits to decline its turn.
const PASS_MARKER: &str = "[PASS]";
/// Marker an agent emits to signal agreement with the current consensus.
const AGREE_MARKER: &str = "[AGREE]";

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

/// Implements budget accounting, consensus tracking, and marker detection.
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

    /// Check if a response is a pass signal: the first non-empty line must
    /// start with the pass marker, because a pass is a whole-response
    /// decision, not a footnote.
    pub fn is_pass(&self, content: &str) -> bool {
        content
            .lines()
            .find(|line| !line.trim().is_empty())
            .is_some_and(|line| line.trim_start().starts_with(PASS_MARKER))
    }

    /// Check if a response contains an agreement signal on any line start.
    /// Line-anchored so prose that merely mentions the marker does not count.
    pub fn contains_agreement(&self, content: &str) -> bool {
        content
            .lines()
            .any(|line| line.trim_start().starts_with(AGREE_MARKER))
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

/// Unit tests for budgets, ceiling, consensus, reset, and marker anchoring.
#[cfg(test)]
mod tests {
    use super::LoopGuard;
    use crate::types::AgentId;
    use uuid::Uuid;

    /// Fresh agent id helper.
    fn agent() -> AgentId {
        AgentId(Uuid::new_v4())
    }

    /// Verifies the per-agent budget caps contributions.
    #[test]
    fn test_turn_budget_caps_agent_contributions() {
        let a = agent();
        let mut guard = LoopGuard::new(2, 10);
        assert!(guard.can_contribute(a));
        guard.record_contribution(a);
        guard.record_contribution(a);
        assert!(!guard.can_contribute(a));
    }

    /// Verifies the thread ceiling stops everyone once total turns hit it.
    #[test]
    fn test_thread_ceiling_stops_all_agents() {
        let a = agent();
        let b = agent();
        let mut guard = LoopGuard::new(10, 2);
        guard.record_contribution(a);
        guard.record_contribution(b);
        assert!(!guard.can_contribute(a));
        assert!(!guard.can_contribute(b));
        assert_eq!(guard.total_turns(), 2);
    }

    /// Verifies reset restores budgets and clears agreements so a room can
    /// host a new topic (regression test for finding N2: reset had no
    /// callers and the room died permanently at the ceiling).
    #[test]
    fn test_reset_restores_topic_energy() {
        let a = agent();
        let mut guard = LoopGuard::new(1, 1);
        guard.register_agent(a);
        guard.record_contribution(a);
        guard.record_agreement(a);
        assert!(!guard.can_contribute(a));
        assert!(guard.has_consensus());

        guard.reset();
        assert!(guard.can_contribute(a));
        assert!(!guard.has_consensus());
        assert_eq!(guard.total_turns(), 0);
    }

    /// Verifies consensus requires every registered agent to agree.
    #[test]
    fn test_consensus_requires_all_registered_agents() {
        let a = agent();
        let b = agent();
        let mut guard = LoopGuard::new(5, 30);
        guard.register_agent(a);
        guard.register_agent(b);
        guard.record_agreement(a);
        assert!(!guard.has_consensus());
        guard.record_agreement(b);
        assert!(guard.has_consensus());
    }

    /// Verifies an empty room can never reach consensus.
    #[test]
    fn test_no_consensus_with_no_registered_agents() {
        let guard = LoopGuard::new(5, 30);
        assert!(!guard.has_consensus());
    }

    /// Verifies pass detection is anchored to the first non-empty line
    /// (spec P5) and prose mentions do not trip it (spec F6).
    #[test]
    fn test_is_pass_is_first_line_anchored() {
        let guard = LoopGuard::new(5, 30);
        assert!(guard.is_pass("[PASS]"));
        assert!(guard.is_pass("  [PASS] nothing to add"));
        assert!(guard.is_pass("\n\n[PASS]"));
        assert!(!guard.is_pass("I considered whether to emit [PASS] here."));
        assert!(!guard.is_pass("Substantive point first.\n[PASS]"));
    }

    /// Verifies agreement detection is line-anchored: prose mentions do not
    /// count, line-leading markers on any line do.
    #[test]
    fn test_contains_agreement_is_line_anchored() {
        let guard = LoopGuard::new(5, 30);
        assert!(guard.contains_agreement("[AGREE] ship it"));
        assert!(guard.contains_agreement("Summary of my position.\n[AGREE] with the plan."));
        assert!(!guard.contains_agreement("should I emit [AGREE] here?"));
        assert!(!guard.contains_agreement("the [AGREE] protocol is odd"));
    }
}
