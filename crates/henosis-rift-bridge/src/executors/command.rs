//! Generic external-harness executor.
//!
//! Runs any agent CLI as a subprocess so operators can bring their own harness
//! without patching Henosis. The contract is deliberately the smallest thing
//! every agent CLI already satisfies: a prompt goes in as an argument, the
//! answer comes back on stdout.
//!
//! Harnesses that emit newline-delimited JSON can opt into live progress by
//! setting `progress_format = "jsonl"`; anything unparseable in that mode is
//! still surfaced as plain text, so a harness that only sometimes emits JSON
//! degrades instead of breaking.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Stdio;

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::Deserialize;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;
use tokio::sync::mpsc;

use crate::context::to_cli_prompt;
use crate::executor::{
    AgentExecutor, AgentResponse, Capability, DiscussionContext, ExecutionResult, ExecutionSandbox,
    HealthStatus, ProgressUpdate, TaskContext,
};

/// Placeholder replaced by the assembled prompt or task description.
///
/// Substitution is whole-element only: the placeholder must be the entire
/// argument. That keeps room content from being spliced into a larger argument
/// where a leading `-` could be reinterpreted as a flag by the target binary.
const PROMPT_PLACEHOLDER: &str = "{prompt}";

/// Default wall-clock ceiling for one external execution session.
const DEFAULT_MAX_RUNTIME_SECS: u64 = 600;

/// Maximum bytes retained from either child output stream.
const MAX_OUTPUT_BYTES: usize = 256 * 1024;

/// Maximum bytes retained while assembling one JSONL progress record.
const MAX_PROGRESS_LINE_BYTES: usize = 64 * 1024;

/// How the harness reports progress on stdout.
#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ProgressFormat {
    /// Plain text. The whole of stdout is the answer; no incremental progress.
    #[default]
    Text,
    /// Newline-delimited JSON objects, each surfaced to the room as it arrives.
    Jsonl,
}

/// One progress record from a JSONL-speaking harness.
///
/// Every field is optional because this describes other people's output, not a
/// schema Henosis can require. The first populated text-bearing field wins.
#[derive(Debug, Deserialize)]
struct JsonlRecord {
    /// Optional record discriminator, used only to detect terminal records.
    #[serde(default)]
    r#type: Option<String>,
    /// Conventional message field.
    #[serde(default)]
    message: Option<String>,
    /// Alternative text field.
    #[serde(default)]
    text: Option<String>,
    /// Alternative content field.
    #[serde(default)]
    content: Option<String>,
    /// Alternative summary field.
    #[serde(default)]
    summary: Option<String>,
}

/// Extracts the human-readable text a JSONL record carries, if any.
impl JsonlRecord {
    /// Returns the first populated text-bearing field.
    fn display_text(&self) -> Option<&str> {
        self.message
            .as_deref()
            .or(self.text.as_deref())
            .or(self.content.as_deref())
            .or(self.summary.as_deref())
            .map(str::trim)
            .filter(|value| !value.is_empty())
    }
}

/// Executor that delegates to an operator-configured external agent CLI.
pub struct CommandExecutor {
    /// Path to the harness binary.
    binary: PathBuf,
    /// Argument template for a lightweight discussion turn.
    discuss_args: Vec<String>,
    /// Argument template for a full execution session.
    execute_args: Vec<String>,
    /// Working directory for discussion turns. Execution uses the sandbox.
    cwd: Option<PathBuf>,
    /// Wall-clock ceiling for one session, enforced by killing the process.
    max_runtime_secs: u64,
    /// How to interpret the harness's stdout.
    progress_format: ProgressFormat,
    /// Extra environment entries handed to the harness.
    env: BTreeMap<String, String>,
    /// Ambient environment names explicitly inherited by the harness.
    inherit_env: Vec<String>,
    /// Whether to start the child from an empty environment.
    env_clear: bool,
}

/// One bounded child stream and whether bytes were discarded after the cap.
struct BoundedOutput {
    /// Retained prefix of the stream.
    bytes: Vec<u8>,
    /// Whether the stream contained more bytes than were retained.
    truncated: bool,
}

/// Drain one child stream to EOF while retaining only a fixed prefix.
async fn read_capped<R>(mut reader: R, cap: usize) -> std::io::Result<BoundedOutput>
where
    R: AsyncRead + Unpin,
{
    let mut bytes = Vec::with_capacity(cap);
    let mut chunk = [0u8; 8192];
    let mut truncated = false;
    loop {
        let read = reader.read(&mut chunk).await?;
        if read == 0 {
            return Ok(BoundedOutput { bytes, truncated });
        }
        let remaining = cap.saturating_sub(bytes.len());
        let retained = read.min(remaining);
        bytes.extend_from_slice(&chunk[..retained]);
        truncated |= retained < read;
    }
}

/// Drain stdout with fixed memory while forwarding bounded JSONL progress records.
async fn read_stdout_capped<R>(
    mut reader: R,
    format: ProgressFormat,
    progress_tx: Option<&mpsc::Sender<ProgressUpdate>>,
) -> std::io::Result<BoundedOutput>
where
    R: AsyncRead + Unpin,
{
    let mut bytes = Vec::with_capacity(MAX_OUTPUT_BYTES);
    let mut line = Vec::with_capacity(1024);
    let mut chunk = [0u8; 8192];
    let mut output_truncated = false;
    let mut line_truncated = false;
    loop {
        let read = reader.read(&mut chunk).await?;
        if read == 0 {
            if format == ProgressFormat::Jsonl && !line.is_empty() && !line_truncated {
                forward_progress(&line, progress_tx);
            }
            return Ok(BoundedOutput {
                bytes,
                truncated: output_truncated,
            });
        }

        let remaining = MAX_OUTPUT_BYTES.saturating_sub(bytes.len());
        let retained = read.min(remaining);
        bytes.extend_from_slice(&chunk[..retained]);
        output_truncated |= retained < read;

        if format != ProgressFormat::Jsonl {
            continue;
        }
        for byte in &chunk[..read] {
            if *byte == b'\n' {
                if !line_truncated {
                    forward_progress(&line, progress_tx);
                }
                line.clear();
                line_truncated = false;
            } else if !line_truncated {
                if line.len() < MAX_PROGRESS_LINE_BYTES {
                    line.push(*byte);
                } else {
                    line.clear();
                    line_truncated = true;
                }
            }
        }
    }
}

/// Parse and forward one progress record without blocking child pipe drainage.
fn forward_progress(line: &[u8], progress_tx: Option<&mpsc::Sender<ProgressUpdate>>) {
    let Some(sender) = progress_tx else {
        return;
    };
    let line = String::from_utf8_lossy(line);
    if let Some(text) = parse_progress_line(&line) {
        let _ = sender.try_send(ProgressUpdate::Message(text));
    }
}

/// Convert bounded process bytes into display text with an explicit truncation marker.
fn bounded_text(output: BoundedOutput) -> String {
    let mut text = String::from_utf8_lossy(&output.bytes).into_owned();
    if output.truncated {
        text.push_str("\n[truncated]");
    }
    text
}

/// Implements construction and process handling for an external harness.
impl CommandExecutor {
    /// Create an executor for one configured harness.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        binary: PathBuf,
        discuss_args: Vec<String>,
        execute_args: Vec<String>,
        cwd: Option<PathBuf>,
        max_runtime_secs: Option<u64>,
        progress_format: Option<ProgressFormat>,
        env: BTreeMap<String, String>,
        inherit_env: Vec<String>,
        env_clear: bool,
    ) -> Self {
        // Resolve a bare executable while the parent PATH is still available.
        // The child may start from an empty environment and therefore cannot
        // safely depend on PATH lookup at spawn time.
        let binary = which::which(&binary).unwrap_or(binary);
        Self {
            binary,
            discuss_args,
            execute_args,
            cwd,
            max_runtime_secs: max_runtime_secs.unwrap_or(DEFAULT_MAX_RUNTIME_SECS),
            progress_format: progress_format.unwrap_or_default(),
            env,
            inherit_env,
            env_clear,
        }
    }

    /// Substitute the prompt into an argument template.
    ///
    /// Only an argument that is exactly the placeholder is replaced, so the
    /// prompt always lands as a single argv element and can never introduce
    /// additional arguments.
    fn render_args(template: &[String], prompt: &str) -> Vec<String> {
        template
            .iter()
            .map(|arg| {
                if arg == PROMPT_PLACEHOLDER {
                    prompt.to_string()
                } else {
                    arg.clone()
                }
            })
            .collect()
    }

    /// Build the child process for one invocation.
    fn build_command(&self, args: Vec<String>, working_dir: Option<&std::path::Path>) -> Command {
        let mut cmd = Command::new(&self.binary);
        cmd.args(args);
        if let Some(dir) = working_dir.or(self.cwd.as_deref()) {
            cmd.current_dir(dir);
        }
        // The configuration default is an empty environment. An operator must
        // explicitly name inherited variables or opt out of clearing entirely.
        if self.env_clear {
            cmd.env_clear();
        }
        for name in &self.inherit_env {
            if let Some(value) = std::env::var_os(name) {
                cmd.env(name, value);
            }
        }
        for (key, value) in &self.env {
            cmd.env(key, value);
        }
        cmd.stdin(Stdio::null());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        cmd.kill_on_drop(true);
        cmd
    }

    /// Run one invocation to completion, streaming progress and enforcing the timeout.
    ///
    /// The timeout kills the child rather than abandoning it. A harness left
    /// running past its limit would keep mutating the sandbox worktree while the
    /// bridge believes the session ended, and would race any retry attempt in
    /// that same worktree.
    async fn run(
        &self,
        mut cmd: Command,
        progress_tx: Option<&mpsc::Sender<ProgressUpdate>>,
    ) -> Result<CommandOutcome> {
        let mut child = cmd
            .spawn()
            .with_context(|| format!("spawning harness {}", self.binary.display()))?;

        let stdout = child
            .stdout
            .take()
            .context("harness stdout pipe was not captured")?;
        let stderr = child
            .stderr
            .take()
            .context("harness stderr pipe was not captured")?;

        let format = self.progress_format;
        let deadline = std::time::Duration::from_secs(self.max_runtime_secs);
        let combined = async {
            let (stdout, stderr, status) = tokio::try_join!(
                read_stdout_capped(stdout, format, progress_tx),
                read_capped(stderr, MAX_OUTPUT_BYTES),
                child.wait(),
            )?;
            Ok::<CommandOutcome, std::io::Error>(CommandOutcome {
                stdout: bounded_text(stdout),
                stderr: bounded_text(stderr),
                success: status.success(),
                status: status.to_string(),
                timed_out: false,
            })
        };

        match tokio::time::timeout(deadline, combined).await {
            Ok(result) => Ok(result?),
            Err(_) => {
                // kill_on_drop covers the abnormal paths; this makes the normal
                // timeout path deterministic instead of leaving an orphan.
                let _ = child.start_kill();
                let _ = child.wait().await;
                Ok(CommandOutcome {
                    stdout: String::new(),
                    stderr: String::new(),
                    success: false,
                    status: format!("timed out after {}s", self.max_runtime_secs),
                    timed_out: true,
                })
            }
        }
    }
}

/// Captured result of one harness invocation.
struct CommandOutcome {
    /// Everything the harness wrote to stdout.
    stdout: String,
    /// Everything the harness wrote to stderr.
    stderr: String,
    /// Whether the process exited zero.
    success: bool,
    /// Human-readable exit description for error reporting.
    status: String,
    /// Whether the wall-clock ceiling killed the process.
    timed_out: bool,
}

/// Extract room-displayable text from one stdout line in JSONL mode.
///
/// A line that is not valid JSON is still shown, because a harness that
/// interleaves plain logging with JSON records should not go silent.
fn parse_progress_line(line: &str) -> Option<String> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }
    match serde_json::from_str::<JsonlRecord>(trimmed) {
        Ok(record) => {
            if record.r#type.as_deref() == Some("result") {
                return None;
            }
            record.display_text().map(str::to_string)
        }
        Err(_) => Some(trimmed.to_string()),
    }
}

/// Return the worktree's current HEAD commit, or `None` when it is not a repo.
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

#[async_trait]
/// Implements the executor contract by delegating to an external agent CLI.
impl AgentExecutor for CommandExecutor {
    /// An external harness holds no Henosis-granted capabilities of its own.
    fn required_capabilities(&self) -> Vec<Capability> {
        vec![]
    }

    /// Declare the wall-clock ceiling; the bridge supplies the real worktree.
    fn sandbox(&self) -> ExecutionSandbox {
        ExecutionSandbox {
            branch: "agent/command/unset".into(),
            working_dir: self.cwd.clone().unwrap_or_else(std::env::temp_dir),
            max_runtime_secs: self.max_runtime_secs,
            cargo_target_dir: None,
        }
    }

    /// Run one discussion turn through the harness and return its stdout.
    async fn discuss(&self, context: DiscussionContext) -> Result<Option<AgentResponse>> {
        let prompt = to_cli_prompt(&context);
        let args = Self::render_args(&self.discuss_args, &prompt);
        let cmd = self.build_command(args, None);

        let outcome = self.run(cmd, None).await?;
        if !outcome.success {
            anyhow::bail!(
                "harness {} failed ({}): {}",
                self.binary.display(),
                outcome.status,
                outcome.stderr.trim()
            );
        }

        let response = outcome.stdout.trim().to_string();
        if response.is_empty() {
            Ok(None)
        } else {
            Ok(Some(AgentResponse {
                text: response,
                execution_proposal: None,
            }))
        }
    }

    /// Run a full execution session inside the bridge-provided sandbox worktree.
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

        let args = Self::render_args(&self.execute_args, &task.description);
        let mut cmd = self.build_command(args, Some(&task.sandbox.working_dir));
        if let Some(target_dir) = &task.sandbox.cargo_target_dir {
            cmd.env("CARGO_TARGET_DIR", target_dir);
        }

        let outcome = self.run(cmd, Some(&progress_tx)).await?;
        let _ = progress_tx.send(ProgressUpdate::Done).await;

        if outcome.timed_out {
            return Ok(ExecutionResult::Failed {
                reason: format!("harness {}", outcome.status),
                partial_work: git_head(&task.sandbox.working_dir).await != head_before,
            });
        }

        let stdout = outcome.stdout.trim().to_string();
        if outcome.success {
            let commit_hash = match git_head(&task.sandbox.working_dir).await {
                Some(after) if Some(&after) != head_before.as_ref() => Some(after),
                _ => None,
            };
            Ok(ExecutionResult::Success {
                summary: stdout.lines().last().unwrap_or("task complete").to_string(),
                commit_hash,
                evidence: (!stdout.is_empty()).then_some(stdout),
            })
        } else {
            Ok(ExecutionResult::Failed {
                reason: format!(
                    "harness {} exited {}: {}",
                    self.binary.display(),
                    outcome.status,
                    outcome.stderr.trim()
                ),
                partial_work: git_head(&task.sandbox.working_dir).await != head_before,
            })
        }
    }

    /// Report readiness by checking that the configured binary is present.
    async fn health_check(&self) -> Result<HealthStatus> {
        if which::which(&self.binary).is_ok() {
            return Ok(HealthStatus::Ready);
        }
        Ok(HealthStatus::Unavailable(format!(
            "harness binary is not executable at {}; install it or configure an executable absolute path",
            self.binary.display()
        )))
    }
}

/// Covers argument rendering, JSONL progress parsing, and timeout enforcement.
#[cfg(test)]
mod tests {
    use super::*;

    /// The prompt replaces only a whole placeholder argument.
    #[test]
    fn render_substitutes_whole_elements_only() {
        let template = vec![
            "--message".to_string(),
            PROMPT_PLACEHOLDER.to_string(),
            "--yes".to_string(),
        ];
        let rendered = CommandExecutor::render_args(&template, "hello world");
        assert_eq!(rendered, vec!["--message", "hello world", "--yes"]);
    }

    /// Room content that looks like a flag stays one argument and is never split.
    #[test]
    fn prompt_resembling_a_flag_stays_a_single_argument() {
        let template = vec!["--message".to_string(), PROMPT_PLACEHOLDER.to_string()];
        let rendered = CommandExecutor::render_args(&template, "--dangerously-skip-permissions x");
        assert_eq!(rendered.len(), 2);
        assert_eq!(rendered[1], "--dangerously-skip-permissions x");
    }

    /// A placeholder embedded in a larger argument is left alone.
    #[test]
    fn embedded_placeholder_is_not_substituted() {
        let template = vec![format!("--message={PROMPT_PLACEHOLDER}")];
        let rendered = CommandExecutor::render_args(&template, "hi");
        assert_eq!(rendered, vec!["--message={prompt}"]);
    }

    /// Conventional JSONL progress fields are surfaced.
    #[test]
    fn jsonl_progress_fields_are_extracted() {
        assert_eq!(
            parse_progress_line(r#"{"type":"progress","message":"running tests"}"#),
            Some("running tests".to_string())
        );
        assert_eq!(
            parse_progress_line(r#"{"text":"editing main.rs"}"#),
            Some("editing main.rs".to_string())
        );
    }

    /// Terminal result records are not echoed as progress.
    #[test]
    fn jsonl_result_record_is_not_progress() {
        assert_eq!(
            parse_progress_line(r#"{"type":"result","text":"done"}"#),
            None
        );
    }

    /// Non-JSON output still reaches the room instead of vanishing.
    #[test]
    fn non_json_line_degrades_to_text() {
        assert_eq!(
            parse_progress_line("plain log line"),
            Some("plain log line".to_string())
        );
        assert_eq!(parse_progress_line("   "), None);
    }

    /// A harness that ignores its deadline is killed, not abandoned.
    #[tokio::test]
    #[cfg(unix)]
    async fn timeout_kills_the_child() {
        let executor = CommandExecutor::new(
            PathBuf::from("sh"),
            vec!["-c".to_string(), "sleep 30".to_string()],
            vec![],
            None,
            Some(1),
            None,
            BTreeMap::new(),
            vec![],
            false,
        );
        let cmd = executor.build_command(vec!["-c".to_string(), "sleep 30".to_string()], None);
        let started = std::time::Instant::now();
        let outcome = executor.run(cmd, None).await.expect("run completes");
        assert!(outcome.timed_out, "the deadline must fire");
        assert!(!outcome.success);
        assert!(
            started.elapsed() < std::time::Duration::from_secs(10),
            "timeout must not wait for the child's own exit"
        );
    }

    /// A well-behaved harness returns its stdout.
    #[tokio::test]
    #[cfg(unix)]
    async fn successful_run_returns_stdout() {
        let executor = CommandExecutor::new(
            PathBuf::from("sh"),
            vec![],
            vec![],
            None,
            Some(30),
            None,
            BTreeMap::new(),
            vec![],
            false,
        );
        let cmd = executor.build_command(vec!["-c".to_string(), "echo hello".to_string()], None);
        let outcome = executor.run(cmd, None).await.expect("run completes");
        assert!(outcome.success);
        assert_eq!(outcome.stdout.trim(), "hello");
    }

    /// A verbose harness is drained fully while retained output stays byte-bounded.
    #[tokio::test]
    #[cfg(unix)]
    async fn verbose_run_is_bounded_during_execution() {
        let executor = CommandExecutor::new(
            PathBuf::from("sh"),
            vec![],
            vec![],
            None,
            Some(30),
            None,
            BTreeMap::new(),
            vec![],
            true,
        );
        let cmd = executor.build_command(
            vec![
                "-c".to_string(),
                format!("yes x | head -c {}", MAX_OUTPUT_BYTES * 4),
            ],
            None,
        );
        let outcome = executor.run(cmd, None).await.expect("run completes");

        assert!(outcome.success);
        assert!(outcome.stdout.len() <= MAX_OUTPUT_BYTES + "\n[truncated]".len());
        assert!(outcome.stdout.ends_with("[truncated]"));
    }

    /// A bare executable is resolved before an empty child environment removes PATH.
    #[tokio::test]
    async fn bare_binary_is_resolved_and_health_checked() {
        let executor = CommandExecutor::new(
            PathBuf::from("sh"),
            vec!["-c".to_string(), PROMPT_PLACEHOLDER.to_string()],
            vec![
                "-c".to_string(),
                "exec ".to_string(),
                PROMPT_PLACEHOLDER.to_string(),
            ],
            None,
            Some(30),
            None,
            BTreeMap::new(),
            vec![],
            true,
        );

        assert!(executor.binary.is_absolute());
        assert_eq!(
            executor.health_check().await.expect("health"),
            HealthStatus::Ready
        );
    }

    /// A missing harness reports an actionable unavailable status before dispatch.
    #[tokio::test]
    async fn missing_binary_is_not_reported_ready() {
        let executor = CommandExecutor::new(
            PathBuf::from("henosis-missing-harness-for-test"),
            vec!["--discuss".to_string(), PROMPT_PLACEHOLDER.to_string()],
            vec!["--execute".to_string(), PROMPT_PLACEHOLDER.to_string()],
            None,
            Some(30),
            None,
            BTreeMap::new(),
            vec![],
            true,
        );

        let status = executor.health_check().await.expect("health");
        assert!(
            matches!(status, HealthStatus::Unavailable(message) if message.contains("install it"))
        );
    }

    /// Named ambient variables survive env clearing while unrelated variables do not.
    #[test]
    fn named_environment_inheritance_is_explicit() {
        let executor = CommandExecutor::new(
            PathBuf::from("sh"),
            vec!["-c".to_string(), PROMPT_PLACEHOLDER.to_string()],
            vec![
                "-c".to_string(),
                "exec ".to_string(),
                PROMPT_PLACEHOLDER.to_string(),
            ],
            None,
            Some(30),
            None,
            BTreeMap::new(),
            vec!["PATH".to_string()],
            true,
        );
        let command = executor.build_command(vec!["-c".to_string(), "true".to_string()], None);
        let inherited: Vec<_> = command
            .as_std()
            .get_envs()
            .filter_map(|(name, value)| value.map(|_| name.to_string_lossy().into_owned()))
            .collect();

        assert_eq!(inherited, vec!["PATH"]);
    }
}
