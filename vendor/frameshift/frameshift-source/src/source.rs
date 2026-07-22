use std::fs;
use std::path::{Path, PathBuf};

use crate::error::SourceError;
use crate::patterns::PatternSet;
use crate::persona::Persona;
use crate::rules::RuleSet;
use crate::skills::SkillSet;

const PERSONA_FILE: &str = "persona.toml";
const RULES_FILE: &str = "rules.toml";
const SKILLS_FILE: &str = "skills.toml";
const PATTERNS_FILE: &str = "patterns.toml";

/// Configuration for loading persona source files with safety limits.
///
/// Defaults are generous for local development but can be tightened
/// when loading network-sourced content (e.g., after pack extraction).
#[derive(Debug, Clone)]
pub struct LoadOptions {
    /// Maximum file size in bytes for any single TOML file. Default: 1 MiB.
    pub max_file_size: usize,
    /// Maximum number of rules allowed. Default: 500.
    pub max_rules: usize,
    /// Maximum number of skills allowed. Default: 200.
    pub max_skills: usize,
    /// Maximum number of pattern entries (stack + antipatterns + examples + patterns). Default: 500.
    pub max_patterns: usize,
}

/// Provides bounded defaults for local persona-source loading.
impl Default for LoadOptions {
    /// Returns default load options suitable for local development use.
    fn default() -> Self {
        Self {
            max_file_size: 1024 * 1024, // 1 MiB
            max_rules: 500,
            max_skills: 200,
            max_patterns: 500,
        }
    }
}

/// Composite persona source. Read from / written to a directory containing
/// `persona.toml`, `rules.toml`, `skills.toml`, `patterns.toml`.
#[derive(Debug, Clone, PartialEq)]
pub struct PersonaSource {
    /// Core persona identity, voice, anchors, and classification config.
    pub persona: Persona,
    /// Behavioral rules (L1/L2/L3 layers).
    pub rules: RuleSet,
    /// Named skills invoked at specific moments.
    pub skills: SkillSet,
    /// Tech stack declarations, anti-patterns, and code examples.
    pub patterns: PatternSet,
}

/// Loads, validates, constructs, and writes composite persona sources.
impl PersonaSource {
    /// Constructs a new `PersonaSource` with the given persona and empty rules, skills, and patterns.
    pub fn new(persona: Persona) -> Self {
        Self {
            persona,
            rules: RuleSet::default(),
            skills: SkillSet::default(),
            patterns: PatternSet::default(),
        }
    }

    /// Load a persona source from a directory using default `LoadOptions`.
    ///
    /// The directory must contain `persona.toml`. `rules.toml`, `skills.toml`,
    /// and `patterns.toml` are optional -- absent files yield empty sets.
    /// Delegates to `load_from_dir_with_options` with `LoadOptions::default()`.
    pub fn load_from_dir(dir: &Path) -> Result<Self, SourceError> {
        Self::load_from_dir_with_options(dir, &LoadOptions::default())
    }

    /// Load a persona source from a directory with explicit size and count limits.
    ///
    /// Checks each file's size against `opts.max_file_size` before reading it.
    /// After deserialization, validates rule, skill, and pattern counts against
    /// the limits in `opts`. Returns `SourceError::ContentLimitExceeded` if any
    /// limit is breached.
    pub fn load_from_dir_with_options(dir: &Path, opts: &LoadOptions) -> Result<Self, SourceError> {
        let persona =
            load_required_with_limit::<Persona>(&dir.join(PERSONA_FILE), opts.max_file_size)?;
        let rules = load_optional_with_limit::<RuleSet>(&dir.join(RULES_FILE), opts.max_file_size)?
            .unwrap_or_default();
        let skills =
            load_optional_with_limit::<SkillSet>(&dir.join(SKILLS_FILE), opts.max_file_size)?
                .unwrap_or_default();
        let patterns =
            load_optional_with_limit::<PatternSet>(&dir.join(PATTERNS_FILE), opts.max_file_size)?
                .unwrap_or_default();

        // Validate counts against configured limits.
        if rules.rules.len() > opts.max_rules {
            return Err(SourceError::ContentLimitExceeded {
                detail: format!(
                    "rules count {} exceeds max_rules {}",
                    rules.rules.len(),
                    opts.max_rules
                ),
            });
        }
        if skills.skills.len() > opts.max_skills {
            return Err(SourceError::ContentLimitExceeded {
                detail: format!(
                    "skills count {} exceeds max_skills {}",
                    skills.skills.len(),
                    opts.max_skills
                ),
            });
        }
        let pattern_count = patterns.stack.len()
            + patterns.antipatterns.len()
            + patterns.examples.len()
            + patterns.patterns.len();
        if pattern_count > opts.max_patterns {
            return Err(SourceError::ContentLimitExceeded {
                detail: format!(
                    "pattern entry count {pattern_count} exceeds max_patterns {}",
                    opts.max_patterns
                ),
            });
        }

        Ok(Self {
            persona,
            rules,
            skills,
            patterns,
        })
    }

    /// Write a persona source as four TOML files in `dir`. Creates the
    /// directory if it does not exist. Always writes all four files
    /// (including empty `rules.toml` / `skills.toml` / `patterns.toml` for
    /// predictable layout).
    pub fn write_to_dir(&self, dir: &Path) -> Result<(), SourceError> {
        fs::create_dir_all(dir).map_err(|source| SourceError::Io {
            path: dir.to_path_buf(),
            source,
        })?;

        write_toml(&dir.join(PERSONA_FILE), &self.persona)?;
        write_toml(&dir.join(RULES_FILE), &self.rules)?;
        write_toml(&dir.join(SKILLS_FILE), &self.skills)?;
        write_toml(&dir.join(PATTERNS_FILE), &self.patterns)?;
        Ok(())
    }
}

/// Loads a required TOML file with a file-size pre-check. Returns `MissingFile`
/// if the path does not exist or `ContentLimitExceeded` if the file is too large.
fn load_required_with_limit<T: serde::de::DeserializeOwned>(
    path: &Path,
    max_bytes: usize,
) -> Result<T, SourceError> {
    if !path.exists() {
        return Err(SourceError::MissingFile(path.to_path_buf()));
    }
    read_toml_with_limit(path, max_bytes)
}

/// Loads an optional TOML file with a file-size pre-check. Returns `None` if
/// the path does not exist or `ContentLimitExceeded` if the file is too large.
fn load_optional_with_limit<T: serde::de::DeserializeOwned>(
    path: &Path,
    max_bytes: usize,
) -> Result<Option<T>, SourceError> {
    if !path.exists() {
        return Ok(None);
    }
    read_toml_with_limit(path, max_bytes).map(Some)
}

/// Reads and deserializes a TOML file, first checking that the file size does
/// not exceed `max_bytes`. Returns `ContentLimitExceeded` if the file is too large.
fn read_toml_with_limit<T: serde::de::DeserializeOwned>(
    path: &Path,
    max_bytes: usize,
) -> Result<T, SourceError> {
    let metadata = fs::metadata(path).map_err(|source| SourceError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let file_size = metadata.len() as usize;
    if file_size > max_bytes {
        return Err(SourceError::ContentLimitExceeded {
            detail: format!(
                "{} is {file_size} bytes, exceeds max_file_size {max_bytes}",
                path.display()
            ),
        });
    }
    let raw = fs::read_to_string(path).map_err(|source| SourceError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    toml::from_str(&raw).map_err(|source| SourceError::TomlDeserialize {
        path: path.to_path_buf(),
        source,
    })
}

/// Serialize `value` to TOML and write it to `path`, returning `SourceError`
/// on serialization or I/O failure.
fn write_toml<T: serde::Serialize>(path: &PathBuf, value: &T) -> Result<(), SourceError> {
    let serialized = toml::to_string(value)?;
    fs::write(path, serialized).map_err(|source| SourceError::Io {
        path: path.clone(),
        source,
    })
}

#[cfg(test)]
/// Verifies persona-source persistence and loading failures.
mod tests {
    use super::*;
    use crate::patterns::{AntiPattern, PatternSet, StackCategory};
    use crate::persona::{Anchor, DefaultQuestion, Persona, Voice};
    use crate::rules::{Layer, Rule, RuleSet};
    use crate::skills::{Skill, SkillSet};
    use std::collections::BTreeMap;

    /// Builds a populated persona source shared by persistence tests.
    fn sample() -> PersonaSource {
        let mut anchor = BTreeMap::new();
        anchor.insert(
            "l2".to_string(),
            Anchor {
                text: "anchor body".to_string(),
                tagline: None,
                default_question: None,
            },
        );

        PersonaSource {
            persona: Persona {
                schema_version: 1,
                name: "demo".to_string(),
                version: None,
                description: None,
                tags: vec![],
                license: None,
                author: None,
                extends: None,
                mixin: vec![],
                voice: Voice {
                    tone: "demo voice".to_string(),
                    text: None,
                    questions: vec![],
                },
                anchor,
                classification_tiers: vec![],
                conflict_resolution: None,
                cascade_anchors: vec![],
                self_eval: vec![],
                ambiguity_questions: vec![],
                safety_layer: None,
                growth: None,
                references: vec![],
                capability_manifest: None,
                conformance: None,
                default_questions: vec![DefaultQuestion {
                    question: "what is this?".to_string(),
                }],
            },
            rules: RuleSet {
                rules: vec![Rule {
                    id: "r1".to_string(),
                    layer: Layer::L1,
                    text: "rule one".to_string(),
                    reasoning: None,
                    override_inherited: false,
                }],
            },
            skills: SkillSet {
                skills: vec![Skill {
                    id: "s1".to_string(),
                    invoke_when: "always".to_string(),
                    mandatory: false,
                }],
            },
            patterns: PatternSet::default(),
        }
    }

    #[test]
    /// Round-trips a persona source directory through disk.
    fn write_then_load_roundtrip() {
        let tmp = tempfile_dir();
        let original = sample();
        original.write_to_dir(&tmp).unwrap();

        let loaded = PersonaSource::load_from_dir(&tmp).unwrap();
        assert_eq!(loaded, original);
    }

    #[test]
    /// Rejects a source directory without its required persona file.
    fn missing_persona_file_errors() {
        let tmp = tempfile_dir();
        let err = PersonaSource::load_from_dir(&tmp).unwrap_err();
        assert!(matches!(err, SourceError::MissingFile(_)));
    }

    /// Verify that a PersonaSource with populated patterns writes `patterns.toml`
    /// to disk and that the file is present after `write_to_dir`.
    #[test]
    fn write_then_load_with_patterns_roundtrip() {
        let tmp = tempfile_dir();
        let mut original = sample();
        original.patterns = PatternSet {
            schema_version: 1,
            stack: vec![StackCategory {
                category: "signing".to_string(),
                items: vec!["ed25519-dalek".to_string()],
            }],
            antipatterns: vec![AntiPattern {
                id: "no-openssl".to_string(),
                text: "Do not use OpenSSL".to_string(),
                use_instead: Some("RustCrypto".to_string()),
                reasoning: None,
            }],
            examples: vec![],
            patterns: vec![],
        };

        original.write_to_dir(&tmp).unwrap();

        // patterns.toml must exist on disk
        assert!(
            tmp.join("patterns.toml").exists(),
            "patterns.toml was not written"
        );

        // full roundtrip: load back and compare
        let loaded = PersonaSource::load_from_dir(&tmp).unwrap();
        assert_eq!(loaded, original);
    }

    /// Verify that loading with default options succeeds for normal content
    /// (sample has one rule, one skill, no patterns -- well within limits).
    #[test]
    fn load_with_options_default_succeeds() {
        let tmp = tempfile_dir();
        let original = sample();
        original.write_to_dir(&tmp).unwrap();

        let loaded =
            PersonaSource::load_from_dir_with_options(&tmp, &LoadOptions::default()).unwrap();
        assert_eq!(loaded, original);
    }

    /// Verify that loading with max_rules=0 fails with ContentLimitExceeded
    /// when the source has at least one rule.
    #[test]
    fn load_with_options_max_rules_zero_fails() {
        let tmp = tempfile_dir();
        let original = sample(); // has 1 rule
        original.write_to_dir(&tmp).unwrap();

        let opts = LoadOptions {
            max_rules: 0,
            ..LoadOptions::default()
        };
        let err = PersonaSource::load_from_dir_with_options(&tmp, &opts).unwrap_err();
        assert!(
            matches!(&err, SourceError::ContentLimitExceeded { detail } if detail.contains("max_rules")),
            "unexpected error: {err}"
        );
    }

    /// Build a fresh empty temp dir without pulling tempfile as a dep --
    /// frameshift-source has no dev-deps and we want to keep it that way
    /// for this scaffolding milestone.
    fn tempfile_dir() -> PathBuf {
        let base = std::env::temp_dir();
        let unique = format!(
            "frameshift-source-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        );
        let dir = base.join(unique);
        fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }
}
