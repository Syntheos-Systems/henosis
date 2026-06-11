//! Error type for the durable Axon sidecar.

/// What can go wrong persisting, replaying, or consuming durable events.
#[derive(Debug, thiserror::Error)]
pub enum DurableAxonError {
    /// The SQLite backend failed (or a stored row failed to parse back).
    #[error("durable axon backend: {0}")]
    Backend(String),
    /// The caller's input was structurally invalid (e.g. an empty consumer name).
    #[error("invalid input: {0}")]
    InvalidInput(String),
}
