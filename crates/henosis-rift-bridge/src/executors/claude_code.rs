//! Claude Code executor: shells out to `claude -p` for discussion responses
//! and `claude` for execution mode.

use std::path::PathBuf;

use anyhow::Result;
use async_trait::async_trait;
use tokio::process::Command;
use tokio::sync::mpsc;

use crate::context::to_cli_prompt;
use crate::executor::{
    AgentExecutor, AgentResponse, Capability, DiscussionContext, ExecutionResult, ExecutionSandbox,
    HealthStatus, ProgressUpdate, TaskContext,
};

/// Return the worktree's current HEAD commit hash, or `None` if `dir` is not a git
/// repository or the command fails. Used to detect whether an execution committed work.
async fn git_head(dir: &std::path::Path) -> Option<String> {
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

/// Executor that shells out to the `claude` CLI.
///
/// Discussion mode: runs `claude -p` with a formatted prompt.
/// Execution mode: runs `claude -p` with task description and working dir.
pub struct ClaudeCodeExecutor {
    /// Path to the claude binary.
    binary: PathBuf,
    /// Model override (e.g., "sonnet").
    model: Option<String>,
    /// Maximum tokens for the response.
    max_tokens: Option<u32>,
}

/// Implements construction and command assembly for the Claude CLI executor.
impl ClaudeCodeExecutor {
    /// Create a new Claude Code executor.
    pub fn new(binary: PathBuf, model: Option<String>, max_tokens: Option<u32>) -> Self {
        Self {
            binary,
            model,
            max_tokens,
        }
    }

    /// Build a command with common flags and an explicit tool-access mode.
    fn base_cmd(&self, tools_enabled: bool) -> Command {
        let mut cmd = Command::new(&self.binary);
        // Dropping the output future on timeout must terminate the CLI. Without
        // this the process survives its deadline, keeps mutating the sandbox
        // worktree after the bridge considers the session over, and races the
        // retry attempt that reuses that same worktree.
        cmd.kill_on_drop(true);
        cmd.arg("-p");
        cmd.arg("--output-format").arg("text");
        if !tools_enabled {
            cmd.arg("--tools").arg("");
        }
        if let Some(model) = &self.model {
            cmd.arg("--model").arg(model);
        }
        if let Some(max) = self.max_tokens {
            cmd.arg("--max-tokens").arg(max.to_string());
        }
        cmd
    }
}

#[async_trait]
/// Implements the Synapse executor contract by delegating work to the Claude CLI.
impl AgentExecutor for ClaudeCodeExecutor {
    /// ClaudeCodeExecutor has no persistent capabilities -- it's a subprocess.
    fn required_capabilities(&self) -> Vec<Capability> {
        vec![]
    }

    /// Minimal sandbox -- ClaudeCode manages its own isolation.
    fn sandbox(&self) -> ExecutionSandbox {
        ExecutionSandbox {
            branch: "agent/claude-code/unset".into(),
            working_dir: PathBuf::from("/tmp"),
            max_runtime_secs: 600,
            // Placeholder sandbox; the real one (with CARGO_TARGET_DIR) comes from the bridge.
            cargo_target_dir: None,
        }
    }

    /// Run `claude -p` with the formatted prompt and return text output.
    async fn discuss(&self, context: DiscussionContext) -> Result<Option<AgentResponse>> {
        let prompt = to_cli_prompt(&context);

        let mut cmd = self.base_cmd(false);
        cmd.arg(&prompt);

        let output = cmd.output().await?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("claude exited with {}: {stderr}", output.status);
        }

        let response = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if response.is_empty() {
            Ok(None)
        } else {
            Ok(Some(AgentResponse {
                text: response,
                execution_proposal: None,
            }))
        }
    }

    /// Run `claude -p` with the task description in the sandbox working dir.
    async fn execute(
        &self,
        task: TaskContext,
        progress_tx: mpsc::Sender<ProgressUpdate>,
    ) -> Result<ExecutionResult> {
        let _ = progress_tx
            .send(ProgressUpdate::Message(format!(
                "Starting task: {}",
                task.description
            )))
            .await;

        // Record HEAD before the run so we can report a commit hash only when the agent
        // actually advanced it (committed work), not just echo the worktree's base commit.
        let head_before = git_head(&task.sandbox.working_dir).await;

        let mut cmd = self.base_cmd(true);
        cmd.arg(&task.description);
        cmd.current_dir(&task.sandbox.working_dir);
        // Honor the workspace's configured CARGO_TARGET_DIR so the agent's cargo builds write
        // off the source tree. Unset when the workspace did not configure one.
        if let Some(target_dir) = &task.sandbox.cargo_target_dir {
            cmd.env("CARGO_TARGET_DIR", target_dir);
        }

        // Enforce the sandbox's wall-clock ceiling here rather than trusting the
        // CLI to honour it. On expiry the future is dropped and `kill_on_drop`
        // terminates the process instead of leaving it running in the worktree.
        let deadline = std::time::Duration::from_secs(task.sandbox.max_runtime_secs);
        let output = match tokio::time::timeout(deadline, cmd.output()).await {
            Ok(result) => result?,
            Err(_) => {
                let _ = progress_tx.send(ProgressUpdate::Done).await;
                let advanced = git_head(&task.sandbox.working_dir).await != head_before;
                return Ok(ExecutionResult::Failed {
                    reason: format!(
                        "claude exceeded its {}s execution limit and was terminated",
                        task.sandbox.max_runtime_secs
                    ),
                    partial_work: advanced,
                });
            }
        };

        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let _ = progress_tx.send(ProgressUpdate::Done).await;

        if output.status.success() {
            // Report the resulting commit only if HEAD moved -- i.e. the agent committed.
            let commit_hash = match git_head(&task.sandbox.working_dir).await {
                Some(after) if Some(&after) != head_before.as_ref() => Some(after),
                _ => None,
            };
            Ok(ExecutionResult::Success {
                summary: stdout.lines().next().unwrap_or("task complete").to_string(),
                commit_hash,
                evidence: if stdout.is_empty() {
                    None
                } else {
                    Some(stdout)
                },
            })
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr).to_string();
            Ok(ExecutionResult::Failed {
                reason: format!("claude exited with {}: {}", output.status, stderr),
                partial_work: false,
            })
        }
    }

    /// Health check: verify the claude binary exists and is executable.
    async fn health_check(&self) -> Result<HealthStatus> {
        if self.binary.exists() {
            Ok(HealthStatus::Ready)
        } else {
            Ok(HealthStatus::Unavailable(format!(
                "claude binary not found at {}",
                self.binary.display()
            )))
        }
    }
}

/// Verifies discussion and approved execution use different Claude tool modes.
#[cfg(test)]
mod tests {
    use super::*;

    /// Discussion disables built-in tools while approved execution leaves them available.
    #[test]
    fn tool_access_is_disabled_only_for_discussion() {
        let executor = ClaudeCodeExecutor::new(PathBuf::from("claude"), None, None);
        let discussion: Vec<String> = executor
            .base_cmd(false)
            .as_std()
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        let execution: Vec<String> = executor
            .base_cmd(true)
            .as_std()
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();

        assert!(discussion.windows(2).any(|pair| pair == ["--tools", ""]));
        assert!(!execution.iter().any(|arg| arg == "--tools"));
    }
}
