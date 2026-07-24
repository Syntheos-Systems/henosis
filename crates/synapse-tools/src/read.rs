//! File read tool.

use crate::ToolExecutionContext;
use crate::confined_fs::ConfinedPath;
use crate::tool::{AgentTool, ToolResult};
use anyhow::Result;
use serde_json::Value;
use std::path::Path;

const DEFAULT_LIMIT: usize = 2000;

/// Reads bounded, line-numbered text from files.
pub struct ReadTool;

/// Implements the agent tool contract for file reads.
#[async_trait::async_trait]
impl AgentTool for ReadTool {
    /// Returns the read tool's stable registry name.
    fn name(&self) -> &str {
        "read"
    }

    /// Describes line-oriented file reading behavior.
    fn description(&self) -> &str {
        "Read the contents of a file with line numbers. Supports optional \
         line offset and limit for reading large files in chunks."
    }

    /// Returns the accepted file, offset, and line-limit parameters.
    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "file_path": {
                    "type": "string",
                    "description": "File path relative to the task root. Absolute paths and parent traversal are rejected."
                },
                "offset": {
                    "type": "number",
                    "description": "Line number to start reading from (1-based). Defaults to 1."
                },
                "limit": {
                    "type": "number",
                    "description": "Maximum number of lines to read. Defaults to 2000."
                }
            },
            "required": ["file_path"]
        })
    }

    /// Reads the requested text range and renders it with one-based line numbers.
    async fn execute(&self, params: Value, cwd: &Path) -> Result<ToolResult> {
        let context = ToolExecutionContext::new(cwd.to_path_buf())?;
        self.execute_with_context(params, &context).await
    }

    /// Reads through the task-root capability retained by the agent session.
    async fn execute_with_context(
        &self,
        params: Value,
        context: &ToolExecutionContext,
    ) -> Result<ToolResult> {
        let file_path = match params.get("file_path").and_then(|v| v.as_str()) {
            Some(p) => p.to_string(),
            None => {
                return Ok(ToolResult {
                    content: "Missing required parameter: file_path".to_string(),
                    is_error: true,
                });
            }
        };

        let offset = params
            .get("offset")
            .and_then(|v| v.as_u64())
            .map(|n| n as usize)
            .unwrap_or(1)
            .saturating_sub(1); // convert 1-based to 0-based index

        let limit = params
            .get("limit")
            .and_then(|v| v.as_u64())
            .map(|n| n as usize)
            .unwrap_or(DEFAULT_LIMIT);

        let path = match ConfinedPath::new(context, &file_path, false) {
            Ok(path) => path,
            Err(e) => {
                return Ok(ToolResult {
                    content: format!("Invalid file path: {e}"),
                    is_error: true,
                });
            }
        };

        let bytes = match path.read() {
            Ok(b) => b,
            Err(e) => {
                return Ok(ToolResult {
                    content: format!("Error reading file: {e}"),
                    is_error: true,
                });
            }
        };

        // Binary detection: look for null bytes in first 8KB
        let probe = &bytes[..bytes.len().min(8192)];
        if probe.contains(&0u8) {
            return Ok(ToolResult {
                content: format!(
                    "File appears to be binary ({} bytes). Cannot display as text.",
                    bytes.len()
                ),
                is_error: true,
            });
        }

        let text = String::from_utf8_lossy(&bytes);
        let lines: Vec<&str> = text.lines().collect();
        let total = lines.len();

        let start = offset.min(total);
        let end = (start + limit).min(total);
        let slice = &lines[start..end];

        let mut output = String::new();
        for (i, line) in slice.iter().enumerate() {
            let lineno = start + i + 1; // back to 1-based for display
            output.push_str(&format!("{lineno:>6}\t{line}\n"));
        }

        if output.is_empty() {
            output = "(empty file or offset past end of file)".to_string();
        }

        Ok(ToolResult {
            content: output,
            is_error: false,
        })
    }
}
