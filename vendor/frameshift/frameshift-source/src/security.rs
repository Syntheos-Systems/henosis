//! Security foundation types for the Frameshift trust and capability model.
//!
//! This module defines data types that the CLI, registry, and runtime layers
//! implement against. No implementations of security enforcement live here --
//! only the shapes. Coverage:
//!
//! - SD1/SG1.1 + SG1.3: Capability manifest audit (`ManifestAudit`, `audit_manifest`)
//! - SD3/SG3.2: Trust display types (`TrustLevel`, `TrustSummary`, `CapabilitySummary`)
//! - SD4/SG4.1-4.2: Key pinning TOFU model (`PinnedKey`, `KeyPinCheck`)
//! - SD7/SG7.1: Key revocation list entries (`RevocationEntry`, `RevocationCheck`)
//! - SD8/SG8.1: Growth file security (`GrowthFilePermissions`, `is_growth_file`)

use serde::{Deserialize, Serialize};

// ---- SD1/SG1.1 + SG1.3: Capability Manifest Validation ----------------------

/// Result of validating a capability manifest against safety heuristics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestAudit {
    /// Findings from the audit. Empty means no concerns were raised.
    pub findings: Vec<ManifestFinding>,
}

/// A single finding from a capability manifest audit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestFinding {
    /// What aspect of the manifest triggered this finding.
    pub aspect: ManifestAspect,
    /// Severity of this finding.
    pub severity: ManifestSeverity,
    /// Human-readable description of why this finding was raised.
    pub detail: String,
}

/// Which aspect of the manifest is being flagged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManifestAspect {
    /// The `filesystem_scope` field is overly broad.
    FilesystemScope,
    /// Network egress is requested.
    NetworkEgress,
    /// A value in `required_tools` raises concerns.
    RequiredTool,
}

/// Severity levels for manifest findings, ordered least to most severe.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ManifestSeverity {
    /// Worth noting but not blocking installation.
    Info,
    /// Should be reviewed before installation proceeds.
    Warning,
    /// Blocks installation without explicit user override.
    Block,
}

/// Shell-execution tool names that raise a `Warning` finding when declared as
/// required tools. These can be used to escape capability sandboxing.
const SHELL_TOOLS: &[&str] = &["bash", "sh", "exec", "zsh", "fish", "cmd", "powershell"];

/// Audits a capability manifest for overly broad or suspicious declarations.
///
/// Checks:
/// - `filesystem_scope` of `/`, `/**`, or `~/**` → `Block` (full-system access)
/// - `filesystem_scope` matching patterns that target hidden home dirs → `Warning`
/// - `network_egress = true` → `Warning` (always flagged for human review)
/// - `required_tools` containing shell execution tools → `Warning`
pub fn audit_manifest(manifest: &crate::persona::CapabilityManifest) -> ManifestAudit {
    let mut findings = Vec::new();

    // Check filesystem scope for overly broad patterns that grant full access.
    let scope = manifest.filesystem_scope.trim();
    if scope == "/" || scope == "/**" || scope == "~/**" {
        findings.push(ManifestFinding {
            aspect: ManifestAspect::FilesystemScope,
            severity: ManifestSeverity::Block,
            detail: format!("filesystem_scope '{scope}' grants unrestricted filesystem access"),
        });
    } else if scope.contains("~/.")
        || scope.starts_with("/etc")
        || scope.starts_with("/root")
        || scope.starts_with("/home/**")
    {
        // Patterns that hit sensitive hidden directories or all home dirs.
        findings.push(ManifestFinding {
            aspect: ManifestAspect::FilesystemScope,
            severity: ManifestSeverity::Warning,
            detail: format!("filesystem_scope '{scope}' may expose sensitive directories"),
        });
    }

    // Network egress is always flagged for review -- it's an opt-in capability
    // that should be explicitly acknowledged during installation.
    if manifest.network_egress {
        findings.push(ManifestFinding {
            aspect: ManifestAspect::NetworkEgress,
            severity: ManifestSeverity::Warning,
            detail: "persona requests outbound network access".to_string(),
        });
    }

    // Warn on any required tool that provides shell execution, which can be
    // used to bypass declared filesystem or network restrictions.
    for tool in &manifest.required_tools {
        let tool_lower = tool.to_lowercase();
        if SHELL_TOOLS.contains(&tool_lower.as_str()) {
            findings.push(ManifestFinding {
                aspect: ManifestAspect::RequiredTool,
                severity: ManifestSeverity::Warning,
                detail: format!(
                    "required tool '{tool}' provides shell execution and may bypass sandboxing"
                ),
            });
        }
    }

    ManifestAudit { findings }
}

// ---- SD3/SG3.2: Trust Display Types -----------------------------------------

/// Trust level assigned to a pack based on its authorship and signing.
///
/// Levels are ordered: `Unknown < Community < Curated`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum TrustLevel {
    /// Unknown author with no signature verification.
    Unknown,
    /// Community pack signed with a software Ed25519 key.
    Community,
    /// Curated pack signed with a hardware-bound Ed25519 key (e.g. a hardware security key).
    Curated,
}

/// Summary of a pack's trust signals rendered to the user during installation.
///
/// The CLI formats this struct as a pre-install prompt so the user can make an
/// informed decision before accepting the pack.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrustSummary {
    /// Pack name as declared in `pack.toml`.
    pub pack_name: String,
    /// Semver version string of the pack.
    pub version: String,
    /// Author handle from the registry.
    pub author: String,
    /// Trust level derived from signing and curation status.
    pub trust_level: TrustLevel,
    /// Whether the signing key is hardware-bound (e.g. lives on a hardware security key or smartcard).
    pub hardware_bound: bool,
    /// Condensed view of the capability manifest, if the pack declares one.
    pub capabilities: Option<CapabilitySummary>,
}

/// Condensed view of a capability manifest for display in a trust prompt.
///
/// Omits detail that is not meaningful to a user skimming an install prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapabilitySummary {
    /// Filesystem scope requested by the pack.
    pub filesystem_scope: String,
    /// Whether the pack requests outbound network access.
    pub network_egress: bool,
    /// Number of tools declared in `required_tools`.
    pub tool_count: usize,
}

// ---- SD4/SG4.1-4.2: Key Pinning (TOFU Model) --------------------------------

/// A pinned author public key following the SSH `known_hosts` trust model.
///
/// The first install from an author pins their public key locally. Subsequent
/// installs verify the presented key against the pinned value. A mismatch
/// requires explicit user acceptance and is surfaced as a high-severity alert.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PinnedKey {
    /// Author handle this key is bound to.
    pub author: String,
    /// Hex-encoded Ed25519 public key bytes.
    pub public_key: String,
    /// ISO 8601 timestamp of when this key was first pinned locally.
    pub first_seen: String,
    /// ISO 8601 timestamp of the most recent successful verification.
    pub last_verified: String,
}

/// Result of checking an author's presented key against the local pin store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyPinCheck {
    /// No pinned key exists for this author. Treat as a first-time install.
    FirstSeen,
    /// The presented key matches the locally pinned key. Installation may proceed.
    Trusted,
    /// The presented key does NOT match the pinned key. Installation must be
    /// blocked or require explicit user override.
    KeyChanged {
        /// The hex-encoded key that was previously pinned.
        pinned: String,
        /// The hex-encoded key being presented by the pack.
        presented: String,
    },
}

// ---- SD7/SG7.1: Key Revocation ----------------------------------------------

/// An entry in the key revocation list (KRL).
///
/// The CLI fetches the KRL from the registry on install and sync, and rejects
/// any pack whose signing key appears in this list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RevocationEntry {
    /// Hex-encoded public key that has been revoked.
    pub public_key: String,
    /// ISO 8601 timestamp of when the revocation was issued by the registry.
    pub revoked_at: String,
    /// Human-readable reason for revocation (e.g. "key compromise").
    pub reason: String,
    /// Author handle that held this key at the time of revocation.
    pub author: String,
}

/// Result of checking a key against the revocation list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RevocationCheck {
    /// The key does not appear in the revocation list.
    NotRevoked,
    /// The key has been revoked. Installation must be blocked.
    Revoked {
        /// ISO 8601 timestamp from the revocation entry.
        revoked_at: String,
        /// Reason for revocation from the registry.
        reason: String,
    },
}

// ---- SD8/SG8.1: Growth File Security ----------------------------------------

/// Permission settings for growth files written to the central install root.
///
/// Growth files accumulate project-specific learnings over sessions. They must
/// never flow upstream into packs and must never be world-readable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GrowthFilePermissions {
    /// Unix file mode. Default is `0o600` (owner read/write only).
    pub mode: u32,
}

/// Provides owner-only permissions for newly created growth files.
impl Default for GrowthFilePermissions {
    /// Returns the safe default: `0o600` (owner read/write, no group/other access).
    fn default() -> Self {
        Self { mode: 0o600 }
    }
}

/// Checks whether a filename matches a growth file naming convention.
///
/// Growth files must be excluded from pack builds and registry uploads.
/// Returns `true` for `GROWTH.md`, `private.md`, `shared.md`,
/// `candidates.md`, and `entities.toml` (case-insensitive).
pub fn is_growth_file(filename: &str) -> bool {
    matches!(
        filename.to_lowercase().as_str(),
        "growth.md" | "private.md" | "shared.md" | "candidates.md" | "entities.toml"
    )
}

// ---- Tests ------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persona::CapabilityManifest;

    /// Helper that builds a minimal manifest with no concerning fields set.
    fn safe_manifest() -> CapabilityManifest {
        CapabilityManifest {
            required_tools: vec!["read_file".to_string()],
            filesystem_scope: "~/projects/myapp/**".to_string(),
            network_egress: false,
            primary_intents: vec![],
            anti_keywords: vec![],
        }
    }

    // -- audit_manifest tests -------------------------------------------------

    /// Broad filesystem scope "/" must produce a Block finding.
    #[test]
    fn audit_broad_scope_root_is_block() {
        let manifest = CapabilityManifest {
            filesystem_scope: "/".to_string(),
            ..safe_manifest()
        };
        let audit = audit_manifest(&manifest);
        assert!(
            audit
                .findings
                .iter()
                .any(|f| f.aspect == ManifestAspect::FilesystemScope
                    && f.severity == ManifestSeverity::Block),
            "expected a Block finding for scope '/'"
        );
    }

    /// Broad filesystem scope "/**" must produce a Block finding.
    #[test]
    fn audit_broad_scope_double_star_is_block() {
        let manifest = CapabilityManifest {
            filesystem_scope: "/**".to_string(),
            ..safe_manifest()
        };
        let audit = audit_manifest(&manifest);
        assert!(
            audit
                .findings
                .iter()
                .any(|f| f.aspect == ManifestAspect::FilesystemScope
                    && f.severity == ManifestSeverity::Block),
            "expected a Block finding for scope '/**'"
        );
    }

    /// Broad filesystem scope "~/**" must produce a Block finding.
    #[test]
    fn audit_broad_scope_home_double_star_is_block() {
        let manifest = CapabilityManifest {
            filesystem_scope: "~/**".to_string(),
            ..safe_manifest()
        };
        let audit = audit_manifest(&manifest);
        assert!(
            audit
                .findings
                .iter()
                .any(|f| f.aspect == ManifestAspect::FilesystemScope
                    && f.severity == ManifestSeverity::Block),
            "expected a Block finding for scope '~/**'"
        );
    }

    /// network_egress = true must produce a Warning finding.
    #[test]
    fn audit_network_egress_is_warning() {
        let manifest = CapabilityManifest {
            network_egress: true,
            ..safe_manifest()
        };
        let audit = audit_manifest(&manifest);
        assert!(
            audit
                .findings
                .iter()
                .any(|f| f.aspect == ManifestAspect::NetworkEgress
                    && f.severity == ManifestSeverity::Warning),
            "expected a Warning finding for network_egress"
        );
    }

    /// Shell tool "bash" in required_tools must produce a Warning finding.
    #[test]
    fn audit_shell_tool_is_warning() {
        let manifest = CapabilityManifest {
            required_tools: vec!["bash".to_string()],
            ..safe_manifest()
        };
        let audit = audit_manifest(&manifest);
        assert!(
            audit
                .findings
                .iter()
                .any(|f| f.aspect == ManifestAspect::RequiredTool
                    && f.severity == ManifestSeverity::Warning),
            "expected a Warning finding for required tool 'bash'"
        );
    }

    /// A reasonable, non-suspicious manifest must produce zero findings.
    #[test]
    fn audit_clean_manifest_no_findings() {
        let manifest = safe_manifest();
        let audit = audit_manifest(&manifest);
        assert!(
            audit.findings.is_empty(),
            "expected zero findings for a safe manifest, got: {:?}",
            audit.findings
        );
    }

    // -- is_growth_file tests -------------------------------------------------

    /// Canonical growth filenames must be identified correctly.
    #[test]
    fn is_growth_file_recognizes_growth_filenames() {
        for name in &[
            "GROWTH.md",
            "growth.md",
            "private.md",
            "shared.md",
            "candidates.md",
            "entities.toml",
        ] {
            assert!(
                is_growth_file(name),
                "'{name}' should be recognized as a growth file"
            );
        }
    }

    /// Non-growth filenames must not be misidentified.
    #[test]
    fn is_growth_file_rejects_non_growth_filenames() {
        for name in &["README.md", "persona.toml", "rules.toml", "main.rs", ""] {
            assert!(
                !is_growth_file(name),
                "'{name}' should NOT be recognized as a growth file"
            );
        }
    }

    // -- GrowthFilePermissions tests ------------------------------------------

    /// Default permissions must be 0o600 (owner read/write only).
    #[test]
    fn growth_file_permissions_default_is_0o600() {
        let perms = GrowthFilePermissions::default();
        assert_eq!(
            perms.mode, 0o600,
            "expected default mode 0o600, got 0o{:o}",
            perms.mode
        );
    }

    // -- KeyPinCheck construction tests ---------------------------------------

    /// All three KeyPinCheck variants must construct without issue.
    #[test]
    fn key_pin_check_variants_construct() {
        let first = KeyPinCheck::FirstSeen;
        let trusted = KeyPinCheck::Trusted;
        let changed = KeyPinCheck::KeyChanged {
            pinned: "aabb".to_string(),
            presented: "ccdd".to_string(),
        };

        assert_eq!(first, KeyPinCheck::FirstSeen);
        assert_eq!(trusted, KeyPinCheck::Trusted);
        assert_eq!(
            changed,
            KeyPinCheck::KeyChanged {
                pinned: "aabb".to_string(),
                presented: "ccdd".to_string()
            }
        );
    }

    // -- RevocationCheck construction tests -----------------------------------

    /// Both RevocationCheck variants must construct without issue.
    #[test]
    fn revocation_check_variants_construct() {
        let not_revoked = RevocationCheck::NotRevoked;
        let revoked = RevocationCheck::Revoked {
            revoked_at: "2025-01-01T00:00:00Z".to_string(),
            reason: "key compromise".to_string(),
        };

        assert_eq!(not_revoked, RevocationCheck::NotRevoked);
        assert_eq!(
            revoked,
            RevocationCheck::Revoked {
                revoked_at: "2025-01-01T00:00:00Z".to_string(),
                reason: "key compromise".to_string()
            }
        );
    }

    // -- TrustLevel ordering test ---------------------------------------------

    /// TrustLevel ordering must hold: Unknown < Community < Curated.
    #[test]
    fn trust_level_ordering() {
        assert!(TrustLevel::Unknown < TrustLevel::Community);
        assert!(TrustLevel::Community < TrustLevel::Curated);
        assert!(TrustLevel::Unknown < TrustLevel::Curated);
    }
}
