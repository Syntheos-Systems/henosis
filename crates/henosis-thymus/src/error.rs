//! The Thymus error type.

/// A Thymus quality operation failed.
///
/// `#[non_exhaustive]`: variants may grow as more of the Kleos surface (session quality, the
/// judge) is ported into this crate.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ThymusError {
    /// A storage backend operation failed.
    #[error("thymus backend error: {0}")]
    Backend(String),

    /// No rubric with this id is owned by the requesting principal.
    #[error("rubric not found: {0}")]
    RubricNotFound(i64),

    /// No evaluation with this id is owned by the requesting principal.
    #[error("evaluation not found: {0}")]
    EvaluationNotFound(i64),

    /// A rubric still referenced by evaluations cannot be deleted (the evaluations are the
    /// audit record; deleting their rubric would orphan their scores' meaning).
    #[error("rubric {0} is referenced by evaluations and cannot be deleted")]
    RubricInUse(i64),

    /// A caller-supplied value is structurally invalid (e.g. a rubric with no criteria, or
    /// scores missing a criterion).
    #[error("invalid input: {0}")]
    InvalidInput(String),

    /// A token read from storage is not a known enum value.
    #[error("invalid token: {0:?}")]
    InvalidToken(String),
}
