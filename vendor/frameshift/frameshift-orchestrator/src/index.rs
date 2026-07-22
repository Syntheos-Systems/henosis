//! Persona index: matchable representations of installed personas.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use frameshift_source::PersonaSource;
use serde::Deserialize;

use crate::error::OrchestratorError;

/// Stopwords excluded from persona keyword extraction.
const STOPWORDS: &[&str] = &[
    "the", "and", "for", "with", "that", "this", "you", "are", "not", "its", "use", "all", "can",
    "has", "was", "will", "any", "but", "our", "have", "from", "they", "when", "your", "how",
    "what", "who",
];

/// A matchable, pre-processed representation of a single persona.
///
/// Built from a `PersonaSource` by extracting and normalizing all textual content
/// into deduplicated keyword bags and structured sets for fast overlap scoring.
#[derive(Debug, Clone)]
pub struct PersonaProfile {
    /// The persona's canonical name.
    pub name: String,

    /// Optional human-readable description of the persona.
    pub description: Option<String>,

    /// Programming languages this persona is associated with, derived from
    /// `CodeExample.language` fields, keyword scan of name/description, and
    /// known-language name detection.
    pub languages: BTreeSet<String>,

    /// Deduplicated lowercase keyword tokens extracted from name, description,
    /// voice (tone + text), anchor texts, rule texts, skill `invoke_when` fields,
    /// pattern category/items, antipattern text/replacement/reasoning, and
    /// general-pattern text. Stopwords and short tokens removed.
    pub keywords: Vec<String>,

    /// Required tools declared in the capability manifest (empty if none).
    pub required_tools: Vec<String>,

    /// Whether the capability manifest declares network egress required.
    pub network_egress: bool,

    /// Task intents this persona is strongest at, parsed from capability manifest
    /// or inferred from keywords.
    pub primary_intents: Vec<crate::intent::Intent>,

    /// Keywords that should repel this persona during selection scoring.
    pub anti_keywords: Vec<String>,
}

/// Minimal pack.toml structure for freeform personas that lack `persona.toml`.
///
/// Only the fields we care about for profile extraction are deserialized.
#[derive(Debug, Deserialize, Default)]
struct PackManifest {
    /// Canonical name of the persona pack.
    #[serde(default)]
    name: Option<String>,

    /// Optional human-readable description.
    #[serde(default)]
    description: Option<String>,

    /// Topical tags that bias persona selection toward matching tasks.
    #[serde(default)]
    tags: Vec<String>,

    /// Optional capability manifest section.
    #[serde(default)]
    capability_manifest: Option<PackCapabilityManifest>,
}

/// Capability manifest section inside pack.toml.
#[derive(Debug, Deserialize, Default)]
struct PackCapabilityManifest {
    /// Tools this persona requires to be available.
    #[serde(default)]
    required_tools: Vec<String>,

    /// Whether network egress is required.
    #[serde(default)]
    network_egress: bool,

    /// Task intents this persona is designed for (e.g. "debugging", "security").
    #[serde(default)]
    primary_intents: Vec<String>,

    /// Keywords that should repel this persona during selection scoring.
    #[serde(default)]
    anti_keywords: Vec<String>,
}

/// Profile construction and persona source loading operations.
impl PersonaProfile {
    /// Build a `PersonaProfile` from a loaded `PersonaSource`.
    ///
    /// Extracts languages from code examples and keyword scans, builds a
    /// deduplicated keyword corpus from all textual fields, and copies capability
    /// manifest data if present.
    pub fn from_source(src: &PersonaSource) -> Self {
        let name = src.persona.name.clone();
        let description = src.persona.description.clone();

        // Collect language hints from code examples.
        let mut languages: BTreeSet<String> = BTreeSet::new();
        for ex in &src.patterns.examples {
            let lang = ex.language.to_lowercase();
            if !lang.is_empty() {
                languages.insert(lang);
            }
        }

        // Keyword corpus: gather all text fields, then tokenize + dedup.
        let mut text_parts: Vec<String> = Vec::new();
        text_parts.push(src.persona.name.clone());
        if let Some(desc) = &src.persona.description {
            text_parts.push(desc.clone());
        }
        // Topical tags bias keyword-based selection, mirroring the pack.toml path.
        for tag in &src.persona.tags {
            text_parts.push(tag.clone());
        }
        text_parts.push(src.persona.voice.tone.clone());
        if let Some(vt) = &src.persona.voice.text {
            text_parts.push(vt.clone());
        }
        for q in &src.persona.voice.questions {
            text_parts.push(q.text.clone());
        }
        for anchor in src.persona.anchor.values() {
            text_parts.push(anchor.text.clone());
            if let Some(tl) = &anchor.tagline {
                text_parts.push(tl.clone());
            }
        }
        for rule in &src.rules.rules {
            text_parts.push(rule.text.clone());
        }
        for skill in &src.skills.skills {
            text_parts.push(skill.invoke_when.clone());
        }
        for cat in &src.patterns.stack {
            text_parts.push(cat.category.clone());
            for item in &cat.items {
                text_parts.push(item.clone());
            }
        }
        for ex in &src.patterns.examples {
            text_parts.push(ex.language.clone());
            text_parts.push(ex.context.clone());
        }
        // Anti-patterns and general patterns are first-class pattern categories
        // (schema, validation, merge, conflict, and render all treat them as
        // such) so their text must feed the same keyword corpus as stack items
        // and examples do, or a persona whose distinguishing content lives here
        // is systematically under-ranked during selection.
        for antipattern in &src.patterns.antipatterns {
            text_parts.push(antipattern.text.clone());
            if let Some(use_instead) = &antipattern.use_instead {
                text_parts.push(use_instead.clone());
            }
            if let Some(reasoning) = &antipattern.reasoning {
                text_parts.push(reasoning.clone());
            }
        }
        for pattern in &src.patterns.patterns {
            text_parts.push(pattern.text.clone());
        }

        let combined = text_parts.join(" ");
        let keywords = extract_keywords(&combined);

        // Language detection via keyword scan: if a known language name appears
        // in keywords, add it to the language set.
        for lang in KNOWN_LANGUAGES {
            if keywords.iter().any(|k| k == *lang) {
                languages.insert(lang.to_string());
            }
        }

        // Capability manifest.
        let (required_tools, network_egress) = if let Some(cm) = &src.persona.capability_manifest {
            (cm.required_tools.clone(), cm.network_egress)
        } else {
            (Vec::new(), false)
        };

        let primary_intents = if let Some(cm) = &src.persona.capability_manifest {
            cm.primary_intents
                .iter()
                .filter_map(|s| parse_intent(s))
                .collect()
        } else {
            infer_intents_from_keywords(&keywords)
        };

        let anti_keywords = if let Some(cm) = &src.persona.capability_manifest {
            cm.anti_keywords.clone()
        } else {
            Vec::new()
        };

        PersonaProfile {
            name,
            description,
            languages,
            keywords,
            required_tools,
            network_egress,
            primary_intents,
            anti_keywords,
        }
    }

    /// Build a `PersonaProfile` from a freeform AGENTS.md persona directory.
    ///
    /// `dir` must contain `AGENTS.md`. `pack.toml` is optional but used for
    /// name, description, and capability_manifest when present. The markdown
    /// body is tokenized for keywords; high-signal sections (L2 anchor, Tech
    /// Stack, Concrete Patterns, Operating Frame) are weighted by being
    /// prepended to the corpus ahead of the full body. `extract_keywords`
    /// dedups by first-seen order, so a section's tokens must be the first
    /// occurrence it sees to matter at all -- appending them after a body that
    /// already contains the same text (the sections are extracted from the
    /// body, so it always does) would contribute nothing. Prepending instead
    /// makes the tokens land first in the resulting keyword vector, which is
    /// what `persona_text` (policy.rs) and any future length-limited embedder
    /// rely on for priority. Language detection runs the language lexicon over
    /// the resulting keyword set.
    pub fn from_agents_md(dir: &Path) -> Result<Self, OrchestratorError> {
        let agents_md_path = dir.join("AGENTS.md");
        let body = std::fs::read_to_string(&agents_md_path)?;

        // Read pack.toml if present; default on absence, propagate error on parse failure.
        // A malformed manifest must not silently become an empty (permissive) one.
        let pack: PackManifest = {
            let pack_path = dir.join("pack.toml");
            if pack_path.exists() {
                let raw = std::fs::read_to_string(&pack_path)?;
                toml::from_str(&raw)?
            } else {
                PackManifest::default()
            }
        };

        // Name: pack.toml `name` > directory file_name.
        let name = pack.name.filter(|n| !n.is_empty()).unwrap_or_else(|| {
            dir.file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "unknown".to_string())
        });

        // Build keyword corpus. High-signal sections go first so their tokens
        // are the first occurrence `extract_keywords` records (see doc comment
        // above); the persona name follows for the same reason, then the full
        // body supplies everything else. Appending the sections after the body
        // instead (the previous behavior) made them inert: every token they
        // contain already occurred once when the body was scanned.
        let mut corpus = String::new();
        for section in extract_high_signal_sections(&body) {
            corpus.push_str(&section);
            corpus.push(' ');
        }
        // Include the persona name itself for self-match.
        corpus.push_str(&name);
        corpus.push(' ');
        corpus.push_str(&body);
        // Fold curated pack.toml metadata into the corpus so the persona's
        // description and topical tags bias keyword-based selection alongside
        // the AGENTS.md body. Empty values contribute nothing.
        if let Some(desc) = &pack.description {
            corpus.push(' ');
            corpus.push_str(desc);
        }
        for tag in &pack.tags {
            corpus.push(' ');
            corpus.push_str(tag);
        }

        let mut keywords = extract_keywords(&corpus);
        // Ensure the persona name (as a keyword token) is always present.
        let name_tok = name.to_lowercase();
        if name_tok.len() >= 3 && !keywords.iter().any(|k| k == &name_tok) {
            keywords.push(name_tok.clone());
        }

        // Language detection via lexicon.
        let mut languages: BTreeSet<String> = BTreeSet::new();
        for (trigger, canonical) in LANGUAGE_LEXICON {
            if keywords.iter().any(|k| k == *trigger) {
                languages.insert(canonical.to_string());
            }
        }
        // Also check for language names in the KNOWN_LANGUAGES list.
        for lang in KNOWN_LANGUAGES {
            if keywords.iter().any(|k| k == *lang) {
                languages.insert(lang.to_string());
            }
        }
        // If the persona name IS a known language, add it.
        let name_lower = name.to_lowercase();
        for lang in KNOWN_LANGUAGES {
            if name_lower == *lang {
                languages.insert(lang.to_string());
            }
        }

        // Capability manifest from pack.toml.
        let (required_tools, network_egress, primary_intents, anti_keywords) =
            if let Some(cm) = pack.capability_manifest {
                let intents: Vec<crate::intent::Intent> = cm
                    .primary_intents
                    .iter()
                    .filter_map(|s| parse_intent(s))
                    .collect();
                let resolved_intents = if intents.is_empty() {
                    infer_intents_from_keywords(&keywords)
                } else {
                    intents
                };
                (
                    cm.required_tools,
                    cm.network_egress,
                    resolved_intents,
                    cm.anti_keywords,
                )
            } else {
                (
                    Vec::new(),
                    false,
                    infer_intents_from_keywords(&keywords),
                    Vec::new(),
                )
            };

        Ok(PersonaProfile {
            name,
            description: pack.description,
            languages,
            keywords,
            required_tools,
            network_egress,
            primary_intents,
            anti_keywords,
        })
    }

    /// Build a `PersonaProfile` from a persona directory using dual-source logic.
    ///
    /// Prefers `persona.toml` when present (typed source path). Falls back to
    /// `AGENTS.md` for freeform personas. Returns an error if neither file exists.
    pub fn from_persona_dir(dir: &Path) -> Result<Self, OrchestratorError> {
        let persona_toml = dir.join("persona.toml");
        let agents_md = dir.join("AGENTS.md");

        if persona_toml.exists() {
            let src = PersonaSource::load_from_dir(dir)?;
            Ok(Self::from_source(&src))
        } else if agents_md.exists() {
            Self::from_agents_md(dir)
        } else {
            Err(OrchestratorError::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!(
                    "persona dir {} has neither persona.toml nor AGENTS.md",
                    dir.display()
                ),
            )))
        }
    }
}

/// Language lexicon: maps trigger tokens to canonical language names.
///
/// Each entry is (trigger_keyword, canonical_language). Multiple triggers can
/// map to the same canonical language (e.g., "cargo" -> "rust"). The pseudo-
/// language "prose" captures writing/documentation domain personas so they
/// compete on equal footing with code-language personas.
const LANGUAGE_LEXICON: &[(&str, &str)] = &[
    ("rust", "rust"),
    ("cargo", "rust"),
    ("clippy", "rust"),
    ("rustc", "rust"),
    ("tauri", "rust"),
    ("typescript", "typescript"),
    ("tsx", "typescript"),
    ("react", "typescript"),
    ("svelte", "typescript"),
    ("vue", "typescript"),
    ("javascript", "javascript"),
    ("node", "javascript"),
    ("npm", "javascript"),
    ("discord", "javascript"),
    ("python", "python"),
    ("pip", "python"),
    ("pytest", "python"),
    ("django", "python"),
    ("go", "go"),
    ("golang", "go"),
    ("bash", "shell"),
    ("shell", "shell"),
    ("zsh", "shell"),
    ("cpp", "cpp"),
    ("c++", "cpp"),
    ("java", "java"),
    ("kotlin", "kotlin"),
    ("swift", "swift"),
    ("ruby", "ruby"),
    ("scala", "scala"),
    ("haskell", "haskell"),
    ("elixir", "elixir"),
    ("erlang", "erlang"),
    ("clojure", "clojure"),
    ("sql", "sql"),
    ("yaml", "yaml"),
    ("toml", "toml"),
    ("markdown", "markdown"),
    // Writing/documentation domain: maps to pseudo-language "prose" so that
    // writer-specialist personas accumulate a language signal. Only distinctive
    // writing-domain terms are included here (not generic "documentation" which
    // appears in all AGENTS.md files), so only genuine writing personas get the
    // prose language tag.
    ("prose", "prose"),
    ("changelog", "prose"),
    ("changelogs", "prose"),
    ("tutorial", "prose"),
    ("tutorials", "prose"),
    ("copywriting", "prose"),
    ("slop", "prose"),
    ("antiSlop", "prose"),
];

/// Known language identifiers used for keyword-based language detection.
const KNOWN_LANGUAGES: &[&str] = &[
    "rust",
    "typescript",
    "javascript",
    "python",
    "go",
    "java",
    "ruby",
    "c",
    "cpp",
    "markdown",
    "toml",
    "shell",
    "bash",
    "sql",
    "yaml",
    "haskell",
    "kotlin",
    "swift",
    "scala",
    "elixir",
    "erlang",
    "clojure",
    "prose",
];

/// Extract section bodies from high-signal headings for double-weighting.
///
/// Scans `body` for headings containing any of the high-signal keywords
/// (case-insensitive). Returns the text under each matching heading until the
/// next heading of the same or higher level -- a deeper subheading (e.g. a
/// `###` nested under a matching `##`) does not end the section; its own
/// heading text is folded into the captured content instead, so subsections
/// like "### Anti-patterns (do NOT use)" nested under "## Concrete Patterns"
/// still contribute their tokens.
fn extract_high_signal_sections(body: &str) -> Vec<String> {
    const HIGH_SIGNAL: &[&str] = &[
        "l2 anchor",
        "tech stack",
        "concrete patterns",
        "operating frame",
        "who you are",
        "language",
        "stack",
        "tools",
    ];

    let mut sections: Vec<String> = Vec::new();
    let mut current_heading_level: usize = 0;
    let mut current_is_signal = false;
    let mut current_section = String::new();

    for line in body.lines() {
        if line.starts_with('#') {
            // Compute heading level (number of leading '#' chars).
            let level = line.chars().take_while(|c| *c == '#').count();

            // A heading only ends the current signal section when its level is
            // the same as or shallower than (numerically <=) the level that
            // opened the section. A deeper heading is a subsection of the
            // signal content, not a sibling boundary.
            if current_is_signal && level <= current_heading_level {
                if !current_section.is_empty() {
                    sections.push(current_section.clone());
                }
                current_section.clear();
                current_is_signal = false;
            }

            if current_is_signal {
                // Nested subheading inside an already-open signal section:
                // fold its own heading text into the captured content rather
                // than treating it as a new section boundary.
                current_section.push_str(line);
                current_section.push('\n');
            } else {
                // Either no section was open, or the one that was open just
                // closed above -- evaluate this heading as a fresh candidate.
                let heading_text = line.trim_start_matches('#').trim().to_lowercase();
                current_heading_level = level;
                current_is_signal = HIGH_SIGNAL.iter().any(|s| heading_text.contains(s));
            }
        } else if current_is_signal {
            current_section.push_str(line);
            current_section.push('\n');
        }
    }

    // Capture trailing section.
    if current_is_signal && !current_section.is_empty() {
        sections.push(current_section);
    }

    sections
}

/// Extract deduplicated, lowercase, stopword-filtered keyword tokens from `text`.
///
/// Splits on non-alphanumeric characters, lowercases, drops tokens shorter than
/// 3 characters, removes stopwords, and deduplicates while preserving first-seen order.
fn extract_keywords(text: &str) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    text.split(|c: char| !c.is_alphanumeric())
        .map(|t| t.to_lowercase())
        .filter(|t| t.len() >= 3)
        .filter(|t| !STOPWORDS.contains(&t.as_str()))
        .filter(|t| seen.insert(t.clone()))
        .collect()
}

/// Parse an intent string into an Intent enum value.
///
/// Matches the lowercased string against known intent category names. Returns
/// `None` for unrecognized strings so callers can filter cleanly.
fn parse_intent(s: &str) -> Option<crate::intent::Intent> {
    use crate::intent::Intent;
    match s.to_lowercase().as_str() {
        "implementation" => Some(Intent::Implementation),
        "debugging" => Some(Intent::Debugging),
        "review" => Some(Intent::Review),
        "security" => Some(Intent::Security),
        "writing" => Some(Intent::Writing),
        "ops" | "ops/infra" => Some(Intent::Ops),
        "testing" => Some(Intent::Testing),
        "refactoring" => Some(Intent::Refactoring),
        "performance" => Some(Intent::Performance),
        "design" => Some(Intent::Design),
        _ => None,
    }
}

/// Infer primary intents from a persona's keyword set when no capability manifest declares them.
///
/// Delegates to `crate::intent::classify` over the keyword slice, collecting
/// the result (at most one intent) into a Vec.
fn infer_intents_from_keywords(keywords: &[String]) -> Vec<crate::intent::Intent> {
    crate::intent::classify(keywords).into_iter().collect()
}

/// An in-memory index of all installed persona profiles, ready for scoring.
#[derive(Debug, Clone)]
pub struct PersonaIndex {
    /// Ordered list of pre-processed persona profiles.
    pub profiles: Vec<PersonaProfile>,
}

/// Index construction and installed-persona discovery operations.
impl PersonaIndex {
    /// Build a `PersonaIndex` from a slice of already-loaded persona sources.
    pub fn build(sources: &[PersonaSource]) -> Self {
        let profiles = sources.iter().map(PersonaProfile::from_source).collect();
        PersonaIndex { profiles }
    }

    /// Load persona sources from a list of directories and build an index.
    ///
    /// Each directory is processed via `PersonaProfile::from_persona_dir`, which
    /// accepts both `persona.toml` (typed) and `AGENTS.md` (freeform) personas.
    /// Directories that have neither file are skipped with a warning instead of
    /// failing the whole batch.
    pub fn from_dirs(dirs: &[PathBuf]) -> Result<Self, OrchestratorError> {
        let mut profiles = Vec::with_capacity(dirs.len());
        for dir in dirs {
            match PersonaProfile::from_persona_dir(dir) {
                Ok(profile) => profiles.push(profile),
                Err(OrchestratorError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {
                    tracing::warn!(
                        dir = %dir.display(),
                        "skipping persona dir: no persona.toml or AGENTS.md found"
                    );
                }
                Err(e) => return Err(e),
            }
        }
        Ok(PersonaIndex { profiles })
    }

    /// Build a `PersonaIndex` from all immediate subdirectories of `catalog_root`.
    ///
    /// Enumerates subdirs of `catalog_root`, skipping `bin`, `.git`, and any
    /// entry whose name starts with `.`. Each subdir is indexed via
    /// `PersonaProfile::from_persona_dir`; dirs with neither persona.toml nor
    /// AGENTS.md are skipped with a warning.
    pub fn from_catalog(catalog_root: &Path) -> Result<Self, OrchestratorError> {
        let mut profiles = Vec::new();

        // Directories to skip at the catalog root level.
        const SKIP_DIRS: &[&str] = &["bin", ".git"];

        let mut entries: Vec<_> = std::fs::read_dir(catalog_root)?
            .filter_map(|e| e.ok())
            .collect();
        entries.sort_by_key(|e| e.file_name());

        for entry in entries {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }

            let name = entry.file_name();
            let name_str = name.to_string_lossy();

            // Skip dotfiles, bin, .git.
            if name_str.starts_with('.') || SKIP_DIRS.contains(&name_str.as_ref()) {
                continue;
            }

            match PersonaProfile::from_persona_dir(&path) {
                Ok(profile) => profiles.push(profile),
                Err(OrchestratorError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {
                    tracing::warn!(
                        dir = %path.display(),
                        "skipping catalog dir: no persona.toml or AGENTS.md found"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        dir = %path.display(),
                        error = %e,
                        "skipping catalog dir: failed to load persona"
                    );
                }
            }
        }

        Ok(PersonaIndex { profiles })
    }
}

#[cfg(test)]
/// Persona profile extraction and index loading tests.
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    /// Build a minimal PersonaSource for testing.
    fn minimal_source(name: &str, tone: &str) -> PersonaSource {
        use frameshift_source::*;
        PersonaSource {
            persona: Persona {
                schema_version: 1,
                name: name.to_string(),
                version: None,
                description: Some(format!("{name} persona for testing")),
                tags: vec![],
                license: None,
                author: None,
                extends: None,
                mixin: vec![],
                voice: Voice {
                    tone: tone.to_string(),
                    text: None,
                    questions: vec![],
                },
                anchor: std::collections::BTreeMap::new(),
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
                default_questions: vec![],
            },
            rules: RuleSet::default(),
            skills: SkillSet::default(),
            patterns: PatternSet::default(),
        }
    }

    /// from_source extracts the persona name.
    #[test]
    fn profile_extracts_name() {
        let src = minimal_source("rust-expert", "precise and performant");
        let profile = PersonaProfile::from_source(&src);
        assert_eq!(profile.name, "rust-expert");
    }

    /// from_source detects rust keyword in the name.
    #[test]
    fn profile_detects_language_from_name() {
        let src = minimal_source("rust-expert", "precise and performant");
        let profile = PersonaProfile::from_source(&src);
        assert!(profile
            .keywords
            .iter()
            .any(|k| k == "rust" || k == "expert"));
    }

    /// persona.toml tags land in the selection keyword corpus.
    #[test]
    fn profile_includes_tags_in_keywords() {
        let mut src = minimal_source("helper", "neutral tone");
        src.persona.tags = vec!["embedded".to_string(), "Firmware".to_string()];
        let profile = PersonaProfile::from_source(&src);
        assert!(
            profile.keywords.iter().any(|k| k == "embedded"),
            "tag should appear in keywords"
        );
        assert!(
            profile.keywords.iter().any(|k| k == "firmware"),
            "tags should be lowercased like the rest of the corpus"
        );
    }

    /// PersonaIndex::build creates one profile per source.
    #[test]
    fn index_build_count() {
        let sources = vec![
            minimal_source("alpha", "tone a"),
            minimal_source("beta", "tone b"),
        ];
        let index = PersonaIndex::build(&sources);
        assert_eq!(index.profiles.len(), 2);
    }

    /// from_agents_md extracts name from pack.toml and rust from a rust-flavored body.
    #[test]
    fn from_agents_md_extracts_rust() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("rust");
        fs::create_dir_all(&dir).unwrap();

        fs::write(
            dir.join("pack.toml"),
            "schema_version = 1\nname = \"rust\"\nauthor_handle = \"test\"\nauthor_pubkey = \"local-unsigned\"\nversion = \"0.1.0\"\n",
        ).unwrap();

        fs::write(
            dir.join("AGENTS.md"),
            "# AGENTS.md -- Rust Context\n\n## L2 Anchor -- Who You Are Here\n\nYou work on Rust code. cargo clippy rustc are your tools.\nOwnership, lifetimes, memory safety. No unwraps in library code.\n",
        ).unwrap();

        let profile = PersonaProfile::from_agents_md(&dir).unwrap();
        assert_eq!(profile.name, "rust");
        assert!(
            profile.languages.contains("rust"),
            "rust must be in languages; got: {:?}",
            profile.languages
        );
        assert!(
            profile
                .keywords
                .iter()
                .any(|k| k == "rust" || k == "cargo" || k == "clippy"),
            "expected rust/cargo/clippy in keywords; got: {:?}",
            profile.keywords
        );
    }

    /// from_persona_dir prefers persona.toml when both exist.
    #[test]
    fn from_persona_dir_prefers_persona_toml() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("mypersona");
        fs::create_dir_all(&dir).unwrap();

        // Write persona.toml (typed source).
        fs::write(
            dir.join("persona.toml"),
            "schema_version = 1\nname = \"from-persona-toml\"\nauthor_handle = \"test\"\nauthor_pubkey = \"local-unsigned\"\nversion = \"0.1.0\"\n[voice]\ntone = \"precise\"\n",
        ).unwrap();

        // Also write AGENTS.md with a different name -- should be ignored.
        fs::write(dir.join("AGENTS.md"), "# from-agents-md\n\nSome content.\n").unwrap();

        let profile = PersonaProfile::from_persona_dir(&dir).unwrap();
        assert_eq!(profile.name, "from-persona-toml");
    }

    /// from_persona_dir uses AGENTS.md when no persona.toml.
    #[test]
    fn from_persona_dir_uses_agents_md_fallback() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("writer");
        fs::create_dir_all(&dir).unwrap();

        fs::write(
            dir.join("pack.toml"),
            "schema_version = 1\nname = \"writer\"\nauthor_handle = \"test\"\nauthor_pubkey = \"local-unsigned\"\nversion = \"0.1.0\"\n",
        ).unwrap();
        fs::write(
            dir.join("AGENTS.md"),
            "# AGENTS.md -- Writer Context\n\nDocumentation, changelogs, READMEs, prose, tutorials.\n",
        ).unwrap();

        let profile = PersonaProfile::from_persona_dir(&dir).unwrap();
        assert_eq!(profile.name, "writer");
        assert!(
            profile
                .keywords
                .iter()
                .any(|k| k == "documentation" || k == "docs" || k == "writer" || k == "prose"),
            "expected writing-related keywords; got: {:?}",
            profile.keywords
        );
    }

    /// profile_extracts_primary_intents_from_capability_manifest verifies that
    /// declared primary_intents strings are parsed into Intent enum values.
    #[test]
    fn profile_extracts_primary_intents_from_capability_manifest() {
        use frameshift_source::*;
        let mut src = minimal_source("rust-expert", "precise");
        src.persona.capability_manifest = Some(CapabilityManifest {
            required_tools: vec![],
            filesystem_scope: String::new(),
            network_egress: false,
            primary_intents: vec!["implementation".to_string(), "debugging".to_string()],
            anti_keywords: vec![],
        });
        let profile = PersonaProfile::from_source(&src);
        assert_eq!(profile.primary_intents.len(), 2);
        assert!(profile
            .primary_intents
            .contains(&crate::intent::Intent::Implementation));
        assert!(profile
            .primary_intents
            .contains(&crate::intent::Intent::Debugging));
    }

    /// profile_extracts_anti_keywords verifies that anti_keywords from the
    /// capability manifest are copied verbatim into PersonaProfile.
    #[test]
    fn profile_extracts_anti_keywords() {
        use frameshift_source::*;
        let mut src = minimal_source("rust-expert", "precise");
        src.persona.capability_manifest = Some(CapabilityManifest {
            required_tools: vec![],
            filesystem_scope: String::new(),
            network_egress: false,
            primary_intents: vec![],
            anti_keywords: vec!["deployment".to_string(), "css".to_string()],
        });
        let profile = PersonaProfile::from_source(&src);
        assert_eq!(profile.anti_keywords, vec!["deployment", "css"]);
    }

    /// from_catalog indexes multiple dirs and skips one with neither file.
    #[test]
    fn from_catalog_indexes_dirs_and_skips_invalid() {
        let tmp = TempDir::new().unwrap();
        let catalog = tmp.path();

        // Valid freeform persona.
        let rust_dir = catalog.join("rust");
        fs::create_dir_all(&rust_dir).unwrap();
        fs::write(
            rust_dir.join("AGENTS.md"),
            "# Rust\n\ncargo clippy rustc ownership\n",
        )
        .unwrap();

        // Valid typed persona.
        let typed_dir = catalog.join("typed");
        fs::create_dir_all(&typed_dir).unwrap();
        fs::write(
            typed_dir.join("persona.toml"),
            "schema_version = 1\nname = \"typed\"\nauthor_handle = \"test\"\nauthor_pubkey = \"local-unsigned\"\nversion = \"0.1.0\"\n[voice]\ntone = \"direct\"\n",
        ).unwrap();

        // Dir with neither file -- should be skipped.
        let empty_dir = catalog.join("empty");
        fs::create_dir_all(&empty_dir).unwrap();

        // Dotfile dir -- should be skipped by name.
        let dot_dir = catalog.join(".hidden");
        fs::create_dir_all(&dot_dir).unwrap();
        fs::write(dot_dir.join("AGENTS.md"), "# hidden\n").unwrap();

        // bin dir -- should be skipped by name.
        let bin_dir = catalog.join("bin");
        fs::create_dir_all(&bin_dir).unwrap();

        let index = PersonaIndex::from_catalog(catalog).unwrap();
        assert_eq!(
            index.profiles.len(),
            2,
            "expected rust + typed, got: {:?}",
            index.profiles.iter().map(|p| &p.name).collect::<Vec<_>>()
        );
        let names: Vec<&str> = index.profiles.iter().map(|p| p.name.as_str()).collect();
        assert!(names.contains(&"rust"), "rust persona must be indexed");
        assert!(names.contains(&"typed"), "typed persona must be indexed");
    }

    /// from_source feeds anti-pattern text (text, use_instead, reasoning) and
    /// general-pattern text into the keyword corpus, not just stack/examples.
    #[test]
    fn profile_includes_antipattern_and_general_pattern_text_in_keywords() {
        use frameshift_source::*;
        let mut src = minimal_source("crypto-guard", "wary and precise");
        src.patterns.antipatterns = vec![AntiPattern {
            id: "no-openssl".to_string(),
            text: "roll your own xchachapoly implementation".to_string(),
            use_instead: Some("ring or rustcrypto primitives".to_string()),
            reasoning: Some("handrolled ciphers leak timing sidechannels".to_string()),
        }];
        src.patterns.patterns = vec![GeneralPattern {
            id: "config-lookup".to_string(),
            text: "envelopeencryption keyrotation schedule".to_string(),
        }];

        let profile = PersonaProfile::from_source(&src);

        assert!(
            profile.keywords.iter().any(|k| k == "xchachapoly"),
            "antipattern.text should be in keywords; got: {:?}",
            profile.keywords
        );
        assert!(
            profile.keywords.iter().any(|k| k == "rustcrypto"),
            "antipattern.use_instead should be in keywords; got: {:?}",
            profile.keywords
        );
        assert!(
            profile.keywords.iter().any(|k| k == "sidechannels"),
            "antipattern.reasoning should be in keywords; got: {:?}",
            profile.keywords
        );
        assert!(
            profile.keywords.iter().any(|k| k == "envelopeencryption"),
            "general pattern text should be in keywords; got: {:?}",
            profile.keywords
        );
    }

    /// extract_high_signal_sections must not close a section on a deeper
    /// subheading: content nested under "### Signing" and "### Anti-patterns"
    /// inside a matching "## Concrete Patterns" section belongs to that one
    /// section, mirroring how render.rs actually nests generated documents.
    #[test]
    fn extract_high_signal_sections_captures_nested_subheadings() {
        let body = "## Concrete Patterns\n\n### Signing\n\ned25519dalek\n\n### Anti-patterns (do NOT use)\n\nrot13warning\n\n## Some Other Heading\n\nirrelevant\n";

        let sections = extract_high_signal_sections(body);

        assert_eq!(
            sections.len(),
            1,
            "expected exactly one captured section, got: {sections:?}"
        );
        assert!(
            sections[0].contains("ed25519dalek"),
            "content nested under a ### subheading must survive; got: {:?}",
            sections[0]
        );
        assert!(
            sections[0].contains("rot13warning"),
            "content nested under a second ### subheading must survive; got: {:?}",
            sections[0]
        );
        assert!(
            !sections[0].contains("irrelevant"),
            "a sibling ## heading (same level as the opener) must still close the section; got: {:?}",
            sections[0]
        );
    }

    /// A same-or-shallower heading closes the section as documented, so two
    /// consecutive top-level signal headings produce two separate sections
    /// rather than merging into one.
    #[test]
    fn extract_high_signal_sections_closes_on_same_level_heading() {
        let body = "## Tech Stack\n\nzzalpha\n\n## Concrete Patterns\n\nzzbravo\n";

        let sections = extract_high_signal_sections(body);

        assert_eq!(
            sections.len(),
            2,
            "expected two sections, got: {sections:?}"
        );
        assert!(sections[0].contains("zzalpha"));
        assert!(sections[1].contains("zzbravo"));
    }

    /// from_agents_md previously appended high-signal sections after the full
    /// body, so extract_keywords's first-seen dedup meant they contributed
    /// nothing (every token already occurred once in the body). Prepending
    /// them means signal-section-only tokens now appear earlier in the
    /// resulting keyword vector than tokens that occur only outside a signal
    /// section -- proof the weighting is no longer inert.
    #[test]
    fn from_agents_md_high_signal_tokens_precede_body_only_tokens() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("weighted");
        fs::create_dir_all(&dir).unwrap();

        fs::write(
            dir.join("AGENTS.md"),
            "# Weighted Context\n\nzzalpha zzbravo zzcharlie\n\n## Tech Stack\n\nzzdelta zzecho zzfoxtrot\n",
        )
        .unwrap();

        let profile = PersonaProfile::from_agents_md(&dir).unwrap();

        let idx_signal = profile
            .keywords
            .iter()
            .position(|k| k == "zzdelta")
            .expect("zzdelta (from the Tech Stack signal section) must be a keyword");
        let idx_body_only = profile
            .keywords
            .iter()
            .position(|k| k == "zzalpha")
            .expect("zzalpha (body-only, non-signal) must be a keyword");

        assert!(
            idx_signal < idx_body_only,
            "high-signal token 'zzdelta' (index {}) should precede body-only token 'zzalpha' (index {}); got: {:?}",
            idx_signal,
            idx_body_only,
            profile.keywords
        );
    }
}
