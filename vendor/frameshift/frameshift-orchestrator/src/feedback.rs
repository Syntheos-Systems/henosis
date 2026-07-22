//! Feedback loop: learn from user overrides to adjust per-persona preference bias.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::OrchestratorError;

/// Maximum absolute value for any persona bias entry.
const BIAS_MAX: f32 = 0.2;

/// Amount to increase the chosen persona's bias on each override.
const BUMP: f32 = 0.05;

/// Amount to decrease the auto-picked persona's bias on each override.
const DECAY: f32 = 0.03;

/// Floor multiplier applied by time decay so bias never reaches zero.
const DECAY_FLOOR: f32 = 0.3;

/// Rate of time decay per day (1% per day).
const DECAY_RATE_PER_DAY: f32 = 0.01;

/// Per-persona preference entry with global and per-intent biases.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PersonaBias {
    /// Global additive bias.
    pub global: f32,
    /// Per-intent additive biases keyed by lowercase intent name.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub by_intent: BTreeMap<String, f32>,
    /// Days since the last override that touched this persona.
    #[serde(default)]
    pub last_override_days_ago: u32,
}

/// Per-persona additive scoring bias, persisted across sessions.
///
/// Bias values are clamped to `[-BIAS_MAX, BIAS_MAX]`. Positive bias means
/// the user has historically preferred this persona; negative means they have
/// been switched away from it.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Preferences {
    /// Map from persona name to structured bias entry.
    #[serde(default)]
    pub entries: BTreeMap<String, PersonaBias>,
    /// Legacy flat bias map (preserved for migration).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub bias: BTreeMap<String, f32>,
}

/// Preference mutation, lookup, decay, and persistence operations.
impl Preferences {
    /// Create a new empty `Preferences` with no recorded biases.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a user override: bump `chosen` bias up and decay `auto_pick` bias down.
    ///
    /// Both adjustments are clamped to `[-BIAS_MAX, BIAS_MAX]`. If `auto_pick`
    /// is `None` or equals `chosen`, only the chosen persona is bumped.
    /// Delegates to `record_override_with_intent` with no intent.
    pub fn record_override(&mut self, auto_pick: Option<&str>, chosen: &str) {
        self.record_override_with_intent(auto_pick, chosen, None);
    }

    /// Record a user override with an optional intent context.
    ///
    /// Updates both the global bias and, when `intent` is `Some`, the per-intent
    /// bias for the chosen persona. The auto-picked persona's global bias is
    /// decremented. All values are clamped to `[-BIAS_MAX, BIAS_MAX]`.
    pub fn record_override_with_intent(
        &mut self,
        auto_pick: Option<&str>,
        chosen: &str,
        intent: Option<crate::intent::Intent>,
    ) {
        // Update legacy flat map for backward compatibility.
        let chosen_bias_legacy = self.bias.entry(chosen.to_string()).or_insert(0.0);
        *chosen_bias_legacy = (*chosen_bias_legacy + BUMP).min(BIAS_MAX);

        if let Some(auto) = auto_pick {
            if auto != chosen {
                let auto_bias_legacy = self.bias.entry(auto.to_string()).or_insert(0.0);
                *auto_bias_legacy = (*auto_bias_legacy - DECAY).max(-BIAS_MAX);
            }
        }

        // Update structured entries map.
        let chosen_entry = self.entries.entry(chosen.to_string()).or_default();
        chosen_entry.global = (chosen_entry.global + BUMP).min(BIAS_MAX);

        if let Some(intent) = intent {
            // Derive the key from the Debug representation, lowercased.
            let intent_key = format!("{intent:?}").to_lowercase();
            let intent_bias = chosen_entry.by_intent.entry(intent_key).or_insert(0.0);
            *intent_bias = (*intent_bias + BUMP).min(BIAS_MAX);
        }

        if let Some(auto) = auto_pick {
            if auto != chosen {
                let auto_entry = self.entries.entry(auto.to_string()).or_default();
                auto_entry.global = (auto_entry.global - DECAY).max(-BIAS_MAX);
            }
        }
    }

    /// Push `persona`'s global bias down by one `DECAY` step.
    ///
    /// This is the inverse of the bump that `record_override` applies to a
    /// chosen persona. Both the structured `entries` map and the legacy `bias`
    /// map are decremented and clamped to `-BIAS_MAX`. Unlike the historical
    /// implementation it introduces no sentinel persona entries.
    pub fn decay(&mut self, persona: &str) {
        let legacy = self.bias.entry(persona.to_string()).or_insert(0.0);
        *legacy = (*legacy - DECAY).max(-BIAS_MAX);

        let entry = self.entries.entry(persona.to_string()).or_default();
        entry.global = (entry.global - DECAY).max(-BIAS_MAX);
    }

    /// Return the current additive bias for `persona`.
    ///
    /// Checks `entries` first (global bias), falls back to the legacy `bias` map,
    /// and returns 0.0 if the persona has no recorded bias in either.
    pub fn bias_for(&self, persona: &str) -> f32 {
        if let Some(entry) = self.entries.get(persona) {
            return entry.global;
        }
        self.bias.get(persona).copied().unwrap_or(0.0)
    }

    /// Return the bias for `persona` scoped to the given `intent`.
    ///
    /// Lookup rules:
    /// - `intent` is `None`: return global bias.
    /// - `intent` is `Some` and `by_intent` contains the key: return the per-intent bias.
    /// - `intent` is `Some`, `by_intent` is non-empty but the key is absent: return 0.0
    ///   (this persona has intent-scoped biases but not for this intent).
    /// - `intent` is `Some`, `by_intent` is empty: fall back to global bias (persona was
    ///   recorded without any intent context, so global bias is the best signal).
    /// - Persona not in `entries`: fall back to legacy `bias` map.
    pub fn bias_for_intent(&self, persona: &str, intent: Option<crate::intent::Intent>) -> f32 {
        if let Some(intent) = intent {
            if let Some(entry) = self.entries.get(persona) {
                if !entry.by_intent.is_empty() {
                    // Intent-specific biases exist -- only return one if this intent is tracked.
                    let intent_key = format!("{intent:?}").to_lowercase();
                    return entry.by_intent.get(&intent_key).copied().unwrap_or(0.0);
                }
                // No per-intent data; fall back to global entry bias.
                return entry.global;
            }
            // Persona not in structured entries; check legacy map.
            return self.bias.get(persona).copied().unwrap_or(0.0);
        }
        // No intent context: return global bias.
        self.bias_for(persona)
    }

    /// Return the effective bias for `persona` after applying time decay.
    ///
    /// Decay formula: `raw * max(DECAY_FLOOR, 1.0 - days * DECAY_RATE_PER_DAY)`.
    /// The floor ensures bias never decays to zero regardless of elapsed days.
    pub fn effective_bias_for(
        &self,
        persona: &str,
        intent: Option<crate::intent::Intent>,
        days_since_override: u32,
    ) -> f32 {
        let raw = self.bias_for_intent(persona, intent);
        let decay_multiplier =
            (1.0 - days_since_override as f32 * DECAY_RATE_PER_DAY).max(DECAY_FLOOR);
        raw * decay_multiplier
    }

    /// Load preferences from a JSON file.
    ///
    /// Returns an empty `Preferences` if the file does not exist.
    pub fn load(path: &Path) -> Result<Self, OrchestratorError> {
        if !path.exists() {
            return Ok(Self::new());
        }
        let data = std::fs::read_to_string(path)?;
        let prefs = serde_json::from_str(&data)?;
        Ok(prefs)
    }

    /// Persist preferences to a JSON file, creating parent directories as needed.
    ///
    /// The write is atomic: data is serialized to a uniquely-named temp file in
    /// the same directory and then renamed over the target, so a crash or a
    /// concurrent writer never leaves a half-written file that `load` would fail
    /// to parse.
    pub fn save(&self, path: &Path) -> Result<(), OrchestratorError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let data = serde_json::to_string_pretty(self)?;
        let file_name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("preferences.json");
        let tmp_path = path.with_file_name(format!(".{file_name}.tmp.{}", std::process::id()));
        std::fs::write(&tmp_path, data.as_bytes())?;
        std::fs::rename(&tmp_path, path)?;
        Ok(())
    }
}

#[cfg(test)]
/// Preference scoring and persistence tests.
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// record_override increases bias for the chosen persona.
    #[test]
    fn override_increases_chosen_bias() {
        let mut prefs = Preferences::new();
        prefs.record_override(Some("auto"), "chosen");
        assert!(prefs.bias_for("chosen") > 0.0);
    }

    /// record_override decreases bias for the auto-picked persona.
    #[test]
    fn override_decreases_auto_bias() {
        let mut prefs = Preferences::new();
        prefs.record_override(Some("auto"), "chosen");
        assert!(prefs.bias_for("auto") < 0.0);
    }

    /// Bias is clamped at BIAS_MAX even after many bumps.
    #[test]
    fn bias_clamped_at_max() {
        let mut prefs = Preferences::new();
        for _ in 0..100 {
            prefs.record_override(None, "target");
        }
        assert!(prefs.bias_for("target") <= BIAS_MAX + f32::EPSILON);
    }

    /// Bias is clamped at -BIAS_MAX even after many decays.
    #[test]
    fn bias_clamped_at_min() {
        let mut prefs = Preferences::new();
        for _ in 0..100 {
            prefs.record_override(Some("victim"), "other");
        }
        assert!(prefs.bias_for("victim") >= -BIAS_MAX - f32::EPSILON);
    }

    /// Load returns empty Preferences when the file does not exist.
    #[test]
    fn load_missing_returns_empty() {
        let tmp = TempDir::new().unwrap();
        let prefs = Preferences::load(&tmp.path().join("nonexistent.json")).unwrap();
        assert!(prefs.bias.is_empty());
    }

    /// Save and load round-trip preserves bias values.
    #[test]
    fn save_load_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("prefs.json");

        let mut prefs = Preferences::new();
        prefs.record_override(Some("a"), "b");
        prefs.save(&path).unwrap();

        let loaded = Preferences::load(&path).unwrap();
        assert!((loaded.bias_for("b") - prefs.bias_for("b")).abs() < f32::EPSILON);
        assert!((loaded.bias_for("a") - prefs.bias_for("a")).abs() < f32::EPSILON);
    }

    /// Saving writes atomically and leaves no temporary file behind.
    #[test]
    fn save_leaves_no_temp_file() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("prefs.json");

        let mut prefs = Preferences::new();
        prefs.record_override(Some("a"), "b");
        prefs.save(&path).unwrap();

        assert!(path.exists());
        let leftovers: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp."))
            .collect();
        // No temp file should remain after a successful save.
        assert!(leftovers.is_empty());
        // And the saved file loads cleanly.
        Preferences::load(&path).unwrap();
    }

    /// record_override_with_intent records a per-intent bias for the chosen persona.
    #[test]
    fn per_intent_bias_is_recorded() {
        use crate::intent::Intent;
        let mut prefs = Preferences::new();
        prefs.record_override_with_intent(Some("auto"), "chosen", Some(Intent::Debugging));
        assert!(prefs.bias_for_intent("chosen", Some(Intent::Debugging)) > 0.0);
        assert_eq!(prefs.bias_for_intent("chosen", Some(Intent::Security)), 0.0);
    }

    /// Global bias recorded via record_override is visible through bias_for and bias_for_intent.
    #[test]
    fn global_bias_still_works() {
        let mut prefs = Preferences::new();
        prefs.record_override(Some("auto"), "chosen");
        assert!(prefs.bias_for("chosen") > 0.0);
        assert!(prefs.bias_for_intent("chosen", None) > 0.0);
    }

    /// Time decay reduces effective bias relative to the undecayed value.
    #[test]
    fn time_decay_reduces_bias() {
        let mut prefs = Preferences::new();
        prefs.record_override(Some("auto"), "chosen");
        let fresh_bias = prefs.effective_bias_for("chosen", None, 0);
        let decayed_bias = prefs.effective_bias_for("chosen", None, 70);
        assert!(decayed_bias < fresh_bias);
        assert!(decayed_bias > 0.0, "floor prevents complete decay");
    }

    /// Time decay bottoms out at DECAY_FLOOR and does not continue decreasing beyond it.
    #[test]
    fn time_decay_has_floor() {
        let mut prefs = Preferences::new();
        prefs.record_override(Some("auto"), "chosen");
        let bias_200_days = prefs.effective_bias_for("chosen", None, 200);
        let bias_500_days = prefs.effective_bias_for("chosen", None, 500);
        assert!((bias_200_days - bias_500_days).abs() < f32::EPSILON);
    }
}
