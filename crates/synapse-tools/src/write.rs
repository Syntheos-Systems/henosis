//! File write tool.

use crate::ToolExecutionContext;
use crate::confined_fs::ConfinedPath;
use crate::tool::{AgentTool, ToolResult};
use anyhow::Result;
use serde_json::Value;
use std::path::Path;

/// Writes complete file contents and creates missing parent directories.
pub struct WriteTool;

/// Implements the agent tool contract for file writes.
#[async_trait::async_trait]
impl AgentTool for WriteTool {
    /// Returns the write tool's stable registry name.
    fn name(&self) -> &str {
        "write"
    }

    /// Describes whole-file write behavior.
    fn description(&self) -> &str {
        "Write content to a file, creating parent directories as needed. \
         Overwrites the file if it already exists."
    }

    /// Returns the accepted file path and content parameters.
    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "file_path": {
                    "type": "string",
                    "description": "File path relative to the task root. Absolute paths and parent traversal are rejected."
                },
                "content": {
                    "type": "string",
                    "description": "Content to write to the file."
                }
            },
            "required": ["file_path", "content"]
        })
    }

    /// Writes the requested content and reports the resulting byte count.
    async fn execute(&self, params: Value, cwd: &Path) -> Result<ToolResult> {
        let context = ToolExecutionContext::new(cwd.to_path_buf())?;
        self.execute_with_context(params, &context).await
    }

    /// Writes through the task-root capability retained by the agent session.
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

        let content = match params.get("content").and_then(|v| v.as_str()) {
            Some(c) => c.to_string(),
            None => {
                return Ok(ToolResult {
                    content: "Missing required parameter: content".to_string(),
                    is_error: true,
                });
            }
        };

        let path = match ConfinedPath::new(context, &file_path, false) {
            Ok(path) => path,
            Err(e) => {
                return Ok(ToolResult {
                    content: format!("Invalid file path: {e}"),
                    is_error: true,
                });
            }
        };

        let byte_count = content.len();
        if let Err(e) = path.write(content.as_bytes()) {
            return Ok(ToolResult {
                content: format!("Failed to write file: {e}"),
                is_error: true,
            });
        }

        Ok(ToolResult {
            content: format!(
                "Written {} bytes to {}",
                byte_count,
                path.display().display()
            ),
            is_error: false,
        })
    }
}
