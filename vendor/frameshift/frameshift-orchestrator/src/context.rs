//! Context sensing: infer the work context from project layout and an optional task hint.

use std::collections::BTreeMap;
use std::path::Path;

/// Maximum number of files to scan during a project walk.
const MAX_FILES: usize = 2000;

/// Maximum directory depth to descend during a project walk.
const MAX_DEPTH: usize = 6;

/// Directories to skip entirely during the walk.
const SKIP_DIRS: &[&str] = &[
    ".git",
    "target",
    "node_modules",
    ".cache",
    "__pycache__",
    ".hg",
    ".svn",
];

/// Weight assigned to languages present in the active git working set.
///
/// Sits above the file-count census ceiling (1.0) but below the prose sentinel
/// (2.0, see [`augment_languages_from_task`]) so that what the user is actively
/// editing outranks the static census without overriding an explicit
/// prose-writing signal.
const ACTIVE_LANGUAGE_WEIGHT: f32 = 1.5;

/// Upper bound on changed-file paths processed from `git status` output.
///
/// Bounds work on pathological repos (e.g. a freshly checked-out tree reported
/// as fully untracked); far more than enough to characterize the active set.
const MAX_CHANGED_FILES: usize = 5000;

/// Upper bound on dependency-name tokens harvested from project manifests.
///
/// Keeps the additive context-token signal bounded on dependency-heavy projects.
const MAX_DEP_TOKENS: usize = 100;

/// Manifest section headers under which a TOML key names a dependency.
const CARGO_DEP_SECTIONS: &[&str] = &[
    "[dependencies]",
    "[dev-dependencies]",
    "[build-dependencies]",
    "[workspace.dependencies]",
];

/// A snapshot of the inferred work context for a project directory.
#[derive(Debug, Clone, PartialEq)]
pub struct ContextSignal {
    /// The basename of the project root directory (used as a human-readable label).
    pub project_name: String,

    /// Weighted language presence derived from file-extension scanning.
    /// Values are in (0.0, 1.0] and sum to at most 1.0 per language contribution.
    /// Example: `{"rust": 1.0, "toml": 0.2}`.
    pub languages: BTreeMap<String, f32>,

    /// Framework/build-system markers detected from well-known files.
    /// E.g. "cargo", "npm", "go", "python".
    pub frameworks: Vec<String>,

    /// Normalized, lowercase tokens extracted from `task_hint`.
    /// Used by the policy scorer for lexical matching against persona keywords.
    pub task_tokens: Vec<String>,

    /// Lowercase dependency names parsed from project manifests (Cargo.toml,
    /// package.json). Scored by the policy as a small additive bonus when they
    /// match persona keywords -- they sharpen framework-level matching (e.g.
    /// `axum` vs `actix`) without diluting the task-token lexical score.
    pub context_tokens: Vec<String>,

    /// The inferred task intent from task token analysis, if any.
    pub inferred_intent: Option<crate::intent::Intent>,
}

/// Walk `project_root` (bounded by depth and file count), scan extensions for
/// language weights, detect marker files for frameworks, and tokenize `task_hint`.
///
/// The walk is deterministic: entries are processed in sorted order. `.git`,
/// `target`, `node_modules`, and similar directories are skipped entirely.
/// Language weights are proportional to file count normalized to a [0.0, 1.0]
/// scale relative to the most-seen language.
pub fn sense(project_root: &Path, task_hint: Option<&str>) -> ContextSignal {
    let project_name = project_root
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown")
        .to_string();

    let mut raw_counts: BTreeMap<String, usize> = BTreeMap::new();
    let mut frameworks: Vec<String> = Vec::new();
    let mut file_count = 0usize;

    walk(
        project_root,
        0,
        &mut raw_counts,
        &mut frameworks,
        &mut file_count,
    );

    // Deduplicate frameworks (marker files may appear at multiple levels).
    frameworks.sort();
    frameworks.dedup();

    // Normalize language counts to a [0.0, 1.0] scale.
    let max_count = raw_counts.values().copied().max().unwrap_or(1).max(1) as f32;
    let languages: BTreeMap<String, f32> = raw_counts
        .into_iter()
        .map(|(lang, count)| (lang, (count as f32 / max_count).min(1.0)))
        .collect();

    let mut task_tokens = tokenize(task_hint.unwrap_or(""));

    // Expand task tokens with domain synonyms so that natural-language phrasing
    // maps to canonical terminology used in persona keyword sets.
    expand_task_tokens(&mut task_tokens);

    // Augment language signals from task hint: if the task itself uses
    // writing-domain terms, inject a "prose" language signal so writer-type
    // personas can compete with code-language personas on equal footing.
    let languages = augment_languages_from_task(languages, &task_tokens);

    // Emphasize the languages of the active git working set (best-effort): the
    // files the user is editing right now are a stronger signal than the static
    // whole-repo census. Non-git directories degrade to the census unchanged.
    let changed_languages = changed_file_languages(&git_changed_paths(project_root));
    let languages = augment_languages_from_git(languages, &changed_languages);

    // Harvest dependency names from project manifests (best-effort). These feed
    // the policy as a small additive bonus, sharpening framework-level matching.
    let context_tokens = manifest_dependency_tokens(project_root);

    // Classify the inferred task intent from task token analysis.
    let inferred_intent = crate::intent::classify(&task_tokens);

    ContextSignal {
        project_name,
        languages,
        frameworks,
        task_tokens,
        context_tokens,
        inferred_intent,
    }
}

/// Recursively walk `dir` up to `MAX_DEPTH` and `MAX_FILES`, updating
/// `raw_counts` with extension-to-language hits and `frameworks` with
/// marker-file discoveries.
fn walk(
    dir: &Path,
    depth: usize,
    raw_counts: &mut BTreeMap<String, usize>,
    frameworks: &mut Vec<String>,
    file_count: &mut usize,
) {
    if depth > MAX_DEPTH || *file_count >= MAX_FILES {
        return;
    }

    // Collect entries in sorted order for determinism.
    let mut entries: Vec<_> = match std::fs::read_dir(dir) {
        Ok(rd) => rd.filter_map(|e| e.ok()).collect(),
        Err(_) => return,
    };
    entries.sort_by_key(|e| e.file_name());

    for entry in entries {
        if *file_count >= MAX_FILES {
            break;
        }

        // Use the entry's own file type (this does not follow symlinks) so a
        // planted symlink cannot redirect the walk outside the project tree.
        let file_type = match entry.file_type() {
            Ok(ft) => ft,
            Err(_) => continue,
        };
        if file_type.is_symlink() {
            continue;
        }

        let path = entry.path();
        let name = entry.file_name();
        let name_str = name.to_string_lossy();

        if file_type.is_dir() {
            if SKIP_DIRS.contains(&name_str.as_ref()) {
                continue;
            }
            walk(&path, depth + 1, raw_counts, frameworks, file_count);
        } else if file_type.is_file() {
            *file_count += 1;

            // Check for framework marker files.
            detect_markers(&name_str, raw_counts, frameworks);
        }
    }
}

/// Given a file name, detect framework markers and map its extension to a language.
fn detect_markers(
    name: &str,
    raw_counts: &mut BTreeMap<String, usize>,
    frameworks: &mut Vec<String>,
) {
    // Marker-file -> framework + implied language entries.
    match name {
        "Cargo.toml" => {
            push_unique(frameworks, "cargo");
            *raw_counts.entry("rust".to_string()).or_insert(0) += 1;
        }
        "package.json" => {
            push_unique(frameworks, "npm");
        }
        "go.mod" => {
            push_unique(frameworks, "go");
            *raw_counts.entry("go".to_string()).or_insert(0) += 1;
        }
        "pyproject.toml" | "requirements.txt" | "setup.py" | "setup.cfg" => {
            push_unique(frameworks, "python");
            *raw_counts.entry("python".to_string()).or_insert(0) += 1;
        }
        _ => {}
    }

    // Extension-based language mapping.
    if let Some(ext) = Path::new(name).extension().and_then(|e| e.to_str()) {
        if let Some(lang) = ext_to_language(ext) {
            *raw_counts.entry(lang.to_string()).or_insert(0) += 1;
        }
    }
}

/// Domain clusters for bidirectional synonym expansion.
///
/// When any token in a cluster appears in the task, ALL tokens in that
/// cluster become available for matching. This closes vocabulary gaps
/// between task descriptions and persona keyword sets.
const DOMAIN_CLUSTERS: &[&[&str]] = &[
    &[
        "debug",
        "debugging",
        "error",
        "crash",
        "panic",
        "backtrace",
        "stacktrace",
        "fix",
        "trace",
        "segfault",
        "coredump",
    ],
    &[
        "security",
        "vulnerability",
        "cve",
        "audit",
        "pentest",
        "exploit",
        "hardening",
        "threat",
        "compliance",
        "owasp",
    ],
    &[
        "test",
        "testing",
        "unittest",
        "integration",
        "coverage",
        "assertion",
        "mock",
        "fixture",
        "snapshot",
        "e2e",
    ],
    &[
        "docs",
        "documentation",
        "readme",
        "tutorial",
        "changelog",
        "prose",
        "copywriting",
        "draft",
        "publish",
        "article",
    ],
    &[
        "perf",
        "performance",
        "benchmark",
        "profiling",
        "flamegraph",
        "latency",
        "throughput",
        "optimization",
        "hotpath",
    ],
    &[
        "deploy",
        "deployment",
        "infrastructure",
        "ci",
        "cd",
        "pipeline",
        "container",
        "docker",
        "kubernetes",
        "systemd",
        "nginx",
    ],
    &[
        "refactor",
        "refactoring",
        "cleanup",
        "restructure",
        "extract",
        "inline",
        "rename",
        "decompose",
    ],
    &["review", "reviewing", "pullrequest", "approve", "critique"],
    &[
        "implement",
        "implementing",
        "build",
        "create",
        "feature",
        "scaffold",
        "wire",
        "integrate",
    ],
    &[
        "design",
        "architect",
        "architecture",
        "plan",
        "planning",
        "spec",
        "specification",
        "rfc",
        "proposal",
    ],
];

/// Expand task tokens with domain cluster members.
///
/// For each token in `tokens`, if it belongs to any domain cluster, all
/// other members of that cluster are added (if not already present).
fn expand_task_tokens(tokens: &mut Vec<String>) {
    let mut additions: Vec<String> = Vec::new();

    for cluster in DOMAIN_CLUSTERS {
        let has_member = tokens.iter().any(|t| cluster.contains(&t.as_str()));
        if has_member {
            for member in *cluster {
                let s = member.to_string();
                if !tokens.contains(&s) && !additions.contains(&s) {
                    additions.push(s);
                }
            }
        }
    }

    tokens.extend(additions);
}

/// Writing-domain task tokens that imply a `prose` language signal.
///
/// When any of these appear in the task hint, the context gets a `prose`
/// language entry so that writer-type personas can rank on language overlap.
/// These terms must be specific enough to identify a writing task without
/// triggering for incidental mentions in code-focused personas.
const PROSE_TASK_TRIGGERS: &[&str] = &[
    "docs",
    "doc",
    "documentation",
    "changelog",
    "changelogs",
    "readme",
    "tutorial",
    "tutorials",
    "release",
    "notes",
    "prose",
    "writing",
    "copywriting",
    "blog",
    "post",
    "draft",
    "publish",
    "article",
    "essay",
];

/// Augment `languages` with a `prose` entry if the task tokens mention writing
/// domain terms. The injected weight is 2.0 -- higher than the max file-based
/// language weight (1.0) -- to ensure prose-specialist personas rank above
/// generalist personas that happen to also have prose in their language set.
fn augment_languages_from_task(
    mut languages: BTreeMap<String, f32>,
    task_tokens: &[String],
) -> BTreeMap<String, f32> {
    let has_prose_signal = task_tokens
        .iter()
        .any(|t| PROSE_TASK_TRIGGERS.contains(&t.as_str()));
    if has_prose_signal {
        languages.entry("prose".to_string()).or_insert(2.0);
    }
    languages
}

/// Best-effort list of files in the active git working set for `project_root`.
///
/// Runs `git status --porcelain` (which reports staged, unstaged, and untracked
/// changes) with the project as the working directory. Returns an empty vector
/// on any failure -- not a git repository, git not installed, or a non-zero
/// exit -- so callers degrade gracefully to the static project census.
fn git_changed_paths(project_root: &Path) -> Vec<String> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(project_root)
        .args(["status", "--porcelain", "--untracked-files=all"])
        .output();
    match output {
        Ok(o) if o.status.success() => {
            let stdout = String::from_utf8_lossy(&o.stdout);
            parse_porcelain_paths(&stdout)
        }
        _ => Vec::new(),
    }
}

/// Extract file paths from `git status --porcelain` (v1) output.
///
/// Each line is `XY <path>`: a two-character status code, a space, then the
/// path. Rename and copy entries use `XY <old> -> <new>`; the destination path
/// is taken. Surrounding quotes (added by git for paths with special
/// characters) are stripped -- extension mapping still works on the suffix.
/// Capped at [`MAX_CHANGED_FILES`] entries.
fn parse_porcelain_paths(porcelain: &str) -> Vec<String> {
    porcelain
        .lines()
        .filter_map(|line| {
            // Need at least the 2 status chars, a space, and one path char.
            if line.len() < 4 {
                return None;
            }
            let rest = &line[3..];
            // Rename/copy: "old -> new" keeps the destination path.
            let path = match rest.rsplit_once(" -> ") {
                Some((_, new)) => new,
                None => rest,
            };
            let path = path.trim().trim_matches('"');
            if path.is_empty() {
                None
            } else {
                Some(path.to_string())
            }
        })
        .take(MAX_CHANGED_FILES)
        .collect()
}

/// Map a set of changed file paths to per-language hit counts using the same
/// extension table as the project census ([`ext_to_language`]). Paths whose
/// extension does not map to a tracked language are ignored.
pub(crate) fn changed_file_languages(paths: &[String]) -> BTreeMap<String, usize> {
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for path in paths {
        if let Some(ext) = Path::new(path).extension().and_then(|e| e.to_str()) {
            if let Some(lang) = ext_to_language(ext) {
                *counts.entry(lang.to_string()).or_insert(0) += 1;
            }
        }
    }
    counts
}

/// Raise the weight of every language in the active git working set to
/// [`ACTIVE_LANGUAGE_WEIGHT`], so files the user is currently editing outrank
/// the static file-count census. A language already weighted higher (e.g. the
/// prose sentinel) is left untouched.
fn augment_languages_from_git(
    mut languages: BTreeMap<String, f32>,
    changed: &BTreeMap<String, usize>,
) -> BTreeMap<String, f32> {
    for lang in changed.keys() {
        let entry = languages.entry(lang.clone()).or_insert(0.0);
        if *entry < ACTIVE_LANGUAGE_WEIGHT {
            *entry = ACTIVE_LANGUAGE_WEIGHT;
        }
    }
    languages
}

/// Best-effort dependency-name tokens harvested from the project's root
/// manifests (`Cargo.toml` and `package.json`).
///
/// Returns lowercase, deduplicated names (length >= 2) capped at
/// [`MAX_DEP_TOKENS`], in first-seen order. Missing or unreadable manifests
/// contribute nothing, so non-manifest projects yield an empty list.
fn manifest_dependency_tokens(project_root: &Path) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    if let Some(content) = read_capped(&project_root.join("Cargo.toml")) {
        names.extend(parse_cargo_dependencies(&content));
    }
    if let Some(content) = read_capped(&project_root.join("package.json")) {
        names.extend(parse_package_json_dependencies(&content));
    }

    let mut seen = std::collections::HashSet::new();
    names
        .into_iter()
        .map(|n| n.to_lowercase())
        .filter(|n| n.len() >= 2 && seen.insert(n.clone()))
        .take(MAX_DEP_TOKENS)
        .collect()
}

/// Read a file to a string, returning `None` on any error and truncating (at a
/// char boundary) to a bounded size so a pathological manifest cannot blow up
/// the scan. Manifests are normally tiny; the cap is purely defensive.
fn read_capped(path: &Path) -> Option<String> {
    const MAX_MANIFEST_BYTES: usize = 256 * 1024;
    let mut content = std::fs::read_to_string(path).ok()?;
    if content.len() > MAX_MANIFEST_BYTES {
        let mut end = MAX_MANIFEST_BYTES;
        while end > 0 && !content.is_char_boundary(end) {
            end -= 1;
        }
        content.truncate(end);
    }
    Some(content)
}

/// Extract dependency names from `Cargo.toml` content via a line scan.
///
/// Tracks the current `[section]` header and, while inside a dependency section
/// ([`CARGO_DEP_SECTIONS`]), takes the key to the left of `=` on each entry. The
/// `name.workspace = true` / `name.version = ".."` forms yield `name` (the part
/// before the first dot). The `[dependencies.NAME]` sub-table header yields
/// `NAME`. This is a best-effort scan, not a full TOML parse -- unusual
/// formatting simply yields fewer names.
fn parse_cargo_dependencies(content: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut in_dep_section = false;
    for raw in content.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') {
            in_dep_section = CARGO_DEP_SECTIONS.contains(&line);
            if let Some(name) = cargo_subtable_dep(line) {
                if is_valid_dep_name(&name) {
                    names.push(name);
                }
            }
            continue;
        }
        if !in_dep_section {
            continue;
        }
        if let Some((key, _)) = line.split_once('=') {
            // `name.workspace`/`name.version` -> take the leading segment.
            let name = key
                .trim()
                .split('.')
                .next()
                .unwrap_or("")
                .trim()
                .trim_matches('"');
            if is_valid_dep_name(name) {
                names.push(name.to_string());
            }
        }
    }
    names
}

/// Extract the dependency name from a `[dependencies.NAME]` style sub-table
/// header, or `None` if the header is not a dependency sub-table.
fn cargo_subtable_dep(header: &str) -> Option<String> {
    let inner = header.strip_prefix('[')?.strip_suffix(']')?;
    for prefix in [
        "dependencies.",
        "dev-dependencies.",
        "build-dependencies.",
        "workspace.dependencies.",
    ] {
        if let Some(rest) = inner.strip_prefix(prefix) {
            let name = rest.split('.').next().unwrap_or("").trim();
            return Some(name.to_string());
        }
    }
    None
}

/// Extract dependency names from `package.json` content via a line scan.
///
/// Tracks whether the scan is inside a dependency object (`dependencies`,
/// `devDependencies`, `peerDependencies`, `optionalDependencies`) and takes the
/// quoted key from each `"name": "range"` entry, stopping at the closing brace.
/// Best-effort for the conventional pretty-printed layout (one dependency per
/// line); a minified single-line file yields little or nothing.
fn parse_package_json_dependencies(content: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut in_deps = false;
    for raw in content.lines() {
        let line = raw.trim();
        if line.contains("\"dependencies\"")
            || line.contains("\"devDependencies\"")
            || line.contains("\"peerDependencies\"")
            || line.contains("\"optionalDependencies\"")
        {
            in_deps = true;
            continue;
        }
        if !in_deps {
            continue;
        }
        if line.starts_with('}') {
            in_deps = false;
            continue;
        }
        // Entry form: `"name": "range",`.
        if let Some(rest) = line.strip_prefix('"') {
            if let Some((key, _)) = rest.split_once('"') {
                if is_valid_dep_name(key) {
                    names.push(key.to_string());
                }
            }
        }
    }
    names
}

/// Whether `name` looks like a real dependency identifier rather than a parsing
/// artifact: non-empty, bounded length, made of name characters, and containing
/// at least one alphanumeric. Allows the npm scope/path characters `@` and `/`.
fn is_valid_dep_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '/' | '@' | '.'))
        && name.chars().any(|c| c.is_ascii_alphanumeric())
}

/// Push `value` into `vec` only if not already present (cheap dedup during walk).
fn push_unique(vec: &mut Vec<String>, value: &str) {
    if !vec.iter().any(|v| v == value) {
        vec.push(value.to_string());
    }
}

/// Map a file extension to a canonical language name.
/// Returns `None` for extensions that do not map to a tracked language.
fn ext_to_language(ext: &str) -> Option<&'static str> {
    match ext {
        "rs" => Some("rust"),
        "ts" | "tsx" => Some("typescript"),
        "js" | "jsx" | "mjs" | "cjs" => Some("javascript"),
        "py" | "pyi" => Some("python"),
        "go" => Some("go"),
        "java" => Some("java"),
        "rb" => Some("ruby"),
        "c" | "h" => Some("c"),
        "cpp" | "cc" | "cxx" | "hpp" | "hh" => Some("cpp"),
        "md" | "mdx" => Some("markdown"),
        "toml" => Some("toml"),
        "sh" | "bash" | "zsh" => Some("shell"),
        "yaml" | "yml" => Some("yaml"),
        "json" => Some("json"),
        "sql" => Some("sql"),
        _ => None,
    }
}

/// Tokenize `text` into lowercase, alphanumeric tokens of length >= 2,
/// preserving insertion order and deduplicating.
pub(crate) fn tokenize(text: &str) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    text.split(|c: char| !c.is_alphanumeric())
        .map(|t| t.to_lowercase())
        .filter(|t| t.len() >= 2)
        .filter(|t| seen.insert(t.clone()))
        .collect()
}

#[cfg(test)]
/// Verifies project-context collection and tokenization behavior.
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    /// Helper: create a temp dir with a Cargo.toml and a few .rs files.
    fn make_rust_project() -> TempDir {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("Cargo.toml"), "[package]\nname=\"test\"").unwrap();
        fs::write(tmp.path().join("main.rs"), "fn main() {}").unwrap();
        fs::write(tmp.path().join("lib.rs"), "// lib").unwrap();
        tmp
    }

    /// sense() on a Rust project detects rust language and cargo framework.
    #[test]
    fn sense_rust_project() {
        let tmp = make_rust_project();
        let sig = sense(tmp.path(), None);
        assert!(
            sig.languages.contains_key("rust"),
            "expected rust in languages"
        );
        assert!(
            sig.frameworks.contains(&"cargo".to_string()),
            "expected cargo framework"
        );
    }

    /// Task hint tokens are normalized and deduplicated.
    #[test]
    fn sense_task_tokens() {
        let tmp = make_rust_project();
        let sig = sense(tmp.path(), Some("Clippy lint check"));
        assert!(sig.task_tokens.contains(&"clippy".to_string()));
        assert!(sig.task_tokens.contains(&"lint".to_string()));
        assert!(sig.task_tokens.contains(&"check".to_string()));
    }

    /// tokenize drops tokens shorter than 2 chars.
    #[test]
    fn tokenize_filters_short() {
        let tokens = tokenize("a bb ccc");
        assert!(!tokens.contains(&"a".to_string()));
        assert!(tokens.contains(&"bb".to_string()));
        assert!(tokens.contains(&"ccc".to_string()));
    }

    /// SKIP_DIRS entries are not descended into.
    #[test]
    fn sense_skips_git_and_target() {
        let tmp = tempfile::tempdir().unwrap();
        let git_dir = tmp.path().join(".git");
        fs::create_dir_all(&git_dir).unwrap();
        fs::write(git_dir.join("HEAD"), "ref: refs/heads/main").unwrap();
        let sig = sense(tmp.path(), None);
        // Walking .git would produce "shell" or nothing useful; we just verify no panic
        // and that we get a valid signal back.
        let _ = sig;
    }

    /// Debug cluster: matching "debug" injects all other cluster members.
    #[test]
    fn cluster_expansion_adds_all_members() {
        let mut tokens = vec!["debug".to_string(), "rust".to_string()];
        expand_task_tokens(&mut tokens);
        assert!(tokens.contains(&"debugging".to_string()));
        assert!(tokens.contains(&"error".to_string()));
        assert!(tokens.contains(&"crash".to_string()));
    }

    /// Debug cluster expansion is bidirectional: a non-primary member triggers the full cluster.
    #[test]
    fn cluster_expansion_is_bidirectional() {
        let mut tokens = vec!["backtrace".to_string()];
        expand_task_tokens(&mut tokens);
        assert!(tokens.contains(&"debug".to_string()));
        assert!(tokens.contains(&"debugging".to_string()));
    }

    /// Security cluster: matching "vulnerability" injects all other cluster members.
    #[test]
    fn security_cluster_expands() {
        let mut tokens = vec!["vulnerability".to_string()];
        expand_task_tokens(&mut tokens);
        assert!(tokens.contains(&"security".to_string()));
        assert!(tokens.contains(&"cve".to_string()));
    }

    /// Symlinked directories are not followed during the project walk.
    #[cfg(unix)]
    #[test]
    fn walk_does_not_follow_symlinks() {
        use std::os::unix::fs::symlink;
        let tmp = tempfile::tempdir().unwrap();

        // A directory outside the project, full of rust files.
        let outside = tmp.path().join("outside");
        fs::create_dir_all(&outside).unwrap();
        for i in 0..3 {
            fs::write(outside.join(format!("lib{i}.rs")), "fn x() {}").unwrap();
        }

        // The project contains one python file and a symlink to `outside`.
        let project = tmp.path().join("project");
        fs::create_dir_all(&project).unwrap();
        fs::write(project.join("app.py"), "x = 1").unwrap();
        symlink(&outside, project.join("linked")).unwrap();

        let sig = sense(&project, None);
        assert!(
            sig.languages.contains_key("python"),
            "the real project file should still be detected"
        );
        assert!(
            !sig.languages.contains_key("rust"),
            "a symlinked directory must not be followed"
        );
    }

    /// changed_file_languages maps recognized extensions and ignores the rest.
    #[test]
    fn changed_file_languages_maps_extensions() {
        let paths = vec![
            "src/main.rs".to_string(),
            "web/app.py".to_string(),
            "README".to_string(),
            "notes.rs".to_string(),
        ];
        let langs = changed_file_languages(&paths);
        assert_eq!(langs.get("rust").copied(), Some(2));
        assert_eq!(langs.get("python").copied(), Some(1));
        // README has no extension and contributes nothing.
        assert_eq!(langs.len(), 2);
    }

    /// parse_porcelain_paths handles status codes, untracked, and renames.
    #[test]
    fn parse_porcelain_extracts_paths() {
        let out = " M src/main.rs\n?? new.py\nA  staged.go\nR  old.rs -> renamed.rs\n";
        let paths = parse_porcelain_paths(out);
        assert!(paths.contains(&"src/main.rs".to_string()));
        assert!(paths.contains(&"new.py".to_string()));
        assert!(paths.contains(&"staged.go".to_string()));
        // Rename keeps the destination, not the source.
        assert!(paths.contains(&"renamed.rs".to_string()));
        assert!(!paths.iter().any(|p| p.contains("old.rs")));
    }

    /// The git boost lifts an actively-edited minority language above the
    /// census-dominant one.
    #[test]
    fn git_boost_lifts_minority_language() {
        use std::collections::BTreeMap;
        let mut census = BTreeMap::new();
        census.insert("javascript".to_string(), 1.0_f32);
        census.insert("rust".to_string(), 0.3_f32);
        let mut changed = BTreeMap::new();
        changed.insert("rust".to_string(), 2_usize);

        let boosted = augment_languages_from_git(census, &changed);
        assert_eq!(boosted.get("rust").copied(), Some(ACTIVE_LANGUAGE_WEIGHT));
        assert!(boosted["rust"] > boosted["javascript"]);
    }

    /// The git boost never lowers a language already weighted above the active
    /// weight (e.g. the prose sentinel).
    #[test]
    fn git_boost_preserves_prose_sentinel() {
        use std::collections::BTreeMap;
        let mut census = BTreeMap::new();
        census.insert("prose".to_string(), 2.0_f32);
        let mut changed = BTreeMap::new();
        changed.insert("rust".to_string(), 1_usize);

        let boosted = augment_languages_from_git(census, &changed);
        assert_eq!(boosted.get("prose").copied(), Some(2.0));
        assert_eq!(boosted.get("rust").copied(), Some(ACTIVE_LANGUAGE_WEIGHT));
    }

    /// A non-git directory yields no boost: the census weight (<= 1.0) stands
    /// and sense() does not panic.
    #[test]
    fn sense_on_non_git_dir_does_not_boost() {
        let tmp = make_rust_project();
        let sig = sense(tmp.path(), None);
        let rust_w = sig.languages.get("rust").copied().unwrap_or(0.0);
        assert!(
            rust_w > 0.0 && rust_w <= 1.0,
            "expected census weight, not the active boost: {rust_w}"
        );
    }

    /// In a real repo, an actively-edited Rust file outranks a census-dominant
    /// JavaScript majority. Skipped if git is unavailable in the environment.
    #[test]
    fn sense_boosts_git_changed_languages() {
        use std::process::Command;
        if Command::new("git").arg("--version").output().is_err() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let git = |args: &[&str]| {
            Command::new("git")
                .arg("-C")
                .arg(root)
                .args(args)
                .output()
                .expect("git invocation");
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "t@example.com"]);
        git(&["config", "user.name", "tester"]);

        // Census-dominant language: three committed JavaScript files.
        for i in 0..3 {
            fs::write(root.join(format!("a{i}.js")), "var x = 1;").unwrap();
        }
        git(&["add", "-A"]);
        git(&["commit", "-q", "-m", "init"]);

        // Active edit: a single untracked Rust file.
        fs::write(root.join("lib.rs"), "fn x() {}").unwrap();

        let sig = sense(root, None);
        let rust_w = sig.languages.get("rust").copied().unwrap_or(0.0);
        let js_w = sig.languages.get("javascript").copied().unwrap_or(0.0);
        assert!(
            rust_w >= ACTIVE_LANGUAGE_WEIGHT,
            "the actively-edited rust file should boost rust, got {rust_w}"
        );
        assert!(
            rust_w > js_w,
            "actively-edited rust ({rust_w}) should outrank census-dominant js ({js_w})"
        );
    }

    /// parse_cargo_dependencies pulls names from dependency sections only.
    #[test]
    fn parse_cargo_deps_extracts_names() {
        let toml = "[package]\nname = \"x\"\n\n[dependencies]\nserde = \"1\"\n\
                    axum = { version = \"0.7\" }\ntokio.workspace = true\n\n\
                    [dev-dependencies]\ntempfile = \"3\"\n";
        let deps = parse_cargo_dependencies(toml);
        assert!(deps.contains(&"serde".to_string()));
        assert!(deps.contains(&"axum".to_string()));
        assert!(deps.contains(&"tokio".to_string()));
        assert!(deps.contains(&"tempfile".to_string()));
        // The [package] name field is not a dependency.
        assert!(!deps.contains(&"name".to_string()));
    }

    /// parse_cargo_dependencies handles `[dependencies.NAME]` sub-tables and does
    /// not treat their fields as dependencies.
    #[test]
    fn parse_cargo_deps_handles_subtables() {
        let toml = "[dependencies.bb8]\nversion = \"0.9\"\n[dependencies]\nserde = \"1\"\n";
        let deps = parse_cargo_dependencies(toml);
        assert!(deps.contains(&"bb8".to_string()));
        assert!(deps.contains(&"serde".to_string()));
        assert!(!deps.contains(&"version".to_string()));
    }

    /// parse_package_json_dependencies pulls names from dependency objects only.
    #[test]
    fn parse_package_json_deps_extracts_names() {
        let json = "{\n  \"name\": \"app\",\n  \"dependencies\": {\n    \
                    \"react\": \"^18\",\n    \"next\": \"14\"\n  },\n  \
                    \"devDependencies\": {\n    \"vitest\": \"^1\"\n  }\n}\n";
        let deps = parse_package_json_dependencies(json);
        assert!(deps.contains(&"react".to_string()));
        assert!(deps.contains(&"next".to_string()));
        assert!(deps.contains(&"vitest".to_string()));
        // The top-level package name is not a dependency.
        assert!(!deps.contains(&"name".to_string()));
    }

    /// manifest_dependency_tokens lowercases and deduplicates harvested names.
    #[test]
    fn manifest_tokens_lowercases_and_dedups() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(
            tmp.path().join("Cargo.toml"),
            "[dependencies]\naxum = \"0.7\"\nSERDE = \"1\"\naxum = \"0.7\"\n",
        )
        .unwrap();
        let tokens = manifest_dependency_tokens(tmp.path());
        assert!(tokens.contains(&"axum".to_string()));
        assert!(tokens.contains(&"serde".to_string()));
        assert_eq!(
            tokens.iter().filter(|t| *t == "axum").count(),
            1,
            "duplicate dependency names are collapsed"
        );
    }

    /// sense() exposes manifest dependency names via context_tokens.
    #[test]
    fn sense_populates_context_tokens_from_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(
            tmp.path().join("Cargo.toml"),
            "[package]\nname = \"x\"\n[dependencies]\naxum = \"0.7\"\n",
        )
        .unwrap();
        let sig = sense(tmp.path(), None);
        assert!(sig.context_tokens.contains(&"axum".to_string()));
    }
}
