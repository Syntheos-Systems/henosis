//! The Chiasm error type.

use syntheos_contracts::TaskId;

/// A Chiasm task operation failed.
///
/// `#[non_exhaustive]`: variants may grow as more of the Kleos surface (queue, claims,
/// dependencies) is ported into this crate.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ChiasmError {
    /// A storage backend operation failed.
    #[error("chiasm backend error: {0}")]
    Backend(String),

    /// No task with this id is owned by the requesting principal. `lookup`-style methods return
    /// `Ok(None)` instead; this is for mutate-by-id paths that require the task to exist.
    #[error("task not found: {0}")]
    NotFound(TaskId),

    /// A status string read from storage or supplied by a caller is not a known [`crate::TaskStatus`].
    #[error("invalid task status: {0:?}")]
    InvalidStatus(String),

    /// A task may not depend on itself.
    #[error("task {0} cannot depend on itself")]
    SelfDependency(TaskId),

    /// Adding this dependency edge would close a cycle in the dependency DAG.
    #[error("circular dependency: {task_id} -> {depends_on} creates a cycle")]
    DependencyCycle {
        /// The task the edge was being added to.
        task_id: TaskId,
        /// The dependency target that can already reach `task_id`.
        depends_on: TaskId,
    },
}
