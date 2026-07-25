//! Crucible tool registry. Each submodule implements one CLI subcommand.
//! Shared types (`ToolResult`, `ToolError`) and the session-active marker live here.

pub mod approaches;
/// Tree-sitter repo map + code search tools.
pub mod ast;
pub mod comments;
pub mod hypothesis;
pub mod session;
pub mod skills;
pub mod spec;
pub mod stats;
pub mod think;
pub mod verify;

use crate::json_io::Output;

/// Standard return type for every tool: structured `Output` on success, `ToolError` on failure.
pub type ToolResult = Result<Output, ToolError>;

/// Categorised failure modes for tool execution; rendered to the JSON output's `error` field.
#[derive(Debug)]
pub enum ToolError {
    /// A required input field was absent.
    MissingField(String),
    /// An input field was present but unacceptable.
    InvalidValue(String),
    /// The forge database failed.
    DatabaseError(String),
    /// File/process/bridge I/O failed.
    IoError(String),
}

/// Render `ToolError` as a short human string for the CLI's error output.
impl std::fmt::Display for ToolError {
    /// Human-readable form used when an error bubbles to the CLI output.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ToolError::MissingField(s) => write!(f, "Missing required field: {s}"),
            ToolError::InvalidValue(s) => write!(f, "Invalid value: {s}"),
            ToolError::DatabaseError(s) => write!(f, "Database error: {s}"),
            ToolError::IoError(s) => write!(f, "I/O error: {s}"),
        }
    }
}

/// Marker impl so `ToolError` plays nicely with `?` and any `dyn Error` chain.
impl std::error::Error for ToolError {}

/// Mark a forge artifact as the currently-active gate state for external
/// enforcement hooks. Best-effort: failures here must not abort the caller,
/// since the DB record (the source of truth) is already committed.
///
/// Writes `crucible-active` and a legacy compatibility marker containing "<id>:<kind>".
/// `<dir>` prefers `CRUCIBLE_STATE_DIR`, then `AGENT_FORGE_STATE_DIR`, then an existing
/// legacy state directory, and otherwise uses `${XDG_STATE_HOME}/crucible`.
pub fn set_session_active(id: &str, kind: &str) {
    let explicit_dir = std::env::var_os("CRUCIBLE_STATE_DIR")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("AGENT_FORGE_STATE_DIR").map(std::path::PathBuf::from));
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| home.as_ref().map(|path| path.join(".local").join("state")));
    let dir = explicit_dir.or_else(|| {
        base.map(|base| {
            let new_default = base.join("crucible");
            let legacy_default = base.join("agent-forge");
            if new_default.exists() || !legacy_default.exists() {
                new_default
            } else {
                legacy_default
            }
        })
    });
    let Some(dir) = dir else {
        return;
    };
    let _ = std::fs::create_dir_all(&dir);
    let payload = format!("{id}:{kind}");
    let _ = std::fs::write(dir.join("crucible-active"), &payload);
    let _ = std::fs::write(dir.join("agent-forge-active"), payload);
}
