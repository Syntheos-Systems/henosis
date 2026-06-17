//! File write tool.

use crate::tool::{AgentTool, ToolResult};
use anyhow::Result;
use serde_json::Value;
use std::path::Path;

pub struct WriteTool;

#[async_trait::async_trait]
impl AgentTool for WriteTool {
    fn name(&self) -> &str {
        "write"
    }

    fn description(&self) -> &str {
        "Write content to a file, creating parent directories as needed. \
         Overwrites the file if it already exists."
    }

    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "file_path": {
                    "type": "string",
                    "description": "Absolute or relative path to the file to write."
                },
                "content": {
                    "type": "string",
                    "description": "Content to write to the file."
                }
            },
            "required": ["file_path", "content"]
        })
    }

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

        let content = match params.get("content").and_then(|v| v.as_str()) {
            Some(c) => c.to_string(),
            None => {
                return Ok(ToolResult {
                    content: "Missing required parameter: content".to_string(),
                    is_error: true,
                });
            }
        };

        let path = resolve_path(cwd, &file_path);

        if let Some(parent) = path.parent()
            && let Err(e) = tokio::fs::create_dir_all(parent).await
        {
            return Ok(ToolResult {
                content: format!("Failed to create parent directories: {}", e),
                is_error: true,
            });
        }

        let byte_count = content.len();
        if let Err(e) = tokio::fs::write(&path, content.as_bytes()).await {
            return Ok(ToolResult {
                content: format!("Failed to write file: {}", e),
                is_error: true,
            });
        }

        Ok(ToolResult {
            content: format!("Written {} bytes to {}", byte_count, path.display()),
            is_error: false,
        })
    }
}

fn resolve_path(cwd: &Path, file_path: &str) -> std::path::PathBuf {
    let p = std::path::Path::new(file_path);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        cwd.join(p)
    }
}
