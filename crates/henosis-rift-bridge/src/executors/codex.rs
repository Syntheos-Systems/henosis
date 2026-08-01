//! Codex CLI executor with explicit read-only discussion and workspace-write execution modes.

use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde_json::Value;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::sync::mpsc;

use crate::context::to_cli_prompt;
use crate::executor::{
    AgentExecutor, AgentResponse, Capability, DiscussionContext, ExecutionResult, ExecutionSandbox,
    HealthStatus, ProgressUpdate, TaskContext,
};
use crate::materialize::{MediatedCommandOutput, ResolvedExecutionMode};

/// Codex sandbox selected for one CLI invocation.
#[derive(Debug, Clone, Copy)]
enum CodexSandbox {
    /// Discussion may inspect context but cannot modify the workspace.
    ReadOnly,
    /// Approved execution may modify only the selected workspace.
    WorkspaceWrite,
}

/// Converts a sandbox choice into the exact Codex CLI value.
impl CodexSandbox {
    /// Return the stable command-line spelling for this sandbox.
    fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "read-only",
            Self::WorkspaceWrite => "workspace-write",
        }
    }
}

/// Executor that invokes `codex exec` without persistent sessions or bypass flags.
pub struct CodexExecutor {
    /// Path to the Codex CLI binary.
    binary: PathBuf,
    /// Model identifier passed to every invocation.
    model: String,
    /// Optional Codex reasoning-effort override.
    reasoning_effort: Option<String>,
    /// Host-session or broker-mediated credential execution selected at materialization.
    execution_mode: ResolvedExecutionMode,
}

/// Normalized output shared by direct and broker-mediated Codex invocations.
struct CodexOutput {
    /// Whether the child completed successfully.
    success: bool,
    /// Stable status detail used only in safe executor errors.
    status: String,
    /// Captured or broker-scrubbed standard output.
    stdout: Vec<u8>,
    /// Captured or broker-scrubbed standard error.
    stderr: Vec<u8>,
}

/// Implements construction and guarded command assembly for the Codex CLI.
impl CodexExecutor {
    /// Create a Codex CLI executor with one deployment-selected model.
    pub fn new(binary: PathBuf, model: String, reasoning_effort: Option<String>) -> Self {
        Self {
            binary,
            model,
            reasoning_effort,
            execution_mode: ResolvedExecutionMode::HostSession,
        }
    }

    /// Select a validated runtime credential path for this executor.
    pub fn with_execution_mode(mut self, execution_mode: ResolvedExecutionMode) -> Self {
        self.execution_mode = execution_mode;
        self
    }

    /// Build stable arguments shared by direct and broker-mediated invocation.
    fn arguments(
        &self,
        sandbox: CodexSandbox,
        working_dir: Option<&Path>,
        prompt_argument: Option<&str>,
    ) -> Vec<String> {
        let mut arguments = vec![
            "exec".to_string(),
            "--ephemeral".to_string(),
            "--json".to_string(),
            "--model".to_string(),
            self.model.clone(),
        ];
        if let Some(working_dir) = working_dir {
            arguments.push("--cd".to_string());
            arguments.push(working_dir.display().to_string());
        }
        arguments.push("--sandbox".to_string());
        arguments.push(sandbox.as_str().to_string());
        if let Some(reasoning_effort) = &self.reasoning_effort {
            let quoted = serde_json::to_string(reasoning_effort)
                .expect("serializing a Rust string to JSON cannot fail");
            arguments.push("-c".to_string());
            arguments.push(format!("model_reasoning_effort={quoted}"));
        }
        arguments.push("--".to_string());
        arguments.push(prompt_argument.unwrap_or("-").to_string());
        arguments
    }

    /// Build an ephemeral JSONL command with an explicit sandbox and optional working directory.
    fn command(&self, sandbox: CodexSandbox, working_dir: Option<&Path>) -> Command {
        let mut command = Command::new(&self.binary);
        command.args(self.arguments(sandbox, working_dir, None));
        command
    }

    /// Run one Codex invocation with the entire prompt supplied through piped stdin.
    async fn run(
        &self,
        prompt: &str,
        sandbox: CodexSandbox,
        working_dir: Option<&Path>,
        cargo_target_dir: Option<&Path>,
    ) -> Result<CodexOutput> {
        match &self.execution_mode {
            ResolvedExecutionMode::HostSession => {
                let mut command = self.command(sandbox, working_dir);
                command
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .kill_on_drop(true);
                command.env_remove("CARGO_TARGET_DIR");
                if let Some(cargo_target_dir) = cargo_target_dir {
                    command.env("CARGO_TARGET_DIR", cargo_target_dir);
                }

                let mut child = command.spawn().with_context(|| {
                    format!("failed to start Codex CLI at {}", self.binary.display())
                })?;
                let mut stdin = child
                    .stdin
                    .take()
                    .context("Codex CLI did not expose piped stdin")?;
                stdin
                    .write_all(prompt.as_bytes())
                    .await
                    .context("failed to write the Codex prompt")?;
                drop(stdin);
                let output: Output = child
                    .wait_with_output()
                    .await
                    .context("failed while waiting for the Codex CLI")?;
                Ok(CodexOutput {
                    success: output.status.success(),
                    status: output.status.to_string(),
                    stdout: output.stdout,
                    stderr: output.stderr,
                })
            }
            ResolvedExecutionMode::Phylax(binding) => {
                let mut argv = vec![self.binary.display().to_string()];
                argv.extend(self.arguments(sandbox, working_dir, Some(prompt)));
                let output: MediatedCommandOutput = binding
                    .runner
                    .run(&binding.category, &binding.slot, &binding.env_var, &argv)
                    .await
                    .map_err(anyhow::Error::new)?;
                Ok(CodexOutput {
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

#[cfg(test)]
/// Command-assembly tests for treating brokered prompt text as positional data.
mod tests {
    use super::*;

    /// A flag-shaped prompt follows the explicit Codex option terminator.
    #[test]
    fn mediated_arguments_terminate_options_before_prompt() {
        let executor =
            CodexExecutor::new(PathBuf::from("/opt/codex"), "gpt-5.6-sol".to_string(), None);

        let arguments = executor.arguments(
            CodexSandbox::WorkspaceWrite,
            Some(Path::new("/workspace")),
            Some("--dangerously-bypass-approvals-and-sandbox"),
        );

        assert_eq!(
            &arguments[arguments.len() - 2..],
            ["--", "--dangerously-bypass-approvals-and-sandbox"]
        );
    }
}

/// Return the final completed agent-message text from a Codex JSONL stream.
fn final_agent_message(stdout: &[u8]) -> Result<Option<String>> {
    let output = std::str::from_utf8(stdout).context("Codex JSONL output was not UTF-8")?;
    let mut final_message = None;
    for (index, line) in output.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let event: Value = serde_json::from_str(line)
            .with_context(|| format!("invalid Codex JSONL event on line {}", index + 1))?;
        if event.get("type").and_then(Value::as_str) != Some("item.completed")
            || event
                .get("item")
                .and_then(|item| item.get("type"))
                .and_then(Value::as_str)
                != Some("agent_message")
        {
            continue;
        }
        let text = event
            .get("item")
            .and_then(|item| item.get("text"))
            .and_then(Value::as_str)
            .context("completed Codex agent message did not contain text")?
            .trim()
            .to_string();
        final_message = (!text.is_empty()).then_some(text);
    }
    Ok(final_message)
}

/// Return the worktree HEAD when execution created or advanced a commit.
async fn git_head(working_dir: &Path) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(working_dir)
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

#[async_trait]
/// Implements room discussion and approved task execution through `codex exec`.
impl AgentExecutor for CodexExecutor {
    /// Codex declares no fixed capability requirement beyond the task-specific Pistis grant.
    fn required_capabilities(&self) -> Vec<Capability> {
        Vec::new()
    }

    /// Return the bridge-owned placeholder policy replaced by the approved task sandbox.
    fn sandbox(&self) -> ExecutionSandbox {
        ExecutionSandbox {
            branch: "agent/codex/unset".to_string(),
            working_dir: PathBuf::from("/tmp"),
            max_runtime_secs: 600,
            cargo_target_dir: None,
        }
    }

    /// Generate one room response under Codex's read-only sandbox.
    async fn discuss(&self, context: DiscussionContext) -> Result<Option<AgentResponse>> {
        let output = self
            .run(&to_cli_prompt(&context), CodexSandbox::ReadOnly, None, None)
            .await?;
        if !output.success {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            anyhow::bail!("codex exited with {}: {stderr}", output.status);
        }
        Ok(
            final_agent_message(&output.stdout)?.map(|text| AgentResponse {
                text,
                execution_proposal: None,
            }),
        )
    }

    /// Execute an approved task in its bridge-created workspace-write sandbox.
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
        let head_before = git_head(&task.sandbox.working_dir).await;
        let output = self
            .run(
                &task.description,
                CodexSandbox::WorkspaceWrite,
                Some(&task.sandbox.working_dir),
                task.sandbox.cargo_target_dir.as_deref(),
            )
            .await?;
        let _ = progress_tx.send(ProgressUpdate::Done).await;

        if !output.success {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            return Ok(ExecutionResult::Failed {
                reason: format!("codex exited with {}: {stderr}", output.status),
                partial_work: false,
            });
        }

        let response = final_agent_message(&output.stdout)?;
        let commit_hash = match git_head(&task.sandbox.working_dir).await {
            Some(after) if Some(&after) != head_before.as_ref() => Some(after),
            _ => None,
        };
        Ok(ExecutionResult::Success {
            summary: response
                .as_deref()
                .and_then(|text| text.lines().next())
                .unwrap_or("task complete")
                .to_string(),
            commit_hash,
            evidence: response,
        })
    }

    /// Report whether the configured Codex binary exists on this host.
    async fn health_check(&self) -> Result<HealthStatus> {
        if crate::catalog::command_available(&self.binary) {
            Ok(HealthStatus::Ready)
        } else {
            Ok(HealthStatus::Unavailable(format!(
                "codex binary not found at {}",
                self.binary.display()
            )))
        }
    }
}
