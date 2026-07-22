//! Shared selection helper: single entry point used by CLI, MCP, and daemon.
//!
//! All three surfaces call `select` with a `SelectionInputs` struct so that
//! parity is structural rather than duplicated across callers.
//!
//! # SwitchController is intentionally NOT used here
//!
//! `select()` returns raw ranked results without applying the
//! `SwitchController` hysteresis policy (debounce, switch margin, minimum
//! confidence). This is by design:
//!
//! - **CLI `select` / MCP `frameshift_select`**: read-only ranking for user
//!   inspection. The user decides what to do with the results -- no side effects.
//! - **CLI `use` / MCP `frameshift_use`**: explicit user override that bypasses
//!   automation policy. These actions feed into `Preferences` as learned bias.
//! - **Daemon `evaluate_and_apply`**: the only caller that applies
//!   `SwitchController` on top of `select()` results, because it acts
//!   autonomously without a human in the loop.
//!
//! If you need policy-gated selection outside the daemon, construct a
//! `SwitchController` and call `controller.decide(&ranked)` on the results.

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::context::sense;
use crate::embed::Embedder;
use crate::error::OrchestratorError;
use crate::feedback::Preferences;
use crate::index::PersonaIndex;
use crate::intent::Intent;
use crate::policy::{rank_with_embedder, PolicyWeights, Scored};

/// All inputs required to run a persona selection pass.
pub struct SelectionInputs<'a> {
    /// The project root directory used for context sensing.
    pub project_root: &'a Path,

    /// Optional task hint text to steer lexical scoring.
    pub task_hint: Option<&'a str>,

    /// List of persona source directories to index.
    /// Each must contain a valid `persona.toml` or `AGENTS.md` (loaded by
    /// `PersonaIndex::from_dirs`). Ignored when `catalog_root` is set.
    pub source_dirs: Vec<PathBuf>,

    /// Optional catalog root to index instead of `source_dirs`.
    ///
    /// When set, the index is built via `PersonaIndex::from_catalog`, which
    /// enumerates immediate subdirs of the given path. `source_dirs` is ignored.
    pub catalog_root: Option<PathBuf>,

    /// Per-persona scoring bias preferences.
    pub prefs: Preferences,

    /// Scoring weight configuration.
    pub weights: PolicyWeights,
}

/// Rich selection output for JSON mode, consumed by host LLM reranking.
#[derive(Debug, Clone, Serialize)]
pub struct SelectionOutput {
    /// Sensed context for the project and task.
    pub context: ContextSnapshot,
    /// Ranked candidates with full metadata.
    pub candidates: Vec<CandidateOutput>,
}

/// Snapshot of the sensed context.
#[derive(Debug, Clone, Serialize)]
pub struct ContextSnapshot {
    /// Project directory name.
    pub project: String,
    /// Detected languages with weights.
    pub languages: std::collections::BTreeMap<String, f32>,
    /// Detected framework markers.
    pub frameworks: Vec<String>,
    /// The original task description.
    pub task: Option<String>,
    /// Classified intent from task tokens.
    pub inferred_intent: Option<Intent>,
}

/// A single candidate in the rich output.
#[derive(Debug, Clone, Serialize)]
pub struct CandidateOutput {
    /// Persona name.
    pub name: String,
    /// Blended score.
    pub score: f32,
    /// Confidence score.
    pub confidence: f32,
    /// Per-component score breakdown.
    pub components: ComponentsOutput,
    /// One-line persona description.
    pub description: Option<String>,
    /// Declared primary intents.
    pub primary_intents: Vec<Intent>,
    /// Task tokens that matched persona keywords.
    pub matched_tokens: Vec<String>,
    /// Task tokens that matched anti-keywords.
    pub anti_matched: Vec<String>,
    /// Human-readable rationale.
    pub rationale: String,
}

/// Score component breakdown for JSON output.
#[derive(Debug, Clone, Serialize)]
pub struct ComponentsOutput {
    /// Language overlap component.
    pub language: f32,
    /// Lexical overlap component.
    pub lexical: f32,
    /// Intent matching component.
    pub intent: f32,
    /// Capability heuristic component.
    pub capability: f32,
}

/// Run a full persona selection pass and return ranked results.
///
/// Senses the project context from `inputs.project_root`, builds a
/// `PersonaIndex` from `inputs.catalog_root` (when set) or `inputs.source_dirs`,
/// applies scoring weights and preferences, and returns the sorted result from
/// `policy::rank`.
///
/// Returns an empty `Vec` (not an error) when both `catalog_root` is absent
/// and `source_dirs` is empty.
pub fn select(inputs: &SelectionInputs<'_>) -> Result<Vec<Scored>, OrchestratorError> {
    select_with_embedder(inputs, None)
}

/// Run a persona selection pass using an optional semantic `embedder`.
///
/// Identical to [`select`] but threads `embedder` into the scorer so the
/// semantic-similarity channel is active when an embedder is supplied. Passing
/// `None` reproduces [`select`] exactly, so existing callers are unaffected.
pub fn select_with_embedder(
    inputs: &SelectionInputs<'_>,
    embedder: Option<&dyn Embedder>,
) -> Result<Vec<Scored>, OrchestratorError> {
    let index = if let Some(catalog_root) = &inputs.catalog_root {
        PersonaIndex::from_catalog(catalog_root)?
    } else {
        if inputs.source_dirs.is_empty() {
            return Ok(Vec::new());
        }
        PersonaIndex::from_dirs(&inputs.source_dirs)?
    };

    if index.profiles.is_empty() {
        return Ok(Vec::new());
    }

    let ctx = sense(inputs.project_root, inputs.task_hint);
    let ranked = rank_with_embedder(&ctx, &index, &inputs.weights, &inputs.prefs, embedder);
    Ok(ranked)
}

/// Run a full selection pass and return rich output for JSON serialization.
///
/// Builds the persona index (from catalog or source dirs), senses project context,
/// ranks all candidates, and assembles a `SelectionOutput` with per-candidate
/// metadata including matched/anti-matched tokens, description, and primary intents.
///
/// Returns an empty `SelectionOutput` (not an error) when both `catalog_root` is
/// absent and `source_dirs` is empty.
pub fn select_rich(inputs: &SelectionInputs<'_>) -> Result<SelectionOutput, OrchestratorError> {
    select_rich_with_embedder(inputs, None)
}

/// Run a rich selection pass using an optional semantic `embedder`.
///
/// Identical to [`select_rich`] but threads `embedder` into the scorer so the
/// semantic channel is active when one is supplied. Passing `None` reproduces
/// [`select_rich`] exactly, so existing callers are unaffected.
pub fn select_rich_with_embedder(
    inputs: &SelectionInputs<'_>,
    embedder: Option<&dyn Embedder>,
) -> Result<SelectionOutput, OrchestratorError> {
    let index = if let Some(catalog_root) = &inputs.catalog_root {
        PersonaIndex::from_catalog(catalog_root)?
    } else {
        if inputs.source_dirs.is_empty() {
            return Ok(SelectionOutput {
                context: ContextSnapshot {
                    project: String::new(),
                    languages: std::collections::BTreeMap::new(),
                    frameworks: vec![],
                    task: inputs.task_hint.map(|s| s.to_string()),
                    inferred_intent: None,
                },
                candidates: vec![],
            });
        }
        PersonaIndex::from_dirs(&inputs.source_dirs)?
    };

    let ctx = sense(inputs.project_root, inputs.task_hint);
    let ranked = rank_with_embedder(&ctx, &index, &inputs.weights, &inputs.prefs, embedder);

    let candidates = ranked
        .iter()
        .map(|s| {
            let profile = index.profiles.iter().find(|p| p.name == s.persona);
            let matched_tokens: Vec<String> = ctx
                .task_tokens
                .iter()
                .filter(|t| profile.is_some_and(|p| p.keywords.contains(*t)))
                .cloned()
                .collect();
            let anti_matched: Vec<String> = ctx
                .task_tokens
                .iter()
                .filter(|t| profile.is_some_and(|p| p.anti_keywords.contains(*t)))
                .cloned()
                .collect();

            CandidateOutput {
                name: s.persona.clone(),
                score: s.score,
                confidence: s.confidence,
                components: ComponentsOutput {
                    language: s.components.language,
                    lexical: s.components.lexical,
                    intent: s.components.intent,
                    capability: s.components.capability,
                },
                description: profile.and_then(|p| p.description.clone()),
                primary_intents: profile.map_or_else(Vec::new, |p| p.primary_intents.clone()),
                matched_tokens,
                anti_matched,
                rationale: s.rationale.clone(),
            }
        })
        .collect();

    Ok(SelectionOutput {
        context: ContextSnapshot {
            project: ctx.project_name,
            languages: ctx.languages,
            frameworks: ctx.frameworks,
            task: inputs.task_hint.map(|s| s.to_string()),
            inferred_intent: ctx.inferred_intent,
        },
        candidates,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    /// Build a minimal persona source directory suitable for indexing (persona.toml).
    fn make_persona_source(dir: &Path, name: &str) {
        fs::create_dir_all(dir).unwrap();
        let toml = format!(
            r#"schema_version = 1
name = "{name}"
version = "0.1.0"
author_handle = "test"
author_pubkey = "local-unsigned"
description = "Test persona for {name}"

[voice]
tone = "precise"
"#
        );
        fs::write(dir.join("persona.toml"), toml).unwrap();
    }

    /// Build a freeform persona dir with only AGENTS.md + pack.toml.
    fn make_freeform_persona(dir: &Path, name: &str, body: &str) {
        fs::create_dir_all(dir).unwrap();
        fs::write(
            dir.join("pack.toml"),
            format!(
                "schema_version = 1\nname = \"{name}\"\nauthor_handle = \"test\"\nauthor_pubkey = \"local-unsigned\"\nversion = \"0.1.0\"\n"
            ),
        ).unwrap();
        fs::write(dir.join("AGENTS.md"), body).unwrap();
    }

    /// select() returns one entry per installed persona source.
    #[test]
    fn select_returns_all_personas() {
        let tmp = TempDir::new().unwrap();
        let dir_a = tmp.path().join("alpha");
        let dir_b = tmp.path().join("beta");
        make_persona_source(&dir_a, "alpha");
        make_persona_source(&dir_b, "beta");

        let project = TempDir::new().unwrap();
        let inputs = SelectionInputs {
            project_root: project.path(),
            task_hint: None,
            source_dirs: vec![dir_a, dir_b],
            catalog_root: None,
            prefs: Preferences::new(),
            weights: PolicyWeights::default(),
        };

        let ranked = select(&inputs).unwrap();
        assert_eq!(ranked.len(), 2, "expected one entry per persona");
    }

    /// select() with an empty source_dirs returns an empty vec, not an error.
    #[test]
    fn select_empty_source_dirs_returns_empty() {
        let project = TempDir::new().unwrap();
        let inputs = SelectionInputs {
            project_root: project.path(),
            task_hint: None,
            source_dirs: vec![],
            catalog_root: None,
            prefs: Preferences::new(),
            weights: PolicyWeights::default(),
        };

        let ranked = select(&inputs).unwrap();
        assert!(
            ranked.is_empty(),
            "expected empty result for no source dirs"
        );
    }

    /// select() with a task hint passes tokens through to the lexical scorer.
    #[test]
    fn select_with_task_hint_does_not_error() {
        let tmp = TempDir::new().unwrap();
        let dir_a = tmp.path().join("alpha");
        make_persona_source(&dir_a, "alpha");

        let project = TempDir::new().unwrap();
        let inputs = SelectionInputs {
            project_root: project.path(),
            task_hint: Some("refactor rust module"),
            source_dirs: vec![dir_a],
            catalog_root: None,
            prefs: Preferences::new(),
            weights: PolicyWeights::default(),
        };

        let ranked = select(&inputs).unwrap();
        assert_eq!(ranked.len(), 1);
    }

    /// select() with catalog_root indexes the catalog instead of source_dirs.
    #[test]
    fn select_with_catalog_root_indexes_catalog() {
        let tmp = TempDir::new().unwrap();
        let catalog = tmp.path().join("catalog");

        make_freeform_persona(
            &catalog.join("rust"),
            "rust",
            "# Rust\n\n## L2 Anchor\n\ncargo clippy rustc ownership lifetimes\n",
        );
        make_freeform_persona(
            &catalog.join("writer"),
            "writer",
            "# Writer\n\nDocumentation, READMEs, changelogs, prose, tutorials.\n",
        );

        let project = TempDir::new().unwrap();
        let inputs = SelectionInputs {
            project_root: project.path(),
            task_hint: Some("refactor a rust clippy lint"),
            source_dirs: vec![],
            catalog_root: Some(catalog),
            prefs: Preferences::new(),
            weights: PolicyWeights::default(),
        };

        let ranked = select(&inputs).unwrap();
        assert_eq!(ranked.len(), 2, "catalog should index both personas");
        assert_eq!(
            ranked[0].persona, "rust",
            "rust should rank first for rust task"
        );
    }

    /// select_rich() produces JSON-serializable output with all required top-level keys.
    #[test]
    fn selection_output_serializes_to_json() {
        let tmp = TempDir::new().unwrap();
        let dir_a = tmp.path().join("alpha");
        make_freeform_persona(
            &dir_a,
            "alpha",
            "# Alpha\n\nRust cargo clippy. Debugging and implementation.\n",
        );

        let project = TempDir::new().unwrap();
        let inputs = SelectionInputs {
            project_root: project.path(),
            task_hint: Some("debug a rust error"),
            source_dirs: vec![dir_a],
            catalog_root: None,
            prefs: Preferences::new(),
            weights: PolicyWeights::default(),
        };

        let output = select_rich(&inputs).unwrap();
        let json = serde_json::to_string_pretty(&output).unwrap();
        assert!(json.contains("\"inferred_intent\""));
        assert!(json.contains("\"candidates\""));
        assert!(json.contains("\"context\""));
    }
}
