use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Author attribution for a persona pack.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Author {
    /// The author's handle or display name.
    pub handle: String,
    /// Optional public key for signature verification.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pubkey: Option<String>,
}

/// The voice section of a persona: tone, optional extended text, and guiding
/// questions that help the model maintain voice consistency.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Voice {
    /// Short descriptor of the voice style (e.g. "citation-driven, careful").
    pub tone: String,
    /// Optional extended prose describing the voice in more detail.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// Questions the model asks itself to stay in voice.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub questions: Vec<VoiceQuestion>,
}

/// A single self-check question used to maintain voice consistency.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct VoiceQuestion {
    /// The question text.
    pub text: String,
}

/// A named anchor block inserted at a specific position in the rendered prompt.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Anchor {
    /// The anchor body text.
    pub text: String,
    /// Short tagline shown in summary views.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tagline: Option<String>,
    /// Default question associated with this anchor position.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_question: Option<String>,
}

/// A single default question surfaced to the user or agent.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DefaultQuestion {
    /// The question text.
    pub question: String,
}

/// One tier in the L1/L2/L3 classification hierarchy.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ClassificationTier {
    /// Tier name (e.g. "L1", "L2", "L3").
    pub name: String,
    /// Human-readable description of what belongs in this tier.
    pub description: String,
    /// Optional extra guidance for applying this tier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guidance: Option<String>,
}

/// Conflict resolution policy: how the model arbitrates between competing
/// instructions or persona constraints.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ConflictResolution {
    /// High-level stance (e.g. "L1 beats L2 beats L3").
    pub stance: String,
    /// Individual aspects of the resolution policy.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub aspects: Vec<Aspect>,
}

/// A named aspect of a conflict-resolution policy.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Aspect {
    /// Machine-readable key for this aspect.
    pub key: String,
    /// Human-readable explanation of this aspect.
    pub text: String,
}

/// A cascade anchor: a prompt fragment injected at a specific render position.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CascadeAnchor {
    /// The render position where this anchor is injected (e.g. "top", "recency").
    pub position: String,
    /// The anchor text.
    pub text: String,
}

/// One step in the model's self-evaluation loop.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SelfEvalStep {
    /// Description of what the model checks in this step.
    pub step: String,
}

/// A question posed when the model is uncertain about scope or intent.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AmbiguityQuestion {
    /// The question text.
    pub text: String,
}

/// A safety layer appended to the rendered prompt.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SafetyLayer {
    /// The safety layer body text.
    pub text: String,
}

/// Configuration for the growth pipeline (dual-write tagging and sourcing).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GrowthConfig {
    /// Tags written to both private and shared growth stores.
    pub dual_write_tags: String,
    /// Source identifier for dual-write events.
    pub dual_write_source: String,
}

/// A group of reference entries sharing a common category.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ReferenceGroup {
    /// Category label (e.g. "specifications", "papers").
    pub category: String,
    /// Individual reference strings within this category.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub entries: Vec<String>,
}

/// Declares what tools and filesystem/network access this persona requires.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CapabilityManifest {
    /// Tool IDs that must be available for this persona to function.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub required_tools: Vec<String>,
    /// Glob or description of allowed filesystem paths.
    pub filesystem_scope: String,
    /// Whether outbound network egress is permitted. Defaults to `false` (deny)
    /// when absent from TOML.
    #[serde(default)]
    pub network_egress: bool,
    /// Task intents this persona is designed for (e.g. "debugging", "security").
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub primary_intents: Vec<String>,
    /// Keywords that should repel this persona during selection.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub anti_keywords: Vec<String>,
}

/// Conformance record: self-reported score and bundle hash for audit trails.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ConformanceConfig {
    /// Self-reported conformance score in [0.0, 1.0].
    pub score: f64,
    /// Hash of the bundle that produced this conformance record.
    pub bundle_hash: String,
}

/// The `persona.toml` file -- identity, voice, anchors, behavioral architecture.
///
/// All optional and list fields use serde defaults so that minimal
/// `persona.toml` files (with only `schema_version`, `name`, and `voice`)
/// still parse without error.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Persona {
    /// Schema version for forward-compatibility checks.
    pub schema_version: u32,
    /// Canonical persona name used as a key throughout the system.
    pub name: String,
    /// Semver version of this persona pack.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// Short human-readable description of what this persona does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Topical tags that bias keyword-based persona selection and feed
    /// registry search, mirroring the `tags` field in `pack.toml`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// SPDX license identifier for this persona pack.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub license: Option<String>,
    /// Author attribution.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<Author>,
    /// Name of the base persona this one extends (single-parent composition).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extends: Option<String>,
    /// Additional personas mixed into this one (multi-parent composition).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mixin: Vec<String>,
    /// Voice configuration: tone, extended prose, and self-check questions.
    pub voice: Voice,
    /// Named anchor blocks keyed by anchor name (e.g. "l2", "cascade_top").
    /// Order is not significant here; render order is chosen by the projection
    /// layer.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub anchor: BTreeMap<String, Anchor>,
    /// Classification tier definitions (L1/L2/L3 hierarchy).
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        rename = "classification_tier"
    )]
    pub classification_tiers: Vec<ClassificationTier>,
    /// How this persona resolves conflicting instructions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conflict_resolution: Option<ConflictResolution>,
    /// Cascade anchors injected at specific render positions.
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        rename = "cascade_anchor"
    )]
    pub cascade_anchors: Vec<CascadeAnchor>,
    /// Steps in the model's self-evaluation loop.
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        rename = "self_eval_step"
    )]
    pub self_eval: Vec<SelfEvalStep>,
    /// Questions asked when scope or intent is ambiguous.
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        rename = "ambiguity_question"
    )]
    pub ambiguity_questions: Vec<AmbiguityQuestion>,
    /// Safety layer appended to the rendered prompt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub safety_layer: Option<SafetyLayer>,
    /// Growth pipeline configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub growth: Option<GrowthConfig>,
    /// Reference groups (specs, papers, links).
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        rename = "reference_group"
    )]
    pub references: Vec<ReferenceGroup>,
    /// Capability manifest declaring required tools and access scope.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capability_manifest: Option<CapabilityManifest>,
    /// Conformance record for audit purposes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conformance: Option<ConformanceConfig>,
    /// Default questions surfaced to the user or agent.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub default_questions: Vec<DefaultQuestion>,
}

impl Persona {
    /// Minimal valid persona -- used as a default when scaffolding.
    ///
    /// All optional fields are `None` and all list fields are empty.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            schema_version: 1,
            name: name.into(),
            version: None,
            description: None,
            tags: Vec::new(),
            license: None,
            author: None,
            extends: None,
            mixin: Vec::new(),
            voice: Voice {
                tone: String::new(),
                text: None,
                questions: Vec::new(),
            },
            anchor: BTreeMap::new(),
            classification_tiers: Vec::new(),
            conflict_resolution: None,
            cascade_anchors: Vec::new(),
            self_eval: Vec::new(),
            ambiguity_questions: Vec::new(),
            safety_layer: None,
            growth: None,
            references: Vec::new(),
            capability_manifest: None,
            conformance: None,
            default_questions: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verifies that the minimal persona (name + voice) still round-trips
    /// through TOML after the Voice struct migration.
    #[test]
    fn persona_toml_roundtrip() {
        let mut anchor = BTreeMap::new();
        anchor.insert(
            "l2".to_string(),
            Anchor {
                text: "You are a cryptographic correctness practitioner.".to_string(),
                tagline: None,
                default_question: None,
            },
        );
        anchor.insert(
            "cascade_top".to_string(),
            Anchor {
                text: "specification first, reference implementation second".to_string(),
                tagline: None,
                default_question: None,
            },
        );

        let original = Persona {
            schema_version: 1,
            name: "cryptographic".to_string(),
            version: None,
            description: None,
            tags: Vec::new(),
            license: None,
            author: None,
            extends: None,
            mixin: Vec::new(),
            voice: Voice {
                tone: "citation-driven, careful, willing to say I don't know".to_string(),
                text: None,
                questions: Vec::new(),
            },
            anchor,
            classification_tiers: Vec::new(),
            conflict_resolution: None,
            cascade_anchors: Vec::new(),
            self_eval: Vec::new(),
            ambiguity_questions: Vec::new(),
            safety_layer: None,
            growth: None,
            references: Vec::new(),
            capability_manifest: None,
            conformance: None,
            default_questions: vec![DefaultQuestion {
                question: "Which specification or RFC governs this code?".to_string(),
            }],
        };

        let serialized = toml::to_string(&original).unwrap();
        let parsed: Persona = toml::from_str(&serialized).unwrap();
        assert_eq!(parsed, original);
    }

    /// Verifies that a fully-populated Persona with every new field set
    /// round-trips through TOML without data loss.
    #[test]
    fn enriched_persona_toml_roundtrip() {
        let mut anchor = BTreeMap::new();
        anchor.insert(
            "l2".to_string(),
            Anchor {
                text: "anchor body".to_string(),
                tagline: Some("short tagline".to_string()),
                default_question: Some("What is the invariant here?".to_string()),
            },
        );

        let original = Persona {
            schema_version: 1,
            name: "enriched".to_string(),
            version: Some("1.2.3".to_string()),
            description: Some("A fully-enriched test persona.".to_string()),
            tags: vec!["security".to_string(), "rust".to_string()],
            license: Some("MIT".to_string()),
            author: Some(Author {
                handle: "testauthor".to_string(),
                pubkey: Some("ed25519:abc123".to_string()),
            }),
            extends: Some("base-persona".to_string()),
            mixin: vec!["security-mixin".to_string(), "review-mixin".to_string()],
            voice: Voice {
                tone: "precise and rigorous".to_string(),
                text: Some("Extended voice prose goes here.".to_string()),
                questions: vec![VoiceQuestion {
                    text: "Am I being precise?".to_string(),
                }],
            },
            anchor,
            classification_tiers: vec![
                ClassificationTier {
                    name: "L1".to_string(),
                    description: "Hard invariants.".to_string(),
                    guidance: Some("Never override.".to_string()),
                },
                ClassificationTier {
                    name: "L2".to_string(),
                    description: "Strong preferences.".to_string(),
                    guidance: None,
                },
            ],
            conflict_resolution: Some(ConflictResolution {
                stance: "L1 beats L2 beats L3".to_string(),
                aspects: vec![Aspect {
                    key: "safety".to_string(),
                    text: "Safety rules always win.".to_string(),
                }],
            }),
            cascade_anchors: vec![CascadeAnchor {
                position: "top".to_string(),
                text: "Remember the prime directive.".to_string(),
            }],
            self_eval: vec![SelfEvalStep {
                step: "Did I cite a source?".to_string(),
            }],
            ambiguity_questions: vec![AmbiguityQuestion {
                text: "Which scope applies here?".to_string(),
            }],
            safety_layer: Some(SafetyLayer {
                text: "Do not generate harmful content.".to_string(),
            }),
            growth: Some(GrowthConfig {
                dual_write_tags: "enriched,test".to_string(),
                dual_write_source: "persona:enriched".to_string(),
            }),
            references: vec![ReferenceGroup {
                category: "specifications".to_string(),
                entries: vec!["RFC 8446".to_string(), "FIPS 140-2".to_string()],
            }],
            capability_manifest: Some(CapabilityManifest {
                required_tools: vec!["read_file".to_string(), "write_file".to_string()],
                filesystem_scope: "/home/**".to_string(),
                network_egress: false,
                primary_intents: vec![],
                anti_keywords: vec![],
            }),
            conformance: Some(ConformanceConfig {
                score: 0.95,
                bundle_hash: "sha256:deadbeef".to_string(),
            }),
            default_questions: vec![DefaultQuestion {
                question: "What is the invariant here?".to_string(),
            }],
        };

        let serialized = toml::to_string(&original).unwrap();
        let parsed: Persona = toml::from_str(&serialized).unwrap();
        assert_eq!(parsed, original);
    }
}
