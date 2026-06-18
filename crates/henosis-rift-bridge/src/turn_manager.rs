//! Turn queue, pacing, interleaving.
//!
//! Prevents agents from posting consecutively and adds natural-feeling
//! jitter delays between responses.

use rand::Rng;
use std::time::Duration;

use crate::types::AgentId;

/// Manages turn ordering, pacing, and interleaving between agents.
pub struct TurnManager {
    /// Who posted last (None = human or no one yet).
    last_poster: Option<AgentId>,
    /// Minimum jitter delay in milliseconds.
    jitter_min_ms: u64,
    /// Maximum jitter delay in milliseconds.
    jitter_max_ms: u64,
}

impl TurnManager {
    /// Create a new turn manager with the given jitter range.
    pub fn new(jitter_min_ms: u64, jitter_max_ms: u64) -> Self {
        Self {
            last_poster: None,
            jitter_min_ms,
            jitter_max_ms,
        }
    }

    /// Check if an agent is allowed to post next (interleaving rule).
    /// An agent cannot post twice in a row.
    pub fn can_post_next(&self, agent_id: AgentId) -> bool {
        self.last_poster != Some(agent_id)
    }

    /// Record that an agent posted (updates last_poster).
    pub fn record_post(&mut self, agent_id: AgentId) {
        self.last_poster = Some(agent_id);
    }

    /// Record that a human posted (resets interleaving so any agent can respond).
    pub fn record_human_post(&mut self) {
        self.last_poster = None;
    }

    /// Get a random jittered delay for pacing responses.
    pub fn jitter_delay(&self) -> Duration {
        let mut rng = rand::rng();
        let ms = rng.random_range(self.jitter_min_ms..=self.jitter_max_ms);
        Duration::from_millis(ms)
    }
}
