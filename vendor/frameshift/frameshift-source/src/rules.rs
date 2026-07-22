//! Behavioral rule types for `PersonaSource`.
//!
//! A `RuleSet` is the deserialized form of `rules.toml`.  Each `Rule` has a
//! machine-readable `id`, an enforcement `Layer`, and a human-readable `text`.

use serde::{Deserialize, Serialize};

/// The `rules.toml` file -- a flat list of `[[rule]]` entries.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct RuleSet {
    #[serde(default, rename = "rule", skip_serializing_if = "Vec::is_empty")]
    pub rules: Vec<Rule>,
}

/// A single behavioral rule with an id, enforcement layer, and descriptive text.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Rule {
    /// Unique machine-readable identifier for this rule (e.g. `no-rolling-crypto`).
    pub id: String,
    /// Enforcement layer that determines how strictly this rule is applied.
    pub layer: Layer,
    /// Human-readable statement of the rule.
    pub text: String,
    /// Extended explanation of why this rule exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
    /// When `true`, this rule explicitly overrides an inherited rule with the
    /// same `id`. Only meaningful in the root persona layer -- mixins cannot
    /// use this to override L1 rules from the base.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub override_inherited: bool,
}

/// Behavioral layer for a rule.
///
/// - `L1` -- non-negotiable invariants
/// - `L2` -- contextual defaults, overridable with explicit justification
/// - `L3` -- preferences and stylistic guidance
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum Layer {
    /// Non-negotiable invariant; must never be violated.
    L1,
    /// Contextual default; overridable only with explicit justification.
    L2,
    /// Preference or stylistic guidance; lowest enforcement weight.
    L3,
}

/// Constructs behavioral rule collections for persona sources.
impl RuleSet {
    /// Construct an empty `RuleSet`. Equivalent to `RuleSet::default()`.
    pub fn new() -> Self {
        Self::default()
    }
}

#[cfg(test)]
/// Verifies rule collection TOML serialization.
mod tests {
    use super::*;

    #[test]
    /// Round-trips a populated rule collection through TOML.
    fn ruleset_toml_roundtrip() {
        let original = RuleSet {
            rules: vec![
                Rule {
                    id: "no-rolling-crypto".to_string(),
                    layer: Layer::L1,
                    text: "Never roll a new cryptographic primitive when an audited implementation exists.".to_string(),
                    reasoning: None,
                    override_inherited: false,
                },
                Rule {
                    id: "preserve-constant-time".to_string(),
                    layer: Layer::L1,
                    text: "Never replace constant-time code with variable-time code.".to_string(),
                    reasoning: None,
                    override_inherited: false,
                },
                Rule {
                    id: "prefer-rfc-citations".to_string(),
                    layer: Layer::L3,
                    text: "Cite the RFC or specification when discussing protocol behavior.".to_string(),
                    reasoning: None,
                    override_inherited: false,
                },
            ],
        };

        let serialized = toml::to_string(&original).unwrap();
        let parsed: RuleSet = toml::from_str(&serialized).unwrap();
        assert_eq!(parsed, original);
    }

    #[test]
    /// Round-trips an empty rule collection through TOML.
    fn empty_ruleset_roundtrips() {
        let original = RuleSet::default();
        let serialized = toml::to_string(&original).unwrap();
        let parsed: RuleSet = toml::from_str(&serialized).unwrap();
        assert_eq!(parsed, original);
    }

    #[test]
    /// Preserves optional rule reasoning through TOML serialization.
    fn rule_with_reasoning_roundtrips() {
        let original = RuleSet {
            rules: vec![Rule {
                id: "no-rolling-crypto".to_string(),
                layer: Layer::L1,
                text: "Never roll a new cryptographic primitive.".to_string(),
                reasoning: Some(
                    "Audited implementations have received expert scrutiny; hand-rolled code has not.".to_string(),
                ),
                override_inherited: false,
            }],
        };

        let serialized = toml::to_string(&original).unwrap();
        let parsed: RuleSet = toml::from_str(&serialized).unwrap();
        assert_eq!(parsed, original);
        // Verify the reasoning survives the round-trip with its exact value.
        assert_eq!(
            parsed.rules[0].reasoning.as_deref(),
            Some(
                "Audited implementations have received expert scrutiny; hand-rolled code has not."
            )
        );
    }
}
