//! Drift-controlled switching state machine for automate mode.

use crate::policy::Scored;

/// The current state of the automate-mode controller.
#[derive(Debug, Clone, PartialEq)]
pub enum AutomateState {
    /// Automate mode is disabled.
    Off,

    /// Mode is enabled but no persona has been selected yet (or re-evaluation is pending).
    Armed,

    /// A persona is actively in use with recorded confidence.
    Active {
        /// The name of the currently active persona.
        persona: String,
        /// The confidence score at the time of last selection.
        confidence: f32,
    },

    /// The persona was explicitly pinned by the user; auto-switching is suppressed.
    Locked {
        /// The name of the locked persona.
        persona: String,
    },
}

/// Hysteresis parameters controlling when a persona switch is permitted.
#[derive(Debug, Clone)]
pub struct SwitchPolicy {
    /// Minimum confidence required to accept the top candidate as active.
    pub min_confidence: f32,

    /// Minimum score advantage the new candidate must have over the current
    /// active persona before a switch is allowed.
    pub switch_margin: f32,

    /// Number of consecutive `decide` calls the new candidate must be top-ranked
    /// before the switch is actually executed (debounce).
    pub debounce_ticks: u32,

    /// Z-score threshold for identifying a clear winner relative to the score distribution.
    pub z_threshold: f32,

    /// Minimum normalized gap between rank-1 and rank-2 scores for a meaningful advantage.
    pub min_gap_fraction: f32,
}

/// Maps operator sensitivity into concrete switching thresholds.
impl SwitchPolicy {
    /// Construct a SwitchPolicy from a user-facing sensitivity value in [0.0, 1.0].
    ///
    /// Sensitivity maps to internal parameters:
    /// - z_threshold: 1.0 - sensitivity * 0.7 (range 1.0 to 0.3)
    /// - min_gap_fraction: 0.15 - sensitivity * 0.12 (range 0.15 to 0.03)
    /// - debounce_ticks: max(0, 3 - floor(sensitivity * 3)) (range 3 to 0)
    /// - min_confidence: max(0, 0.5 - sensitivity) * 0.6 (range 0.3 to 0.0)
    /// - switch_margin: max(0, 0.5 - sensitivity) * 0.3 (range 0.15 to 0.0)
    pub fn from_sensitivity(sensitivity: f32) -> Self {
        let s = sensitivity.clamp(0.0, 1.0);
        SwitchPolicy {
            // Below the midpoint (the conservative end) impose a confidence floor
            // and a score margin the challenger must clear; at and above 0.5 both
            // are 0.0, leaving switching to the distribution/debounce heuristics.
            min_confidence: (0.5 - s).max(0.0) * 0.6,
            switch_margin: (0.5 - s).max(0.0) * 0.3,
            debounce_ticks: (3.0 - (s * 3.0).floor()).max(0.0) as u32,
            z_threshold: 1.0 - s * 0.7,
            min_gap_fraction: 0.15 - s * 0.12,
        }
    }
}

/// Provides balanced switching thresholds as the default policy.
impl Default for SwitchPolicy {
    /// Returns the default policy from sensitivity 0.5 (balanced).
    fn default() -> Self {
        Self::from_sensitivity(0.5)
    }
}

/// The outcome of a `decide` call.
#[derive(Debug, Clone, PartialEq)]
pub enum Decision {
    /// Stay with the current persona (or remain unarmed); no change warranted.
    Hold,

    /// Switch to a new persona.
    Switch {
        /// The name of the persona to switch to.
        to: String,
        /// Human-readable rationale for the switch.
        rationale: String,
        /// Confidence score at time of switch.
        confidence: f32,
    },

    /// No candidates passed the confidence threshold (ranked list is empty or all below min).
    NoCandidates,
}

/// Drift-controlled state machine that decides when to switch personas.
///
/// Implements hysteresis: an active persona is not displaced unless the
/// challenger has been consistently ranked higher for `debounce_ticks`
/// consecutive calls AND its score advantage exceeds `switch_margin`.
pub struct SwitchController {
    /// Current automate state.
    state: AutomateState,

    /// Switching hysteresis policy parameters.
    policy: SwitchPolicy,

    /// Number of consecutive ticks the current challenger has been ahead.
    debounce_count: u32,

    /// The persona that is currently being tracked as a challenger (for debounce).
    challenger: Option<String>,
}

/// Controls automate-mode transitions and persona-switch hysteresis.
impl SwitchController {
    /// Create a new `SwitchController` in the `Off` state with the given policy.
    pub fn new(policy: SwitchPolicy) -> Self {
        SwitchController {
            state: AutomateState::Off,
            policy,
            debounce_count: 0,
            challenger: None,
        }
    }

    /// Transition to `Armed` state, enabling automate mode.
    ///
    /// If currently `Active`, the active persona is retained but the controller
    /// will re-evaluate on the next `decide` call. If `Off`, transitions to `Armed`.
    pub fn arm(&mut self) {
        match &self.state {
            AutomateState::Off | AutomateState::Armed => {
                self.state = AutomateState::Armed;
            }
            AutomateState::Active { .. } => {
                // Retain active persona but signal re-evaluation is welcome.
                self.state = AutomateState::Armed;
            }
            AutomateState::Locked { .. } => {
                // Locking takes priority over arming; no-op.
            }
        }
        self.debounce_count = 0;
        self.challenger = None;
    }

    /// Explicitly lock the current or named persona; auto-switching is suppressed.
    ///
    /// If currently `Active`, locks that persona. If `Armed` or `Off`, does nothing
    /// meaningful (no persona to lock) but records a Locked state with empty name.
    pub fn lock(&mut self) {
        let locked_name = match &self.state {
            AutomateState::Active { persona, .. } => persona.clone(),
            AutomateState::Locked { persona } => persona.clone(),
            _ => String::new(),
        };
        self.state = AutomateState::Locked {
            persona: locked_name,
        };
        self.debounce_count = 0;
        self.challenger = None;
    }

    /// Unlock, returning to `Armed` state for re-evaluation.
    pub fn unlock(&mut self) {
        self.state = AutomateState::Armed;
        self.debounce_count = 0;
        self.challenger = None;
    }

    /// Update the policy parameters (e.g., when sensitivity changes).
    pub fn set_policy(&mut self, policy: SwitchPolicy) {
        self.policy = policy;
    }

    /// Return a reference to the current state.
    pub fn state(&self) -> &AutomateState {
        &self.state
    }

    /// Return the name of the persona the controller currently tracks as active.
    ///
    /// Returns `Some` when the controller is `Active` or `Locked` on a named
    /// persona, and `None` when `Off`, `Armed`, or `Locked` with no name. The
    /// daemon uses this as its "last auto-pick" to detect when the on-disk
    /// active marker has been changed out from under it (a manual override).
    pub fn active_persona(&self) -> Option<&str> {
        match &self.state {
            AutomateState::Active { persona, .. } | AutomateState::Locked { persona } => {
                if persona.is_empty() {
                    None
                } else {
                    Some(persona.as_str())
                }
            }
            AutomateState::Off | AutomateState::Armed => None,
        }
    }

    /// Adopt an externally-applied persona as the new active baseline.
    ///
    /// Used when something outside the controller (a user manual switch) has
    /// already changed the active persona. The controller records `persona` as
    /// `Active` so subsequent `decide` calls apply hysteresis from the user's
    /// choice rather than the controller's stale auto-pick, and so the same
    /// override is not detected and re-learned on every tick. Debounce and
    /// challenger tracking are reset. A user-asserted choice is treated as
    /// maximally confident; the stored confidence is informational only and is
    /// not read by `decide`. Locked state is left untouched.
    pub fn adopt_active(&mut self, persona: &str) {
        if matches!(self.state, AutomateState::Locked { .. }) {
            return;
        }
        self.state = AutomateState::Active {
            persona: persona.to_string(),
            confidence: 1.0,
        };
        self.debounce_count = 0;
        self.challenger = None;
    }

    /// Evaluate a fresh ranking and decide whether to switch personas.
    ///
    /// Logic:
    /// - `Locked`: always `Hold` regardless of ranking.
    /// - Empty `ranked`: `NoCandidates`.
    /// - `Armed` / `Off`: switch immediately to top candidate.
    /// - `Active(p)`: distribution-aware switching -- gap and z-score must pass
    ///   policy thresholds, and debounce must be satisfied unless the winner is
    ///   unambiguously dominant (z-confident AND gap meaningful).
    ///
    /// Updates internal state and debounce counter on switch.
    pub fn decide(&mut self, ranked: &[Scored]) -> Decision {
        // Locked: always hold, never auto-switch.
        if matches!(self.state, AutomateState::Locked { .. }) {
            return Decision::Hold;
        }

        // No candidates at all.
        if ranked.is_empty() {
            return Decision::NoCandidates;
        }

        let top = &ranked[0];

        match &self.state.clone() {
            AutomateState::Off | AutomateState::Armed => {
                // Do not activate a candidate below the confidence floor.
                if top.confidence < self.policy.min_confidence {
                    return Decision::Hold;
                }
                self.state = AutomateState::Active {
                    persona: top.persona.clone(),
                    confidence: top.confidence,
                };
                self.debounce_count = 0;
                self.challenger = None;
                Decision::Switch {
                    to: top.persona.clone(),
                    rationale: top.rationale.clone(),
                    confidence: top.confidence,
                }
            }

            AutomateState::Active {
                persona: current, ..
            } => {
                if top.persona == *current {
                    // Same persona still at top -- reset challenger tracking, hold.
                    self.debounce_count = 0;
                    self.challenger = None;
                    return Decision::Hold;
                }

                // Confidence floor: never switch to a challenger we are not
                // confident in.
                if top.confidence < self.policy.min_confidence {
                    self.debounce_count = 0;
                    self.challenger = None;
                    return Decision::Hold;
                }

                // Margin floor: the challenger must beat the current active persona
                // by at least switch_margin before any switch is considered.
                let current_score = ranked
                    .iter()
                    .find(|s| s.persona == *current)
                    .map(|s| s.score)
                    .unwrap_or(0.0);
                if top.score - current_score < self.policy.switch_margin {
                    self.debounce_count = 0;
                    self.challenger = None;
                    return Decision::Hold;
                }

                // Distribution-aware switching: compute mean and stddev of all scores.
                let scores: Vec<f32> = ranked.iter().map(|s| s.score).collect();
                let mean = scores.iter().sum::<f32>() / scores.len() as f32;
                let variance =
                    scores.iter().map(|s| (s - mean).powi(2)).sum::<f32>() / scores.len() as f32;
                let stddev = variance.sqrt().max(f32::EPSILON);

                // Z-score confidence: is the top score a statistical outlier above the mean?
                let is_z_confident = top.score > mean + self.policy.z_threshold * stddev;

                // Normalized gap: how large is the lead relative to the score range?
                let score_range = scores.iter().cloned().fold(0.0_f32, f32::max)
                    - scores.iter().cloned().fold(f32::MAX, f32::min);
                let second_score = ranked.get(1).map(|s| s.score).unwrap_or(0.0);
                let gap = top.score - second_score;
                let normalized_gap = if score_range > f32::EPSILON {
                    gap / score_range
                } else {
                    0.0
                };
                let is_gap_meaningful = normalized_gap > self.policy.min_gap_fraction;

                // Gap too small -- ambiguous cluster, hold without debouncing.
                if !is_gap_meaningful {
                    self.debounce_count = 0;
                    self.challenger = None;
                    return Decision::Hold;
                }

                // Track debounce for this challenger.
                let same_challenger = self.challenger.as_deref() == Some(top.persona.as_str());
                if same_challenger {
                    self.debounce_count += 1;
                } else {
                    self.challenger = Some(top.persona.clone());
                    self.debounce_count = 1;
                }

                // Clear winner (both z-confident AND gap meaningful): skip debounce.
                let required_ticks = if is_z_confident && is_gap_meaningful {
                    1
                } else {
                    self.policy.debounce_ticks
                };

                if self.debounce_count >= required_ticks {
                    // Debounce satisfied -- execute switch.
                    let new_persona = top.persona.clone();
                    let rationale = top.rationale.clone();
                    let confidence = top.confidence;
                    self.state = AutomateState::Active {
                        persona: new_persona.clone(),
                        confidence,
                    };
                    self.debounce_count = 0;
                    self.challenger = None;
                    Decision::Switch {
                        to: new_persona,
                        rationale,
                        confidence,
                    }
                } else {
                    Decision::Hold
                }
            }

            // Locked handled at top.
            AutomateState::Locked { .. } => Decision::Hold,
        }
    }
}

#[cfg(test)]
/// Verifies controller state transitions, thresholds, and debounce behavior.
mod tests {
    use super::*;
    use crate::policy::ScoreComponents;

    /// Build a minimal Scored entry for controller tests.
    fn scored(name: &str, score: f32, confidence: f32) -> Scored {
        Scored {
            persona: name.to_string(),
            score,
            confidence,
            rationale: format!("{name} rationale"),
            components: ScoreComponents {
                language: score,
                lexical: 0.0,
                intent: 0.0,
                capability: 0.0,
                context: 0.0,
                semantic: 0.0,
            },
        }
    }

    /// A brief weaker competitor must NOT switch an Active persona (gap too small).
    #[test]
    fn weaker_competitor_does_not_switch_active() {
        let policy = SwitchPolicy {
            min_confidence: 0.5,
            switch_margin: 0.15,
            debounce_ticks: 2,
            z_threshold: 0.65,
            min_gap_fraction: 0.09,
        };
        let mut ctrl = SwitchController::new(policy);
        ctrl.arm();

        // Establish active persona with high confidence.
        let ranked_a = vec![scored("alpha", 0.9, 0.85), scored("beta", 0.5, 0.3)];
        let d = ctrl.decide(&ranked_a);
        assert!(matches!(d, Decision::Switch { to, .. } if to == "alpha"));

        // Beta briefly tops the list but normalized gap is too small.
        // 3 candidates: beta=0.901, alpha=0.900, gamma=0.800
        // range = 0.901 - 0.800 = 0.101; gap = 0.901 - 0.900 = 0.001; normalized_gap = 0.0099 < 0.09.
        let ranked_b = vec![
            scored("beta", 0.901, 0.8),
            scored("alpha", 0.900, 0.6),
            scored("gamma", 0.800, 0.4),
        ];
        let d2 = ctrl.decide(&ranked_b);
        assert_eq!(
            d2,
            Decision::Hold,
            "small normalized gap must not trigger switch"
        );
    }

    /// A sustained clearly-stronger competitor DOES switch after debounce.
    #[test]
    fn sustained_strong_competitor_does_switch() {
        let policy = SwitchPolicy {
            min_confidence: 0.5,
            switch_margin: 0.15,
            debounce_ticks: 2,
            z_threshold: 0.65,
            min_gap_fraction: 0.09,
        };
        let mut ctrl = SwitchController::new(policy);
        ctrl.arm();

        // Establish alpha as active.
        let ranked_a = vec![scored("alpha", 0.7, 0.65), scored("beta", 0.3, 0.2)];
        ctrl.decide(&ranked_a);
        assert!(
            matches!(ctrl.state(), AutomateState::Active { persona, .. } if persona == "alpha")
        );

        // Beta clearly stronger: gap = 0.9 - 0.3 = 0.6; range = 0.6; normalized_gap = 1.0 > 0.09.
        // stddev will be large, so z-score may or may not be confident -- but gap is meaningful.
        // With z_threshold=0.65 and debounce=2, non-z-confident path needs 2 ticks.
        let ranked_b = vec![scored("beta", 0.9, 0.85), scored("alpha", 0.3, 0.2)];

        // First tick -- debounce_count becomes 1, still Hold (unless z-confident skips debounce).
        // mean=(0.9+0.3)/2=0.6, stddev=0.3, z=(0.9-0.6)/0.3=1.0 > 0.65 => is_z_confident=true.
        // Both z_confident and gap_meaningful => required_ticks=1, debounce_count=1 >= 1 => Switch.
        let d1 = ctrl.decide(&ranked_b);
        assert!(
            matches!(d1, Decision::Switch { to, .. } if to == "beta"),
            "clear winner (z-confident + gap) switches on first tick"
        );
    }

    /// Locked state never auto-switches regardless of ranking.
    #[test]
    fn locked_never_switches() {
        let policy = SwitchPolicy::default();
        let mut ctrl = SwitchController::new(policy);
        ctrl.arm();

        // Establish active.
        let ranked_a = vec![scored("alpha", 0.9, 0.85)];
        ctrl.decide(&ranked_a);

        ctrl.lock();
        assert!(matches!(ctrl.state(), AutomateState::Locked { .. }));

        // Even a perfect candidate doesn't switch.
        let ranked_b = vec![scored("beta", 1.0, 1.0), scored("alpha", 0.1, 0.1)];
        let d = ctrl.decide(&ranked_b);
        assert_eq!(d, Decision::Hold, "locked must never switch");
        assert!(matches!(ctrl.state(), AutomateState::Locked { .. }));
    }

    /// Ambiguous (tight) scores yield Hold when Active.
    #[test]
    fn ambiguous_scores_yield_hold_when_active() {
        let policy = SwitchPolicy::from_sensitivity(0.5);
        let mut ctrl = SwitchController::new(policy);
        ctrl.arm();

        // Establish alpha.
        let ranked_a = vec![scored("alpha", 0.5, 0.4), scored("beta", 0.3, 0.2)];
        ctrl.decide(&ranked_a);

        // Beta marginally ahead but the field is wide -- normalized gap is tiny.
        // range = 0.50 - 0.05 = 0.45; gap = 0.50 - 0.49 = 0.01; normalized_gap = 0.022 < 0.09.
        let ranked_b = vec![
            scored("beta", 0.50, 0.4),
            scored("alpha", 0.49, 0.3),
            scored("gamma", 0.30, 0.2),
            scored("delta", 0.05, 0.1),
        ];
        let d = ctrl.decide(&ranked_b);
        assert_eq!(d, Decision::Hold, "ambiguous scores must yield Hold");
    }

    /// arm() while Active transitions to Armed (re-evaluation welcome).
    #[test]
    fn arm_while_active_rearms() {
        let policy = SwitchPolicy::default();
        let mut ctrl = SwitchController::new(policy);
        ctrl.arm();
        let ranked = vec![scored("alpha", 0.9, 0.85)];
        ctrl.decide(&ranked);
        assert!(matches!(ctrl.state(), AutomateState::Active { .. }));

        ctrl.arm();
        assert_eq!(*ctrl.state(), AutomateState::Armed);
    }

    /// unlock() returns to Armed state.
    #[test]
    fn unlock_returns_to_armed() {
        let policy = SwitchPolicy::default();
        let mut ctrl = SwitchController::new(policy);
        ctrl.arm();
        let ranked = vec![scored("alpha", 0.9, 0.85)];
        ctrl.decide(&ranked);
        ctrl.lock();
        ctrl.unlock();
        assert_eq!(*ctrl.state(), AutomateState::Armed);
    }

    /// High sensitivity switches immediately on a clear winner.
    #[test]
    fn high_sensitivity_switches_immediately_on_clear_winner() {
        let policy = SwitchPolicy::from_sensitivity(1.0);
        let mut ctrl = SwitchController::new(policy);
        ctrl.arm();

        // Establish alpha as active.
        let ranked_a = vec![scored("alpha", 0.6, 0.55), scored("beta", 0.3, 0.2)];
        ctrl.decide(&ranked_a);

        // Beta clearly ahead -- should switch immediately at high sensitivity.
        let ranked_b = vec![scored("beta", 0.8, 0.75), scored("alpha", 0.4, 0.3)];
        let d = ctrl.decide(&ranked_b);
        assert!(
            matches!(d, Decision::Switch { to, .. } if to == "beta"),
            "high sensitivity should switch immediately on clear winner"
        );
    }

    /// Low sensitivity requires more debounce ticks before switching.
    #[test]
    fn low_sensitivity_holds_longer() {
        let policy = SwitchPolicy::from_sensitivity(0.0);
        let mut ctrl = SwitchController::new(policy);
        ctrl.arm();

        let ranked_a = vec![scored("alpha", 0.6, 0.55), scored("beta", 0.3, 0.2)];
        ctrl.decide(&ranked_a);

        // Beta ahead but low sensitivity requires more debounce.
        // With sensitivity=0.0: z_threshold=1.0, debounce_ticks=3, min_gap_fraction=0.15.
        // scores=[0.9,0.3], mean=0.6, stddev=0.3, z=(0.9-0.6)/0.3=1.0 NOT > 1.0 => not z-confident.
        // gap=0.6, range=0.6, normalized_gap=1.0 > 0.15 => gap meaningful.
        // required_ticks = debounce_ticks = 3.
        let ranked_b = vec![scored("beta", 0.9, 0.85), scored("alpha", 0.3, 0.2)];
        let d1 = ctrl.decide(&ranked_b);
        assert_eq!(d1, Decision::Hold, "low sensitivity first tick should hold");

        let d2 = ctrl.decide(&ranked_b);
        assert_eq!(
            d2,
            Decision::Hold,
            "low sensitivity second tick should hold"
        );

        let d3 = ctrl.decide(&ranked_b);
        assert!(
            matches!(d3, Decision::Switch { to, .. } if to == "beta"),
            "low sensitivity should switch after 3 ticks"
        );
    }

    /// Default sensitivity produces balanced parameters.
    #[test]
    fn default_sensitivity_is_balanced() {
        let policy = SwitchPolicy::from_sensitivity(0.5);
        assert!((policy.z_threshold - 0.65).abs() < 0.01);
        assert_eq!(policy.debounce_ticks, 2);
        // At and above the midpoint the confidence/margin floors are disabled,
        // so default behavior is governed by the distribution heuristics only.
        assert_eq!(policy.min_confidence, 0.0);
        assert_eq!(policy.switch_margin, 0.0);
    }

    /// A top candidate below min_confidence does not activate.
    #[test]
    fn min_confidence_blocks_low_confidence_activation() {
        let policy = SwitchPolicy {
            min_confidence: 0.5,
            switch_margin: 0.0,
            debounce_ticks: 0,
            z_threshold: 0.65,
            min_gap_fraction: 0.09,
        };
        let mut ctrl = SwitchController::new(policy);
        ctrl.arm();
        // Top candidate confidence 0.3 is below the 0.5 floor.
        let ranked = vec![scored("alpha", 0.9, 0.3)];
        let d = ctrl.decide(&ranked);
        assert_eq!(d, Decision::Hold, "low-confidence top must not activate");
        assert_eq!(*ctrl.state(), AutomateState::Armed, "state stays Armed");
    }

    /// A challenger that does not beat the active persona by switch_margin holds.
    #[test]
    fn switch_margin_blocks_marginal_challenger() {
        let policy = SwitchPolicy {
            min_confidence: 0.0,
            switch_margin: 0.3,
            debounce_ticks: 0,
            z_threshold: 0.0,
            min_gap_fraction: 0.0,
        };
        let mut ctrl = SwitchController::new(policy);
        ctrl.arm();
        // Activate alpha.
        let ranked_a = vec![scored("alpha", 0.60, 0.9), scored("beta", 0.10, 0.2)];
        ctrl.decide(&ranked_a);
        // Beta edges ahead by only 0.05, below the 0.3 switch_margin. Without the
        // margin floor the lenient z/gap/debounce settings would switch here.
        let ranked_b = vec![scored("beta", 0.65, 0.9), scored("alpha", 0.60, 0.5)];
        let d = ctrl.decide(&ranked_b);
        assert_eq!(d, Decision::Hold, "margin below switch_margin must hold");
    }
}
