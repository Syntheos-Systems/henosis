//! Persona resolution for Frameshift integration.
//!
//! A persona is a directory under `~/.local/share/frameshift/personas-private/`
//! that contains `AGENTS.md` (the canonical voice and operating frame) and
//! optionally `GROWTH.md` (running learnings log) plus `pack.toml` (metadata
//! and cwd-matching patterns).
//!
//! Resolution cascade, highest priority first:
//!
//! 1. `FRAMESHIFT_SESSION_KEY` env var -- if set to a persona name and the
//!    directory exists, that wins. This is how the `/persona` slash command
//!    and the Frameshift session activator inject a choice for this session.
//! 2. `pack.toml` patterns -- forward-compatible. If a persona's `pack.toml`
//!    declares glob patterns under a `[match]` table, the resolver checks
//!    them against the cwd. Today no persona ships with patterns, so this
//!    arm is dormant until the Frameshift schema bumps.
//!
//! There are no per-project `.frameshift` files. The persona system is
//! Frameshift-owned -- project directories never carry persona metadata.
//!
//! The resolver returns the `Persona` (name plus loaded AGENTS.md body and
//! optionally a recent slice of GROWTH.md). The CLI feeds it into the
//! `SystemPromptBuilder`.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::fs;
use std::path::{Path, PathBuf};

/// One resolved persona ready for injection into the system prompt.
#[derive(Debug, Clone)]
pub struct Persona {
    /// Persona directory name (e.g. "rust", "frontend"). Stable identifier.
    pub name: String,
    /// AGENTS.md body. Treated as the canonical persona text.
    pub agents_body: String,
    /// Tail of GROWTH.md (most recent observations), if the file exists.
    /// Capped to `growth_tail_lines` lines so it never crowds the prompt.
    pub growth_tail: Option<String>,
    /// Resolved persona directory, useful for follow-up reads.
    pub root: PathBuf,
    /// How the persona was discovered. Surfaced in startup logs.
    pub source: ResolutionSource,
}

/// Where the resolver found the persona. Used for diagnostics and to let
/// the user understand why a given identity is active.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolutionSource {
    /// `FRAMESHIFT_SESSION_KEY` env var picked the persona.
    SessionEnv,
    /// A persona's `pack.toml` declared a cwd-matching pattern that matched.
    PackPattern,
    /// Explicit choice passed at runtime (e.g. via the `/persona` command).
    Explicit,
}

/// Caller-provided knobs for resolution. Defaults work for the normal
/// interactive case; tests and integration callers override the search
/// roots so they don't depend on the real filesystem.
#[derive(Debug, Clone)]
pub struct ResolverOptions {
    /// Personas root. Defaults to `~/.local/share/frameshift/personas-private`.
    pub personas_root: Option<PathBuf>,
    /// Number of trailing GROWTH.md lines to include. 0 disables growth.
    pub growth_tail_lines: usize,
}

/// Implements `Default` behavior for `ResolverOptions`.
impl Default for ResolverOptions {
    /// Handles `default` behavior.
    fn default() -> Self {
        Self {
            personas_root: None,
            growth_tail_lines: 200,
        }
    }
}

/// Resolve a persona for `cwd` using the cascade described in the module
/// docs. Returns `Ok(None)` when no persona can be found -- callers should
/// fall back to the base system prompt rather than panicking.
pub fn resolve(cwd: &Path, opts: &ResolverOptions) -> Result<Option<Persona>> {
    let root = personas_root(opts)?;
    if !root.exists() {
        return Ok(None);
    }

    // 1. Env var wins. The session activator or `/persona` skill sets this.
    if let Ok(name) = std::env::var("FRAMESHIFT_SESSION_KEY")
        && !name.is_empty()
        && let Some(p) = load_persona_by_name(&root, &name, ResolutionSource::SessionEnv, opts)?
    {
        return Ok(Some(p));
    }

    // 2. pack.toml pattern match. The cwd_patterns field on each persona's
    // pack.toml is the only project-side signal we honor -- project trees
    // themselves never carry persona metadata.
    if let Some(p) = match_pack_patterns(cwd, &root, opts)? {
        return Ok(Some(p));
    }

    Ok(None)
}

/// Load a persona by its directory name without going through the cascade.
/// Used by the `/persona <name>` slash command and by rift-bridge when it
/// hands an agent a fixed identity at spawn time.
pub fn load_by_name(name: &str, opts: &ResolverOptions) -> Result<Option<Persona>> {
    let root = personas_root(opts)?;
    load_persona_by_name(&root, name, ResolutionSource::Explicit, opts)
}

/// List every persona on disk. Powers `/persona list` and lets callers
/// enumerate options for a UI picker.
pub fn list_available(opts: &ResolverOptions) -> Result<Vec<String>> {
    let root = personas_root(opts)?;
    if !root.exists() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in fs::read_dir(&root).with_context(|| format!("read {}", root.display()))? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        if let Some(name) = entry.file_name().to_str() {
            // Only count directories that actually carry an AGENTS.md.
            if entry.path().join("AGENTS.md").exists() {
                out.push(name.to_string());
            }
        }
    }
    out.sort();
    Ok(out)
}

// ---------------------------------------------------------------------------
// Internals
// ---------------------------------------------------------------------------

/// Resolve the root directory holding all personas, honoring options first
/// and otherwise the default `$HOME/.local/share/frameshift/personas-private`.
fn personas_root(opts: &ResolverOptions) -> Result<PathBuf> {
    if let Some(p) = &opts.personas_root {
        return Ok(p.clone());
    }
    let home = dirs::home_dir().context("no home directory")?;
    Ok(home
        .join(".local")
        .join("share")
        .join("frameshift")
        .join("personas-private"))
}

/// Load a persona by directory name, returning `None` if the dir or its
/// AGENTS.md is missing rather than erroring -- a typo on the env var
/// should fall back to "no persona", not crash the agent loop.
fn load_persona_by_name(
    root: &Path,
    name: &str,
    source: ResolutionSource,
    opts: &ResolverOptions,
) -> Result<Option<Persona>> {
    let dir = root.join(name);
    let agents = dir.join("AGENTS.md");
    if !agents.exists() {
        return Ok(None);
    }
    let agents_body =
        fs::read_to_string(&agents).with_context(|| format!("read {}", agents.display()))?;

    let growth_tail = if opts.growth_tail_lines > 0 {
        let g = dir.join("GROWTH.md");
        if g.exists() {
            Some(read_tail(&g, opts.growth_tail_lines)?)
        } else {
            None
        }
    } else {
        None
    };

    Ok(Some(Persona {
        name: name.to_string(),
        agents_body,
        growth_tail,
        root: dir,
        source,
    }))
}

/// Read the last `n` lines of a file efficiently for small `n`. For very
/// large GROWTH.md files we still read the whole file -- it's bounded in
/// practice and the simplicity is worth it until measured otherwise.
fn read_tail(path: &Path, n: usize) -> Result<String> {
    let body = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let lines: Vec<&str> = body.lines().collect();
    let start = lines.len().saturating_sub(n);
    Ok(lines[start..].join("\n"))
}

/// Forward-compatible `pack.toml` schema. Today no persona declares
/// `[match].cwd_patterns`, so this is dormant -- but giving the resolver
/// the shape now means a future pack.toml addition becomes a one-line
/// frontend change rather than a refactor. `name` is parsed but unused
/// here; the persona identifier always comes from the directory name.
#[derive(Debug, Deserialize, Default)]
#[allow(dead_code)]
struct PackToml {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    r#match: Option<MatchTable>,
}

/// Match rules inside `pack.toml`. `cwd_patterns` is a list of glob-style
/// strings; the persona resolves if the current path's string form
/// contains any of the patterns as a substring. Real glob support will
/// land when Frameshift commits to a syntax.
#[derive(Debug, Deserialize, Default)]
struct MatchTable {
    #[serde(default)]
    cwd_patterns: Vec<String>,
}

/// Scan every persona's `pack.toml`. The first persona whose patterns
/// match `cwd` wins. Personas are visited in alphabetical order so the
/// behavior is deterministic.
fn match_pack_patterns(cwd: &Path, root: &Path, opts: &ResolverOptions) -> Result<Option<Persona>> {
    let cwd_str = cwd.to_string_lossy();
    let mut dirs: Vec<PathBuf> = fs::read_dir(root)
        .with_context(|| format!("read {}", root.display()))?
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .map(|e| e.path())
        .collect();
    dirs.sort();

    for dir in dirs {
        let pack = dir.join("pack.toml");
        if !pack.exists() {
            continue;
        }
        let text = fs::read_to_string(&pack).with_context(|| format!("read {}", pack.display()))?;
        let parsed: PackToml = toml::from_str(&text).unwrap_or_default();
        let patterns = parsed.r#match.map(|m| m.cwd_patterns).unwrap_or_default();
        for pat in patterns {
            if !pat.is_empty() && cwd_str.contains(&pat) {
                let name = dir
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("")
                    .to_string();
                if let Some(p) =
                    load_persona_by_name(root, &name, ResolutionSource::PackPattern, opts)?
                {
                    return Ok(Some(p));
                }
            }
        }
    }
    Ok(None)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::Mutex;
    use tempfile::TempDir;

    /// Serializes tests that mutate the process-wide Frameshift session env var.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// Restores the Frameshift session env var after an env-sensitive test.
    fn restore_session_key(original: Option<String>) {
        match original {
            Some(value) => unsafe { std::env::set_var("FRAMESHIFT_SESSION_KEY", value) },
            None => unsafe { std::env::remove_var("FRAMESHIFT_SESSION_KEY") },
        }
    }

    /// Build a fake personas root with one persona for hermetic tests.
    fn fake_persona_root(name: &str, body: &str) -> TempDir {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join(name);
        fs::create_dir_all(&p).unwrap();
        fs::write(p.join("AGENTS.md"), body).unwrap();
        dir
    }

    /// Handles `env_var_resolution` behavior.
    #[test]
    fn env_var_resolution() {
        let _guard = ENV_LOCK.lock().unwrap();
        let original = std::env::var("FRAMESHIFT_SESSION_KEY").ok();
        let td = fake_persona_root("rust", "# Rust persona body");
        unsafe { std::env::set_var("FRAMESHIFT_SESSION_KEY", "rust") };
        let opts = ResolverOptions {
            personas_root: Some(td.path().to_path_buf()),
            ..Default::default()
        };
        let resolved = resolve(Path::new("/"), &opts).unwrap().unwrap();
        restore_session_key(original);
        assert_eq!(resolved.name, "rust");
        assert_eq!(resolved.source, ResolutionSource::SessionEnv);
        assert!(resolved.agents_body.contains("Rust persona body"));
    }

    /// Handles `pack_pattern_resolution` behavior.
    #[test]
    fn pack_pattern_resolution() {
        let _guard = ENV_LOCK.lock().unwrap();
        let original = std::env::var("FRAMESHIFT_SESSION_KEY").ok();
        let td = TempDir::new().unwrap();
        let dir = td.path().join("rust");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("AGENTS.md"), "# Rust").unwrap();
        fs::write(
            dir.join("pack.toml"),
            r#"
name = "rust"

[match]
cwd_patterns = ["projects/Kleos", "projects/synapse"]
"#,
        )
        .unwrap();

        let opts = ResolverOptions {
            personas_root: Some(td.path().to_path_buf()),
            ..Default::default()
        };
        unsafe { std::env::remove_var("FRAMESHIFT_SESSION_KEY") };
        let matching_cwd = td.path().join("workspace/projects/synapse");
        fs::create_dir_all(&matching_cwd).unwrap();
        let resolved = resolve(&matching_cwd, &opts).unwrap().unwrap();
        restore_session_key(original);
        assert_eq!(resolved.name, "rust");
        assert_eq!(resolved.source, ResolutionSource::PackPattern);
    }

    /// Handles `missing_persona_returns_none` behavior.
    #[test]
    fn missing_persona_returns_none() {
        let _guard = ENV_LOCK.lock().unwrap();
        let original = std::env::var("FRAMESHIFT_SESSION_KEY").ok();
        let td = TempDir::new().unwrap();
        let opts = ResolverOptions {
            personas_root: Some(td.path().to_path_buf()),
            ..Default::default()
        };
        unsafe { std::env::remove_var("FRAMESHIFT_SESSION_KEY") };
        assert!(resolve(Path::new("/"), &opts).unwrap().is_none());
        restore_session_key(original);
    }

    /// Handles `list_available_returns_only_personas_with_agents_md` behavior.
    #[test]
    fn list_available_returns_only_personas_with_agents_md() {
        let td = TempDir::new().unwrap();
        // valid persona
        let p1 = td.path().join("rust");
        fs::create_dir_all(&p1).unwrap();
        fs::write(p1.join("AGENTS.md"), "x").unwrap();
        // dir without AGENTS.md should be skipped
        let p2 = td.path().join("empty");
        fs::create_dir_all(&p2).unwrap();
        let opts = ResolverOptions {
            personas_root: Some(td.path().to_path_buf()),
            ..Default::default()
        };
        let names = list_available(&opts).unwrap();
        assert_eq!(names, vec!["rust".to_string()]);
    }
}
