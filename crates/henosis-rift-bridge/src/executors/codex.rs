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
}

/// Implements construction and guarded command assembly for the Codex CLI.
impl CodexExecutor {
    /// Create a Codex CLI executor with one deployment-selected model.
    pub fn new(binary: PathBuf, model: String, reasoning_effort: Option<String>) -> Self {
        Self {
            binary,
            model,
            reasoning_effort,
        }
    }

    /// Build an ephemeral JSONL command with an explicit sandbox and optional working directory.
    fn command(&self, sandbox: CodexSandbox, working_dir: Option<&Path>) -> Command {
        let mut command = Command::new(&self.binary);
        command
            .arg("exec")
            .arg("--ephemeral")
            .arg("--json")
            .arg("--model")
            .arg(&self.model);
        if let Some(working_dir) = working_dir {
            command.arg("--cd").arg(working_dir);
        }
        command.arg("--sandbox").arg(sandbox.as_str());
        if let Some(reasoning_effort) = &self.reasoning_effort {
            let quoted = serde_json::to_string(reasoning_effort)
                .expect("serializing a Rust string to JSON cannot fail");
            command
                .arg("-c")
                .arg(format!("model_reasoning_effort={quoted}"));
        }
        command.arg("-");
        command
    }

    /// Run one Codex invocation with the entire prompt supplied through piped stdin.
    async fn run(
        &self,
        prompt: &str,
        sandbox: CodexSandbox,
        working_dir: Option<&Path>,
        cargo_target_dir: Option<&Path>,
    ) -> Result<Output> {
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

        let mut child = command
            .spawn()
            .with_context(|| format!("failed to start Codex CLI at {}", self.binary.display()))?;
        let mut stdin = child
            .stdin
            .take()
            .context("Codex CLI did not expose piped stdin")?;
        stdin
            .write_all(prompt.as_bytes())
            .await
            .context("failed to write the Codex prompt")?;
        drop(stdin);
        child
            .wait_with_output()
            .await
            .context("failed while waiting for the Codex CLI")
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
        if !output.status.success() {
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

        if !output.status.success() {
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
        if self.binary.exists() {
            Ok(HealthStatus::Ready)
        } else {
            Ok(HealthStatus::Unavailable(format!(
                "codex binary not found at {}",
                self.binary.display()
            )))
        }
    }
}
