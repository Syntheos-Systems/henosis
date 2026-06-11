//! The Phylax error type.

/// A Phylax credential operation failed.
///
/// `#[non_exhaustive]`: variants grow as the resolve modes and the gate land in later slices.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum PhylaxError {
    /// A storage backend operation failed.
    #[error("phylax backend error: {0}")]
    Backend(String),

    /// No secret with this (tenant, category, name) exists.
    #[error("secret not found: {category}/{name}")]
    SecretNotFound {
        /// The secret's category.
        category: String,
        /// The secret's name.
        name: String,
    },

    /// Encrypting a secret value failed.
    #[error("encryption error: {0}")]
    Encryption(String),

    /// Decrypting a stored secret failed (wrong key, corrupt ciphertext, or a
    /// truncated blob). Deliberately opaque: it never echoes key or plaintext.
    #[error("decryption error: {0}")]
    Decryption(String),

    /// A caller-supplied value was malformed (bad base64, unknown algorithm,
    /// empty derive purpose, oversize length, relative exec path).
    #[error("invalid input: {0}")]
    InvalidInput(String),

    /// The requested operation is not permitted: no policy allows it, the
    /// resolve mode is not in the matched policy's allowed set, or an exec
    /// argv[0] is not on the allowlist. Distinct from a [`PhylaxError`] that
    /// could-not-decide -- this is a definitive deny.
    #[error("permission denied: {0}")]
    PermissionDenied(String),
}
