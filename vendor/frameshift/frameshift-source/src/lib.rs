//! Structured persona source.
//!
//! Persona source is a typed schema (TOML) split across four files:
//! `persona.toml`, `rules.toml`, `skills.toml`, `patterns.toml`. Markdown
//! is a *render target* produced from this typed source -- agents and CLIs
//! operate on typed fields, never on string-replace-in-markdown.
//!
//! This crate owns:
//! - the TOML schema for each file (`persona`, `rules`, `skills`, `patterns`)
//! - the composite `PersonaSource` with load/write split across the four files
//! - deterministic markdown projection (`render`)
//! - typed patch operations (`patch`)
//! - semantic diff between two `PersonaSource` snapshots (`diff`)
//!
//! Schema serde round-trip, the load/write split, markdown projection, typed
//! patch operations, and semantic diff are all implemented; this module is
//! past its M1 scaffolding stage.

pub mod diff;
pub mod error;
pub mod patch;
pub mod patterns;
pub mod persona;
pub mod render;
pub mod rules;
pub mod security;
pub mod skills;
pub mod source;
pub mod validate;

pub use diff::{diff, SemanticDiff};
pub use error::SourceError;
pub use patch::{apply_patch, AnchorPosition, PatchError, PatchOp};
pub use patterns::{AntiPattern, CodeExample, GeneralPattern, PatternSet, StackCategory};
pub use persona::{
    AmbiguityQuestion, Anchor, Aspect, Author, CapabilityManifest, CascadeAnchor,
    ClassificationTier, ConflictResolution, ConformanceConfig, DefaultQuestion, GrowthConfig,
    Persona, ReferenceGroup, SafetyLayer, SelfEvalStep, Voice, VoiceQuestion,
};
pub use render::{render_to_markdown, RenderTarget};
pub use rules::{Layer, Rule, RuleSet};
pub use security::{
    audit_manifest, is_growth_file, CapabilitySummary, GrowthFilePermissions, KeyPinCheck,
    ManifestAspect, ManifestAudit, ManifestFinding, ManifestSeverity, PinnedKey, RevocationCheck,
    RevocationEntry, TrustLevel, TrustSummary,
};
pub use skills::{Skill, SkillSet};
pub use source::PersonaSource;
pub use validate::{validate_content, ContentWarning, Severity, WarningCategory};
