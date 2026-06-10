//! The Soma error type.

use syntheos_contracts::PrincipalId;

/// A Soma presence operation failed.
///
/// `#[non_exhaustive]`: variants may grow as more of the Kleos surface (groups, agent logs)
/// is ported into this crate.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SomaError {
    /// A storage backend operation failed.
    #[error("soma backend error: {0}")]
    Backend(String),

    /// No presence row exists for this principal. `get`-style lookups return `Ok(None)`
    /// instead; this is for mutate-by-id paths that require the registration to exist.
    #[error("agent not registered: {0}")]
    NotFound(PrincipalId),

    /// Registration named a principal that does not exist in the canonical directory. Soma is
    /// a projection: it never mints principals (projection convention section 1), so the agent
    /// must be enrolled before it can register presence.
    #[error("principal not enrolled in the directory: {0}")]
    UnknownPrincipal(PrincipalId),

    /// The directory lookup itself failed (storage error in syntheos-identity).
    #[error("principal directory error: {0}")]
    Directory(String),

    /// Another agent in the same tenant already uses this name.
    #[error("agent name already registered in this tenant: {0:?}")]
    NameTaken(String),

    /// A status string read from storage or supplied by a caller is not a known
    /// [`crate::PresenceStatus`].
    #[error("invalid presence status: {0:?}")]
    InvalidStatus(String),

    /// A caller-supplied value is structurally invalid (e.g. an empty name).
    #[error("invalid input: {0}")]
    InvalidInput(String),

    /// The one-time legacy backfill failed (unreadable legacy DB, unparseable legacy row, a
    /// cross-tenant name collision, or a directory enrollment error). Per projection
    /// convention 3.3, bad legacy data is an explicit failure naming the problem -- never
    /// silently discarded.
    #[error("legacy backfill failed: {0}")]
    Backfill(String),
}
