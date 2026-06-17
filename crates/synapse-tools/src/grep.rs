//! Content search (grep) tool.

use crate::tool::{AgentTool, ToolResult};
use anyhow::Result;
use regex::Regex;
use serde_json::Value;
use std::path::Path;
use walkdir::WalkDir;

const DEFAULT_MAX_RESULTS: usize = 50;
const CONTEXT_LINES: usize = 2;
const MAX_FILE_BYTES: usize = 10 * 1024 * 1024; // 10 MB — skip larger files

pub struct GrepTool;

#[async_trait::async_trait]
impl AgentTool for GrepTool {
    fn name(&self) -> &str {
        "grep"
    }

    fn description(&self) -> &str {
        "Search file contents using a regex pattern. Returns matching file paths, \
         line numbers, and surrounding context lines."
    }

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
                    "description": "Directory or file to search. Defaults to cwd."
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

    async fn execute(&self, params: Value, cwd: &Path) -> Result<ToolResult> {
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
                    content: format!("Invalid regex pattern: {}", e),
                    is_error: true,
                });
            }
        };

        let search_path_str = params.get("path").and_then(|v| v.as_str()).unwrap_or(".");
        let search_path = resolve_path(cwd, search_path_str);

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

        for entry in WalkDir::new(&search_path)
            .follow_links(false)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_file())
        {
            if total_matches >= max_results {
                break;
            }

            let file_path = entry.path();

            // Apply include glob filter
            if let Some(ref glob_pat) = include_glob {
                let file_name = file_path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if !glob_matches(glob_pat, file_name) {
                    continue;
                }
            }

            // Skip large files
            if let Ok(meta) = entry.metadata()
                && meta.len() > MAX_FILE_BYTES as u64
            {
                continue;
            }

            let bytes = match std::fs::read(file_path) {
                Ok(b) => b,
                Err(_) => continue,
            };

            // Skip binary files
            let probe = &bytes[..bytes.len().min(8192)];
            if probe.contains(&0u8) {
                continue;
            }

            let text = String::from_utf8_lossy(&bytes);
            let lines: Vec<&str> = text.lines().collect();

            let mut file_matches: Vec<(usize, String)> = Vec::new();
            for (idx, line) in lines.iter().enumerate() {
                if re.is_match(line) {
                    file_matches.push((idx, line.to_string()));
                }
            }

            if file_matches.is_empty() {
                continue;
            }

            let display_path = file_path.to_string_lossy();
            results.push(format!("=== {} ===", display_path));

            for (idx, _line) in &file_matches {
                if total_matches >= max_results {
                    break;
                }

                let start = idx.saturating_sub(CONTEXT_LINES);
                let end = (idx + CONTEXT_LINES + 1).min(lines.len());

                for (ctx_idx, line) in lines.iter().enumerate().take(end).skip(start) {
                    let marker = if ctx_idx == *idx { ">" } else { " " };
                    results.push(format!("{}{:>5}: {}", marker, ctx_idx + 1, line));
                }
                results.push(String::new());
                total_matches += 1;
            }
        }

        if results.is_empty() {
            return Ok(ToolResult {
                content: format!("No matches found for pattern: {}", pattern_str),
                is_error: false,
            });
        }

        if total_matches >= max_results {
            results.push(format!("[Results truncated at {} matches]", max_results));
        }

        Ok(ToolResult {
            content: results.join("\n"),
            is_error: false,
        })
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
            .any(|alt| glob_matches(&format!("{}{}{}", prefix, alt, suffix), name));
    }

    glob_match_simple(pattern, name)
}

fn glob_match_simple(pattern: &str, name: &str) -> bool {
    let pat: Vec<char> = pattern.chars().collect();
    let txt: Vec<char> = name.chars().collect();
    glob_match_chars(&pat, &txt)
}

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

fn resolve_path(cwd: &Path, p: &str) -> std::path::PathBuf {
    let path = std::path::Path::new(p);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}
