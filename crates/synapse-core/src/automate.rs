//! Bridge to the `frameshift` CLI's automate mode.
//!
//! The operator's persona engine already implements ranking and per-project state.
//! Synapse does not reinvent any of it; this module is a thin wrapper around:
//!
//! - `frameshift automate status` -- learn whether automate is on for the
//!   current project and which persona is active.
//! - `frameshift select --library <root> --task <text>` -- rank installed
//!   personas against a task summary. Output is a fixed-width table whose
//!   first data row is the top pick.
//!
//! Activation itself does NOT go through `frameshift-activate.sh` -- that
//! script targets Claude Code's hook surface (writes a per-session marker
//! that hooks re-inject on restart). Synapse loads the persona directly via
//! `persona::load_by_name` instead, which is the right shape for an
//! in-process agent harness.
//!
//! ## Confidence floor
//!
//! `frameshift select` ranks every installed persona, so the "top pick" is
//! always defined even when no persona truly fits. The wrapper enforces a
//! configurable score floor (`min_score`) and tie-margin (`min_margin` over
//! the runner-up) before declaring a winner. Below the floor the caller
//! sees `Pick::None` and falls back to whatever was previously active.

use anyhow::{Context, Result};
use std::path::PathBuf;
use std::process::Command;

/// Result of a single `frameshift select` invocation. `name` is the
/// directory name (matches `persona::load_by_name`). Score and margin are
/// surfaced so the CLI can show a rationale line to the user.
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
    /// One-line rationale lifted from the `frameshift select` rationale
    /// column for surface in `/persona` log lines.
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

/// Call `frameshift select` and return the top pick, gated by the
/// confidence floor in `opts`. Returns `Ok(None)` when no persona meets
/// the floor or when the CLI is unavailable -- the caller falls back to
/// the current persona rather than blocking.
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

    let output = match Command::new("frameshift")
        .args(["select", "--library"])
        .arg(&library)
        .args(["--task", task])
        .output()
    {
        Ok(o) => o,
        Err(_) => return Ok(None),
    };
    if !output.status.success() {
        return Ok(None);
    }

    let text = String::from_utf8_lossy(&output.stdout);
    let rows = parse_select_output(&text);
    let top = match rows.first() {
        Some(r) => r,
        None => return Ok(None),
    };

    if top.score < opts.min_score {
        return Ok(None);
    }
    let margin = rows
        .get(1)
        .map(|r| top.score - r.score)
        .unwrap_or(top.score);
    if margin < opts.min_margin {
        return Ok(None);
    }

    Ok(Some(Pick {
        name: top.name.clone(),
        score: top.score,
        margin,
        rationale: top.rationale.clone(),
    }))
}

/// Internal shape of one parsed row from `frameshift select`.
#[derive(Debug, Clone)]
struct SelectRow {
    name: String,
    score: f64,
    rationale: String,
}

/// Parse the fixed-width table emitted by `frameshift select`. The header
/// row and dashed separator are skipped. Each remaining row collapses
/// run-of-whitespace into single delimiters via `split_whitespace`. The
/// rationale column itself contains spaces, so after consuming the first
/// three tokens (name, score, confidence) the rest is rejoined.
fn parse_select_output(text: &str) -> Vec<SelectRow> {
    let mut rows = Vec::new();
    for line in text.lines() {
        let line = line.trim_end();
        if line.is_empty() {
            continue;
        }
        // Skip the column headers and dashed separator.
        if line.starts_with("persona") || line.starts_with("----") {
            continue;
        }
        let mut iter = line.split_whitespace();
        let name = match iter.next() {
            Some(s) => s.to_string(),
            None => continue,
        };
        let score = match iter.next().and_then(|s| s.parse::<f64>().ok()) {
            Some(v) => v,
            None => continue,
        };
        // Skip the confidence column.
        let _confidence = iter.next();
        let rationale = iter.collect::<Vec<_>>().join(" ");
        rows.push(SelectRow {
            name,
            score,
            rationale,
        });
    }
    rows.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    rows
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

    /// Sample `frameshift select` output captured live for parser coverage.
    const SAMPLE: &str = "\
persona                          score confidence  rationale
--------------------------------------------------------------------------------
cryptographic                    0.494      0.253  cryptographic 0.49: languages {rust,toml}: lang_score=0.46
lab                              0.481      0.000  lab 0.48: languages {rust}
data                             0.460      0.000  data 0.46: languages {rust}
testing                          0.452      0.000  testing 0.45
architecture                     0.429      0.000  architecture 0.43";

    /// Handles `parse_select_output_top_pick` behavior.
    #[test]
    fn parse_select_output_top_pick() {
        let rows = parse_select_output(SAMPLE);
        assert!(!rows.is_empty());
        assert_eq!(rows[0].name, "cryptographic");
        assert!((rows[0].score - 0.494).abs() < 1e-9);
        assert!(rows[0].rationale.contains("languages"));
    }

    /// Handles `parse_select_output_sorted_by_score_descending` behavior.
    #[test]
    fn parse_select_output_sorted_by_score_descending() {
        let rows = parse_select_output(SAMPLE);
        for w in rows.windows(2) {
            assert!(w[0].score >= w[1].score, "{:?} >= {:?}", w[0], w[1]);
        }
    }

    /// Handles `margin_gate_rejects_thin_wins_when_configured` behavior.
    #[test]
    fn margin_gate_rejects_thin_wins_when_configured() {
        // Top two scores in SAMPLE: 0.494, 0.481 -> margin 0.013. The
        // default (zero) margin admits this; an aggressive caller that
        // requires margin >= 0.02 should reject.
        let rows = parse_select_output(SAMPLE);
        let real_margin = rows[0].score - rows[1].score;
        let aggressive = AutomateOptions {
            min_margin: 0.02,
            ..AutomateOptions::default()
        };
        assert!(real_margin < aggressive.min_margin);
        // Defaults take the top pick regardless of margin.
        let default = AutomateOptions::default();
        assert!(rows[0].score >= default.min_score);
        assert!(real_margin >= default.min_margin);
    }

    /// Handles `empty_task_returns_none` behavior.
    #[test]
    fn empty_task_returns_none() {
        let pick = select_for_task("", &AutomateOptions::default()).unwrap();
        assert!(pick.is_none());
    }
}
