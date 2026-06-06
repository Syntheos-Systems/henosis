//! The execution seam: what actually performs an action that has cleared the gate chain.

use async_trait::async_trait;
use syntheos_contracts::{RequestContext, ToolInvocation};

/// Performs an action that has passed the full gate chain.
///
/// Object-safe via `async_trait` so the dispatcher can hold `Box<dyn Executor>`. Phase 0 ships
/// [`crate::stubs::EchoExecutor`]; real executors (Hermes, Synapse) wire in later by trait object.
#[async_trait]
pub trait Executor: Send + Sync {
    /// Execute `invocation` in `ctx`, returning its result as JSON or an [`ExecutorError`].
    async fn execute(
        &self,
        ctx: &RequestContext,
        invocation: &ToolInvocation,
    ) -> Result<serde_json::Value, ExecutorError>;
}

/// An action failed to execute.
///
/// Carries a human-readable message; a real executor maps its own error type into this at the
/// boundary. Richer, structured typing arrives with the real executors.
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct ExecutorError {
    /// Human-readable description of the failure.
    pub message: String,
}

impl ExecutorError {
    /// Build an [`ExecutorError`] from anything string-like.
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}
