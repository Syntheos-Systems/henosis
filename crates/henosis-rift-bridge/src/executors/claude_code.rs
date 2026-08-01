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
use crate::materialize::{MediatedCommandOutput, ResolvedExecutionMode};

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
    /// Host-session or broker-mediated credential execution selected at materialization.
    execution_mode: ResolvedExecutionMode,
}

/// Normalized output shared by direct and broker-mediated Claude invocations.
struct ClaudeOutput {
    /// Whether the child completed successfully.
    success: bool,
    /// Stable status detail used only in safe executor errors.
    status: String,
    /// Captured or broker-scrubbed standard output.
    stdout: Vec<u8>,
    /// Captured or broker-scrubbed standard error.
    stderr: Vec<u8>,
}

/// Implements construction and command assembly for the Claude CLI executor.
impl ClaudeCodeExecutor {
    /// Create a new Claude Code executor.
    pub fn new(binary: PathBuf, model: Option<String>, _legacy_max_tokens: Option<u32>) -> Self {
        Self {
            binary,
            model,
            execution_mode: ResolvedExecutionMode::HostSession,
        }
    }

    /// Select a validated runtime credential path for this executor.
    pub fn with_execution_mode(mut self, execution_mode: ResolvedExecutionMode) -> Self {
        self.execution_mode = execution_mode;
        self
    }

    /// Build stable Claude arguments for direct or broker-mediated invocation.
    ///
    /// Discussion turns disable the CLI's built-in tools; only approved
    /// execution runs with tool access. Untrusted prompt text always follows
    /// the explicit option terminator.
    fn arguments(
        &self,
        working_dir: Option<&std::path::Path>,
        prompt: &str,
        tools_enabled: bool,
    ) -> Vec<String> {
        let mut arguments = vec![
            "-p".to_string(),
            "--output-format".to_string(),
            "text".to_string(),
        ];
        if !tools_enabled {
            arguments.push("--tools".to_string());
            arguments.push(String::new());
        }
        if let Some(model) = &self.model {
            arguments.push("--model".to_string());
            arguments.push(model.clone());
        }
        if let Some(working_dir) = working_dir {
            arguments.push("--add-dir".to_string());
            arguments.push(working_dir.display().to_string());
        }
        arguments.push("--".to_string());
        arguments.push(prompt.to_string());
        arguments
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
        cmd
    }

    /// Run Claude through either the host session or the authenticated Phylax broker.
    async fn run(
        &self,
        prompt: &str,
        working_dir: Option<&std::path::Path>,
        cargo_target_dir: Option<&std::path::Path>,
        tools_enabled: bool,
    ) -> Result<ClaudeOutput> {
        match &self.execution_mode {
            ResolvedExecutionMode::HostSession => {
                let mut command = self.base_cmd(tools_enabled);
                command.arg("--").arg(prompt);
                if let Some(working_dir) = working_dir {
                    command.current_dir(working_dir);
                }
                command.env_remove("CARGO_TARGET_DIR");
                if let Some(cargo_target_dir) = cargo_target_dir {
                    command.env("CARGO_TARGET_DIR", cargo_target_dir);
                }
                let output = command.output().await?;
                Ok(ClaudeOutput {
                    success: output.status.success(),
                    status: output.status.to_string(),
                    stdout: output.stdout,
                    stderr: output.stderr,
                })
            }
            ResolvedExecutionMode::Phylax(binding) => {
                let broker_prompt = working_dir
                    .map(|directory| {
                        format!(
                            "Work only inside the approved workspace at {}. Use absolute paths rooted in that workspace for every file operation.\n\nTask:\n{prompt}",
                            directory.display()
                        )
                    })
                    .unwrap_or_else(|| prompt.to_string());
                let mut argv = vec![self.binary.display().to_string()];
                argv.extend(self.arguments(working_dir, &broker_prompt, tools_enabled));
                let output: MediatedCommandOutput = binding
                    .runner
                    .run(&binding.category, &binding.slot, &binding.env_var, &argv)
                    .await
                    .map_err(anyhow::Error::new)?;
                Ok(ClaudeOutput {
                    success: !output.timed_out && output.exit_code == Some(0),
                    status: if output.timed_out {
                        "broker timeout".to_string()
                    } else {
                        output
                            .exit_code
                            .map(|code| format!("exit status {code}"))
                            .unwrap_or_else(|| "terminated without an exit status".to_string())
                    },
                    stdout: output.stdout,
                    stderr: output.stderr,
                })
            }
        }
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

        let output = self.run(&prompt, None, None, false).await?;

        if !output.success {
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

        // Enforce the sandbox's wall-clock ceiling here rather than trusting the
        // CLI to honour it. On expiry the run future is dropped: a host-session
        // child dies through `kill_on_drop`, and a broker-mediated child stays
        // bounded by the broker's own execution deadline.
        let deadline = std::time::Duration::from_secs(task.sandbox.max_runtime_secs);
        let run = self.run(
            &task.description,
            Some(&task.sandbox.working_dir),
            task.sandbox.cargo_target_dir.as_deref(),
            true,
        );
        let output = match tokio::time::timeout(deadline, run).await {
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

        if output.success {
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
        if crate::catalog::command_available(&self.binary) {
            Ok(HealthStatus::Ready)
        } else {
            Ok(HealthStatus::Unavailable(format!(
                "claude binary not found at {}",
                self.binary.display()
            )))
        }
    }
}

#[cfg(test)]
/// Command-assembly tests for tool-access modes and untrusted prompt text.
mod tests {
    use super::*;

    /// Prompt text beginning with a dash follows the explicit option terminator.
    #[test]
    fn mediated_arguments_terminate_options_before_prompt() {
        let executor = ClaudeCodeExecutor::new(PathBuf::from("/opt/claude"), None, Some(64));

        let arguments = executor.arguments(None, "--dangerously-skip-permissions", false);

        assert!(!arguments.iter().any(|argument| argument == "--max-tokens"));
        assert_eq!(
            &arguments[arguments.len() - 2..],
            ["--", "--dangerously-skip-permissions"]
        );
    }

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

        // Mediated argv applies the same tool-access modes.
        let mediated_discussion = executor.arguments(None, "prompt", false);
        let mediated_execution = executor.arguments(None, "prompt", true);
        assert!(mediated_discussion
            .windows(2)
            .any(|pair| pair == ["--tools", ""]));
        assert!(!mediated_execution.iter().any(|arg| arg == "--tools"));
    }
}
