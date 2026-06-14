use thiserror::Error;

/// Top-level error type for the memory client.
#[derive(Debug, Error)]
pub enum MemoryClientError {
    /// An internal, unexpected error.
    #[error("internal error: {0}")]
    Internal(String),
    /// The caller supplied an invalid input.
    #[error("invalid input: {0}")]
    InvalidInput(String),
}

/// Convenience Result alias for this crate.
pub type Result<T> = std::result::Result<T, MemoryClientError>;
