//! The Broca error type.

/// A Broca narration operation failed.
///
/// `#[non_exhaustive]`: variants may grow as more of the Kleos surface (LLM ask, the legacy
/// backfill) is ported into this crate.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum BrocaError {
    /// A storage backend operation failed.
    #[error("broca backend error: {0}")]
    Backend(String),

    /// No action row with this id exists in the tenant. `get`-style lookups return `Ok(None)`
    /// instead; this is for mutate-by-id paths that require the row to exist.
    #[error("action not found: {0}")]
    NotFound(i64),

    /// A caller-supplied value is structurally invalid (e.g. a non-object payload).
    #[error("invalid input: {0}")]
    InvalidInput(String),

    /// The pluggable [`crate::Narrator`] failed to produce a sentence. The action row itself
    /// is unaffected -- narration is decoration, never the record.
    #[error("narration failed: {0}")]
    Narration(String),
}
