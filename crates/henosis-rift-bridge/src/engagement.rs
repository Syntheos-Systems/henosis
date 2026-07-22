//! Probabilistic engagement engine.
//!
//! Decides whether an agent should respond to a message based on base
//! probability, direct addressing, recency decay, peer coverage, and persona
//! relevance.
//!
//! [`EngagementInputs`] carries no human-vs-agent distinction. The engine
//! treats every message author identically so peer conversations can sustain
//! themselves without favoring human-authored messages.

use rand::Rng;

/// Inputs to a single engagement evaluation for one agent and one message.
#[derive(Debug, Clone, Copy)]
pub struct EngagementInputs {
    /// Agent's configured base response probability (0.0-1.0).
    pub base_chance: f64,
    /// Whether the message mentions this agent by name.
    pub directly_addressed: bool,
    /// Room turns since this agent last posted. `None` means the agent has
    /// never posted and evaluates fully recovered. Feeding a
    /// posts-made counter here would invert the intended recency behavior;
    /// callers must pass actual quiet time.
    pub turns_since_last_post: Option<u32>,
    /// How many peers have already responded to the message under
    /// consideration. Zero when this agent evaluates first.
    pub peer_responses: u32,
    /// Message-to-persona relevance in [0.0, 1.0]; 1.0 is neutral (no
    /// persona signal). Callers pass 1.0 when the agent is directly addressed.
    pub relevance: f64,
}

/// Probabilistic engagement engine for agent response decisions.
pub struct EngagementEngine {
    /// Probability below this threshold means auto-skip (no LLM call).
    pub skip_threshold: f64,
    /// Probability when agent is directly addressed.
    pub direct_address_boost: f64,
    /// Recency decay half-life in turns.
    pub decay_halflife: f64,
    /// Multiplier applied once per peer that already answered the message
    /// so an agent is markedly less eager to add another voice to a point
    /// already covered. Direct addressing is immune: a
    /// named agent should answer even as the fourth voice.
    pub peer_response_damp: f64,
}

/// Provides the tuned default engagement parameters.
impl Default for EngagementEngine {
    /// Default engagement parameters.
    fn default() -> Self {
        Self {
            skip_threshold: 0.05,
            direct_address_boost: 0.8,
            decay_halflife: 2.0,
            peer_response_damp: 0.4,
        }
    }
}

/// Implements probability computation, rolling, and skip gating.
impl EngagementEngine {
    /// Compute response probability for an agent given the full evaluation
    /// inputs. Factors multiply: effective base (or direct-address boost),
    /// recency recovery, peer-coverage damping, persona relevance.
    pub fn compute_probability(&self, inputs: EngagementInputs) -> f64 {
        let effective_base = if inputs.directly_addressed {
            self.direct_address_boost
        } else {
            inputs.base_chance
        };

        // Recency decay: an agent that just posted is damped so it cannot
        // dominate; a quiet agent recovers toward full probability; an agent
        // that has never posted starts fully recovered (the old code damped
        // fresh agents to 0.25 indefinitely).
        let recency = match inputs.turns_since_last_post {
            None => 1.0,
            Some(0) => 0.25,
            Some(turns) => {
                let recovery = 1.0 - (0.5_f64).powf(f64::from(turns) / self.decay_halflife);
                recovery.min(1.0)
            }
        };

        // Peer-coverage damping: each peer that already answered
        // multiplies the probability down, so herds cannot pile onto one
        // message. This is the anti-synchronisation term the recency decay
        // alone cannot provide.
        let peer_damp = if inputs.directly_addressed {
            1.0
        } else {
            self.peer_response_damp
                .clamp(0.0, 1.0)
                .powi(inputs.peer_responses.min(i32::MAX as u32) as i32)
        };

        effective_base * recency * peer_damp * inputs.relevance.clamp(0.0, 1.0)
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

/// Unit tests for recency semantics, peer damping, and addressing rules.
#[cfg(test)]
mod tests {
    use super::{EngagementEngine, EngagementInputs};

    /// Baseline inputs: unaddressed agent, never posted, no peers, neutral
    /// relevance.
    fn base_inputs() -> EngagementInputs {
        EngagementInputs {
            base_chance: 0.5,
            directly_addressed: false,
            turns_since_last_post: None,
            peer_responses: 0,
            relevance: 1.0,
        }
    }

    /// Verifies a never-posted agent evaluates at full base probability
    /// instead of remaining damped to 0.25x indefinitely.
    #[test]
    fn test_never_posted_agent_is_fully_recovered() {
        let engine = EngagementEngine::default();
        let p = engine.compute_probability(base_inputs());
        assert!((p - 0.5).abs() < 1e-9);
    }

    /// Verifies a directly addressed agent that never posted gets the full
    /// boost (the old inverted code gave it 0.8 * 0.25 = 0.2).
    #[test]
    fn test_direct_address_on_fresh_agent_gets_full_boost() {
        let engine = EngagementEngine::default();
        let p = engine.compute_probability(EngagementInputs {
            directly_addressed: true,
            ..base_inputs()
        });
        assert!((p - 0.8).abs() < 1e-9);
    }

    /// Verifies an agent that just posted is heavily damped and recovers
    /// monotonically with quiet turns.
    #[test]
    fn test_recency_damps_just_posted_and_recovers_monotonically() {
        let engine = EngagementEngine::default();
        let p_at = |turns: u32| {
            engine.compute_probability(EngagementInputs {
                turns_since_last_post: Some(turns),
                ..base_inputs()
            })
        };
        assert!((p_at(0) - 0.125).abs() < 1e-9);
        let mut prev = p_at(0);
        for turns in 1..10 {
            let p = p_at(turns);
            assert!(p > prev, "recovery must be monotonic (turn {turns})");
            prev = p;
        }
        assert!(p_at(9) < 0.5 + 1e-9);
    }

    /// Verifies each peer response multiplies probability down for
    /// unaddressed agents so repeated responses become increasingly rare.
    #[test]
    fn test_peer_responses_damp_unaddressed_agents() {
        let engine = EngagementEngine::default();
        let p_with = |peers: u32| {
            engine.compute_probability(EngagementInputs {
                peer_responses: peers,
                ..base_inputs()
            })
        };
        assert!((p_with(0) - 0.5).abs() < 1e-9);
        assert!((p_with(1) - 0.2).abs() < 1e-9);
        assert!((p_with(2) - 0.08).abs() < 1e-9);
        assert!(p_with(3) < engine.skip_threshold);
    }

    /// Verifies direct addressing is immune to peer damping: a named agent
    /// answers even when peers already covered the message.
    #[test]
    fn test_direct_address_is_immune_to_peer_damp() {
        let engine = EngagementEngine::default();
        let p = engine.compute_probability(EngagementInputs {
            directly_addressed: true,
            peer_responses: 5,
            ..base_inputs()
        });
        assert!((p - 0.8).abs() < 1e-9);
    }

    /// Verifies relevance scales probability and is clamped to [0, 1].
    #[test]
    fn test_relevance_scales_and_clamps() {
        let engine = EngagementEngine::default();
        let p_half = engine.compute_probability(EngagementInputs {
            relevance: 0.5,
            ..base_inputs()
        });
        assert!((p_half - 0.25).abs() < 1e-9);
        let p_over = engine.compute_probability(EngagementInputs {
            relevance: 7.0,
            ..base_inputs()
        });
        assert!((p_over - 0.5).abs() < 1e-9);
    }

    /// Verifies the skip threshold boundary behavior.
    #[test]
    fn test_should_skip_threshold() {
        let engine = EngagementEngine::default();
        assert!(engine.should_skip(0.049));
        assert!(!engine.should_skip(0.05));
    }

    /// Verifies roll extremes are deterministic.
    #[test]
    fn test_roll_extremes() {
        let engine = EngagementEngine::default();
        assert!(!engine.roll(0.0));
        assert!(engine.roll(1.0));
    }
}
