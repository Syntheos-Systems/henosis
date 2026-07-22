//! In-process bridge to Frameshift's automate-mode persona ranking.
//!
//! The operator's persona engine already implements ranking and per-project state.
//! Synapse does not reinvent any of it; this module is a thin wrapper around:
//!
//! - In-process [`frameshift_orchestrator::select`] -- rank installed personas
//!   against a task summary. The top-scored persona is the pick, gated by a
//!   confidence floor. This is the same ranking the `frameshift select` CLI
//!   performs, called directly instead of via a subprocess (Story 4.3).
//! - `frameshift automate status` (subprocess) -- learn whether automate is on
//!   for the current project and which persona is active. This reads
//!   Frameshift's per-project automate-mode state, whose on-disk path is
//!   resolved by Frameshift's own client crate (not the orchestrator lib), so
//!   it stays a thin, graceful CLI probe rather than an in-process call. It is
//!   mode *config*, not the persona load/selection path.
//!
//! Activation itself does NOT go through `frameshift-activate.sh` -- that
//! script targets Claude Code's hook surface (writes a per-session marker
//! that hooks re-inject on restart). Synapse loads the persona directly via
//! `persona::load_by_name` instead, which is the right shape for an
//! in-process agent harness.
//!
//! ## Confidence floor
//!
//! `select` ranks every installed persona, so the "top pick" is always defined
//! even when no persona truly fits. The wrapper enforces a configurable score
//! floor (`min_score`) and tie-margin (`min_margin` over the runner-up) before
//! declaring a winner. Below the floor the caller sees `None` and falls back to
//! whatever was previously active.

use anyhow::{Context, Result};
use std::path::PathBuf;
use std::process::Command;

use frameshift_orchestrator::feedback::Preferences;
use frameshift_orchestrator::policy::{PolicyWeights, Scored};
use frameshift_orchestrator::run::{SelectionInputs, select};

/// Result of a single persona ranking. `name` is the persona's canonical name
/// (matches `persona::load_by_name`). Score and margin are surfaced so the CLI
/// can show a rationale line to the user.
#[derive(Debug, Clone)]
pub struct Pick {
    /// Persona name of the top pick. Equal to the directory name under
    /// `~/.local/share/frameshift/personas-private/`.
    pub name: String,
    /// Combined score from the ranker. Higher is better; range roughly 0..1.
    pub score: f64,
    /// Gap between the top pick and the runner-up. Used as a tie-break
    /// signal -- a thin margin means a swap probably is not worth the
    /// context cost.
    pub margin: f64,
    /// One-line rationale lifted from the ranker's rationale field for surface
    /// in `/persona` log lines.
    pub rationale: String,
}

/// Knobs for the wrapper. Defaults match the conservative thresholds the operator
/// uses in the `/automate` skill: take the top pick only when its score is
/// meaningfully above the runner-up.
#[derive(Debug, Clone)]
pub struct AutomateOptions {
    /// Override the personas library root. Defaults to
    /// `~/.local/share/frameshift/personas-private`.
    pub library: Option<PathBuf>,
    /// Below this score the wrapper refuses to pick anything. 0.30 catches
    /// the case where every persona scores low because the task is
    /// off-topic from the catalog.
    pub min_score: f64,
    /// Below this margin over the runner-up the wrapper holds (returns
    /// the current persona instead of swapping).
    pub min_margin: f64,
}

/// Implements `Default` behavior for `AutomateOptions`.
impl Default for AutomateOptions {
    /// Handles `default` behavior.
    fn default() -> Self {
        Self {
            library: None,
            min_score: 0.30,
            // the operator's /automate skill takes the top pick by default
            // and only asks when truly ambiguous. The caller's
            // "skip when pick == current persona" check prevents
            // per-turn churn, so margin defaults to zero -- a tied
            // top pick is still a pick. Raise this in callers that
            // want to require a clearer winner before swapping.
            min_margin: 0.0,
        }
    }
}

/// Snapshot of `frameshift automate status` output.
#[derive(Debug, Clone)]
pub struct AutomateStatus {
    /// True when automate mode is enabled for the current project. False
    /// when the engine reports "mode: off" or when the CLI is missing.
    pub on: bool,
    /// Persona name reported as currently active. May or may not be the
    /// one synapse actually loaded -- the engine's notion of "active" is
    /// the last value sent through `frameshift use`. Useful as a default
    /// when automate is off but a previous run pinned a persona.
    pub active: Option<String>,
}

/// Query `frameshift automate status`. Returns `on: false` if the CLI is
/// unavailable -- a fresh install without frameshift should not crash
/// synapse, it should silently behave as if automate were off.
///
/// This reads Frameshift's per-project automate-mode state, whose on-disk path
/// is computed by Frameshift's client crate (project-root -> state-dir hashing),
/// not exposed by the orchestrator lib. It stays a subprocess probe by design;
/// it is mode config, not the persona load/selection path that Story 4.3 moved
/// in-process.
pub fn status() -> AutomateStatus {
    let output = match Command::new("frameshift")
        .args(["automate", "status"])
        .output()
    {
        Ok(o) => o,
        Err(_) => {
            return AutomateStatus {
                on: false,
                active: None,
            };
        }
    };
    if !output.status.success() {
        return AutomateStatus {
            on: false,
            active: None,
        };
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut on = false;
    let mut active: Option<String> = None;
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("mode:") {
            on = rest.trim() == "on";
        } else if let Some(rest) = line.strip_prefix("active persona:") {
            let name = rest.trim();
            if !name.is_empty() && name != "(none)" && name != "none" {
                active = Some(name.to_string());
            }
        }
    }
    AutomateStatus { on, active }
}

/// Rank installed personas against `task` in-process and return the top pick,
/// gated by the confidence floor in `opts`.
///
/// Returns `Ok(None)` when the task is empty, the library is absent, the ranker
/// finds no personas, or no persona meets the floor -- the caller falls back to
/// the current persona rather than blocking. Errors only on a missing library
/// path that cannot be defaulted.
pub fn select_for_task(task: &str, opts: &AutomateOptions) -> Result<Option<Pick>> {
    let task = task.trim();
    if task.is_empty() {
        return Ok(None);
    }
    let library = opts
        .library
        .clone()
        .or_else(default_library)
        .context("no personas library available")?;
    if !library.exists() {
        return Ok(None);
    }

    // Context is sensed from the current working directory (the project synapse
    // is operating in), matching the prior `frameshift select` CLI semantics
    // which had no explicit project-root flag. Fall back to the library dir if
    // the cwd is unavailable. The catalog to rank is always the library.
    let project_root = std::env::current_dir().unwrap_or_else(|_| library.clone());
    let inputs = SelectionInputs {
        project_root: project_root.as_path(),
        task_hint: Some(task),
        source_dirs: Vec::new(),
        catalog_root: Some(library),
        prefs: Preferences::default(),
        weights: PolicyWeights::default(),
    };

    // A ranking failure (e.g. unreadable catalog) degrades to "no pick" rather
    // than propagating -- the caller keeps the current persona.
    let ranked = match select(&inputs) {
        Ok(r) => r,
        Err(_) => return Ok(None),
    };

    Ok(top_pick(&ranked, opts))
}

/// Pure gating helper: pick the top-ranked persona if it clears the score floor
/// and tie-margin, mapping it to a [`Pick`]. `ranked` is expected sorted
/// descending by score (as `frameshift_orchestrator::select` returns it).
///
/// Decoupled from the orchestrator call so the floor/margin logic is unit
/// testable on synthetic rankings without building a real persona catalog.
fn top_pick(ranked: &[Scored], opts: &AutomateOptions) -> Option<Pick> {
    let top = ranked.first()?;
    let score = top.score as f64;
    if score < opts.min_score {
        return None;
    }
    let margin = ranked
        .get(1)
        .map(|second| score - second.score as f64)
        .unwrap_or(score);
    if margin < opts.min_margin {
        return None;
    }
    Some(Pick {
        name: top.persona.clone(),
        score,
        margin,
        rationale: top.rationale.clone(),
    })
}

/// Default personas library when the caller doesn't override.
fn default_library() -> Option<PathBuf> {
    dirs::home_dir().map(|h| {
        h.join(".local")
            .join("share")
            .join("frameshift")
            .join("personas-private")
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use frameshift_orchestrator::policy::ScoreComponents;
    use std::fs;
    use tempfile::TempDir;

    /// Build a synthetic `Scored` for gating-logic tests.
    fn scored(persona: &str, score: f32) -> Scored {
        Scored {
            persona: persona.to_string(),
            score,
            confidence: 0.0,
            rationale: format!("{persona} {score}"),
            // Every field is listed explicitly rather than spread from a default: this struct
            // comes from the out-of-workspace `frameshift-orchestrator` path dependency, which
            // has now grown fields twice (`context`, then `semantic`). It derives no `Default`,
            // so an exhaustive literal is the only option -- and it makes the next upstream
            // field a compile error here, which is exactly the signal we want.
            components: ScoreComponents {
                language: 0.0,
                lexical: 0.0,
                intent: 0.0,
                capability: 0.0,
                context: 0.0,
                semantic: 0.0,
            },
        }
    }

    /// The top pick is the highest-scored persona above the floor.
    #[test]
    fn top_pick_takes_highest() {
        let ranked = vec![scored("cryptographic", 0.49), scored("lab", 0.48)];
        let pick = top_pick(&ranked, &AutomateOptions::default()).expect("a pick");
        assert_eq!(pick.name, "cryptographic");
        assert!((pick.score - 0.49).abs() < 1e-6);
        assert!((pick.margin - 0.01).abs() < 1e-6);
    }

    /// A top pick below the score floor yields no pick.
    #[test]
    fn top_pick_below_floor_is_none() {
        let ranked = vec![scored("lab", 0.20), scored("data", 0.10)];
        assert!(top_pick(&ranked, &AutomateOptions::default()).is_none());
    }

    /// An aggressive margin requirement rejects a thin win.
    #[test]
    fn top_pick_margin_gate_rejects_thin_win() {
        let ranked = vec![scored("crypto", 0.49), scored("lab", 0.48)];
        let aggressive = AutomateOptions {
            min_margin: 0.02,
            ..AutomateOptions::default()
        };
        assert!(top_pick(&ranked, &aggressive).is_none());
        // Defaults (zero margin) admit the same thin win.
        assert!(top_pick(&ranked, &AutomateOptions::default()).is_some());
    }

    /// A single candidate uses its own score as the margin.
    #[test]
    fn top_pick_single_candidate_margin_is_score() {
        let ranked = vec![scored("solo", 0.55)];
        let pick = top_pick(&ranked, &AutomateOptions::default()).expect("a pick");
        assert!((pick.margin - 0.55).abs() < 1e-6);
    }

    /// An empty task returns no pick before touching the catalog.
    #[test]
    fn empty_task_returns_none() {
        let pick = select_for_task("", &AutomateOptions::default()).unwrap();
        assert!(pick.is_none());
    }

    /// A missing library directory returns no pick (not an error).
    #[test]
    fn missing_library_returns_none() {
        let opts = AutomateOptions {
            library: Some(PathBuf::from("/nonexistent/persona/library")),
            ..AutomateOptions::default()
        };
        let pick = select_for_task("anything", &opts).unwrap();
        assert!(pick.is_none());
    }

    /// End-to-end in-process ranking over a fixture catalog: the task tokens
    /// strongly match one persona's keywords, so it wins. Proves the subprocess
    /// is gone and `frameshift_orchestrator::select` drives the pick.
    #[test]
    fn in_process_select_picks_matching_persona() {
        let catalog = TempDir::new().unwrap();
        let rustacean = catalog.path().join("rustacean");
        fs::create_dir(&rustacean).unwrap();
        fs::write(
            rustacean.join("AGENTS.md"),
            "# Rustacean\n\nRust systems engineer. Expert in cargo, tokio, async \
             runtime, borrow checker, clippy, and trait objects. Writes idiomatic \
             Rust with thiserror and tracing.\n",
        )
        .unwrap();
        let gardener = catalog.path().join("gardener");
        fs::create_dir(&gardener).unwrap();
        fs::write(
            gardener.join("AGENTS.md"),
            "# Gardener\n\nHorticulture specialist. Knows soil, compost, pruning, \
             watering schedules, perennials, and greenhouse climate control.\n",
        )
        .unwrap();

        let opts = AutomateOptions {
            library: Some(catalog.path().to_path_buf()),
            // Drive the decision purely by ranking; the fixture guarantees a
            // clear lexical winner for the task below.
            min_score: 0.0,
            min_margin: 0.0,
        };
        let pick = select_for_task(
            "optimize the rust tokio async runtime and fix cargo clippy warnings",
            &opts,
        )
        .unwrap()
        .expect("a persona should be picked from the fixture catalog");
        assert_eq!(
            pick.name, "rustacean",
            "the rust-heavy task must rank the rustacean persona first"
        );
    }
}
