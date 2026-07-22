//! Directory listing tool.

use crate::tool::{AgentTool, ToolResult};
use anyhow::Result;
use serde_json::Value;
use std::path::Path;

/// Lists directory entries with type indicators and human-readable sizes.
pub struct LsTool;

/// Implements the agent tool contract for directory listings.
#[async_trait::async_trait]
impl AgentTool for LsTool {
    /// Returns the directory-listing tool's stable registry name.
    fn name(&self) -> &str {
        "ls"
    }

    /// Describes directory listing output.
    fn description(&self) -> &str {
        "List the contents of a directory. Shows file type indicators, \
         names, and sizes."
    }

    /// Returns the accepted directory path parameter.
    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Directory to list. Defaults to cwd."
                }
            }
        })
    }

    /// Reads, orders, and renders entries from the requested directory.
    async fn execute(&self, params: Value, cwd: &Path) -> Result<ToolResult> {
        let dir_str = params.get("path").and_then(|v| v.as_str()).unwrap_or(".");

        let dir = resolve_path(cwd, dir_str);

        let mut rd = match tokio::fs::read_dir(&dir).await {
            Ok(rd) => rd,
            Err(e) => {
                return Ok(ToolResult {
                    content: format!("Error listing directory: {e}"),
                    is_error: true,
                });
            }
        };

        let mut entries: Vec<(String, u64, EntryKind)> = Vec::new();

        loop {
            match rd.next_entry().await {
                Ok(Some(entry)) => {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    let meta = match entry.metadata().await {
                        Ok(m) => m,
                        Err(_) => {
                            entries.push((name, 0, EntryKind::Unknown));
                            continue;
                        }
                    };

                    let kind = if meta.is_symlink() {
                        EntryKind::Symlink
                    } else if meta.is_dir() {
                        EntryKind::Dir
                    } else {
                        EntryKind::File
                    };

                    let size = if meta.is_file() { meta.len() } else { 0 };
                    entries.push((name, size, kind));
                }
                Ok(None) => break,
                Err(e) => {
                    entries.push((format!("<error: {e}>"), 0, EntryKind::Unknown));
                }
            }
        }

        // Sort: dirs first, then files, alphabetically within each group
        entries.sort_by(|a, b| {
            let a_is_dir = matches!(a.2, EntryKind::Dir);
            let b_is_dir = matches!(b.2, EntryKind::Dir);
            match (a_is_dir, b_is_dir) {
                (true, false) => std::cmp::Ordering::Less,
                (false, true) => std::cmp::Ordering::Greater,
                _ => a.0.to_lowercase().cmp(&b.0.to_lowercase()),
            }
        });

        if entries.is_empty() {
            return Ok(ToolResult {
                content: "(empty directory)".to_string(),
                is_error: false,
            });
        }

        let mut lines = vec![format!("{}:", dir.display())];
        for (name, size, kind) in &entries {
            let display_name = match kind {
                EntryKind::Dir => format!("{name}/"),
                EntryKind::Symlink => format!("{name}@"),
                EntryKind::File | EntryKind::Unknown => name.clone(),
            };

            let size_str = match kind {
                EntryKind::Dir | EntryKind::Symlink | EntryKind::Unknown => String::new(),
                EntryKind::File => format_size(*size),
            };

            if size_str.is_empty() {
                lines.push(format!("  {display_name}"));
            } else {
                lines.push(format!("  {size_str:8}  {display_name}"));
            }
        }

        Ok(ToolResult {
            content: lines.join("\n"),
            is_error: false,
        })
    }
}

/// Classifies directory entries for sorting and display.
enum EntryKind {
    Dir,
    File,
    Symlink,
    Unknown,
}

/// Formats a byte count with a compact binary unit suffix.
fn format_size(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes}B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1}K", bytes as f64 / 1024.0)
    } else if bytes < 1024 * 1024 * 1024 {
        format!("{:.1}M", bytes as f64 / (1024.0 * 1024.0))
    } else {
        format!("{:.1}G", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
    }
}

/// Resolves a directory path relative to the execution directory.
fn resolve_path(cwd: &Path, p: &str) -> std::path::PathBuf {
    let path = std::path::Path::new(p);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}
