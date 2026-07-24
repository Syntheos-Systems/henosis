//! Content search (grep) tool.

use crate::ToolExecutionContext;
use crate::confined_fs::{ConfinedPath, visit_files};
use crate::tool::{AgentTool, ToolResult};
use anyhow::Result;
use cap_std::fs::DirEntry;
use regex::Regex;
use serde_json::Value;
use std::io::Read;
use std::path::Path;

const DEFAULT_MAX_RESULTS: usize = 50;
const CONTEXT_LINES: usize = 2;
const MAX_FILE_BYTES: usize = 10 * 1024 * 1024; // Skip files larger than 10 MB.

/// Searches text files with regular expressions and bounded context output.
pub struct GrepTool;

/// Implements the agent tool contract for content search.
#[async_trait::async_trait]
impl AgentTool for GrepTool {
    /// Returns the grep tool's stable registry name.
    fn name(&self) -> &str {
        "grep"
    }

    /// Describes regular-expression search behavior.
    fn description(&self) -> &str {
        "Search file contents using a regex pattern. Returns matching file paths, \
         line numbers, and surrounding context lines."
    }

    /// Returns the accepted search, path, filter, and result-limit parameters.
    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "Regex pattern to search for."
                },
                "path": {
                    "type": "string",
                    "description": "File or directory relative to the task root. Defaults to the task root; absolute paths and parent traversal are rejected."
                },
                "include": {
                    "type": "string",
                    "description": "Glob pattern to filter filenames (e.g. '*.rs', '*.{ts,tsx}')."
                },
                "max_results": {
                    "type": "number",
                    "description": "Maximum number of matching lines to return. Defaults to 50."
                }
            },
            "required": ["pattern"]
        })
    }

    /// Searches eligible text files and renders matching lines with context.
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
        let pattern_str = match params.get("pattern").and_then(|v| v.as_str()) {
            Some(p) => p.to_string(),
            None => {
                return Ok(ToolResult {
                    content: "Missing required parameter: pattern".to_string(),
                    is_error: true,
                });
            }
        };

        let re = match Regex::new(&pattern_str) {
            Ok(r) => r,
            Err(e) => {
                return Ok(ToolResult {
                    content: format!("Invalid regex pattern: {e}"),
                    is_error: true,
                });
            }
        };

        let search_path_str = params.get("path").and_then(|v| v.as_str()).unwrap_or(".");
        let search_path = match ConfinedPath::new(context, search_path_str, true) {
            Ok(path) => path,
            Err(e) => {
                return Ok(ToolResult {
                    content: format!("Invalid search path: {e}"),
                    is_error: true,
                });
            }
        };

        let include_glob = params
            .get("include")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        let max_results = params
            .get("max_results")
            .and_then(|v| v.as_u64())
            .map(|n| n as usize)
            .unwrap_or(DEFAULT_MAX_RESULTS);

        let mut results: Vec<String> = Vec::new();
        let mut total_matches = 0usize;
        let metadata = match search_path.metadata() {
            Ok(metadata) => metadata,
            Err(e) => {
                return Ok(ToolResult {
                    content: format!("Error reading search path: {e}"),
                    is_error: true,
                });
            }
        };

        if metadata.is_file() {
            if metadata.len() <= MAX_FILE_BYTES as u64
                && let Ok(bytes) = search_path.read()
            {
                collect_file_matches(
                    &re,
                    search_path.display(),
                    &bytes,
                    include_glob.as_deref(),
                    max_results,
                    &mut results,
                    &mut total_matches,
                );
            }
        } else if metadata.is_dir() {
            let directory = match search_path.open_dir() {
                Ok(directory) => directory,
                Err(e) => {
                    return Ok(ToolResult {
                        content: format!("Error opening search directory: {e}"),
                        is_error: true,
                    });
                }
            };
            let base_display = search_path.display().to_path_buf();
            let walk_result = visit_files(&directory, Path::new(""), &mut |entry, relative| {
                if total_matches >= max_results {
                    return true;
                }
                let Ok(metadata) = entry.metadata() else {
                    return false;
                };
                if metadata.len() > MAX_FILE_BYTES as u64 {
                    return false;
                }
                let Some(bytes) = read_entry(entry) else {
                    return false;
                };
                collect_file_matches(
                    &re,
                    &base_display.join(relative),
                    &bytes,
                    include_glob.as_deref(),
                    max_results,
                    &mut results,
                    &mut total_matches,
                );
                total_matches >= max_results
            });
            if let Err(e) = walk_result {
                return Ok(ToolResult {
                    content: format!("Error walking search directory: {e}"),
                    is_error: true,
                });
            }
        } else {
            return Ok(ToolResult {
                content: "Search path is not a regular file or directory".to_string(),
                is_error: true,
            });
        }

        if results.is_empty() {
            return Ok(ToolResult {
                content: format!("No matches found for pattern: {pattern_str}"),
                is_error: false,
            });
        }

        if total_matches >= max_results {
            results.push(format!("[Results truncated at {max_results} matches]"));
        }

        Ok(ToolResult {
            content: results.join("\n"),
            is_error: false,
        })
    }
}

/// Reads one capability-bound directory entry up to the configured file limit.
fn read_entry(entry: &DirEntry) -> Option<Vec<u8>> {
    let mut file = entry.open().ok()?;
    let mut bytes = Vec::new();
    file.by_ref()
        .take(MAX_FILE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    (bytes.len() <= MAX_FILE_BYTES).then_some(bytes)
}

/// Appends matching lines and bounded context from one eligible text file.
fn collect_file_matches(
    regex: &Regex,
    display_path: &Path,
    bytes: &[u8],
    include_glob: Option<&str>,
    max_results: usize,
    results: &mut Vec<String>,
    total_matches: &mut usize,
) {
    if let Some(glob_pattern) = include_glob {
        let file_name = display_path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("");
        if !glob_matches(glob_pattern, file_name) {
            return;
        }
    }

    let probe = &bytes[..bytes.len().min(8192)];
    if probe.contains(&0u8) {
        return;
    }

    let text = String::from_utf8_lossy(bytes);
    let lines: Vec<&str> = text.lines().collect();
    let file_matches: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter_map(|(index, line)| regex.is_match(line).then_some(index))
        .collect();
    if file_matches.is_empty() {
        return;
    }

    results.push(format!("=== {} ===", display_path.display()));
    for index in file_matches {
        if *total_matches >= max_results {
            break;
        }

        let start = index.saturating_sub(CONTEXT_LINES);
        let end = (index + CONTEXT_LINES + 1).min(lines.len());
        for (context_index, line) in lines.iter().enumerate().take(end).skip(start) {
            let marker = if context_index == index { ">" } else { " " };
            results.push(format!("{}{:>5}: {}", marker, context_index + 1, line));
        }
        results.push(String::new());
        *total_matches += 1;
    }
}

/// Simple glob matching supporting `*`, `**`, and `?`.
/// Operates on a single filename component (no path separators expected in pattern
/// for the typical `*.rs` use case, but `**` short-circuits to always match).
fn glob_matches(pattern: &str, name: &str) -> bool {
    // Handle brace expansion like *.{ts,tsx}
    if let Some(brace_start) = pattern.find('{')
        && let Some(brace_end) = pattern.find('}')
    {
        let prefix = &pattern[..brace_start];
        let suffix = &pattern[brace_end + 1..];
        let alternatives = &pattern[brace_start + 1..brace_end];
        return alternatives
            .split(',')
            .any(|alt| glob_matches(&format!("{prefix}{alt}{suffix}"), name));
    }

    glob_match_simple(pattern, name)
}

/// Matches a filename against a glob expression without brace expansion.
fn glob_match_simple(pattern: &str, name: &str) -> bool {
    let pat: Vec<char> = pattern.chars().collect();
    let txt: Vec<char> = name.chars().collect();
    glob_match_chars(&pat, &txt)
}

/// Recursively matches tokenized filename glob characters.
fn glob_match_chars(pat: &[char], txt: &[char]) -> bool {
    match (pat.first(), txt.first()) {
        (None, None) => true,
        (Some(&'*'), _) => {
            // `**` acts like `*` here (single-component matching)
            let rest_pat = if pat.len() > 1 && pat[1] == '*' {
                &pat[2..]
            } else {
                &pat[1..]
            };
            // Try matching the wildcard against 0..=all remaining chars
            for i in 0..=txt.len() {
                if glob_match_chars(rest_pat, &txt[i..]) {
                    return true;
                }
            }
            false
        }
        (Some(&'?'), Some(_)) => glob_match_chars(&pat[1..], &txt[1..]),
        (Some(p), Some(t)) if p == t => glob_match_chars(&pat[1..], &txt[1..]),
        _ => false,
    }
}
