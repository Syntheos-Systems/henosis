//! The execution seam: what actually performs an action that has cleared the gate chain.

use async_trait::async_trait;
use syntheos_contracts::{RequestContext, ToolInvocation};

/// Performs an action that has passed the full gate chain.
///
/// Object-safe via `async_trait` so the dispatcher can hold `Box<dyn Executor>`. Phase 0 ships
/// the fail-closed [`crate::deny::DenyExecutor`] (tests use the feature-gated
/// `stubs::EchoExecutor`); real executors (Hermes, Synapse) wire in later by trait object.
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
    /// Stable internal classification used by typed callers.
    kind: ExecutorErrorKind,
}

/// Internal execution failure classification that never crosses the wire directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExecutorErrorKind {
    /// General execution or authority dependency failure.
    Failure,
    /// Caller retry identity conflicts with an earlier canonical request.
    Conflict,
}

/// Constructs and classifies dispatcher execution failures.
impl ExecutorError {
    /// Build an [`ExecutorError`] from anything string-like.
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            kind: ExecutorErrorKind::Failure,
        }
    }

    /// Build a request-conflict error without exposing backend details.
    pub fn conflict(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            kind: ExecutorErrorKind::Conflict,
        }
    }

    /// Return whether this error represents conflicting reuse of caller retry identity.
    pub fn is_conflict(&self) -> bool {
        self.kind == ExecutorErrorKind::Conflict
    }
}

#[cfg(test)]
/// Tests typed execution-error classification.
mod tests {
    use super::ExecutorError;

    /// Conflict construction remains distinct from a general execution failure.
    #[test]
    fn conflict_classification_is_explicit() {
        assert!(!ExecutorError::new("failure").is_conflict());
        assert!(ExecutorError::conflict("conflict").is_conflict());
    }
}
