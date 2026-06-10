//! The Loom error type.

use syntheos_contracts::{RunId, WorkflowId};

/// A Loom workflow operation failed.
///
/// `#[non_exhaustive]`: variants may grow as more of the Kleos surface (webhook/LLM steps,
/// the legacy backfill) is ported into this crate.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum LoomError {
    /// A storage backend operation failed.
    #[error("loom backend error: {0}")]
    Backend(String),

    /// No workflow with this id is owned by the requesting principal.
    #[error("workflow not found: {0}")]
    WorkflowNotFound(WorkflowId),

    /// No run with this id is owned by the requesting principal.
    #[error("run not found: {0}")]
    RunNotFound(RunId),

    /// No step with this id exists under the principal's runs.
    #[error("step not found: {0}")]
    StepNotFound(i64),

    /// A workflow definition is structurally invalid: duplicate step names, a `depends_on`
    /// naming a step that does not exist, or a dependency cycle. Caught at definition time so
    /// a run can never deadlock on an unsatisfiable graph (in Kleos it could).
    #[error("invalid workflow definition: {0}")]
    InvalidDefinition(String),

    /// A caller-supplied value is structurally invalid (e.g. completing a step that is not
    /// running, or starting a run of an empty workflow).
    #[error("invalid input: {0}")]
    InvalidInput(String),

    /// A status string read from storage is not a known status token.
    #[error("invalid status: {0:?}")]
    InvalidStatus(String),
}
