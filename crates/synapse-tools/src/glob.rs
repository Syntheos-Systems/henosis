//! File pattern matching (glob) tool.

use crate::tool::{AgentTool, ToolResult};
use anyhow::Result;
use serde_json::Value;
use std::path::Path;
use std::time::SystemTime;
use walkdir::WalkDir;

pub struct GlobTool;

#[async_trait::async_trait]
impl AgentTool for GlobTool {
    fn name(&self) -> &str {
        "glob"
    }

    fn description(&self) -> &str {
        "Find files matching a glob pattern. Supports *, **, and ? wildcards. \
         Returns matching paths sorted by modification time (newest first)."
    }

    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "Glob pattern to match against file paths (e.g. '**/*.rs', 'src/**/*.ts')."
                },
                "path": {
                    "type": "string",
                    "description": "Root directory to search from. Defaults to cwd."
                }
            },
            "required": ["pattern"]
        })
    }

    async fn execute(&self, params: Value, cwd: &Path) -> Result<ToolResult> {
        let pattern = match params.get("pattern").and_then(|v| v.as_str()) {
            Some(p) => p.to_string(),
            None => {
                return Ok(ToolResult {
                    content: "Missing required parameter: pattern".to_string(),
                    is_error: true,
                });
            }
        };

        let search_root_str = params.get("path").and_then(|v| v.as_str()).unwrap_or(".");
        let search_root = resolve_path(cwd, search_root_str);

        let mut matches: Vec<(SystemTime, std::path::PathBuf)> = Vec::new();

        for entry in WalkDir::new(&search_root)
            .follow_links(false)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_file())
        {
            let file_path = entry.path();

            // Get path relative to search root for matching
            let rel = match file_path.strip_prefix(&search_root) {
                Ok(r) => r,
                Err(_) => file_path,
            };

            let rel_str = rel.to_string_lossy().replace('\\', "/");

            if glob_path_matches(&pattern, &rel_str) {
                let mtime = entry
                    .metadata()
                    .ok()
                    .and_then(|m| m.modified().ok())
                    .unwrap_or(SystemTime::UNIX_EPOCH);
                matches.push((mtime, file_path.to_path_buf()));
            }
        }

        // Sort newest first
        matches.sort_by_key(|(mtime, _)| std::cmp::Reverse(*mtime));

        if matches.is_empty() {
            return Ok(ToolResult {
                content: format!("No files matched pattern: {}", pattern),
                is_error: false,
            });
        }

        let output = matches
            .iter()
            .map(|(_, p)| p.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("\n");

        Ok(ToolResult {
            content: output,
            is_error: false,
        })
    }
}

/// Match a glob pattern against a full relative path string (with `/` separators).
/// `**` matches any number of path segments. `*` matches within a single segment. `?` matches one char.
fn glob_path_matches(pattern: &str, path: &str) -> bool {
    glob_path_chars(
        &pattern.chars().collect::<Vec<_>>(),
        &path.chars().collect::<Vec<_>>(),
    )
}

fn glob_path_chars(pat: &[char], txt: &[char]) -> bool {
    match (pat.first(), txt.first()) {
        (None, None) => true,
        (None, _) => false,
        (Some(&'*'), _) => {
            // Check for `**`
            if pat.len() >= 2 && pat[1] == '*' {
                // `**` — consume optional leading slash in pattern
                let rest = if pat.len() >= 3 && pat[2] == '/' {
                    &pat[3..]
                } else {
                    &pat[2..]
                };
                // Match against empty remainder or any suffix
                if glob_path_chars(rest, txt) {
                    return true;
                }
                // Try advancing one character at a time through txt
                for i in 1..=txt.len() {
                    if glob_path_chars(rest, &txt[i..]) {
                        return true;
                    }
                }
                false
            } else {
                // Single `*` — matches anything except `/`
                let rest_pat = &pat[1..];
                for i in 0..=txt.len() {
                    if i > 0 && txt[i - 1] == '/' {
                        break;
                    }
                    if glob_path_chars(rest_pat, &txt[i..]) {
                        return true;
                    }
                }
                false
            }
        }
        (Some(&'?'), Some(t)) if *t != '/' => glob_path_chars(&pat[1..], &txt[1..]),
        (Some(p), Some(t)) if p == t => glob_path_chars(&pat[1..], &txt[1..]),
        _ => false,
    }
}

fn resolve_path(cwd: &Path, p: &str) -> std::path::PathBuf {
    let path = std::path::Path::new(p);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}
