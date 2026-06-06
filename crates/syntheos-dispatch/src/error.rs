//! The dispatcher's error type: reserved for an action genuinely failing to run.

use crate::executor::ExecutorError;

/// A dispatch failed.
///
/// Authorization decisions (deny, approval) are not errors -- they ride
/// [`crate::DispatchOutcome`]. `DispatchError` is for the action failing to execute after it
/// cleared every gate.
///
/// `#[non_exhaustive]`: failure modes may grow as real executors and a durable audit path land.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum DispatchError {
    /// The action passed every gate but failed during execution.
    #[error("action execution failed: {0}")]
    Execution(#[from] ExecutorError),
}
