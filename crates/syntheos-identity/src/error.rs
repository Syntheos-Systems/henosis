//! The directory's error type.

/// An identity-directory operation failed.
///
/// The Phase 0 in-memory directory is infallible; this type exists so a storage-backed
/// implementation (the unit-6 DB decision) can surface backend failures without changing the
/// trait. `#[non_exhaustive]`: variants may grow with real backends.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum DirectoryError {
    /// A storage backend failed. Never produced by the in-memory directory.
    #[error("identity backend error: {0}")]
    Backend(String),
}
