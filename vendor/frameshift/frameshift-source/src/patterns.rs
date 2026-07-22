//! The `patterns.toml` file schema.
//!
//! Holds tech stack declarations (`[[stack]]`), anti-patterns (`[[antipattern]]`),
//! before/after code examples (`[[example]]`), and general named patterns (`[[pattern]]`).

use serde::{Deserialize, Serialize};

/// The `patterns.toml` file -- tech stack, anti-patterns, code examples, and general patterns.
///
/// All Vec fields use TOML array-of-tables syntax, so `antipatterns` serializes
/// as `[[antipattern]]`, `examples` as `[[example]]`, and `patterns` as `[[pattern]]`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PatternSet {
    /// Schema version for forward-compatibility gating. Defaults to `1`.
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    /// Approved tech stack items grouped by category (e.g. "signing", "certificates").
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stack: Vec<StackCategory>,
    /// Anti-patterns: things callers MUST NOT do, with optional replacement guidance.
    #[serde(default, rename = "antipattern", skip_serializing_if = "Vec::is_empty")]
    pub antipatterns: Vec<AntiPattern>,
    /// Before/after code examples anchoring correct behavior.
    #[serde(default, rename = "example", skip_serializing_if = "Vec::is_empty")]
    pub examples: Vec<CodeExample>,
    /// General named patterns that don't fit the before/after code example format.
    #[serde(default, rename = "pattern", skip_serializing_if = "Vec::is_empty")]
    pub patterns: Vec<GeneralPattern>,
}

/// A category of approved tech stack items (e.g. "signing", "certificates").
///
/// Maps to a `[[stack]]` TOML table.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StackCategory {
    /// Human-readable category name, e.g. "signing" or "key-derivation".
    pub category: String,
    /// Ordered list of approved crates or version ranges within this category.
    pub items: Vec<String>,
}

/// A named anti-pattern with optional replacement guidance and reasoning.
///
/// Maps to an `[[antipattern]]` TOML table.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AntiPattern {
    /// Stable machine identifier, e.g. "no-openssl".
    pub id: String,
    /// Human-readable description of what NOT to do.
    pub text: String,
    /// What to use instead. `None` when the anti-pattern has no safe replacement.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub use_instead: Option<String>,
    /// Extended explanation of why this is an anti-pattern.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
}

/// A before/after code example anchoring correct behavior.
///
/// Maps to an `[[example]]` TOML table.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CodeExample {
    /// Stable machine identifier, e.g. "constant-time-compare".
    pub id: String,
    /// Short human-readable title displayed when rendering the example.
    pub title: String,
    /// Description of when this example applies.
    pub context: String,
    /// Programming language for syntax highlighting (e.g. "rust", "python").
    pub language: String,
    /// Incorrect code that demonstrates what NOT to do.
    pub bad: String,
    /// Correct code that demonstrates what TO do.
    pub good: String,
}

/// A general named pattern (not a before/after code example).
///
/// Maps to a `[[pattern]]` TOML table.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GeneralPattern {
    /// Stable machine identifier, e.g. "config-lookup".
    pub id: String,
    /// Human-readable pattern description or invocation template.
    pub text: String,
}

/// Returns the default schema version (`1`) for use in `#[serde(default = ...)]`.
const fn default_schema_version() -> u32 {
    1
}

/// Constructs pattern collections for persona sources.
impl PatternSet {
    /// Constructs an empty `PatternSet` with default schema version.
    pub fn new() -> Self {
        Self::default()
    }
}

/// Provides an empty versioned pattern collection.
impl Default for PatternSet {
    /// Constructs an empty `PatternSet` with `schema_version` set to `1`.
    fn default() -> Self {
        Self {
            schema_version: 1,
            stack: Vec::new(),
            antipatterns: Vec::new(),
            examples: Vec::new(),
            patterns: Vec::new(),
        }
    }
}

#[cfg(test)]
/// Verifies pattern collection TOML serialization.
mod tests {
    use super::*;

    #[test]
    /// Round-trips a populated pattern collection through TOML.
    fn pattern_set_toml_roundtrip() {
        let original = PatternSet {
            schema_version: 1,
            stack: vec![StackCategory {
                category: "signing".to_string(),
                items: vec!["ed25519-dalek 2.x".to_string(), "subtle 2.5".to_string()],
            }],
            antipatterns: vec![AntiPattern {
                id: "no-openssl".to_string(),
                text: "Do NOT use OpenSSL".to_string(),
                use_instead: Some("RustCrypto ecosystem".to_string()),
                reasoning: None,
            }],
            examples: vec![CodeExample {
                id: "constant-time-compare".to_string(),
                title: "Constant-time comparison".to_string(),
                context: "Comparing MACs or signatures".to_string(),
                language: "rust".to_string(),
                bad: "if mac == expected { Ok(()) }".to_string(),
                good: "if mac.ct_eq(&expected).into() { Ok(()) }".to_string(),
            }],
            patterns: vec![GeneralPattern {
                id: "config-lookup".to_string(),
                text: "$TOOL get <key>".to_string(),
            }],
        };

        let serialized = toml::to_string(&original).unwrap();
        let parsed: PatternSet = toml::from_str(&serialized).unwrap();
        assert_eq!(parsed, original);
    }

    #[test]
    /// Round-trips an empty pattern collection through TOML.
    fn empty_pattern_set_roundtrips() {
        let original = PatternSet::default();
        let serialized = toml::to_string(&original).unwrap();
        let parsed: PatternSet = toml::from_str(&serialized).unwrap();
        assert_eq!(parsed, original);
    }
}
