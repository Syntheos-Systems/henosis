//! File-based client for the `agent-forge` CLI. Each public method writes a
//! tempfile of input JSON, invokes `agent-forge --input <in> --output <out>
//! <subcommand>`, reads the output JSON, and returns the result. All failures
//! are best-effort: they log a warning and never block the Hephaestus task
//! lifecycle.

use std::path::PathBuf;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::process::Command;
use tracing::{debug, warn};
use uuid::Uuid;

/// File-based client for the `agent-forge` CLI. Each call writes a tempfile
/// of input JSON, invokes `agent-forge --input <in> --output <out>
/// <subcommand>`, and parses the result. Best-effort: failures are logged
/// and never block the Hephaestus task lifecycle.
pub struct AgentForgeClient {
    /// Path to the `agent-forge` binary.
    pub bin: PathBuf,
    /// Optional path to the agent-forge SQLite database passed as `--db`.
    pub db: Option<PathBuf>,
    /// Per-invocation timeout. Defaults to 5s; long enough for disk I/O but
    /// short enough not to stall a task turn.
    pub timeout: Duration,
}

/// Implements Agent-Forge task registration, verification, and subprocess transport.
impl AgentForgeClient {
    /// Construct a client pointing at the given binary. Uses a 5s default
    /// timeout which is sufficient for all current subcommands.
    pub fn new(bin: PathBuf, db: Option<PathBuf>) -> Self {
        Self {
            bin,
            db,
            timeout: Duration::from_secs(5),
        }
    }

    /// Register a Hephaestus task in agent-forge's spec store. Returns the
    /// spec id (e.g. `spec_abc123`) on success, or None on any failure.
    pub async fn spec_task(&self, task_id: &str, title: &str, description: &str) -> Option<String> {
        let input = json!({
            "task_description": format!("hephaestus task {task_id} -- {title}: {description}"),
            "task_type": "feature",
            "acceptance_criteria": [
                "agent loop reaches end_turn or stop without exceeding max_tool_turns",
                "all tool calls return a tool_result before the next turn"
            ],
            "edge_cases": [
                "Anthropic 401 mid-turn -- one token refresh + retry",
                "ask_human pause -- task suspends and resumes via POST /resume",
                "process crash mid-loop -- resume from latest checkpoint without duplicate threads"
            ],
            "complexity": "medium",
            "files_affected": ["hephaestus/src/tasks.rs", "hephaestus/src/clients.rs"],
            "estimated_loc": 0,
            "interface_contract": format!("POST /tasks task_id={task_id}"),
        });
        let out = self.run("spec-task", &input).await?;
        out.get("id").and_then(|i| i.as_str()).map(String::from)
    }

    /// Run a `verify` step before flipping a task to Completed. Acts as a
    /// programmatic acceptance gate. Returns true if exit_code == 0.
    pub async fn verify(&self, command: &str) -> bool {
        let input = json!({ "command": command });
        let Some(out) = self.run("verify", &input).await else {
            return false;
        };
        out.get("success")
            .and_then(|s| s.as_bool())
            .unwrap_or(false)
    }

    /// Write `input` to a tempfile, invoke `agent-forge <subcommand>`, read
    /// the output file, and parse the result as JSON. Returns None on any
    /// failure (spawn error, timeout, non-zero exit, parse error).
    async fn run(&self, subcommand: &str, input: &Value) -> Option<Value> {
        let nonce = Uuid::new_v4();
        let in_path = std::env::temp_dir().join(format!("af-{nonce}-in.json"));
        let out_path = std::env::temp_dir().join(format!("af-{nonce}-out.json"));

        let bytes = match serde_json::to_vec(input) {
            Ok(b) => b,
            Err(e) => {
                warn!(subcommand, error = %e, "agent-forge input serialize failed");
                return None;
            }
        };
        if let Err(e) = tokio::fs::write(&in_path, &bytes).await {
            warn!(subcommand, error = %e, "agent-forge tempfile write failed");
            return None;
        }

        let mut cmd = Command::new(&self.bin);
        cmd.arg("--input")
            .arg(&in_path)
            .arg("--output")
            .arg(&out_path);
        if let Some(db) = &self.db {
            cmd.arg("--db").arg(db);
        }
        cmd.arg(subcommand);
        cmd.stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped());

        let exec = cmd.output();
        let result = tokio::time::timeout(self.timeout, exec).await;

        // Best-effort cleanup -- ignore failures.
        let _ = tokio::fs::remove_file(&in_path).await;

        let output_bytes = match result {
            Ok(Ok(o)) if o.status.success() => match tokio::fs::read(&out_path).await {
                Ok(b) => {
                    let _ = tokio::fs::remove_file(&out_path).await;
                    b
                }
                Err(e) => {
                    warn!(subcommand, error = %e, "agent-forge output read failed");
                    return None;
                }
            },
            Ok(Ok(o)) => {
                let stderr = String::from_utf8_lossy(&o.stderr).to_string();
                warn!(subcommand, status = ?o.status, %stderr, "agent-forge non-zero exit");
                return None;
            }
            Ok(Err(e)) => {
                warn!(subcommand, error = %e, "agent-forge exec failed");
                return None;
            }
            Err(_) => {
                warn!(subcommand, "agent-forge timed out");
                return None;
            }
        };

        match serde_json::from_slice::<Value>(&output_bytes) {
            Ok(v) => {
                debug!(subcommand, "agent-forge ok");
                Some(v)
            }
            Err(e) => {
                warn!(subcommand, error = %e, "agent-forge output parse failed");
                None
            }
        }
    }
}
