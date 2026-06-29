//! Per-turn checkpoint written to Kleos after every orchestrator iteration.
//! On crash recovery the latest checkpoint feeds back into the orchestrator
//! `resume` path so the loop picks up where it left off.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A snapshot of an in-flight agent loop. Written to Kleos after every turn
/// so a crash mid-loop can be replayed from the last good state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Checkpoint {
    /// Task this checkpoint belongs to.
    pub task_id: String,
    /// Zero-based turn index at the time this checkpoint was written.
    pub step: u32,
    /// Full conversation history up to and including this turn.
    pub messages: Vec<Value>,
    /// Concatenated assistant text accumulated so far across all turns.
    pub accumulated_text: String,
    /// Tenant context forwarded into the provider auth chain.
    pub tenant_id: Option<String>,
    /// Extra system prompt text provided at task submission time.
    pub system: Option<String>,
    /// Set when the task is suspended waiting for human input.
    pub paused: Option<PausedState>,
    /// Wall-clock time this checkpoint was written. Used to pick the latest
    /// checkpoint when Kleos returns multiple hits for the same task.
    pub created_at: DateTime<Utc>,
}

/// Set when the agent is suspended on `ask_human` and the operator has not
/// yet replied. Lets startup recovery rebuild the pause state and re-arm the
/// resume oneshot so a later `POST /tasks/{id}/resume` still works.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PausedState {
    /// The question the model asked the human operator.
    pub question: String,
    /// Anthropic tool_use_id for the `ask_human` call. Needed to construct
    /// the `tool_result` block when the human responds.
    pub tool_use_id: String,
}

impl Checkpoint {
    /// Unique Kleos tag for a task's checkpoints. Used as the search key so
    /// `kleos_load_latest_checkpoint` can find all checkpoints for a task
    /// without scanning unrelated memories.
    pub fn unique_tag(task_id: &str) -> String {
        format!("checkpoint:{task_id}")
    }
}
