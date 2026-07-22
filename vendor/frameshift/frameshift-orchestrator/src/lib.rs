//! `frameshift-orchestrator` -- persona-orchestration subsystem for automate mode.
//!
//! Given a project/task context, this crate:
//! - Senses the work context from the project directory and an optional task hint (`context`).
//! - Indexes installed personas into matchable profiles (`index`).
//! - Scores and ranks personas for a context using configurable policy weights (`policy`).
//! - Manages drift-controlled switching via a hysteresis state machine (`controller`).
//! - Learns from user overrides to adjust per-persona preference bias (`feedback`).
//! - Persists on/off mode state durably (`mode`).
//! - Records an explainable audit log of all transitions (`audit`).
//!
//! All modules are independently usable. The `Orchestrator` facade wires them together
//! for callers that want a single entry point.

pub mod audit;
pub mod context;
pub mod controller;
pub mod embed;
pub mod error;
pub mod feedback;
pub mod index;
pub mod intent;
pub mod mode;
pub mod policy;
pub mod run;

// Re-exports for public API convenience.
pub use audit::{now_timestamp, AuditLog, Transition};
pub use context::{sense, ContextSignal};
pub use controller::{AutomateState, Decision, SwitchController, SwitchPolicy};
pub use embed::{cosine_similarity, semantic_similarity, CachedEmbedder, Embedder};
pub use error::OrchestratorError;
pub use feedback::Preferences;
pub use index::{PersonaIndex, PersonaProfile};
pub use intent::{classify as classify_intent, Intent};
pub use mode::{Mode, ModeState};
pub use policy::{rank, rank_with_embedder, PolicyWeights, ScoreComponents, Scored};
pub use run::{
    select, select_rich, select_rich_with_embedder, select_with_embedder, CandidateOutput,
    ContextSnapshot, SelectionInputs, SelectionOutput,
};

/// Facade that wires together the index, weights, policy, preferences, and controller.
///
/// Provides a high-level API for callers that do not want to assemble the pipeline
/// manually. The `Orchestrator` is purely in-memory; persistence is the caller's
/// responsibility via the individual `save`/`load` methods on the inner types.
pub struct Orchestrator {
    /// The indexed set of available persona profiles.
    pub index: PersonaIndex,

    /// Scoring weight configuration.
    pub weights: PolicyWeights,

    /// Hysteresis controller for drift-controlled switching.
    pub controller: SwitchController,

    /// Learned per-persona preference biases.
    pub preferences: Preferences,
}

/// Selects and switches personas using the orchestrator's indexed state.
impl Orchestrator {
    /// Create a new `Orchestrator` with the given index and default weights/policy/preferences.
    pub fn new(index: PersonaIndex) -> Self {
        Orchestrator {
            index,
            weights: PolicyWeights::default(),
            controller: SwitchController::new(SwitchPolicy::default()),
            preferences: Preferences::new(),
        }
    }

    /// Score and rank all personas for the given context signal.
    ///
    /// Does not update controller state; purely a read operation for inspection.
    pub fn select(&self, ctx: &ContextSignal) -> Vec<Scored> {
        rank(ctx, &self.index, &self.weights, &self.preferences)
    }

    /// Score, rank, and feed into the controller to produce a switching decision.
    ///
    /// Updates controller state; may transition to `Active` or trigger a switch.
    pub fn decide(&mut self, ctx: &ContextSignal) -> Decision {
        let ranked = rank(ctx, &self.index, &self.weights, &self.preferences);
        self.controller.decide(&ranked)
    }
}

#[cfg(test)]
/// Verifies the public orchestration facade.
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    /// Build a minimal PersonaProfile for facade tests.
    fn make_profile(name: &str, languages: &[&str]) -> PersonaProfile {
        PersonaProfile {
            name: name.to_string(),
            description: None,
            languages: languages
                .iter()
                .map(|l| l.to_string())
                .collect::<BTreeSet<_>>(),
            keywords: languages.iter().map(|l| l.to_string()).collect(),
            required_tools: vec![],
            network_egress: false,
            primary_intents: vec![],
            anti_keywords: vec![],
        }
    }

    /// Orchestrator::select returns ranked results.
    #[test]
    fn orchestrator_select_returns_results() {
        let index = PersonaIndex {
            profiles: vec![make_profile("rust-expert", &["rust"])],
        };
        let orch = Orchestrator::new(index);
        let ctx = sense(std::path::Path::new("."), Some("clippy lint"));
        let ranked = orch.select(&ctx);
        assert_eq!(ranked.len(), 1);
    }

    /// Orchestrator::decide arms and decides.
    #[test]
    fn orchestrator_decide_arms() {
        let index = PersonaIndex {
            profiles: vec![make_profile("rust-expert", &["rust"])],
        };
        let mut orch = Orchestrator::new(index);
        orch.controller.arm();

        use std::collections::BTreeMap;
        let ctx = ContextSignal {
            project_name: "test".to_string(),
            languages: {
                let mut m = BTreeMap::new();
                m.insert("rust".to_string(), 1.0);
                m
            },
            frameworks: vec![],
            task_tokens: vec![],
            context_tokens: vec![],
            inferred_intent: None,
        };
        // With only one persona and confidence depends on scoring; just verify no panic.
        let _decision = orch.decide(&ctx);
    }
}
