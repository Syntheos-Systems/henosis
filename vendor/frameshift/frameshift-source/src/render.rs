//! Deterministic markdown renderer for `PersonaSource`.
//!
//! The primary entry point is [`render_to_markdown`], which takes a
//! [`PersonaSource`] and a [`RenderTarget`] and returns a fully-rendered
//! AGENTS.md string. Sections that have no data are omitted entirely --
//! no empty `##` headings ever appear in the output.

use std::fmt::Write;

use crate::rules::Layer;
use crate::source::PersonaSource;

/// Target agent platform for rendering.
///
/// Controls which sections are included in the output. Per-platform
/// differences are documented on each variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenderTarget {
    /// Claude Code / Claude CLI. Full output including Design Notes and all
    /// cascade anchors.
    Claude,
    /// OpenAI Codex / ChatGPT. Omits Design Notes and Safety-Layer sections.
    Codex,
    /// Google Gemini. Omits Design Notes section; keeps everything else.
    Gemini,
    /// Generic / unknown agent. Same as [`RenderTarget::Claude`] (full output).
    Generic,
}

/// Selects platform-specific sections for a render target.
impl RenderTarget {
    /// Returns `true` if the Design Notes section should be rendered for this target.
    fn include_design_notes(self) -> bool {
        matches!(self, RenderTarget::Claude | RenderTarget::Generic)
    }

    /// Returns `true` if the Safety-Layer section should be rendered for this target.
    fn include_safety_layer(self) -> bool {
        !matches!(self, RenderTarget::Codex)
    }
}

/// Renders a `PersonaSource` into a markdown string for the given target platform.
///
/// Section order is fixed and follows the canonical AGENTS.md structure.
/// Sections with no data are omitted. The output is deterministic: two calls
/// with the same arguments always return identical strings.
pub fn render_to_markdown(src: &PersonaSource, target: RenderTarget) -> String {
    let mut out = String::new();

    render_title_tagline(src, &mut out);
    render_l2_anchor(src, &mut out);
    render_operating_frame(src, &mut out);
    render_required_skills(src, &mut out);
    render_l1_rules(src, &mut out);
    render_concrete_patterns(src, &mut out);
    render_ambiguity_guidance(src, &mut out);
    render_cascade_anchor_mid(src, &mut out);
    render_conflict_resolution(src, &mut out);
    render_self_eval_hooks(src, &mut out);
    if target.include_safety_layer() {
        render_safety_layer(src, &mut out);
    }
    render_growth_integration(src, &mut out);
    render_cascade_anchor_recency(src, &mut out);
    if target.include_design_notes() {
        render_design_notes(&mut out);
    }
    render_references(src, &mut out);

    out
}

// ---------------------------------------------------------------------------
// Section renderers
// ---------------------------------------------------------------------------

/// Renders the top-level title and optional italic tagline from anchor["l2"].
fn render_title_tagline(src: &PersonaSource, out: &mut String) {
    let name = &src.persona.name;
    let _ = writeln!(out, "# AGENTS.md -- {name} Context\n");
    if let Some(anchor) = src.persona.anchor.get("l2") {
        if let Some(tagline) = &anchor.tagline {
            if !tagline.is_empty() {
                let _ = writeln!(out, "*{tagline}*\n");
            }
        }
    }
}

/// Renders the L2 Anchor section using `anchor["l2"]`.
///
/// Includes the anchor body text and the default question if present.
fn render_l2_anchor(src: &PersonaSource, out: &mut String) {
    let Some(anchor) = src.persona.anchor.get("l2") else {
        return;
    };
    if anchor.text.is_empty() {
        return;
    }

    let _ = writeln!(out, "## L2 Anchor -- Who You Are Here\n");
    let _ = writeln!(out, "{}\n", anchor.text);
    if let Some(dq) = &anchor.default_question {
        if !dq.is_empty() {
            let _ = writeln!(out, "{dq}\n");
        }
    }
}

/// Renders the Operating Frame section.
///
/// Includes voice tone, extended voice text, voice questions, default
/// questions from the persona, and classification tiers.
fn render_operating_frame(src: &PersonaSource, out: &mut String) {
    let voice = &src.persona.voice;
    let has_tone = !voice.tone.is_empty();
    let has_text = voice
        .text
        .as_deref()
        .map(|s| !s.is_empty())
        .unwrap_or(false);
    let has_questions = !voice.questions.is_empty();
    let has_default_questions = !src.persona.default_questions.is_empty();
    let has_tiers = !src.persona.classification_tiers.is_empty();

    if !has_tone && !has_text && !has_questions && !has_default_questions && !has_tiers {
        return;
    }

    let _ = writeln!(out, "## Operating Frame\n");

    if has_tone {
        let _ = writeln!(out, "{}\n", voice.tone);
    }
    if let Some(text) = &voice.text {
        if !text.is_empty() {
            let _ = writeln!(out, "{text}\n");
        }
    }
    if has_questions {
        for q in &voice.questions {
            let _ = writeln!(out, "- {}", q.text);
        }
        out.push('\n');
    }
    if has_default_questions {
        for dq in &src.persona.default_questions {
            let _ = writeln!(out, "- {}", dq.question);
        }
        out.push('\n');
    }
    if has_tiers {
        // The classification tiers header uses the first tier name as a hint for
        // labeling. Fall back to "level" when tiers have no usable name.
        let _ = writeln!(out, "**Classify every request by enforcement level:**");
        for tier in &src.persona.classification_tiers {
            let _ = write!(out, "- **{}** -- {}", tier.name, tier.description);
            if let Some(guidance) = &tier.guidance {
                let _ = write!(out, ". {guidance}");
            }
            out.push('\n');
        }
        out.push('\n');
    }
}

/// Renders the Required Skills section as a markdown table.
fn render_required_skills(src: &PersonaSource, out: &mut String) {
    if src.skills.skills.is_empty() {
        return;
    }

    let _ = writeln!(out, "## Required Skills\n");
    let _ = writeln!(out, "| Skill | Invoke when |");
    let _ = writeln!(out, "|---|---|");
    for skill in &src.skills.skills {
        let _ = writeln!(out, "| `{}` | {} |", skill.id, skill.invoke_when);
    }
    out.push('\n');
}

/// Renders the L1 Rules section as a bulleted list.
///
/// Each rule is rendered as `- {text}`. If a rule has a reasoning field it
/// is appended in parentheses: `- {text} ({reasoning})`.
fn render_l1_rules(src: &PersonaSource, out: &mut String) {
    let l1: Vec<_> = src
        .rules
        .rules
        .iter()
        .filter(|r| matches!(r.layer, Layer::L1))
        .collect();

    if l1.is_empty() {
        return;
    }

    let _ = writeln!(out, "## L1 Rules -- Hard Constraints\n");
    for r in l1 {
        if let Some(reasoning) = &r.reasoning {
            let _ = writeln!(out, "- {} ({})", r.text, reasoning);
        } else {
            let _ = writeln!(out, "- {}", r.text);
        }
    }
    out.push('\n');
}

/// Renders the Concrete Patterns section.
///
/// Includes stack categories, anti-patterns, and code examples. The section
/// is omitted entirely if all three are empty.
fn render_concrete_patterns(src: &PersonaSource, out: &mut String) {
    let p = &src.patterns;
    if p.stack.is_empty() && p.antipatterns.is_empty() && p.examples.is_empty() {
        return;
    }

    let _ = writeln!(out, "## Concrete Patterns\n");

    for cat in &p.stack {
        let _ = writeln!(out, "### {}\n", cat.category);
        for item in &cat.items {
            let _ = writeln!(out, "- {item}");
        }
        out.push('\n');
    }

    if !p.antipatterns.is_empty() {
        let _ = writeln!(out, "### Anti-patterns (do NOT use)\n");
        for ap in &p.antipatterns {
            if let Some(instead) = &ap.use_instead {
                let _ = writeln!(out, "- Do NOT {}. {}", ap.text, instead);
            } else {
                let _ = writeln!(out, "- Do NOT {}.", ap.text);
            }
        }
        out.push('\n');
    }

    for ex in &p.examples {
        // Sanitize language tag: allow only [a-zA-Z0-9_+-] to prevent
        // fence-break injection via a crafted language field. An empty
        // result renders as a bare ``` fence.
        let safe_lang: String = ex
            .language
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '+' | '-'))
            .collect();

        let _ = writeln!(out, "### {}\n", ex.title);
        let _ = writeln!(out, "{}\n", ex.context);
        let _ = writeln!(out, "**Bad:**");
        let _ = writeln!(out, "```{safe_lang}");
        let _ = writeln!(out, "{}", ex.bad);
        let _ = writeln!(out, "```\n");
        let _ = writeln!(out, "**Good:**");
        let _ = writeln!(out, "```{safe_lang}");
        let _ = writeln!(out, "{}", ex.good);
        let _ = writeln!(out, "```\n");
    }
}

/// Renders the "When the Context Is Unclear" ambiguity guidance section.
fn render_ambiguity_guidance(src: &PersonaSource, out: &mut String) {
    if src.persona.ambiguity_questions.is_empty() {
        return;
    }

    let name = &src.persona.name;
    let _ = writeln!(out, "## When the {name} Is Unclear\n");
    for q in &src.persona.ambiguity_questions {
        let _ = writeln!(out, "- {}", q.text);
    }
    out.push('\n');
}

/// Renders the mid-document cascade anchor, if one with `position = "mid"` exists.
fn render_cascade_anchor_mid(src: &PersonaSource, out: &mut String) {
    let Some(anchor) = src
        .persona
        .cascade_anchors
        .iter()
        .find(|a| a.position == "mid")
    else {
        return;
    };

    let _ = writeln!(out, "## Cascade Anchor (Mid-Document)\n");
    let _ = writeln!(out, "**Re-anchor:** {}\n", anchor.text);
}

/// Renders the Conflict Resolution section.
///
/// Includes the high-level stance as a blockquote followed by unpacked aspects.
fn render_conflict_resolution(src: &PersonaSource, out: &mut String) {
    let Some(cr) = &src.persona.conflict_resolution else {
        return;
    };

    let _ = writeln!(out, "## Conflict Resolution (Semantic Frame)\n");
    let _ = writeln!(out, "> **{}**\n", cr.stance);

    if !cr.aspects.is_empty() {
        let _ = writeln!(out, "Unpacked:\n");
        for aspect in &cr.aspects {
            let _ = writeln!(out, "- **{}** -- {}", aspect.key, aspect.text);
        }
        out.push('\n');
    }
}

/// Renders the Self-Evaluation Hooks section as a numbered list.
fn render_self_eval_hooks(src: &PersonaSource, out: &mut String) {
    if src.persona.self_eval.is_empty() {
        return;
    }

    let _ = writeln!(out, "## Self-Evaluation Hooks\n");
    for (i, step) in src.persona.self_eval.iter().enumerate() {
        let _ = writeln!(out, "{}. {}", i + 1, step.step);
    }
    out.push('\n');
}

/// Renders the Safety-Layer Awareness section.
///
/// Only rendered when the source has a `safety_layer` and the target permits it.
fn render_safety_layer(src: &PersonaSource, out: &mut String) {
    let Some(sl) = &src.persona.safety_layer else {
        return;
    };
    if sl.text.is_empty() {
        return;
    }

    let _ = writeln!(out, "## Safety-Layer Awareness\n");
    let _ = writeln!(out, "{}\n", sl.text);
}

/// Renders the Growth Integration section.
fn render_growth_integration(src: &PersonaSource, out: &mut String) {
    let Some(growth) = &src.persona.growth else {
        return;
    };

    let _ = writeln!(out, "## Growth Integration\n");
    let _ = writeln!(
        out,
        "- **Session start:** Read `./GROWTH.md` before the first prompt."
    );
    let _ = writeln!(
        out,
        "- **During session:** Append observations as they surface."
    );
    let _ = writeln!(
        out,
        "- **Session end:** Note what shifted in understanding."
    );
    let _ = writeln!(
        out,
        "- **Memory dual-write:** Send findings via `$MEMORY_CLI store`. Tags: `{}`. Source: `{}`.",
        growth.dual_write_tags, growth.dual_write_source
    );
    out.push('\n');
}

/// Renders the recency-position cascade anchor, if one with `position = "recency"` exists.
fn render_cascade_anchor_recency(src: &PersonaSource, out: &mut String) {
    let Some(anchor) = src
        .persona
        .cascade_anchors
        .iter()
        .find(|a| a.position == "recency")
    else {
        return;
    };

    let _ = writeln!(out, "## Cascade Anchor (Recency)\n");
    let _ = writeln!(out, "**{}**\n", anchor.text);
}

/// Renders the Design Notes section for editors.
///
/// Contains a static reference to the Schubert rendering pipeline. Omitted
/// for Codex and Gemini targets.
fn render_design_notes(out: &mut String) {
    let _ = writeln!(out, "## Design Notes (For Editors)\n");
    let _ = writeln!(
        out,
        "This file is generated by the Schubert rendering pipeline from structured TOML source.\n"
    );
    let _ = writeln!(
        out,
        "Edit the source files (`persona.toml`, `rules.toml`, `skills.toml`, `patterns.toml`)\
 -- do not hand-edit this file directly.\n"
    );
}

/// Renders the References section grouped by category.
fn render_references(src: &PersonaSource, out: &mut String) {
    if src.persona.references.is_empty() {
        return;
    }

    let _ = writeln!(out, "## References\n");
    for group in &src.persona.references {
        let _ = writeln!(out, "### {}\n", group.category);
        for entry in &group.entries {
            let _ = writeln!(out, "- {entry}");
        }
        out.push('\n');
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::patterns::{AntiPattern, CodeExample, PatternSet, StackCategory};
    use crate::persona::{
        AmbiguityQuestion, Anchor, Aspect, CascadeAnchor, ClassificationTier, ConflictResolution,
        DefaultQuestion, GrowthConfig, Persona, ReferenceGroup, SafetyLayer, SelfEvalStep, Voice,
        VoiceQuestion,
    };
    use crate::rules::{Rule, RuleSet};
    use crate::skills::{Skill, SkillSet};
    use std::collections::BTreeMap;

    /// Builds a fully-populated `PersonaSource` that exercises every section.
    fn full_source() -> PersonaSource {
        let mut anchor = BTreeMap::new();
        anchor.insert(
            "l2".to_string(),
            Anchor {
                text: "You are a correctness practitioner.".to_string(),
                tagline: Some("specification-first".to_string()),
                default_question: Some("Which spec governs this?".to_string()),
            },
        );

        PersonaSource {
            persona: Persona {
                schema_version: 1,
                name: "demo".to_string(),
                version: None,
                description: None,
                tags: vec![],
                license: None,
                author: None,
                extends: None,
                mixin: vec![],
                voice: Voice {
                    tone: "calm and precise".to_string(),
                    text: Some("Extended voice prose.".to_string()),
                    questions: vec![VoiceQuestion {
                        text: "Am I being precise?".to_string(),
                    }],
                },
                anchor,
                classification_tiers: vec![ClassificationTier {
                    name: "L1".to_string(),
                    description: "Non-negotiable invariants.".to_string(),
                    guidance: Some("Never override.".to_string()),
                }],
                conflict_resolution: Some(ConflictResolution {
                    stance: "L1 beats L2 beats L3".to_string(),
                    aspects: vec![Aspect {
                        key: "safety".to_string(),
                        text: "Safety rules always win.".to_string(),
                    }],
                }),
                cascade_anchors: vec![
                    CascadeAnchor {
                        position: "mid".to_string(),
                        text: "You are still a correctness practitioner.".to_string(),
                    },
                    CascadeAnchor {
                        position: "recency".to_string(),
                        text: "Correctness over speed.".to_string(),
                    },
                ],
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
                    dual_write_tags: "demo,test".to_string(),
                    dual_write_source: "persona:demo".to_string(),
                }),
                references: vec![ReferenceGroup {
                    category: "specifications".to_string(),
                    entries: vec!["RFC 8446".to_string()],
                }],
                capability_manifest: None,
                conformance: None,
                default_questions: vec![DefaultQuestion {
                    question: "What is the invariant here?".to_string(),
                }],
            },
            rules: RuleSet {
                rules: vec![Rule {
                    id: "no-panic".to_string(),
                    layer: crate::rules::Layer::L1,
                    text: "Never call unwrap() in library code.".to_string(),
                    reasoning: Some("Panics crash the host process.".to_string()),
                    override_inherited: false,
                }],
            },
            skills: SkillSet {
                skills: vec![Skill {
                    id: "code-review".to_string(),
                    invoke_when: "on all pull requests".to_string(),
                    mandatory: false,
                }],
            },
            patterns: PatternSet {
                schema_version: 1,
                stack: vec![StackCategory {
                    category: "signing".to_string(),
                    items: vec!["ed25519-dalek 2.x".to_string()],
                }],
                antipatterns: vec![AntiPattern {
                    id: "no-openssl".to_string(),
                    text: "use OpenSSL".to_string(),
                    use_instead: Some("Use RustCrypto.".to_string()),
                    reasoning: None,
                }],
                examples: vec![CodeExample {
                    id: "ct-compare".to_string(),
                    title: "Constant-time comparison".to_string(),
                    context: "Comparing MACs or signatures".to_string(),
                    language: "rust".to_string(),
                    bad: "if mac == expected { Ok(()) }".to_string(),
                    good: "if mac.ct_eq(&expected).into() { Ok(()) }".to_string(),
                }],
                patterns: vec![],
            },
        }
    }

    /// Verifies that a full render for Claude target includes all 15 sections
    /// in the correct order, and that code examples use fenced blocks and
    /// the skills table is formatted correctly.
    #[test]
    fn full_render_all_sections_in_order() {
        let src = full_source();
        let out = render_to_markdown(&src, RenderTarget::Claude);

        // All section headers must be present.
        let title_pos = out.find("# AGENTS.md").expect("title");
        let l2_pos = out.find("## L2 Anchor").expect("l2 anchor");
        let frame_pos = out.find("## Operating Frame").expect("operating frame");
        let skills_pos = out.find("## Required Skills").expect("required skills");
        let l1_pos = out.find("## L1 Rules").expect("l1 rules");
        let patterns_pos = out.find("## Concrete Patterns").expect("concrete patterns");
        let ambig_pos = out.find("## When the demo").expect("ambiguity");
        let mid_pos = out.find("## Cascade Anchor (Mid").expect("cascade mid");
        let conflict_pos = out
            .find("## Conflict Resolution")
            .expect("conflict resolution");
        let eval_pos = out.find("## Self-Evaluation Hooks").expect("self eval");
        let safety_pos = out.find("## Safety-Layer Awareness").expect("safety layer");
        let growth_pos = out.find("## Growth Integration").expect("growth");
        let recency_pos = out
            .find("## Cascade Anchor (Recency)")
            .expect("cascade recency");
        let design_pos = out.find("## Design Notes").expect("design notes");
        let refs_pos = out.find("## References").expect("references");

        // Verify order.
        assert!(title_pos < l2_pos, "title before l2 anchor");
        assert!(l2_pos < frame_pos, "l2 anchor before operating frame");
        assert!(frame_pos < skills_pos, "operating frame before skills");
        assert!(skills_pos < l1_pos, "skills before l1 rules");
        assert!(l1_pos < patterns_pos, "l1 rules before concrete patterns");
        assert!(
            patterns_pos < ambig_pos,
            "concrete patterns before ambiguity"
        );
        assert!(ambig_pos < mid_pos, "ambiguity before cascade mid");
        assert!(
            mid_pos < conflict_pos,
            "cascade mid before conflict resolution"
        );
        assert!(
            conflict_pos < eval_pos,
            "conflict resolution before self eval"
        );
        assert!(eval_pos < safety_pos, "self eval before safety layer");
        assert!(safety_pos < growth_pos, "safety layer before growth");
        assert!(growth_pos < recency_pos, "growth before cascade recency");
        assert!(
            recency_pos < design_pos,
            "cascade recency before design notes"
        );
        assert!(design_pos < refs_pos, "design notes before references");

        // Code example fenced blocks.
        assert!(out.contains("```rust"), "fenced code block with language");
        assert!(out.contains("**Bad:**"), "bad label");
        assert!(out.contains("**Good:**"), "good label");

        // Skills table format.
        assert!(
            out.contains("| Skill | Invoke when |"),
            "skills table header"
        );
        assert!(out.contains("|---|---|"), "skills table separator");
        assert!(
            out.contains("| `code-review` | on all pull requests |"),
            "skills table row"
        );

        // L1 rule with reasoning in parentheses.
        assert!(
            out.contains("Never call unwrap() in library code. (Panics crash the host process.)"),
            "l1 rule with reasoning"
        );

        // Tagline rendered as italic.
        assert!(out.contains("*specification-first*"), "tagline italic");

        // Cascade anchor mid uses Re-anchor prefix.
        assert!(
            out.contains("**Re-anchor:**"),
            "cascade mid re-anchor prefix"
        );
    }

    /// Verifies that a minimal source (just name + non-empty voice tone)
    /// produces no empty section headers.
    #[test]
    fn empty_sections_omitted() {
        let src = PersonaSource::new(Persona::new("empty"));
        let out = render_to_markdown(&src, RenderTarget::Generic);

        // No ## headers should appear for sections with no data.
        assert!(!out.contains("## L2 Anchor"), "no l2 anchor");
        assert!(!out.contains("## Operating Frame"), "no operating frame");
        assert!(!out.contains("## Required Skills"), "no skills");
        assert!(!out.contains("## L1 Rules"), "no l1 rules");
        assert!(!out.contains("## Concrete Patterns"), "no patterns");
        assert!(!out.contains("## When the"), "no ambiguity");
        assert!(!out.contains("## Cascade Anchor"), "no cascade anchors");
        assert!(!out.contains("## Conflict Resolution"), "no conflict");
        assert!(!out.contains("## Self-Evaluation Hooks"), "no self eval");
        assert!(!out.contains("## Safety-Layer Awareness"), "no safety");
        assert!(!out.contains("## Growth Integration"), "no growth");
        assert!(!out.contains("## References"), "no references");
    }

    /// Verifies per-target differences: Design Notes absent for Codex, present for Claude.
    #[test]
    fn per_target_design_notes() {
        let src = full_source();

        let codex_out = render_to_markdown(&src, RenderTarget::Codex);
        assert!(
            !codex_out.contains("## Design Notes"),
            "design notes must be absent for Codex"
        );

        let gemini_out = render_to_markdown(&src, RenderTarget::Gemini);
        assert!(
            !gemini_out.contains("## Design Notes"),
            "design notes must be absent for Gemini"
        );

        let claude_out = render_to_markdown(&src, RenderTarget::Claude);
        assert!(
            claude_out.contains("## Design Notes"),
            "design notes must be present for Claude"
        );

        let generic_out = render_to_markdown(&src, RenderTarget::Generic);
        assert!(
            generic_out.contains("## Design Notes"),
            "design notes must be present for Generic"
        );
    }

    /// Verifies per-target differences: Safety-Layer absent for Codex only.
    #[test]
    fn per_target_safety_layer() {
        let src = full_source();

        let codex_out = render_to_markdown(&src, RenderTarget::Codex);
        assert!(
            !codex_out.contains("## Safety-Layer Awareness"),
            "safety layer must be absent for Codex"
        );

        let gemini_out = render_to_markdown(&src, RenderTarget::Gemini);
        assert!(
            gemini_out.contains("## Safety-Layer Awareness"),
            "safety layer must be present for Gemini"
        );

        let claude_out = render_to_markdown(&src, RenderTarget::Claude);
        assert!(
            claude_out.contains("## Safety-Layer Awareness"),
            "safety layer must be present for Claude"
        );
    }

    /// Verifies that two renders of the same source produce identical output.
    #[test]
    fn render_is_deterministic() {
        let src = full_source();
        let first = render_to_markdown(&src, RenderTarget::Claude);
        let second = render_to_markdown(&src, RenderTarget::Claude);
        assert_eq!(first, second, "render must be deterministic");
    }

    /// Updated legacy test: deterministic render and correct section ordering
    /// using the new signature with `RenderTarget::Generic`.
    #[test]
    fn render_is_deterministic_and_ordered() {
        let mut anchor = BTreeMap::new();
        anchor.insert(
            "l2".to_string(),
            Anchor {
                text: "anchor body".to_string(),
                tagline: None,
                default_question: None,
            },
        );

        let src = PersonaSource {
            persona: Persona {
                schema_version: 1,
                name: "demo".to_string(),
                version: None,
                description: None,
                tags: vec![],
                license: None,
                author: None,
                extends: None,
                mixin: vec![],
                voice: Voice {
                    tone: "calm and precise".to_string(),
                    text: None,
                    questions: vec![],
                },
                anchor,
                classification_tiers: vec![],
                conflict_resolution: None,
                cascade_anchors: vec![],
                self_eval: vec![],
                ambiguity_questions: vec![],
                safety_layer: None,
                growth: None,
                references: vec![],
                capability_manifest: None,
                conformance: None,
                default_questions: vec![DefaultQuestion {
                    question: "why?".to_string(),
                }],
            },
            rules: RuleSet {
                rules: vec![
                    Rule {
                        id: "l1a".to_string(),
                        layer: crate::rules::Layer::L1,
                        text: "do not panic".to_string(),
                        reasoning: None,
                        override_inherited: false,
                    },
                    Rule {
                        id: "l3a".to_string(),
                        layer: crate::rules::Layer::L3,
                        text: "prefer brevity".to_string(),
                        reasoning: None,
                        override_inherited: false,
                    },
                ],
            },
            skills: SkillSet {
                skills: vec![Skill {
                    id: "review".to_string(),
                    invoke_when: "on pull requests".to_string(),
                    mandatory: false,
                }],
            },
            patterns: PatternSet::default(),
        };

        let first = render_to_markdown(&src, RenderTarget::Generic);
        let second = render_to_markdown(&src, RenderTarget::Generic);
        assert_eq!(first, second, "render must be deterministic");

        // Verify key sections appear and are in the right relative order.
        let l2_pos = first.find("## L2 Anchor").expect("l2 anchor");
        let frame_pos = first.find("## Operating Frame").expect("operating frame");
        let skills_pos = first.find("## Required Skills").expect("skills");
        let l1_pos = first.find("## L1 Rules").expect("l1 rules");

        assert!(l2_pos < frame_pos, "l2 before operating frame");
        assert!(frame_pos < skills_pos, "operating frame before skills");
        assert!(skills_pos < l1_pos, "skills before l1 rules");
    }

    /// Updated legacy test: empty source produces no section headers.
    #[test]
    fn render_omits_empty_sections() {
        let src = PersonaSource::new(Persona::new("empty"));
        let out = render_to_markdown(&src, RenderTarget::Generic);
        assert!(!out.contains("## Operating Frame"));
        assert!(!out.contains("## L1 Rules"));
        assert!(!out.contains("## Required Skills"));
    }
}
