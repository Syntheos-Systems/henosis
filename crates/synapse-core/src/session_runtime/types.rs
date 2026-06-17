//! Renderer-agnostic types for the concurrent session runtime.

use serde::Serialize;

use crate::types::AgentEvent;

/// Stable per-process identifier for a session.
pub type SessionId = u64;

/// An `AgentEvent` tagged with the session that produced it. This is what the
/// broadcast channel carries so a single receiver can demultiplex every
/// session's stream.
#[derive(Debug, Clone, Serialize)]
pub struct SessionEvent {
    pub id: SessionId,
    pub event: AgentEvent,
}

/// Lifecycle state of a session, derived from its event stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum SessionStatus {
    /// No turn running; waiting for input.
    Idle,
    /// A turn is running, model is generating.
    Thinking,
    /// A turn is running and a tool is executing (carries the tool name).
    RunningTool(String),
    /// The last turn completed cleanly.
    Done,
    /// The last turn ended in an error (carries the message).
    Error(String),
    /// The in-flight turn was cancelled.
    Cancelled,
}

/// A cheap, cloneable snapshot of a session's live state for rendering.
///
/// Counter semantics differ by field: `input_tokens` and `output_tokens` are
/// accumulated across turns (`AgentEvent::Usage` is per-turn), while `total_usd`
/// is assigned from `AgentEvent::Cost::session_total_usd`, which is already a
/// running session total at its source. Do not switch `total_usd` to `+=`.
#[derive(Debug, Clone, Serialize)]
pub struct SessionSnapshot {
    pub id: SessionId,
    pub label: String,
    pub cwd: String,
    pub status: SessionStatus,
    pub model: String,
    pub turns: u32,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_usd: f64,
}

impl SessionSnapshot {
    /// Apply one agent event to the live state, updating status and counters.
    /// Returns nothing -- mutates in place. This is the single place that maps
    /// `AgentEvent` -> visible state, so the TUI never reimplements it.
    ///
    /// Edge case: if the agent emits `ToolResult` after `Error` (a stream error
    /// with tool calls already buffered), status is overwritten back to
    /// `Thinking`. This mirrors the agent loop's event ordering.
    pub fn apply(&mut self, event: &AgentEvent) {
        match event {
            AgentEvent::TurnStart => {
                self.turns += 1;
                self.status = SessionStatus::Thinking;
            }
            AgentEvent::ToolStart { name, .. } => {
                self.status = SessionStatus::RunningTool(name.clone());
            }
            AgentEvent::ToolResult { .. } => {
                self.status = SessionStatus::Thinking;
            }
            AgentEvent::Usage {
                input_tokens,
                output_tokens,
                ..
            } => {
                self.input_tokens += *input_tokens as u64;
                self.output_tokens += *output_tokens as u64;
            }
            AgentEvent::Cost {
                model,
                session_total_usd,
                ..
            } => {
                self.model = model.clone();
                self.total_usd = *session_total_usd;
            }
            AgentEvent::ModelSwitch { to, .. } => {
                self.model = to.clone();
            }
            AgentEvent::TurnEnd => {
                self.status = SessionStatus::Done;
            }
            AgentEvent::Error(msg) => {
                self.status = SessionStatus::Error(msg.clone());
            }
            AgentEvent::Text(_) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap() -> SessionSnapshot {
        SessionSnapshot {
            id: 1,
            label: "test".into(),
            cwd: "/tmp".into(),
            status: SessionStatus::Idle,
            model: "stub".into(),
            turns: 0,
            input_tokens: 0,
            output_tokens: 0,
            total_usd: 0.0,
        }
    }

    #[test]
    fn apply_tracks_status_and_counters() {
        let mut s = snap();
        s.apply(&AgentEvent::TurnStart);
        assert_eq!(s.status, SessionStatus::Thinking);
        assert_eq!(s.turns, 1);

        s.apply(&AgentEvent::ToolStart {
            id: "x".into(),
            name: "bash".into(),
        });
        assert_eq!(s.status, SessionStatus::RunningTool("bash".into()));

        s.apply(&AgentEvent::ToolResult {
            id: "x".into(),
            content: "ok".into(),
            is_error: false,
        });
        assert_eq!(s.status, SessionStatus::Thinking);

        s.apply(&AgentEvent::Usage {
            input_tokens: 10,
            output_tokens: 5,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
        });
        assert_eq!(s.input_tokens, 10);
        assert_eq!(s.output_tokens, 5);

        s.apply(&AgentEvent::TurnEnd);
        assert_eq!(s.status, SessionStatus::Done);
    }

    #[test]
    fn apply_error_sets_error_status() {
        let mut s = snap();
        s.apply(&AgentEvent::Error("boom".into()));
        assert_eq!(s.status, SessionStatus::Error("boom".into()));
    }

    #[test]
    fn apply_cost_assigns_and_model_switch_updates() {
        let mut s = snap();
        s.apply(&AgentEvent::Cost {
            model: "opus".into(),
            input_tokens: 0,
            output_tokens: 0,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            turn_usd: 0.5,
            session_total_usd: 1.25,
        });
        assert_eq!(s.model, "opus");
        assert_eq!(s.total_usd, 1.25);

        // Cost assigns the running session total -- a second Cost does NOT double it.
        s.apply(&AgentEvent::Cost {
            model: "opus".into(),
            input_tokens: 0,
            output_tokens: 0,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            turn_usd: 0.75,
            session_total_usd: 2.0,
        });
        assert_eq!(s.total_usd, 2.0);

        s.apply(&AgentEvent::ModelSwitch {
            from: "opus".into(),
            to: "haiku".into(),
        });
        assert_eq!(s.model, "haiku");
    }
}
