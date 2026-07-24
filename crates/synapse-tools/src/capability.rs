//! `Capability` type used by `PistisGate` to enforce tool access control.
//!
//! This module lives in `synapse-tools` (not `synapse-core`) so the gate layer
//! can use it without creating a reverse dependency. `synapse-core` imports
//! `Capability` from here via `synapse_tools::Capability`.

/// A named capability required for tool execution.
///
/// Capability is a string newtype. The real Pistis grant system will replace
/// this with opaque handles scoped to a specific task; the string representation
/// is kept for serialization and static map lookup in the gate layer.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Capability(pub String);

/// Provides construction and string access for named capabilities.
impl Capability {
    /// Filesystem read access (read, edit, grep, glob, ls, lsp, session).
    pub const FS_READ: &'static str = "fs_read";
    /// Filesystem write access (write and edit).
    pub const FS_WRITE: &'static str = "fs_write";
    /// Shell command execution (bash, delegate, forge_execute).
    pub const BASH: &'static str = "bash";
    /// Outbound network access (web_fetch, web_search, all Kleos tools).
    pub const NETWORK: &'static str = "network";

    /// Construct a capability from any string.
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }

    /// Return the string name of this capability.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Renders a capability as its stable authorization name.
impl std::fmt::Display for Capability {
    /// Write the capability name without additional formatting.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
