//! Bash/shell command execution tool.

use crate::tool::{AgentTool, ToolResult};
use anyhow::Result;
use serde_json::Value;
use std::path::Path;
use std::time::Duration;
use tokio::process::Command;
use tokio::time::timeout;

const DEFAULT_TIMEOUT_MS: u64 = 30_000;
const MAX_OUTPUT_BYTES: usize = 50 * 1024; // 50 KB

/// Executes bounded shell commands and returns combined process output.
pub struct BashTool;

/// Implements the agent tool contract for shell command execution.
#[async_trait::async_trait]
impl AgentTool for BashTool {
    /// Returns the shell tool's stable registry name.
    fn name(&self) -> &str {
        "bash"
    }

    /// Describes the shell command execution capability.
    fn description(&self) -> &str {
        "Execute a shell command and return combined stdout+stderr. \
         Use for running builds, tests, CLI tools, or any system command."
    }

    /// Returns the accepted command and timeout parameters.
    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "The shell command to execute."
                },
                "timeout_ms": {
                    "type": "number",
                    "description": "Timeout in milliseconds. Defaults to 30000."
                }
            },
            "required": ["command"]
        })
    }

    /// Executes a command from the supplied working directory within the timeout.
    async fn execute(&self, params: Value, cwd: &Path) -> Result<ToolResult> {
        let command = match params.get("command").and_then(|v| v.as_str()) {
            Some(c) => c.to_string(),
            None => {
                return Ok(ToolResult {
                    content: "Missing required parameter: command".to_string(),
                    is_error: true,
                });
            }
        };

        let timeout_ms = params
            .get("timeout_ms")
            .and_then(|v| v.as_u64())
            .unwrap_or(DEFAULT_TIMEOUT_MS);

        let mut cmd = build_command(&command);
        cmd.current_dir(cwd);
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());
        cmd.kill_on_drop(true);

        let result = timeout(Duration::from_millis(timeout_ms), async {
            let child = cmd.spawn()?;
            let output = child.wait_with_output().await?;
            Ok::<_, anyhow::Error>(output)
        })
        .await;

        match result {
            Err(_elapsed) => Ok(ToolResult {
                content: format!("Command timed out after {timeout_ms}ms"),
                is_error: true,
            }),
            Ok(Err(e)) => Ok(ToolResult {
                content: format!("Failed to spawn command: {e}"),
                is_error: true,
            }),
            Ok(Ok(output)) => {
                let mut combined = Vec::new();
                combined.extend_from_slice(&output.stdout);
                combined.extend_from_slice(&output.stderr);

                let truncated = combined.len() > MAX_OUTPUT_BYTES;
                if truncated {
                    combined.truncate(MAX_OUTPUT_BYTES);
                }

                let mut text = String::from_utf8_lossy(&combined).into_owned();
                if truncated {
                    text.push_str("\n[truncated]");
                }

                let exit_code = output.status.code().unwrap_or(-1);
                let is_error = !output.status.success();

                if is_error && text.is_empty() {
                    text = format!("Process exited with code {exit_code}");
                }

                Ok(ToolResult {
                    content: text,
                    is_error,
                })
            }
        }
    }
}

#[cfg(target_os = "windows")]
/// Constructs a Windows command-shell process.
fn build_command(command: &str) -> Command {
    let mut cmd = Command::new("cmd");
    cmd.args(["/C", command]);
    cmd
}

#[cfg(not(target_os = "windows"))]
/// Constructs a POSIX shell process.
fn build_command(command: &str) -> Command {
    let mut cmd = Command::new("sh");
    cmd.args(["-c", command]);
    cmd
}
