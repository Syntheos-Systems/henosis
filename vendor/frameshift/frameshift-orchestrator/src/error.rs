//! Error types for the orchestrator subsystem.

use thiserror::Error;

/// All errors that can occur within the orchestrator subsystem.
#[derive(Debug, Error)]
pub enum OrchestratorError {
    /// An I/O operation failed (file read/write for state persistence).
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// JSON serialization or deserialization failed.
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    /// Loading a persona source directory failed.
    #[error("persona source load failed: {0}")]
    SourceLoad(#[from] frameshift_source::SourceError),

    /// TOML deserialization failed (pack.toml parsing).
    #[error("TOML parse error: {0}")]
    TomlDeserialize(#[from] toml::de::Error),
}
