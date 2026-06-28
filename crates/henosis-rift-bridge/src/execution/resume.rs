//! Crash-recovery resume support for the execution supervisor.
//!
//! When a supervised attempt fails leaving partial work on the worktree, the
//! supervisor retries the SAME task once and hands the next attempt a summary of
//! what the previous one left behind. This module owns the two pure decisions
//! that drive that path -- whether a result warrants a retry (`should_retry`)
//! and how the prior-work summary is phrased (`format_resume_context`) -- plus
//! the thin git IO that captures the worktree HEAD and lists the partial
//! commits. The pure decisions are unit-tested; the git IO follows the untested
//! thin-shell pattern used elsewhere (sandbox create, the ClaudeCode `git_head`).

use std::path::Path;

use tokio::process::Command;

use crate::executor::ExecutionResult;

/// Decide whether a finished attempt should be retried.
///
/// Retry only when the attempt FAILED leaving partial work AND the attempt
/// budget is not yet spent. A clean failure (no partial work) and any success
/// never retry, and the budget is spent once `attempt` reaches `max_attempts`.
pub fn should_retry(result: &ExecutionResult, attempt: u32, max_attempts: u32) -> bool {
    matches!(
        result,
        ExecutionResult::Failed {
            partial_work: true,
            ..
        }
    ) && attempt < max_attempts
}

/// Build the `prior_context` summary handed to the retry attempt.
///
/// Names the failure reason and, when the previous attempt committed partial
/// work, lists those commits so the retry can inspect and continue them. With no
/// commits (e.g. a synapse-native attempt that never commits) the list is
/// omitted and the summary says so, so the text degrades cleanly.
pub fn format_resume_context(reason: &str, commits: &[String]) -> String {
    let mut out = String::from("[prior context]\n");
    out.push_str(&format!("A previous attempt failed: {reason}\n"));
    if commits.is_empty() {
        out.push_str("Partial work may exist in the worktree, but no commits were recorded.\n");
    } else {
        out.push_str("Partial work may exist in the worktree. Commits so far:\n");
        for commit in commits {
            out.push_str(&format!("  {commit}\n"));
        }
    }
    out.push_str(
        "Inspect git status and git log, then continue from where the previous attempt left off.\n",
    );
    out.push_str("[/prior context]");
    out
}

/// Return the worktree's current HEAD commit hash, or `None` on any git error.
///
/// Captured before the first attempt so `collect_partial_commits` can list only
/// the commits a failed attempt added on top of it. Mirrors the thin git helper
/// in the ClaudeCode executor; best-effort, never blocks execution.
pub async fn git_head(dir: &Path) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .arg("rev-parse")
        .arg("HEAD")
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let hash = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!hash.is_empty()).then_some(hash)
}

/// List the commits an attempt added on top of `base` (`git log --oneline
/// base..HEAD`).
///
/// Best-effort: returns an empty vec on any git error or when `dir` is not a
/// repository, so a missing or broken worktree never blocks the retry.
pub async fn collect_partial_commits(dir: &Path, base: &str) -> Vec<String> {
    let output = match Command::new("git")
        .arg("-C")
        .arg(dir)
        .arg("log")
        .arg("--oneline")
        .arg(format!("{base}..HEAD"))
        .output()
        .await
    {
        Ok(o) if o.status.success() => o,
        _ => return Vec::new(),
    };
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{format_resume_context, should_retry};
    use crate::executor::ExecutionResult;

    /// A failure that left partial work on the branch.
    fn partial_fail() -> ExecutionResult {
        ExecutionResult::Failed {
            reason: "boom".into(),
            partial_work: true,
        }
    }

    /// A failure that left nothing behind.
    fn clean_fail() -> ExecutionResult {
        ExecutionResult::Failed {
            reason: "config invalid".into(),
            partial_work: false,
        }
    }

    /// A successful result.
    fn success() -> ExecutionResult {
        ExecutionResult::Success {
            summary: "done".into(),
            commit_hash: None,
            evidence: None,
        }
    }

    /// Partial-work failure under budget is the one case that retries.
    #[test]
    fn should_retry_true_on_partial_failure_under_budget() {
        assert!(should_retry(&partial_fail(), 1, 2));
    }

    /// A clean failure is terminal even with budget remaining.
    #[test]
    fn should_retry_false_on_clean_failure() {
        assert!(!should_retry(&clean_fail(), 1, 2));
    }

    /// Success never retries.
    #[test]
    fn should_retry_false_on_success() {
        assert!(!should_retry(&success(), 1, 2));
    }

    /// A spent budget stops the retry even on partial-work failure.
    #[test]
    fn should_retry_false_when_budget_exhausted() {
        assert!(!should_retry(&partial_fail(), 2, 2));
    }

    /// With commits, the summary names the reason and lists each commit.
    #[test]
    fn format_resume_context_lists_commits() {
        let commits = vec!["abc123 first".to_string(), "def456 second".to_string()];
        let ctx = format_resume_context("attempt blew up", &commits);
        assert!(ctx.contains("[prior context]"));
        assert!(ctx.contains("A previous attempt failed: attempt blew up"));
        assert!(ctx.contains("Commits so far:"));
        assert!(ctx.contains("  abc123 first"));
        assert!(ctx.contains("  def456 second"));
        assert!(ctx.contains("[/prior context]"));
    }

    /// With no commits, the commit list is omitted and the summary says so.
    #[test]
    fn format_resume_context_omits_commit_list_when_empty() {
        let ctx = format_resume_context("timed out after 30s", &[]);
        assert!(ctx.contains("A previous attempt failed: timed out after 30s"));
        assert!(ctx.contains("no commits were recorded"));
        assert!(!ctx.contains("Commits so far"));
    }
}
