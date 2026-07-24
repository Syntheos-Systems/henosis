//! File pattern matching (glob) tool.

use crate::ToolExecutionContext;
use crate::confined_fs::{ConfinedPath, visit_files};
use crate::tool::{AgentTool, ToolResult};
use anyhow::Result;
use serde_json::Value;
use std::path::Path;
use std::time::SystemTime;

/// Finds files whose relative paths match a glob expression.
pub struct GlobTool;

/// Implements the agent tool contract for file discovery.
#[async_trait::async_trait]
impl AgentTool for GlobTool {
    /// Returns the glob tool's stable registry name.
    fn name(&self) -> &str {
        "glob"
    }

    /// Describes recursive path-pattern matching behavior.
    fn description(&self) -> &str {
        "Find files matching a glob pattern. Supports *, **, and ? wildcards. \
         Returns matching paths sorted by modification time (newest first)."
    }

    /// Returns the accepted glob pattern and search-root parameters.
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
                    "description": "Root directory relative to the task root. Defaults to the task root; absolute paths and parent traversal are rejected."
                }
            },
            "required": ["pattern"]
        })
    }

    /// Searches the requested root and returns matches ordered by modification time.
    async fn execute(&self, params: Value, cwd: &Path) -> Result<ToolResult> {
        let context = ToolExecutionContext::new(cwd.to_path_buf())?;
        self.execute_with_context(params, &context).await
    }

    /// Searches through the task-root capability retained by the agent session.
    async fn execute_with_context(
        &self,
        params: Value,
        context: &ToolExecutionContext,
    ) -> Result<ToolResult> {
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
        let search_root = match ConfinedPath::new(context, search_root_str, true) {
            Ok(path) => path,
            Err(e) => {
                return Ok(ToolResult {
                    content: format!("Invalid search root: {e}"),
                    is_error: true,
                });
            }
        };
        let directory = match search_root.open_dir() {
            Ok(directory) => directory,
            Err(e) => {
                return Ok(ToolResult {
                    content: format!("Error opening search root: {e}"),
                    is_error: true,
                });
            }
        };

        let mut matches: Vec<(SystemTime, std::path::PathBuf)> = Vec::new();
        let base_display = search_root.display().to_path_buf();
        let walk_result = visit_files(&directory, Path::new(""), &mut |entry, relative| {
            let rel_str = relative.to_string_lossy().replace('\\', "/");
            if glob_path_matches(&pattern, &rel_str) {
                let modified = entry
                    .metadata()
                    .ok()
                    .and_then(|metadata| metadata.modified().ok())
                    .map(cap_std::time::SystemTime::into_std)
                    .unwrap_or(SystemTime::UNIX_EPOCH);
                matches.push((modified, base_display.join(relative)));
            }
            false
        });
        if let Err(e) = walk_result {
            return Ok(ToolResult {
                content: format!("Error walking search root: {e}"),
                is_error: true,
            });
        }

        // Sort newest first
        matches.sort_by_key(|(mtime, _)| std::cmp::Reverse(*mtime));

        if matches.is_empty() {
            return Ok(ToolResult {
                content: format!("No files matched pattern: {pattern}"),
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

/// Recursively matches tokenized glob and path characters.
fn glob_path_chars(pat: &[char], txt: &[char]) -> bool {
    match (pat.first(), txt.first()) {
        (None, None) => true,
        (None, _) => false,
        (Some(&'*'), _) => {
            // Check for `**`
            if pat.len() >= 2 && pat[1] == '*' {
                // `**` consumes an optional leading slash in the pattern.
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
                // A single `*` matches anything except `/`.
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
