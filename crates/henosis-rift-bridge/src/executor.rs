//! Agent executor trait -- re-exported from synapse-core.
//!
//! The bridge uses synapse-core's `AgentExecutor` as the contract between
//! the room state machine and any agent runtime (Synapse, ClaudeCode, etc.).
//! All supporting types (DiscussionContext, AgentResponse, TaskContext, etc.)
//! are also re-exported here for convenience.

pub use synapse_core::{
    AgentExecutor, AgentResponse, Capability, ConversationMessage, DiscussionContext,
    ExecutionProposal, ExecutionResult, ExecutionSandbox, HealthStatus, ProgressUpdate,
    TaskContext,
};
