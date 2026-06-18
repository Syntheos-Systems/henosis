//! Probabilistic engagement engine.
//!
//! Decides whether an agent should respond to a message based on
//! base probability, direct addressing, and recency decay.

use rand::Rng;

/// Probabilistic engagement engine for agent response decisions.
pub struct EngagementEngine {
    /// Probability below this threshold means auto-skip (no LLM call).
    pub skip_threshold: f64,
    /// Probability when agent is directly addressed.
    pub direct_address_boost: f64,
    /// Recency decay half-life in turns.
    pub decay_halflife: f64,
}

impl Default for EngagementEngine {
    /// Default engagement parameters.
    fn default() -> Self {
        Self {
            skip_threshold: 0.05,
            direct_address_boost: 0.8,
            decay_halflife: 2.0,
        }
    }
}

impl EngagementEngine {
    /// Compute response probability for an agent.
    ///
    /// - `base_chance`: agent's configured base probability (0.0-1.0)
    /// - `directly_addressed`: whether the message mentions this agent by name
    /// - `turns_since_last_post`: how many turns since this agent last posted
    /// - `relevance`: message-to-persona relevance in [0.0, 1.0]; 1.0 is neutral
    ///   (no persona signal). Scales the final probability so off-topic agents
    ///   engage less. Callers pass 1.0 when the agent is directly addressed.
    pub fn compute_probability(
        &self,
        base_chance: f64,
        directly_addressed: bool,
        turns_since_last_post: u32,
        relevance: f64,
    ) -> f64 {
        let effective_base = if directly_addressed {
            self.direct_address_boost
        } else {
            base_chance
        };

        // Recency decay: agents that just posted are LESS likely to respond again.
        // As turns_since_last_post increases, the agent recovers response probability.
        let recency_decay = if turns_since_last_post == 0 {
            // Agent just posted -- decay factor very low to prevent dominating.
            0.25
        } else {
            // Agent hasn't posted recently -- probability recovers over time.
            let recovery = 1.0 - (0.5_f64).powf(turns_since_last_post as f64 / self.decay_halflife);
            recovery.min(1.0)
        };

        effective_base * recency_decay * relevance.clamp(0.0, 1.0)
    }

    /// Probabilistic roll: returns true if the agent should respond.
    pub fn roll(&self, probability: f64) -> bool {
        if probability <= 0.0 {
            return false;
        }
        if probability >= 1.0 {
            return true;
        }
        let mut rng = rand::rng();
        rng.random::<f64>() < probability
    }

    /// Whether the probability is below the auto-skip threshold.
    pub fn should_skip(&self, probability: f64) -> bool {
        probability < self.skip_threshold
    }
}
