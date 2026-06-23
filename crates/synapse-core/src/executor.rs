//! `AgentExecutor` trait and supporting types for the Rift bridge contract.
//!
//! Rift-bridge calls into Synapse (and other runtimes) through this trait.
//! The trait is defined here; no Rift code lives in Synapse. The concrete
//! `SynapseExecutor` implementation lives in `crate::executors::synapse_executor`.
//!
//! ## Design notes
//!
//! - `discuss` is intentionally lightweight: no tool access, one LLM round-trip.
//! - `execute` is full agent loop with tool access; progress flows back over a channel.
//! - `required_capabilities` and `sandbox` are declared before any spawn so
//!   the bridge can validate Pistis grants up-front and refuse early.
//! - All error types use `anyhow::Error` at this layer; library callers can
//!   wrap into their own error types as needed.
//!
//! `Capability` is defined in `synapse_tools::capability` (the gate layer) and
//! re-exported here for convenience. It lives there to avoid a reverse dependency
//! from synapse-tools back to synapse-core.

use std::path::PathBuf;

use anyhow::Result;
use async_trait::async_trait;
use tokio::sync::mpsc;

// Re-export Capability from synapse-tools so callers of synapse-core don't need
// to import synapse-tools directly for the type.
pub use synapse_tools::Capability;

// ---------------------------------------------------------------------------
// ExecutionSandbox
// ---------------------------------------------------------------------------

/// Declared sandbox parameters for an execution session.
///
/// The bridge refuses to spawn an executor without a declared sandbox. These
/// values constrain what the executor may do; enforcement is the bridge's
/// responsibility (and eventually Pistis's).
#[derive(Debug, Clone)]
pub struct ExecutionSandbox {
    /// Git branch the executor works on. Must follow the naming convention
    /// `agent/{agent_id}/{chiasm_task_id}`. Bridge creates/destroys it.
    pub branch: String,
    /// Working directory for all tool executions within this session.
    pub working_dir: PathBuf,
    /// Wall-clock time limit in seconds for the full execution session.
    /// Zero means no limit (not recommended for production).
    pub max_runtime_secs: u64,
    /// Optional `CARGO_TARGET_DIR` for the session, sourced from the workspace
    /// config. Executors that shell out to build tooling export this so cargo
    /// artifacts land off the source tree. `None` leaves the environment unset.
    pub cargo_target_dir: Option<PathBuf>,
}

// ---------------------------------------------------------------------------
// DiscussionContext
// ---------------------------------------------------------------------------

/// A single entry in the recent conversation history.
#[derive(Debug, Clone)]
pub struct ConversationMessage {
    /// Author's display name.
    pub author: String,
    /// Raw message text.
    pub text: String,
    /// Unix timestamp (seconds).
    pub timestamp_secs: i64,
}

/// All context the bridge assembles for one discussion turn.
///
/// The executor uses this to decide whether to respond and what to say.
/// Fields map to the "Context Assembly" section of the Rift design doc.
#[derive(Debug, Clone)]
pub struct DiscussionContext {
    /// Recent messages in the channel, oldest first.
    pub recent_messages: Vec<ConversationMessage>,
    /// The active persona name for this thread (thread-stable; set by Frameshift).
    pub persona_name: Option<String>,
    /// Relevant memories retrieved from Kleos for the current topic.
    pub relevant_memories: Vec<String>,
    /// Summary of active Chiasm tasks visible to the room.
    pub active_tasks_summary: Option<String>,
    /// Name or ID of the channel/thread this discussion is happening in.
    pub channel_id: String,
    /// Optional system framing to prepend (e.g., persona instruction).
    pub system_framing: Option<String>,
}

// ---------------------------------------------------------------------------
// AgentResponse
// ---------------------------------------------------------------------------

/// The executor's response to a discussion turn.
///
/// `None` from `discuss` means the executor passes (does not respond).
/// When present, this carries the assistant text for posting to the room.
#[derive(Debug, Clone)]
pub struct AgentResponse {
    /// Text content to post to the room.
    pub text: String,
    /// If the executor is proposing to enter execution mode, this carries
    /// the proposed scope. The bridge handles the actual mode-switch protocol.
    pub execution_proposal: Option<ExecutionProposal>,
}

/// A structured proposal from an agent to take on a task.
///
/// Natural language is NOT used to trigger execution -- this struct is the
/// structured extraction. The bridge validates it and routes it to Chiasm
/// for atomic claiming and human approval.
#[derive(Debug, Clone)]
pub struct ExecutionProposal {
    /// Human-readable summary of the proposed work.
    pub scope_summary: String,
    /// Rough effort estimate (e.g., "15 minutes", "1 hour").
    pub estimated_effort: Option<String>,
    /// Capabilities the executor will need for this task.
    pub required_capabilities: Vec<Capability>,
}

// ---------------------------------------------------------------------------
// TaskContext
// ---------------------------------------------------------------------------

/// Full context provided to the executor when it enters execution mode.
///
/// Assembled by the bridge after Chiasm task creation and human approval.
/// Maps to the "Execution isolation" and "Context Assembly" sections of the design.
#[derive(Debug, Clone)]
pub struct TaskContext {
    /// Chiasm task ID for this execution session.
    pub task_id: String,
    /// The task description as approved by the human.
    pub description: String,
    /// Sandbox parameters (branch, working dir, time limit).
    pub sandbox: ExecutionSandbox,
    /// Pistis-granted capabilities for this specific task. The executor should
    /// not attempt operations outside this set.
    pub granted_capabilities: Vec<Capability>,
    /// Optional initial context: partial work on the branch from a previous
    /// attempt (crash recovery path).
    pub prior_context: Option<String>,
}

// ---------------------------------------------------------------------------
// ProgressUpdate
// ---------------------------------------------------------------------------

/// A progress event sent by the executor back to the bridge during execution.
///
/// The bridge rate-limits these to the room (1 per 30 seconds) and passes
/// them through gate_guard before posting. The executor may send them freely.
#[derive(Debug, Clone)]
pub enum ProgressUpdate {
    /// Informational message for the room (e.g., "Running tests...").
    Message(String),
    /// A tool was called. Name is the tool identifier.
    ToolStarted { tool_name: String },
    /// A tool completed. Carries whether it errored.
    ToolCompleted { tool_name: String, is_error: bool },
    /// Execution is done. Final status carried in `ExecutionResult`.
    Done,
    /// Execution failed unrecoverably. The bridge handles crash protocol.
    Failed(String),
}

// ---------------------------------------------------------------------------
// ExecutionResult
// ---------------------------------------------------------------------------

/// Outcome of a completed execution session.
#[derive(Debug, Clone)]
pub enum ExecutionResult {
    /// Execution completed successfully.
    Success {
        /// Human-readable summary of what was accomplished.
        summary: String,
        /// Git commit hash on the agent branch (if a commit was made).
        commit_hash: Option<String>,
        /// Test output or other verification evidence.
        evidence: Option<String>,
    },
    /// Execution was aborted or could not complete.
    Failed {
        /// Reason for failure.
        reason: String,
        /// Whether partial work was left on the branch.
        partial_work: bool,
    },
}

// ---------------------------------------------------------------------------
// HealthStatus
// ---------------------------------------------------------------------------

/// Reported health of an executor runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HealthStatus {
    /// Runtime is ready to accept work.
    Ready,
    /// Runtime is degraded but may still function (e.g., provider latency).
    Degraded(String),
    /// Runtime is unavailable. The bridge should not attempt to spawn.
    Unavailable(String),
}

// ---------------------------------------------------------------------------
// AgentExecutor trait
// ---------------------------------------------------------------------------

/// The primary contract between Rift-bridge and any agent runtime.
///
/// The bridge discovers capability requirements and sandbox parameters before
/// spawning (`required_capabilities`, `sandbox`), then calls `discuss` for
/// lightweight turns and `execute` for full task sessions. Implementations
/// must be `Send + Sync` because the bridge holds them behind `Arc`.
///
/// Implementations: `SynapseExecutor` (this crate), `HephaestusExecutor`
/// (hephaestus crate), `ClaudeCodeExecutor` (rift-bridge crate).
#[async_trait]
pub trait AgentExecutor: Send + Sync {
    /// Declare which capabilities this executor requires.
    ///
    /// The bridge validates this list against the agent's current Pistis grants
    /// before any `execute` call. If any required capability is missing, the
    /// bridge blocks execution and notifies the room without calling `execute`.
    fn required_capabilities(&self) -> Vec<Capability>;

    /// Declare the sandbox parameters for an execution session.
    ///
    /// The bridge uses this to enforce branch isolation and time limits. An
    /// executor without a declared sandbox is refused by the bridge.
    fn sandbox(&self) -> ExecutionSandbox;

    /// Run one discussion turn.
    ///
    /// Receives assembled conversation context and returns an optional response.
    /// Returning `None` signals that this agent passes on this turn.
    /// Discussion turns are lightweight -- no tool access, one LLM round-trip.
    async fn discuss(&self, context: DiscussionContext) -> Result<Option<AgentResponse>>;

    /// Run a full execution session for an approved task.
    ///
    /// `task` carries the Chiasm task description, sandbox, and Pistis-granted
    /// capabilities. Progress events are sent back to the bridge via
    /// `progress_tx`; the bridge rate-limits them before posting to the room.
    ///
    /// The executor runs until it completes, fails, or is cancelled (via the
    /// `progress_tx` becoming closed, which signals the bridge dropped the
    /// receiver -- treat it as an abort signal).
    async fn execute(
        &self,
        task: TaskContext,
        progress_tx: mpsc::Sender<ProgressUpdate>,
    ) -> Result<ExecutionResult>;

    /// Check whether the executor runtime is healthy.
    ///
    /// Called by the bridge before spawning and on a periodic heartbeat.
    /// A `Degraded` or `Unavailable` result prevents new spawns.
    async fn health_check(&self) -> Result<HealthStatus>;
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Verify all supporting types can be constructed without panicking.
    #[test]
    fn types_construct() {
        let cap = Capability::new(Capability::FS_READ);
        assert_eq!(cap.as_str(), "fs_read");

        let sandbox = ExecutionSandbox {
            branch: "agent/a1/t1".into(),
            working_dir: PathBuf::from("/tmp"),
            max_runtime_secs: 300,
            cargo_target_dir: None,
        };
        assert_eq!(sandbox.max_runtime_secs, 300);

        let ctx = DiscussionContext {
            recent_messages: vec![ConversationMessage {
                author: "alice".into(),
                text: "hello".into(),
                timestamp_secs: 0,
            }],
            persona_name: Some("Architect".into()),
            relevant_memories: vec![],
            active_tasks_summary: None,
            channel_id: "general".into(),
            system_framing: None,
        };
        assert_eq!(ctx.channel_id, "general");

        let resp = AgentResponse {
            text: "I agree.".into(),
            execution_proposal: None,
        };
        assert!(resp.execution_proposal.is_none());

        let task = TaskContext {
            task_id: "t-001".into(),
            description: "Write the thing".into(),
            sandbox: ExecutionSandbox {
                branch: "agent/a1/t-001".into(),
                working_dir: PathBuf::from("/tmp/work"),
                max_runtime_secs: 600,
                cargo_target_dir: None,
            },
            granted_capabilities: vec![Capability::new(Capability::BASH)],
            prior_context: None,
        };
        assert_eq!(task.task_id, "t-001");

        let update = ProgressUpdate::Message("running tests".into());
        assert!(matches!(update, ProgressUpdate::Message(_)));

        let result = ExecutionResult::Success {
            summary: "done".into(),
            commit_hash: Some("abc123".into()),
            evidence: None,
        };
        assert!(matches!(result, ExecutionResult::Success { .. }));

        assert_eq!(HealthStatus::Ready, HealthStatus::Ready);
    }

    /// Verify `Capability` display and equality.
    #[test]
    fn capability_display() {
        let a = Capability::new("bash");
        let b = Capability::new("bash");
        assert_eq!(a, b);
        assert_eq!(format!("{a}"), "bash");
    }
}
