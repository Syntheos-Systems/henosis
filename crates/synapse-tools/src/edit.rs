//! File edit (string replacement) tool.

use crate::tool::{AgentTool, ToolResult};
use anyhow::Result;
use serde_json::Value;
use std::path::Path;

/// Replaces exact text within files and reports a compact diff.
pub struct EditTool;

/// Implements the agent tool contract for file edits.
#[async_trait::async_trait]
impl AgentTool for EditTool {
    /// Returns the edit tool's stable registry name.
    fn name(&self) -> &str {
        "edit"
    }

    /// Describes exact string replacement behavior.
    fn description(&self) -> &str {
        "Replace a specific string in a file with a new string. \
         By default the old_string must appear exactly once in the file. \
         Use replace_all to replace every occurrence."
    }

    /// Returns the accepted file and replacement parameters.
    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "file_path": {
                    "type": "string",
                    "description": "Absolute or relative path to the file to edit."
                },
                "old_string": {
                    "type": "string",
                    "description": "The exact string to find and replace."
                },
                "new_string": {
                    "type": "string",
                    "description": "The replacement string."
                },
                "replace_all": {
                    "type": "boolean",
                    "description": "If true, replace all occurrences. Defaults to false."
                }
            },
            "required": ["file_path", "old_string", "new_string"]
        })
    }

    /// Applies the requested replacement and returns a unified diff snippet.
    async fn execute(&self, params: Value, cwd: &Path) -> Result<ToolResult> {
        let file_path = match params.get("file_path").and_then(|v| v.as_str()) {
            Some(p) => p.to_string(),
            None => {
                return Ok(ToolResult {
                    content: "Missing required parameter: file_path".to_string(),
                    is_error: true,
                });
            }
        };

        let old_string = match params.get("old_string").and_then(|v| v.as_str()) {
            Some(s) => s.to_string(),
            None => {
                return Ok(ToolResult {
                    content: "Missing required parameter: old_string".to_string(),
                    is_error: true,
                });
            }
        };

        let new_string = match params.get("new_string").and_then(|v| v.as_str()) {
            Some(s) => s.to_string(),
            None => {
                return Ok(ToolResult {
                    content: "Missing required parameter: new_string".to_string(),
                    is_error: true,
                });
            }
        };

        let replace_all = params
            .get("replace_all")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let path = resolve_path(cwd, &file_path);

        let original = match tokio::fs::read_to_string(&path).await {
            Ok(s) => s,
            Err(e) => {
                return Ok(ToolResult {
                    content: format!("Error reading file: {e}"),
                    is_error: true,
                });
            }
        };

        let count = original.matches(old_string.as_str()).count();

        if count == 0 {
            return Ok(ToolResult {
                content: "old_string not found in file".to_string(),
                is_error: true,
            });
        }

        if !replace_all && count > 1 {
            return Ok(ToolResult {
                content: format!(
                    "old_string appears {count} times in the file. \
                     Provide more context to make it unique, or set replace_all to true.",
                ),
                is_error: true,
            });
        }

        let new_content = if replace_all {
            original.replace(old_string.as_str(), new_string.as_str())
        } else {
            original.replacen(old_string.as_str(), new_string.as_str(), 1)
        };

        if let Err(e) = tokio::fs::write(&path, new_content.as_bytes()).await {
            return Ok(ToolResult {
                content: format!("Failed to write file: {e}"),
                is_error: true,
            });
        }

        let diff = unified_diff_snippet(&original, &new_content, &file_path);

        Ok(ToolResult {
            content: diff,
            is_error: false,
        })
    }
}

/// Produce a minimal unified diff showing changed lines with a few lines of context.
fn unified_diff_snippet(original: &str, new_content: &str, path: &str) -> String {
    let old_lines: Vec<&str> = original.lines().collect();
    let new_lines: Vec<&str> = new_content.lines().collect();

    // Find first and last changed line
    let first_change = old_lines
        .iter()
        .zip(new_lines.iter())
        .position(|(a, b)| a != b)
        .unwrap_or(0);

    let last_change_old = old_lines
        .iter()
        .enumerate()
        .rev()
        .zip(new_lines.iter().rev())
        .find(|((_, a), b)| a != b)
        .map(|((i, _), _)| i)
        .unwrap_or(old_lines.len().saturating_sub(1));

    let context = 3usize;
    let start = first_change.saturating_sub(context);
    let end_old = (last_change_old + context + 1).min(old_lines.len());

    // Approximate end in new content
    let delta = new_lines.len() as isize - old_lines.len() as isize;
    let end_new = ((end_old as isize + delta) as usize).min(new_lines.len());
    let new_start = start;

    let mut out = String::new();
    out.push_str(&format!("--- {path}\n+++ {path}\n"));
    out.push_str(&format!(
        "@@ -{},{} +{},{} @@\n",
        start + 1,
        end_old - start,
        new_start + 1,
        end_new.saturating_sub(new_start)
    ));

    for line in &old_lines[start..end_old] {
        out.push('-');
        out.push_str(line);
        out.push('\n');
    }
    for line in &new_lines[new_start..end_new] {
        out.push('+');
        out.push_str(line);
        out.push('\n');
    }

    out
}

/// Resolves a user-supplied file path relative to the execution directory.
fn resolve_path(cwd: &Path, file_path: &str) -> std::path::PathBuf {
    let p = std::path::Path::new(file_path);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        cwd.join(p)
    }
}
