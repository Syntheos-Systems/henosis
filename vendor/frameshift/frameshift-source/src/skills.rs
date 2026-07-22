use serde::{Deserialize, Serialize};

/// The `skills.toml` file -- a flat list of `[[skill]]` entries.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct SkillSet {
    #[serde(default, rename = "skill", skip_serializing_if = "Vec::is_empty")]
    pub skills: Vec<Skill>,
}

/// A single skill entry describing when and how a capability should be applied.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Skill {
    /// Unique machine-readable identifier for this skill (e.g. `test-driven-development`).
    pub id: String,
    /// Free-text description of when this skill should be invoked.
    pub invoke_when: String,
    /// Whether this skill must always be invoked (not optional).
    #[serde(default)]
    pub mandatory: bool,
}

/// Constructs skill collections for persona sources.
impl SkillSet {
    /// Constructs an empty skill collection.
    pub fn new() -> Self {
        Self::default()
    }
}

#[cfg(test)]
/// Verifies skill collection TOML serialization.
mod tests {
    use super::*;

    #[test]
    /// Round-trips a populated skill collection through TOML.
    fn skillset_toml_roundtrip() {
        let original = SkillSet {
            skills: vec![
                Skill {
                    id: "test-driven-development".to_string(),
                    invoke_when: "All cryptographic implementations -- tests BEFORE code"
                        .to_string(),
                    mandatory: false,
                },
                Skill {
                    id: "security-audit-remediation".to_string(),
                    invoke_when: "When CVE-class issues are reported against a primitive in use"
                        .to_string(),
                    mandatory: false,
                },
            ],
        };

        let serialized = toml::to_string(&original).unwrap();
        let parsed: SkillSet = toml::from_str(&serialized).unwrap();
        assert_eq!(parsed, original);
    }

    #[test]
    /// Round-trips an empty skill collection through TOML.
    fn empty_skillset_roundtrips() {
        let original = SkillSet::default();
        let serialized = toml::to_string(&original).unwrap();
        let parsed: SkillSet = toml::from_str(&serialized).unwrap();
        assert_eq!(parsed, original);
    }

    #[test]
    /// Preserves the mandatory-skill flag through TOML serialization.
    fn skill_with_mandatory_roundtrips() {
        let original = SkillSet {
            skills: vec![Skill {
                id: "test-driven-development".to_string(),
                invoke_when: "All cryptographic implementations -- tests BEFORE code".to_string(),
                mandatory: true,
            }],
        };

        let serialized = toml::to_string(&original).unwrap();
        let parsed: SkillSet = toml::from_str(&serialized).unwrap();
        assert_eq!(parsed, original);
        // Verify the mandatory flag survives the round-trip.
        assert!(parsed.skills[0].mandatory);
    }
}
