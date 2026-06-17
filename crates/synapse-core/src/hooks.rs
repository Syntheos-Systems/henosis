//! Hooks scaffolding for Synapse.
//!
//! A hook is a shell command Synapse runs at a defined lifecycle point.
//! Hooks let the host wire activity reporting, content guards, secret
//! scanners, and skill-capture suggestions without modifying the agent
//! crate.
//!
//! Configuration lives in `~/.synapse/hooks.toml`; the loader is in
//! `synapse-cli`. This module defines the data types and the
//! `HookGate` adapter that fires PreToolUse / PostToolUse hooks via the
//! `ToolGate` interceptor surface.
//!
//! ## Phases
//! - `SessionStart`: fired once when the REPL opens or a one-shot command begins.
//! - `UserPromptSubmit`: fired before every user message is sent to the LLM.
//! - `PreToolUse`: fired before a tool runs. Exit code 2 denies the tool.
//! - `PostToolUse`: fired after a tool runs. Observation only.
//! - `Stop`: fired once when the session ends.

use serde::{Deserialize, Serialize};
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use synapse_tools::{GateDecision, SharedGate, ToolGate, ToolResult};
use tokio::process::Command;
use tokio::time::timeout;

/// Lifecycle phases at which hooks fire. Stored as lowercase strings in TOML.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookPhase {
    SessionStart,
    UserPromptSubmit,
    PreToolUse,
    PostToolUse,
    Stop,
}

/// A single hook entry. The `command` runs via the user's shell; `matcher`
/// optionally restricts which tools trigger PreToolUse/PostToolUse hooks
/// (default: all tools). `timeout_secs` defaults to 10.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookSpec {
    /// Lifecycle phase when this hook fires.
    pub phase: HookPhase,
    /// Shell command to execute. Runs via `sh -c "<command>"`.
    pub command: String,
    /// Optional tool-name filter for PreToolUse/PostToolUse. None = all tools.
    #[serde(default)]
    pub matcher: Option<String>,
    /// Per-hook timeout in seconds. Defaults to 10.
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
    /// Optional human-friendly label for logs.
    #[serde(default)]
    pub label: Option<String>,
}

fn default_timeout_secs() -> u64 {
    10
}

/// Full hook configuration. Empty by default; populated from
/// `~/.synapse/hooks.toml` if it exists.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HookConfig {
    /// All registered hook entries.
    #[serde(default)]
    pub hooks: Vec<HookSpec>,
}

impl HookConfig {
    /// Return hooks for a given phase, optionally filtered by tool name.
    /// For PreToolUse/PostToolUse, hooks whose `matcher` does not match the
    /// tool name are excluded; entries with no matcher fire on every tool.
    pub fn for_phase<'a>(
        &'a self,
        phase: HookPhase,
        tool_name: Option<&'a str>,
    ) -> impl Iterator<Item = &'a HookSpec> + 'a {
        self.hooks.iter().filter(move |h| {
            if h.phase != phase {
                return false;
            }
            match (&h.matcher, tool_name) {
                (Some(m), Some(n)) => m == n || m == "*",
                (Some(_), None) => false,
                (None, _) => true,
            }
        })
    }
}

/// Result of running a single hook. Currently advisory; PreToolUse hooks
/// with `exit_code == 2` are treated as a deny signal by `HookGate`.
#[derive(Debug, Clone)]
pub struct HookOutcome {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
}

/// Execute a single hook command via `sh -c`. The hook's environment receives
/// the inputs as variables: `SYNAPSE_HOOK_PHASE`, `SYNAPSE_HOOK_TOOL` (when
/// available), and `SYNAPSE_HOOK_PARAMS` (JSON string when available).
/// Stdin is closed; stdout/stderr are captured.
pub async fn run_hook(
    spec: &HookSpec,
    tool_name: Option<&str>,
    params_json: Option<&str>,
    cwd: &Path,
) -> HookOutcome {
    let mut cmd = Command::new("sh");
    cmd.arg("-c")
        .arg(&spec.command)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("SYNAPSE_HOOK_PHASE", phase_str(spec.phase));
    if let Some(name) = tool_name {
        cmd.env("SYNAPSE_HOOK_TOOL", name);
    }
    if let Some(p) = params_json {
        cmd.env("SYNAPSE_HOOK_PARAMS", p);
    }

    let fut = cmd.output();
    let dur = Duration::from_secs(spec.timeout_secs);
    match timeout(dur, fut).await {
        Ok(Ok(out)) => HookOutcome {
            exit_code: out.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            timed_out: false,
        },
        Ok(Err(e)) => HookOutcome {
            exit_code: -1,
            stdout: String::new(),
            stderr: format!("hook spawn error: {e}"),
            timed_out: false,
        },
        Err(_) => HookOutcome {
            exit_code: -1,
            stdout: String::new(),
            stderr: format!("hook timed out after {}s", spec.timeout_secs),
            timed_out: true,
        },
    }
}

/// String form of a phase, used for env vars and logs.
fn phase_str(p: HookPhase) -> &'static str {
    match p {
        HookPhase::SessionStart => "SessionStart",
        HookPhase::UserPromptSubmit => "UserPromptSubmit",
        HookPhase::PreToolUse => "PreToolUse",
        HookPhase::PostToolUse => "PostToolUse",
        HookPhase::Stop => "Stop",
    }
}

/// Run all hooks for a non-tool phase (SessionStart, UserPromptSubmit, Stop).
/// Failures are logged but do not abort the session.
pub async fn run_phase_hooks(config: &HookConfig, phase: HookPhase, cwd: &Path) {
    for spec in config.for_phase(phase, None) {
        let outcome = run_hook(spec, None, None, cwd).await;
        if outcome.exit_code != 0 || outcome.timed_out {
            log::warn!(
                "hook {:?} ({}) failed: exit={} stderr={}",
                phase,
                spec.label.as_deref().unwrap_or("<unlabeled>"),
                outcome.exit_code,
                outcome.stderr.trim()
            );
        }
    }
}

/// ToolGate adapter that fires PreToolUse hooks before tool execution and
/// PostToolUse hooks after. Wraps an inner gate (typically `PermissiveGate`
/// in Phase 0, an interactive confirmation gate in Phase 3) so hook policy
/// composes with permission policy.
///
/// A PreToolUse hook that exits with code 2 denies the tool; any other
/// non-zero exit code is logged as a warning but does not block.
pub struct HookGate {
    /// Hook configuration loaded at startup.
    config: Arc<HookConfig>,
    /// Inner gate consulted after PreToolUse hooks pass.
    inner: SharedGate,
}

impl HookGate {
    /// Wrap an inner gate with hook execution.
    pub fn new(config: Arc<HookConfig>, inner: SharedGate) -> Self {
        Self { config, inner }
    }
}

#[async_trait::async_trait]
impl ToolGate for HookGate {
    async fn before_execute(
        &self,
        name: &str,
        params: &serde_json::Value,
        cwd: &Path,
    ) -> GateDecision {
        let params_json = serde_json::to_string(params).ok();
        for spec in self.config.for_phase(HookPhase::PreToolUse, Some(name)) {
            let outcome = run_hook(spec, Some(name), params_json.as_deref(), cwd).await;
            if outcome.exit_code == 2 {
                return GateDecision::Deny(format!(
                    "PreToolUse hook '{}' denied: {}",
                    spec.label.as_deref().unwrap_or("<unlabeled>"),
                    outcome.stderr.trim()
                ));
            }
            if outcome.exit_code != 0 || outcome.timed_out {
                log::warn!(
                    "PreToolUse hook '{}' failed: exit={} stderr={}",
                    spec.label.as_deref().unwrap_or("<unlabeled>"),
                    outcome.exit_code,
                    outcome.stderr.trim()
                );
            }
        }
        self.inner.before_execute(name, params, cwd).await
    }

    async fn after_execute(
        &self,
        name: &str,
        params: &serde_json::Value,
        result: &ToolResult,
        cwd: &Path,
    ) {
        let params_json = serde_json::to_string(params).ok();
        for spec in self.config.for_phase(HookPhase::PostToolUse, Some(name)) {
            let outcome = run_hook(spec, Some(name), params_json.as_deref(), cwd).await;
            if outcome.exit_code != 0 || outcome.timed_out {
                log::warn!(
                    "PostToolUse hook '{}' failed: exit={} stderr={}",
                    spec.label.as_deref().unwrap_or("<unlabeled>"),
                    outcome.exit_code,
                    outcome.stderr.trim()
                );
            }
        }
        self.inner.after_execute(name, params, result, cwd).await;
    }
}
