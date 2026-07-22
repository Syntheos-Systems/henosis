//! Subprocess lifecycle management for the claude CLI child process.

use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

use crate::claude_max::ndjson;
use crate::claude_max::protocol::{IncomingMessage, InitCapabilities, OutgoingMessage};

/// State of a running claude subprocess.
pub(crate) struct SubprocessState {
    /// The child process handle.
    child: Child,
    /// Stdin pipe for sending NDJSON messages.
    stdin: ChildStdin,
    /// Buffered line reader for stdout NDJSON.
    stdout: Lines<BufReader<ChildStdout>>,
    /// Session ID from the system/init handshake.
    pub(crate) session_id: String,
    /// Number of user messages sent to the subprocess (for delta tracking).
    pub(crate) messages_sent: usize,
}

/// Adds inherent behavior for `SubprocessState`.
impl SubprocessState {
    /// Spawn the claude subprocess, wait for system/init, send initialize handshake.
    pub(crate) async fn spawn(cli_path: &str, model: &str, oauth_token: &str) -> Result<Self> {
        let mut cmd = build_spawn_command(cli_path, model, oauth_token);

        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("failed to spawn claude binary at {cli_path}"))?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow::anyhow!("stdin pipe not available after spawn"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow::anyhow!("stdout pipe not available after spawn"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| anyhow::anyhow!("stderr pipe not available after spawn"))?;

        // Spawn stderr logger (fire and forget).
        tokio::spawn(async move {
            let reader = BufReader::new(stderr);
            let mut lines = reader.lines();
            while let Ok(Some(line)) = lines.next_line().await {
                log::debug!("[claude stderr] {line}");
            }
        });

        let mut stdout_lines = BufReader::new(stdout).lines();

        // Wait for system/init message with a 30-second timeout.
        let (session_id, _model) = tokio::time::timeout(
            Duration::from_secs(30),
            Self::wait_for_init(&mut stdout_lines),
        )
        .await
        .map_err(|_| {
            anyhow::anyhow!("timed out waiting for system/init from claude subprocess (30s)")
        })??;

        let mut state = Self {
            child,
            stdin,
            stdout: stdout_lines,
            session_id: session_id.clone(),
            messages_sent: 0,
        };

        // Send initialize handshake declaring MCP capabilities.
        let init_msg = OutgoingMessage::Initialize {
            protocol_version: "1".into(),
            capabilities: InitCapabilities { mcp: true },
        };
        state.write_message(&init_msg).await?;

        Ok(state)
    }

    /// Read stdout lines until we get a system/init message.
    async fn wait_for_init(stdout: &mut Lines<BufReader<ChildStdout>>) -> Result<(String, String)> {
        loop {
            let line = stdout
                .next_line()
                .await?
                .ok_or_else(|| anyhow::anyhow!("subprocess exited before sending system/init"))?;

            match ndjson::parse_line(&line) {
                Ok(IncomingMessage::System(sys)) if sys.subtype == "init" => {
                    let session_id = sys
                        .session_id
                        .ok_or_else(|| anyhow::anyhow!("system/init missing session_id"))?;
                    let model = sys.model.unwrap_or_default();
                    log::info!(
                        "claude subprocess initialized: session={session_id}, model={model}"
                    );
                    return Ok((session_id, model));
                }
                Ok(_) => continue,
                Err(e) => {
                    log::warn!("ignoring unparseable line during init: {e}");
                    continue;
                }
            }
        }
    }

    /// Write an outgoing NDJSON message to the subprocess stdin.
    pub(crate) async fn write_message(&mut self, msg: &OutgoingMessage) -> Result<()> {
        let line = ndjson::serialize(msg)?;
        self.stdin
            .write_all(line.as_bytes())
            .await
            .context("failed to write to subprocess stdin")?;
        self.stdin.flush().await.context("failed to flush stdin")?;
        Ok(())
    }

    /// Read the next NDJSON line from subprocess stdout.
    /// Returns None if the subprocess has closed stdout.
    pub(crate) async fn read_line(&mut self) -> Result<Option<IncomingMessage>> {
        match self.stdout.next_line().await? {
            Some(line) => {
                let msg = ndjson::parse_line(&line)?;
                Ok(Some(msg))
            }
            None => Ok(None),
        }
    }
}

/// Drop impl kills the subprocess to prevent orphans.
impl Drop for SubprocessState {
    /// Handles `drop` behavior.
    fn drop(&mut self) {
        let _ = self.child.start_kill();
    }
}

/// Build the tokio Command for spawning the claude subprocess.
/// Separated from spawn() for testability.
fn build_spawn_command(cli_path: &str, model: &str, oauth_token: &str) -> Command {
    let mut cmd = Command::new(cli_path);
    cmd.args([
        "--input-format",
        "stream-json",
        "--output-format",
        "stream-json",
        "--tools",
        "",
        "--bare",
        "--verbose",
        // Non-interactive subprocess -- permission control is handled by
        // ToolRegistry/ToolGate in the MCP bridge, not CLI prompts.
        "--dangerously-skip-permissions",
        "--model",
        model,
    ]);
    cmd.env("CLAUDE_CODE_OAUTH_TOKEN", oauth_token);
    // SD1: Token visible in /proc/<pid>/environ for subprocess lifetime.
    // Accepted risk per security definition -- single-user machine.
    cmd
}

/// Groups `{` functionality.
#[cfg(test)]
mod tests {
    use super::*;

    /// Handles `build_command_includes_required_flags` behavior.
    #[test]
    fn build_command_includes_required_flags() {
        let cmd = build_spawn_command("/usr/bin/claude", "claude-sonnet-4-6", "test-token");
        let args: Vec<&std::ffi::OsStr> = cmd.as_std().get_args().collect();
        let args_str: Vec<&str> = args.iter().map(|a| a.to_str().unwrap()).collect();

        assert!(args_str.contains(&"--input-format"));
        assert!(args_str.contains(&"stream-json"));
        assert!(args_str.contains(&"--output-format"));
        assert!(args_str.contains(&"--bare"));
        assert!(args_str.contains(&"--verbose"));
        assert!(args_str.contains(&"--dangerously-skip-permissions"));
        assert!(args_str.contains(&"--model"));
        assert!(args_str.contains(&"claude-sonnet-4-6"));
    }

    /// Handles `build_command_disables_builtin_tools` behavior.
    #[test]
    fn build_command_disables_builtin_tools() {
        let cmd = build_spawn_command("/usr/bin/claude", "claude-sonnet-4-6", "tok");
        let args: Vec<&str> = cmd
            .as_std()
            .get_args()
            .map(|a| a.to_str().unwrap())
            .collect();
        let tools_idx = args.iter().position(|a| *a == "--tools").unwrap();
        assert_eq!(args[tools_idx + 1], "");
    }

    /// Handles `build_command_sets_oauth_env` behavior.
    #[test]
    fn build_command_sets_oauth_env() {
        let cmd = build_spawn_command("/usr/bin/claude", "claude-sonnet-4-6", "my-token");
        let envs: Vec<(&std::ffi::OsStr, Option<&std::ffi::OsStr>)> =
            cmd.as_std().get_envs().collect();
        let oauth_env = envs.iter().find(|(k, _)| *k == "CLAUDE_CODE_OAUTH_TOKEN");
        assert!(oauth_env.is_some());
        assert_eq!(oauth_env.unwrap().1.unwrap().to_str().unwrap(), "my-token");
    }
}
