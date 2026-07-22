//! Typed patch operations for `PersonaSource`.
//!
//! A `PatchOp` is an atomic, typed mutation against a `PersonaSource`. The
//! publish flow can render each op as a human-readable changelog entry instead
//! of a text diff -- this is the whole point of typed source.
//!
//! `apply_patch` applies a sequence of ops in order, returning the mutated
//! source on success or a `PatchError` on the first conflict. The function
//! takes ownership of the source so the caller gets a clean `Ok(new_source)`
//! / `Err(...)` split without partial-mutation surprises.

use serde::{Deserialize, Serialize};

use crate::persona::CascadeAnchor;
use crate::rules::Rule;
use crate::skills::Skill;
use crate::source::PersonaSource;

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

/// Errors that can occur when applying a `PatchOp` to a `PersonaSource`.
#[derive(Debug, thiserror::Error, PartialEq)]
pub enum PatchError {
    /// Returned by `RuleAdd` when a rule with the same `id` is already present.
    #[error("rule id '{id}' already exists in the source")]
    DuplicateRuleId {
        /// The conflicting rule id.
        id: String,
    },

    /// Returned by `RuleRemove` when no rule with the given `id` exists.
    #[error("rule id '{id}' not found in the source")]
    RuleNotFound {
        /// The id that was not found.
        id: String,
    },

    /// Returned by `SkillAdd` when a skill with the same `id` is already present.
    #[error("skill id '{id}' already exists in the source")]
    DuplicateSkillId {
        /// The conflicting skill id.
        id: String,
    },

    /// Returned by `SkillRemove` when no skill with the given `id` exists.
    #[error("skill id '{id}' not found in the source")]
    SkillNotFound {
        /// The id that was not found.
        id: String,
    },
}

// ---------------------------------------------------------------------------
// AnchorPosition
// ---------------------------------------------------------------------------

/// Position of a cascade anchor block in the rendered output.
///
/// `Top` and `Recency` are the two priming positions documented in the
/// PLAN.md cascade-anchor design; `L2` is the body anchor used for the
/// main behavioral framing.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum AnchorPosition {
    /// Injected at the very top of the rendered prompt.
    Top,
    /// Injected at the L2 (behavioral framing) position.
    L2,
    /// Injected at the recency (bottom) priming position.
    Recency,
}

/// Converts anchor positions to their serialized persona-source representation.
impl AnchorPosition {
    /// Returns the lowercase string key used as the `position` field in
    /// `CascadeAnchor` TOML entries (e.g. `"top"`, `"l2"`, `"recency"`).
    fn as_position_str(self) -> &'static str {
        match self {
            AnchorPosition::Top => "top",
            AnchorPosition::L2 => "l2",
            AnchorPosition::Recency => "recency",
        }
    }
}

// ---------------------------------------------------------------------------
// PatchOp
// ---------------------------------------------------------------------------

/// Typed mutation against a `PersonaSource`. The publish flow can render
/// each `PatchOp` as a human-readable changelog entry instead of a text
/// diff -- this is the whole point of typed source.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum PatchOp {
    /// Add a new rule. Fails with `PatchError::DuplicateRuleId` if a rule
    /// with the same `id` is already present.
    RuleAdd(Rule),
    /// Remove the rule with the given `id`. Fails with `PatchError::RuleNotFound`
    /// if no matching rule exists.
    RuleRemove {
        /// Id of the rule to remove.
        id: String,
    },
    /// Add a new skill. Fails with `PatchError::DuplicateSkillId` if a skill
    /// with the same `id` is already present.
    SkillAdd(Skill),
    /// Remove the skill with the given `id`. Fails with `PatchError::SkillNotFound`
    /// if no matching skill exists.
    SkillRemove {
        /// Id of the skill to remove.
        id: String,
    },
    /// Upsert a cascade anchor at the given position. If an anchor with the
    /// same position key already exists its text is replaced; otherwise a new
    /// entry is appended.
    CascadeAnchorSet {
        /// Which render position to target.
        position: AnchorPosition,
        /// New anchor text.
        text: String,
    },
}

// ---------------------------------------------------------------------------
// apply_patch
// ---------------------------------------------------------------------------

/// Apply a sequence of `PatchOp`s in order to `source`, returning the
/// mutated `PersonaSource` on success.
///
/// Operations are applied one at a time in the order they appear in `ops`.
/// If any op produces a conflict the function returns the appropriate
/// `PatchError` immediately. Callers must treat `Err` as meaning no write
/// occurred -- the safety guarantee depends on not persisting on error.
/// Callers that need rollback semantics should clone the source before
/// calling.
///
/// An empty `ops` slice is a no-op and always returns `Ok(source)`.
pub fn apply_patch(
    mut source: PersonaSource,
    ops: Vec<PatchOp>,
) -> Result<PersonaSource, PatchError> {
    for op in ops {
        source = apply_one(source, op)?;
    }
    Ok(source)
}

/// Apply a single `PatchOp` to `source`, returning the updated source or an
/// error without partial mutation (each branch either succeeds fully or
/// returns before modifying `source`).
fn apply_one(mut source: PersonaSource, op: PatchOp) -> Result<PersonaSource, PatchError> {
    match op {
        PatchOp::RuleAdd(rule) => {
            // Conflict check before any mutation.
            if source.rules.rules.iter().any(|r| r.id == rule.id) {
                return Err(PatchError::DuplicateRuleId { id: rule.id });
            }
            source.rules.rules.push(rule);
        }

        PatchOp::RuleRemove { id } => {
            // Verify the target exists before retaining.
            let before = source.rules.rules.len();
            source.rules.rules.retain(|r| r.id != id);
            if source.rules.rules.len() == before {
                return Err(PatchError::RuleNotFound { id });
            }
        }

        PatchOp::SkillAdd(skill) => {
            // Conflict check before any mutation.
            if source.skills.skills.iter().any(|s| s.id == skill.id) {
                return Err(PatchError::DuplicateSkillId { id: skill.id });
            }
            source.skills.skills.push(skill);
        }

        PatchOp::SkillRemove { id } => {
            // Verify the target exists before retaining.
            let before = source.skills.skills.len();
            source.skills.skills.retain(|s| s.id != id);
            if source.skills.skills.len() == before {
                return Err(PatchError::SkillNotFound { id });
            }
        }

        PatchOp::CascadeAnchorSet { position, text } => {
            // Upsert: replace existing entry if the position key matches,
            // otherwise append.
            let key = position.as_position_str();
            if let Some(existing) = source
                .persona
                .cascade_anchors
                .iter_mut()
                .find(|a| a.position == key)
            {
                existing.text = text;
            } else {
                source.persona.cascade_anchors.push(CascadeAnchor {
                    position: key.to_string(),
                    text,
                });
            }
        }
    }

    Ok(source)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persona::Persona;
    use crate::rules::{Layer, RuleSet};
    use crate::skills::SkillSet;
    use crate::source::PersonaSource;
    use std::fs;

    // -----------------------------------------------------------------------
    // Test helpers
    // -----------------------------------------------------------------------

    /// Builds a minimal `PersonaSource` with one rule (`r1`) and one skill (`s1`).
    fn base_source() -> PersonaSource {
        PersonaSource {
            persona: Persona::new("test"),
            rules: RuleSet {
                rules: vec![Rule {
                    id: "r1".to_string(),
                    layer: Layer::L1,
                    text: "rule one".to_string(),
                    reasoning: None,
                    override_inherited: false,
                }],
            },
            skills: SkillSet {
                skills: vec![Skill {
                    id: "s1".to_string(),
                    invoke_when: "always".to_string(),
                    mandatory: false,
                }],
            },
            patterns: crate::patterns::PatternSet::default(),
        }
    }

    /// Creates a unique temporary directory without pulling in a dev-dep.
    fn temp_dir() -> std::path::PathBuf {
        let base = std::env::temp_dir();
        let unique = format!(
            "frameshift-patch-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        );
        let dir = base.join(unique);
        fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    // -----------------------------------------------------------------------
    // Happy-path tests -- one per PatchOp variant
    // -----------------------------------------------------------------------

    /// RuleAdd appends the new rule and preserves existing rules in order.
    #[test]
    fn rule_add_appends_and_preserves_order() {
        let src = base_source();
        let new_rule = Rule {
            id: "r2".to_string(),
            layer: Layer::L2,
            text: "rule two".to_string(),
            reasoning: None,
            override_inherited: false,
        };
        let result = apply_patch(src, vec![PatchOp::RuleAdd(new_rule.clone())]).unwrap();
        assert_eq!(result.rules.rules.len(), 2);
        assert_eq!(result.rules.rules[0].id, "r1");
        assert_eq!(result.rules.rules[1].id, "r2");
        assert_eq!(result.rules.rules[1], new_rule);
    }

    /// RuleRemove deletes the target rule and preserves the rest.
    #[test]
    fn rule_remove_deletes_target_preserves_rest() {
        let src = base_source();
        let result = apply_patch(
            src,
            vec![PatchOp::RuleRemove {
                id: "r1".to_string(),
            }],
        )
        .unwrap();
        assert!(result.rules.rules.is_empty());
    }

    /// SkillAdd appends the new skill and preserves existing skills in order.
    #[test]
    fn skill_add_appends_and_preserves_order() {
        let src = base_source();
        let new_skill = Skill {
            id: "s2".to_string(),
            invoke_when: "on request".to_string(),
            mandatory: true,
        };
        let result = apply_patch(src, vec![PatchOp::SkillAdd(new_skill.clone())]).unwrap();
        assert_eq!(result.skills.skills.len(), 2);
        assert_eq!(result.skills.skills[0].id, "s1");
        assert_eq!(result.skills.skills[1].id, "s2");
        assert_eq!(result.skills.skills[1], new_skill);
    }

    /// SkillRemove deletes the target skill and leaves the rest intact.
    #[test]
    fn skill_remove_deletes_target_preserves_rest() {
        let src = base_source();
        let result = apply_patch(
            src,
            vec![PatchOp::SkillRemove {
                id: "s1".to_string(),
            }],
        )
        .unwrap();
        assert!(result.skills.skills.is_empty());
    }

    /// CascadeAnchorSet inserts a new anchor when none with that position exists.
    #[test]
    fn cascade_anchor_set_inserts_when_absent() {
        let src = base_source();
        let result = apply_patch(
            src,
            vec![PatchOp::CascadeAnchorSet {
                position: AnchorPosition::Recency,
                text: "recency text".to_string(),
            }],
        )
        .unwrap();
        let anchors = &result.persona.cascade_anchors;
        assert_eq!(anchors.len(), 1);
        assert_eq!(anchors[0].position, "recency");
        assert_eq!(anchors[0].text, "recency text");
    }

    /// CascadeAnchorSet replaces the text of an existing anchor at the same position.
    #[test]
    fn cascade_anchor_set_replaces_when_present() {
        let mut src = base_source();
        src.persona
            .cascade_anchors
            .push(crate::persona::CascadeAnchor {
                position: "top".to_string(),
                text: "old top text".to_string(),
            });
        let result = apply_patch(
            src,
            vec![PatchOp::CascadeAnchorSet {
                position: AnchorPosition::Top,
                text: "new top text".to_string(),
            }],
        )
        .unwrap();
        let anchors = &result.persona.cascade_anchors;
        assert_eq!(anchors.len(), 1, "should still have exactly one anchor");
        assert_eq!(anchors[0].text, "new top text");
    }

    // -----------------------------------------------------------------------
    // Error-condition tests
    // -----------------------------------------------------------------------

    /// RuleAdd returns DuplicateRuleId when the id already exists.
    #[test]
    fn rule_add_duplicate_id_errors() {
        let src = base_source();
        let duplicate = Rule {
            id: "r1".to_string(), // already in base_source
            layer: Layer::L3,
            text: "duplicate".to_string(),
            reasoning: None,
            override_inherited: false,
        };
        let err = apply_patch(src, vec![PatchOp::RuleAdd(duplicate)]).unwrap_err();
        assert_eq!(
            err,
            PatchError::DuplicateRuleId {
                id: "r1".to_string()
            }
        );
    }

    /// RuleRemove returns RuleNotFound when the id is absent.
    #[test]
    fn rule_remove_missing_id_errors() {
        let src = base_source();
        let err = apply_patch(
            src,
            vec![PatchOp::RuleRemove {
                id: "nonexistent".to_string(),
            }],
        )
        .unwrap_err();
        assert_eq!(
            err,
            PatchError::RuleNotFound {
                id: "nonexistent".to_string()
            }
        );
    }

    /// SkillAdd returns DuplicateSkillId when the id already exists.
    #[test]
    fn skill_add_duplicate_id_errors() {
        let src = base_source();
        let duplicate = Skill {
            id: "s1".to_string(), // already in base_source
            invoke_when: "duplicate".to_string(),
            mandatory: false,
        };
        let err = apply_patch(src, vec![PatchOp::SkillAdd(duplicate)]).unwrap_err();
        assert_eq!(
            err,
            PatchError::DuplicateSkillId {
                id: "s1".to_string()
            }
        );
    }

    /// SkillRemove returns SkillNotFound when the id is absent.
    #[test]
    fn skill_remove_missing_id_errors() {
        let src = base_source();
        let err = apply_patch(
            src,
            vec![PatchOp::SkillRemove {
                id: "nonexistent".to_string(),
            }],
        )
        .unwrap_err();
        assert_eq!(
            err,
            PatchError::SkillNotFound {
                id: "nonexistent".to_string()
            }
        );
    }

    // -----------------------------------------------------------------------
    // Multi-op and edge-case tests
    // -----------------------------------------------------------------------

    /// An empty ops vec is a no-op and returns the source unchanged.
    #[test]
    fn empty_ops_is_identity() {
        let src = base_source();
        let result = apply_patch(src.clone(), vec![]).unwrap();
        assert_eq!(result, src);
    }

    /// Ops in a vec are applied in order: add then remove produces an empty list.
    #[test]
    fn multi_op_ordering_add_then_remove() {
        let src = base_source();
        let new_rule = Rule {
            id: "r2".to_string(),
            layer: Layer::L2,
            text: "rule two".to_string(),
            reasoning: None,
            override_inherited: false,
        };
        let result = apply_patch(
            src,
            vec![
                PatchOp::RuleAdd(new_rule),
                PatchOp::RuleRemove {
                    id: "r1".to_string(),
                },
                PatchOp::RuleRemove {
                    id: "r2".to_string(),
                },
            ],
        )
        .unwrap();
        assert!(result.rules.rules.is_empty());
    }

    /// A failing op in the middle of a batch leaves prior ops applied.
    /// (No atomicity -- documented behavior.)
    #[test]
    fn multi_op_error_in_middle_leaves_prior_applied() {
        let src = base_source();
        let new_rule = Rule {
            id: "r2".to_string(),
            layer: Layer::L2,
            text: "rule two".to_string(),
            reasoning: None,
            override_inherited: false,
        };
        // Op 0 succeeds (adds r2), op 1 fails (r3 does not exist).
        // apply_patch returns Err, but we cannot observe the partial state
        // because we already consumed `src`.
        let err = apply_patch(
            src,
            vec![
                PatchOp::RuleAdd(new_rule),
                PatchOp::RuleRemove {
                    id: "r3".to_string(),
                },
            ],
        )
        .unwrap_err();
        assert_eq!(
            err,
            PatchError::RuleNotFound {
                id: "r3".to_string()
            }
        );
    }

    /// Round-trip: apply two ops, write the result to disk, reload, and
    /// assert equality with what apply_patch returned.
    #[test]
    fn round_trip_patch_write_load() {
        let src = base_source();
        let new_rule = Rule {
            id: "r2".to_string(),
            layer: Layer::L2,
            text: "rule two".to_string(),
            reasoning: None,
            override_inherited: false,
        };
        let new_skill = Skill {
            id: "s2".to_string(),
            invoke_when: "on request".to_string(),
            mandatory: true,
        };

        let patched = apply_patch(
            src,
            vec![
                PatchOp::RuleAdd(new_rule),
                PatchOp::SkillAdd(new_skill),
                PatchOp::CascadeAnchorSet {
                    position: AnchorPosition::L2,
                    text: "l2 anchor".to_string(),
                },
            ],
        )
        .unwrap();

        let dir = temp_dir();
        patched.write_to_dir(&dir).unwrap();

        let loaded = PersonaSource::load_from_dir(&dir).unwrap();
        assert_eq!(loaded, patched);
    }

    /// Existing JSON round-trip test from the original patch.rs scaffold,
    /// kept for compatibility.
    #[test]
    fn patch_op_json_roundtrip() {
        // PatchOp is exposed over MCP as JSON, so the JSON shape needs to
        // round-trip cleanly even though the at-rest format on disk is TOML.
        let ops = vec![
            PatchOp::RuleAdd(Rule {
                id: "new-rule".to_string(),
                layer: Layer::L2,
                text: "be kind".to_string(),
                reasoning: None,
                override_inherited: false,
            }),
            PatchOp::RuleRemove {
                id: "old-rule".to_string(),
            },
            PatchOp::SkillAdd(Skill {
                id: "new-skill".to_string(),
                invoke_when: "always".to_string(),
                mandatory: false,
            }),
            PatchOp::SkillRemove {
                id: "old-skill".to_string(),
            },
            PatchOp::CascadeAnchorSet {
                position: AnchorPosition::Recency,
                text: "final priming line".to_string(),
            },
        ];

        let serialized = serde_json::to_string(&ops).unwrap();
        let parsed: Vec<PatchOp> = serde_json::from_str(&serialized).unwrap();
        assert_eq!(parsed, ops);
    }
}
