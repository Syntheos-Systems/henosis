//! Durable execution-boundary hooks for audit and exact-once approval enforcement.

use async_trait::async_trait;
use serde_json::Value;
use syntheos_contracts::GateRequest;

use crate::executor::ExecutorError;

/// Durable pre-execution decision for one idempotent request.
#[derive(Debug, Clone, PartialEq)]
pub enum ExecutionDecision {
    /// The guard acquired a new durable claim, so the executor may run once.
    Execute,
    /// The same request completed earlier and its final filtered result can be replayed.
    Cached(Value),
}

/// Terminal executor result presented to the durable execution guard.
#[derive(Debug, Clone, PartialEq)]
pub enum ExecutionOutcome {
    /// The executor returned a value after output policy filtering.
    Succeeded {
        /// Final filtered value safe to persist and replay without re-filtering.
        result: Value,
    },
    /// The executor returned an error.
    Failed,
}

/// Guards the final boundary immediately before and after authorized execution.
#[async_trait]
pub trait ExecutionGuard: Send + Sync {
    /// Claims the request before its side effect and returns the only permitted next action.
    ///
    /// Implementations return [`ExecutionDecision::Execute`] only for a newly acquired claim,
    /// return [`ExecutionDecision::Cached`] only for a completed matching request, and reject
    /// matching claimed or indeterminate records without invoking the executor.
    async fn before_execute(
        &self,
        request: &GateRequest,
        allowed_gates: &[String],
    ) -> Result<ExecutionDecision, ExecutorError>;

    /// Persists a terminal outcome before the dispatcher reports completion or failure.
    ///
    /// A successful outcome contains the final filtered value that can be replayed directly.
    async fn after_execute(
        &self,
        request: &GateRequest,
        outcome: ExecutionOutcome,
    ) -> Result<(), ExecutorError>;

    /// Persists an indeterminate marker when a claimed execution lacks a durable completion.
    async fn mark_indeterminate(&self, request: &GateRequest) -> Result<(), ExecutorError>;
}
