/// Semantic diff between two `PersonaSource` snapshots.
///
/// Rules and skills are matched by exact `id` equality. Voice comparison is
/// field-equality on the structured `Voice` type. Anchor similarity uses
/// Jaccard over the normalized token sets of all `cascade_anchor` texts
/// concatenated.
use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::persona::CascadeAnchor;
use crate::rules::Rule;
use crate::skills::Skill;
use crate::source::PersonaSource;

/// Typed diff between two `PersonaSource` snapshots.
///
/// Rules and skills are compared by exact `id` equality. `voice_changed`
/// is a plain equality check on the structured `Voice` type (any field
/// differs => `true`). `anchor_similarity` is the Jaccard similarity over
/// the normalized token sets of all `cascade_anchor` texts.
///
/// `None` means the similarity was not computed. `Default` yields `None`
/// because no computation has taken place. `diff()` always produces `Some`
/// because it always runs the Jaccard computation.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct SemanticDiff {
    /// Rule ids present in `b` but not in `a`.
    pub added_rules: Vec<String>,
    /// Rule ids present in `a` but not in `b`.
    pub removed_rules: Vec<String>,
    /// Rule ids present in both sides with at least one field differing.
    pub modified_rules: Vec<String>,
    /// Skill ids present in `b` but not in `a`.
    /// Note: a skill whose id is unchanged but whose content changed will
    /// appear in both `removed_skills` and `added_skills` (no `modified_skills`
    /// slot exists in this struct).
    pub added_skills: Vec<String>,
    /// Skill ids present in `a` but not in `b`.
    /// Note: see `added_skills` comment for the modified-skill convention.
    pub removed_skills: Vec<String>,
    /// `true` when any field of the `Voice` struct differs between `a` and `b`.
    pub voice_changed: bool,
    /// Jaccard similarity over the normalized token sets of all cascade anchor
    /// texts.  Range [0.0, 1.0]; 1.0 = identical token sets.  `Some(1.0)` when
    /// both sides have no cascade anchors.  `Some(0.0)` when one side is empty
    /// and the other is not.  `None` when no computation has been performed
    /// (e.g., produced by `Default`).  `diff()` always yields `Some`.
    pub anchor_similarity: Option<f32>,
}

/// Compute the semantic diff between two `PersonaSource` snapshots.
///
/// Rule and skill comparison uses exact `id` equality. A "modified" entry is
/// one whose id exists on both sides but whose content differs field-by-field
/// via `PartialEq`. Because `SemanticDiff` has no `modified_skills` slot, a
/// skill whose id is unchanged but whose content changed is recorded in both
/// `removed_skills` (old form) and `added_skills` (new form).
pub fn diff(a: &PersonaSource, b: &PersonaSource) -> SemanticDiff {
    let (added_rules, removed_rules, modified_rules) = diff_rules(&a.rules.rules, &b.rules.rules);

    let (added_skills, removed_skills) = diff_skills(&a.skills.skills, &b.skills.skills);

    let voice_changed = a.persona.voice != b.persona.voice;

    let anchor_similarity = Some(jaccard_anchor_similarity(
        &a.persona.cascade_anchors,
        &b.persona.cascade_anchors,
    ));

    SemanticDiff {
        added_rules,
        removed_rules,
        modified_rules,
        added_skills,
        removed_skills,
        voice_changed,
        anchor_similarity,
    }
}

/// Diff two rule lists by `id`.
///
/// Returns `(added, removed, modified)` id vecs. "Added" = id in `b` not in
/// `a`. "Removed" = id in `a` not in `b`. "Modified" = id in both sides but
/// `Rule` fields differ.
fn diff_rules(a_rules: &[Rule], b_rules: &[Rule]) -> (Vec<String>, Vec<String>, Vec<String>) {
    // Build id -> Rule maps. If an id is duplicated within one side, last
    // entry wins; duplicate ids are invalid input but we degrade gracefully.
    let a_map: HashMap<&str, &Rule> = a_rules.iter().map(|r| (r.id.as_str(), r)).collect();
    let b_map: HashMap<&str, &Rule> = b_rules.iter().map(|r| (r.id.as_str(), r)).collect();

    let mut added = Vec::new();
    let mut removed = Vec::new();
    let mut modified = Vec::new();

    // Rules in a: check for removal or modification.
    for (id, a_rule) in &a_map {
        match b_map.get(id) {
            None => removed.push((*id).to_string()),
            Some(b_rule) => {
                if a_rule != b_rule {
                    modified.push((*id).to_string());
                }
            }
        }
    }

    // Rules in b not in a: additions.
    for id in b_map.keys() {
        if !a_map.contains_key(id) {
            added.push((*id).to_string());
        }
    }

    // Sort for deterministic output (HashMap iteration order is unspecified).
    added.sort_unstable();
    removed.sort_unstable();
    modified.sort_unstable();

    (added, removed, modified)
}

/// Diff two skill lists by `id`.
///
/// Returns `(added, removed)` id vecs. Because `SemanticDiff` has no
/// `modified_skills` slot, a skill whose id is unchanged but whose content
/// changed appears in both `removed` (id from the old form in `a`) and
/// `added` (id from the new form in `b`).
fn diff_skills(a_skills: &[Skill], b_skills: &[Skill]) -> (Vec<String>, Vec<String>) {
    let a_map: HashMap<&str, &Skill> = a_skills.iter().map(|s| (s.id.as_str(), s)).collect();
    let b_map: HashMap<&str, &Skill> = b_skills.iter().map(|s| (s.id.as_str(), s)).collect();

    let mut added = Vec::new();
    let mut removed = Vec::new();

    // Skills in a: removed if not in b, or content-changed (treated as
    // remove + re-add since there is no modified_skills slot).
    for (id, a_skill) in &a_map {
        match b_map.get(id) {
            None => removed.push((*id).to_string()),
            Some(b_skill) => {
                if a_skill != b_skill {
                    // Content changed: record as removed-then-added.
                    removed.push((*id).to_string());
                    added.push((*id).to_string());
                }
            }
        }
    }

    // Skills in b not in a: pure additions.
    for id in b_map.keys() {
        if !a_map.contains_key(id) {
            added.push((*id).to_string());
        }
    }

    added.sort_unstable();
    removed.sort_unstable();

    (added, removed)
}

/// Compute Jaccard similarity over the normalized token sets of all cascade
/// anchor texts.
///
/// Normalization: lowercase each character, strip any character that is not
/// alphanumeric or ASCII whitespace, then split on whitespace. Each unique
/// token is one element of the set.
///
/// Special cases:
/// - Both sides empty: returns 1.0 (identical -- both have nothing).
/// - One side empty, the other not: returns 0.0 (fully disjoint).
/// - Non-empty overlap: `|intersection| as f32 / |union| as f32`.
fn jaccard_anchor_similarity(a: &[CascadeAnchor], b: &[CascadeAnchor]) -> f32 {
    let a_tokens = anchor_token_set(a);
    let b_tokens = anchor_token_set(b);

    if a_tokens.is_empty() && b_tokens.is_empty() {
        return 1.0;
    }
    if a_tokens.is_empty() || b_tokens.is_empty() {
        return 0.0;
    }

    let intersection = a_tokens.intersection(&b_tokens).count();
    let union = a_tokens.union(&b_tokens).count();

    // union is at least 1 because both sets are non-empty.
    intersection as f32 / union as f32
}

/// Collect and normalize all tokens from a slice of `CascadeAnchor` texts.
///
/// Each anchor's `text` field is case-folded (Unicode case folding -- the
/// first lowercase code point produced by `char::to_lowercase()` is used),
/// non-(alphanumeric-or-whitespace) characters are replaced with a space,
/// then the result is split on whitespace. Empty tokens produced by repeated
/// whitespace are discarded. Returns a `HashSet` of unique token strings.
fn anchor_token_set(anchors: &[CascadeAnchor]) -> HashSet<String> {
    anchors
        .iter()
        .flat_map(|anchor| {
            // Normalize: Unicode case fold, replace punctuation with space, split.
            let normalized: String = anchor
                .text
                .chars()
                .map(|c| {
                    if c.is_alphanumeric() {
                        // Use Unicode case folding: take the first lowercase
                        // code point produced by to_lowercase(). Falls back to
                        // the original char if the iterator is unexpectedly empty
                        // (which the Unicode spec does not permit, but is safe).
                        c.to_lowercase().next().unwrap_or(c)
                    } else {
                        ' '
                    }
                })
                .collect();
            normalized
                .split_whitespace()
                .map(|t| t.to_string())
                .collect::<Vec<_>>()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::patterns::PatternSet;
    use crate::persona::{CascadeAnchor, Persona, Voice};
    use crate::rules::{Layer, Rule, RuleSet};
    use crate::skills::{Skill, SkillSet};

    /// Construct a minimal `PersonaSource` with a given voice tone, rules,
    /// skills, and cascade anchors for test convenience.
    fn make_source(
        tone: &str,
        rules: Vec<Rule>,
        skills: Vec<Skill>,
        cascade_anchors: Vec<CascadeAnchor>,
    ) -> PersonaSource {
        PersonaSource {
            persona: Persona {
                schema_version: 1,
                name: "test".to_string(),
                version: None,
                description: None,
                tags: vec![],
                license: None,
                author: None,
                extends: None,
                mixin: vec![],
                voice: Voice {
                    tone: tone.to_string(),
                    text: None,
                    questions: vec![],
                },
                anchor: std::collections::BTreeMap::new(),
                classification_tiers: vec![],
                conflict_resolution: None,
                cascade_anchors,
                self_eval: vec![],
                ambiguity_questions: vec![],
                safety_layer: None,
                growth: None,
                references: vec![],
                capability_manifest: None,
                conformance: None,
                default_questions: vec![],
            },
            rules: RuleSet { rules },
            skills: SkillSet { skills },
            patterns: PatternSet::default(),
        }
    }

    /// Helper to build a `Rule` with the given id and text, using Layer::L1.
    fn rule(id: &str, text: &str) -> Rule {
        Rule {
            id: id.to_string(),
            layer: Layer::L1,
            text: text.to_string(),
            reasoning: None,
            override_inherited: false,
        }
    }

    /// Helper to build a `Skill` with the given id and invoke_when.
    fn skill(id: &str, invoke_when: &str) -> Skill {
        Skill {
            id: id.to_string(),
            invoke_when: invoke_when.to_string(),
            mandatory: false,
        }
    }

    /// Helper to build a `CascadeAnchor` with a given text.
    fn anchor(text: &str) -> CascadeAnchor {
        CascadeAnchor {
            position: "top".to_string(),
            text: text.to_string(),
        }
    }

    // --- Identical inputs ---

    /// Diffing identical empty sources produces an empty diff with
    /// anchor_similarity = Some(1.0).
    #[test]
    fn identical_empty_sources_no_diff() {
        let a = make_source("voice", vec![], vec![], vec![]);
        let b = make_source("voice", vec![], vec![], vec![]);
        let d = diff(&a, &b);
        assert!(d.added_rules.is_empty(), "added_rules should be empty");
        assert!(d.removed_rules.is_empty(), "removed_rules should be empty");
        assert!(
            d.modified_rules.is_empty(),
            "modified_rules should be empty"
        );
        assert!(d.added_skills.is_empty(), "added_skills should be empty");
        assert!(
            d.removed_skills.is_empty(),
            "removed_skills should be empty"
        );
        assert!(!d.voice_changed, "voice_changed should be false");
        assert_eq!(d.anchor_similarity, Some(1.0), "empty anchors => 1.0");
    }

    /// Diffing two identical non-empty sources produces an empty diff.
    #[test]
    fn identical_nonempty_sources_no_diff() {
        let rules = vec![rule("r1", "text"), rule("r2", "text2")];
        let skills = vec![skill("s1", "always")];
        let anchors = vec![anchor("API first, ownership second")];
        let a = make_source("precise", rules.clone(), skills.clone(), anchors.clone());
        let b = make_source("precise", rules, skills, anchors);
        let d = diff(&a, &b);
        assert!(d.added_rules.is_empty());
        assert!(d.removed_rules.is_empty());
        assert!(d.modified_rules.is_empty());
        assert!(d.added_skills.is_empty());
        assert!(d.removed_skills.is_empty());
        assert!(!d.voice_changed);
        assert_eq!(d.anchor_similarity, Some(1.0));
    }

    // --- Rule addition ---

    /// A rule id in b not in a appears in added_rules.
    #[test]
    fn rule_added() {
        let a = make_source("v", vec![], vec![], vec![]);
        let b = make_source("v", vec![rule("new-rule", "text")], vec![], vec![]);
        let d = diff(&a, &b);
        assert_eq!(d.added_rules, vec!["new-rule"]);
        assert!(d.removed_rules.is_empty());
        assert!(d.modified_rules.is_empty());
    }

    // --- Rule removal ---

    /// A rule id in a not in b appears in removed_rules.
    #[test]
    fn rule_removed() {
        let a = make_source("v", vec![rule("old-rule", "text")], vec![], vec![]);
        let b = make_source("v", vec![], vec![], vec![]);
        let d = diff(&a, &b);
        assert!(d.added_rules.is_empty());
        assert_eq!(d.removed_rules, vec!["old-rule"]);
        assert!(d.modified_rules.is_empty());
    }

    // --- Rule modification ---

    /// A rule id present in both sides with different text appears in
    /// modified_rules and not in added or removed.
    #[test]
    fn rule_modified() {
        let a = make_source("v", vec![rule("r1", "original text")], vec![], vec![]);
        let b = make_source("v", vec![rule("r1", "updated text")], vec![], vec![]);
        let d = diff(&a, &b);
        assert!(d.added_rules.is_empty());
        assert!(d.removed_rules.is_empty());
        assert_eq!(d.modified_rules, vec!["r1"]);
    }

    /// Changing only the Layer on an existing rule counts as modified.
    #[test]
    fn rule_modified_layer_change() {
        let mut r_a = rule("r1", "text");
        let mut r_b = rule("r1", "text");
        r_a.layer = Layer::L1;
        r_b.layer = Layer::L2;
        let a = make_source("v", vec![r_a], vec![], vec![]);
        let b = make_source("v", vec![r_b], vec![], vec![]);
        let d = diff(&a, &b);
        assert_eq!(d.modified_rules, vec!["r1"]);
        assert!(d.added_rules.is_empty());
        assert!(d.removed_rules.is_empty());
    }

    // --- Skill addition/removal ---

    /// A skill id in b not in a appears in added_skills.
    #[test]
    fn skill_added() {
        let a = make_source("v", vec![], vec![], vec![]);
        let b = make_source("v", vec![], vec![skill("new-skill", "always")], vec![]);
        let d = diff(&a, &b);
        assert_eq!(d.added_skills, vec!["new-skill"]);
        assert!(d.removed_skills.is_empty());
    }

    /// A skill id in a not in b appears in removed_skills.
    #[test]
    fn skill_removed() {
        let a = make_source("v", vec![], vec![skill("old-skill", "always")], vec![]);
        let b = make_source("v", vec![], vec![], vec![]);
        let d = diff(&a, &b);
        assert!(d.added_skills.is_empty());
        assert_eq!(d.removed_skills, vec!["old-skill"]);
    }

    /// A skill with the same id but changed content appears in both
    /// removed_skills and added_skills (no modified_skills slot exists).
    #[test]
    fn skill_content_changed_appears_in_both_slots() {
        let a = make_source("v", vec![], vec![skill("s1", "original trigger")], vec![]);
        let b = make_source("v", vec![], vec![skill("s1", "new trigger")], vec![]);
        let d = diff(&a, &b);
        assert!(
            d.added_skills.contains(&"s1".to_string()),
            "s1 should be in added_skills"
        );
        assert!(
            d.removed_skills.contains(&"s1".to_string()),
            "s1 should be in removed_skills"
        );
    }

    // --- Voice comparison ---

    /// voice_changed is true when voice tone differs.
    #[test]
    fn voice_changed_on_tone_difference() {
        let a = make_source("precise", vec![], vec![], vec![]);
        let b = make_source("casual", vec![], vec![], vec![]);
        let d = diff(&a, &b);
        assert!(d.voice_changed);
    }

    /// voice_changed is false when voice is identical.
    #[test]
    fn voice_unchanged_when_identical() {
        let a = make_source("precise", vec![], vec![], vec![]);
        let b = make_source("precise", vec![], vec![], vec![]);
        let d = diff(&a, &b);
        assert!(!d.voice_changed);
    }

    // --- Anchor similarity ---

    /// Identical non-empty anchor token sets yield similarity 1.0.
    #[test]
    fn anchor_similarity_identical_anchors() {
        let anchors = vec![anchor("api first ownership second implementation last")];
        let a = make_source("v", vec![], vec![], anchors.clone());
        let b = make_source("v", vec![], vec![], anchors);
        let d = diff(&a, &b);
        assert_eq!(d.anchor_similarity, Some(1.0));
    }

    /// Both sides have empty cascade_anchors => similarity 1.0.
    #[test]
    fn anchor_similarity_both_empty() {
        let a = make_source("v", vec![], vec![], vec![]);
        let b = make_source("v", vec![], vec![], vec![]);
        let d = diff(&a, &b);
        assert_eq!(d.anchor_similarity, Some(1.0));
    }

    /// One side empty, other non-empty => similarity 0.0.
    #[test]
    fn anchor_similarity_one_empty() {
        let a = make_source("v", vec![], vec![], vec![]);
        let b = make_source("v", vec![], vec![], vec![anchor("some anchor text")]);
        let d = diff(&a, &b);
        assert_eq!(d.anchor_similarity, Some(0.0));

        // Symmetric.
        let d2 = diff(&b, &a);
        assert_eq!(d2.anchor_similarity, Some(0.0));
    }

    /// Disjoint token sets => similarity 0.0.
    #[test]
    fn anchor_similarity_disjoint() {
        let a = make_source("v", vec![], vec![], vec![anchor("alpha beta gamma")]);
        let b = make_source("v", vec![], vec![], vec![anchor("delta epsilon zeta")]);
        let d = diff(&a, &b);
        assert_eq!(d.anchor_similarity, Some(0.0));
    }

    /// Partial overlap produces a value in (0.0, 1.0).
    #[test]
    fn anchor_similarity_partial_overlap() {
        // a tokens: {api, first, ownership}  b tokens: {api, first, speed}
        // intersection = {api, first} = 2, union = {api, first, ownership, speed} = 4
        // Jaccard = 2/4 = 0.5
        let a = make_source("v", vec![], vec![], vec![anchor("api first ownership")]);
        let b = make_source("v", vec![], vec![], vec![anchor("api first speed")]);
        let d = diff(&a, &b);
        let sim = d.anchor_similarity.expect("anchor_similarity must be Some");
        // Allow small float epsilon.
        assert!((sim - 0.5).abs() < 1e-5, "expected ~0.5, got {sim}");
    }

    /// Anchor normalization strips punctuation and is case-insensitive.
    #[test]
    fn anchor_similarity_normalization() {
        // These two anchors should produce identical token sets after
        // normalization: "API-first!" => "apifirst" -- wait, stripping
        // non-alphanumeric chars turns "API-first!" into "API first " then
        // lowercased to "api first". Let's verify with tokens that only differ
        // by case and punctuation.
        let a = make_source("v", vec![], vec![], vec![anchor("API-First! Ownership.")]);
        let b = make_source("v", vec![], vec![], vec![anchor("api first ownership")]);
        let d = diff(&a, &b);
        assert_eq!(
            d.anchor_similarity,
            Some(1.0),
            "normalization should make these identical"
        );
    }

    /// Unicode case folding: accented-uppercase anchors tokenize identically to
    /// their lowercase forms.  `Ärger` and `ärger` must produce the same token
    /// set; `über` and `uber` share the `uber` token after folding `ü` -> `u`.
    ///
    /// Note: `Ü.to_lowercase() = ü` (not `u`), so `über` and `ÜBER` are
    /// identical after folding but `über` and `uber` are distinct tokens.
    /// This test confirms that multi-byte characters are folded, not silently
    /// dropped or left uppercase.
    #[test]
    fn anchor_similarity_unicode_case_folding() {
        // `Ärger` and `ärger` -- only differ in case of first letter.
        // After Unicode folding both become `ärger`.
        let a = make_source("v", vec![], vec![], vec![anchor("Ärger")]);
        let b = make_source("v", vec![], vec![], vec![anchor("ärger")]);
        let d = diff(&a, &b);
        assert_eq!(
            d.anchor_similarity,
            Some(1.0),
            "`Ärger` and `ärger` should fold to the same token"
        );

        // `über` and `ÜBER` should both produce token `über` after folding.
        let c = make_source("v", vec![], vec![], vec![anchor("über")]);
        let d2 = make_source("v", vec![], vec![], vec![anchor("ÜBER")]);
        let diff2 = diff(&c, &d2);
        assert_eq!(
            diff2.anchor_similarity,
            Some(1.0),
            "`über` and `ÜBER` should fold to the same token"
        );
    }

    /// Existing JSON roundtrip test is preserved and still passes.
    #[test]
    fn semantic_diff_json_roundtrip() {
        let d = SemanticDiff {
            added_rules: vec!["new-rule".to_string()],
            removed_rules: vec!["old-rule".to_string()],
            modified_rules: vec!["tweaked".to_string()],
            added_skills: vec!["s1".to_string()],
            removed_skills: vec![],
            voice_changed: true,
            anchor_similarity: Some(0.87),
        };

        let serialized = serde_json::to_string(&d).unwrap();
        let parsed: SemanticDiff = serde_json::from_str(&serialized).unwrap();
        assert_eq!(parsed, d);
    }

    /// Existing default-is-empty test is preserved and still passes.
    #[test]
    fn default_semantic_diff_is_empty() {
        let d = SemanticDiff::default();
        assert!(d.added_rules.is_empty());
        assert!(d.removed_rules.is_empty());
        assert!(!d.voice_changed);
        assert!(d.anchor_similarity.is_none());
    }

    // --- All-fields-different ---

    /// When every rule, skill, and voice field changes, all diff slots are
    /// populated.
    #[test]
    fn all_fields_different() {
        let a = make_source(
            "formal",
            vec![rule("r-old", "old text")],
            vec![skill("s-old", "old trigger")],
            vec![anchor("alpha beta")],
        );
        let b = make_source(
            "casual",
            vec![rule("r-new", "new text")],
            vec![skill("s-new", "new trigger")],
            vec![anchor("delta epsilon")],
        );
        let d = diff(&a, &b);
        assert_eq!(d.added_rules, vec!["r-new"]);
        assert_eq!(d.removed_rules, vec!["r-old"]);
        assert!(d.modified_rules.is_empty());
        assert_eq!(d.added_skills, vec!["s-new"]);
        assert_eq!(d.removed_skills, vec!["s-old"]);
        assert!(d.voice_changed);
        assert_eq!(d.anchor_similarity, Some(0.0));
    }

    /// Output vecs are sorted lexicographically (deterministic regardless of
    /// HashMap iteration order).
    #[test]
    fn output_is_sorted() {
        let a = make_source(
            "v",
            vec![rule("z-rule", "t"), rule("a-rule", "t")],
            vec![skill("z-skill", "t"), skill("a-skill", "t")],
            vec![],
        );
        let b = make_source("v", vec![], vec![], vec![]);
        let d = diff(&a, &b);
        assert_eq!(d.removed_rules, vec!["a-rule", "z-rule"]);
        assert_eq!(d.removed_skills, vec!["a-skill", "z-skill"]);
    }
}
