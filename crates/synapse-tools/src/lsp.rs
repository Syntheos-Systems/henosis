//! LSP tool -- go-to-definition, find-references, diagnostics via rust-analyzer or other LSP servers.
//! Shells out to LSP client commands rather than managing LSP protocol directly.

use crate::tool::{AgentTool, ToolResult};
use anyhow::Result;
use serde_json::Value;
use std::path::Path;
use tokio::process::Command;

// ─── Diagnostics ────────────────────────────────────────────────────────────

/// Runs language-specific diagnostics for a requested source file.
pub struct LspDiagnosticsTool;

/// Implements the agent tool contract for language diagnostics.
#[async_trait::async_trait]
impl AgentTool for LspDiagnosticsTool {
    /// Returns the diagnostics tool's stable registry name.
    fn name(&self) -> &str {
        "lsp_diagnostics"
    }

    /// Describes the language diagnostics capability.
    fn description(&self) -> &str {
        "Get compiler/linter diagnostics for a file. Returns errors, warnings, and hints \
         from rust-analyzer, typescript, etc. Uses `cargo check` for Rust or language-specific commands."
    }

    /// Returns the accepted file and optional language parameters.
    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "file": { "type": "string", "description": "File path to check." },
                "language": { "type": "string", "description": "Language: rust, typescript, go. Default: auto-detect from extension." }
            },
            "required": ["file"]
        })
    }

    /// Runs the selected diagnostics command with private signing secrets removed.
    async fn execute(&self, params: Value, cwd: &Path) -> Result<ToolResult> {
        let file = match params.get("file").and_then(|v| v.as_str()) {
            Some(f) => f,
            None => {
                return Ok(ToolResult {
                    content: "Missing required parameter: file".into(),
                    is_error: true,
                });
            }
        };

        let lang = params
            .get("language")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let language = if !lang.is_empty() {
            lang.to_string()
        } else {
            detect_language(file)
        };

        let (cmd, args) = match language.as_str() {
            "rust" => (
                "cargo",
                vec!["check".to_string(), "--message-format=json".to_string()],
            ),
            "typescript" | "javascript" => (
                "npx",
                vec![
                    "tsc".to_string(),
                    "--noEmit".to_string(),
                    "--pretty".to_string(),
                ],
            ),
            "go" => ("go", vec!["vet".to_string(), file.to_string()]),
            _ => {
                return Ok(ToolResult {
                    content: format!("No diagnostics command for language: {language}"),
                    is_error: true,
                });
            }
        };

        let output = restricted_command(cmd)
            .args(&args)
            .current_dir(cwd)
            .output()
            .await?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        if language == "rust" {
            // Parse cargo check JSON for relevant file
            let mut diagnostics = Vec::new();
            for line in stdout.lines() {
                if let Ok(msg) = serde_json::from_str::<Value>(line)
                    && let Some(message) = msg.get("message")
                {
                    let level = message.get("level").and_then(|l| l.as_str()).unwrap_or("");
                    let text = message
                        .get("message")
                        .and_then(|m| m.as_str())
                        .unwrap_or("");
                    if let Some(spans) = message.get("spans").and_then(|s| s.as_array()) {
                        for span in spans {
                            let span_file =
                                span.get("file_name").and_then(|f| f.as_str()).unwrap_or("");
                            let line_num =
                                span.get("line_start").and_then(|l| l.as_u64()).unwrap_or(0);
                            if file.is_empty() || span_file.contains(file) {
                                diagnostics
                                    .push(format!("{span_file}:{line_num} [{level}] {text}"));
                            }
                        }
                    }
                    if diagnostics.is_empty() && (level == "error" || level == "warning") {
                        diagnostics.push(format!("[{level}] {text}"));
                    }
                }
            }
            if diagnostics.is_empty() {
                Ok(ToolResult {
                    content: "No diagnostics (clean build)".into(),
                    is_error: false,
                })
            } else {
                Ok(ToolResult {
                    content: diagnostics.join("\n"),
                    is_error: false,
                })
            }
        } else {
            let combined = format!("{stdout}\n{stderr}").trim().to_string();
            Ok(ToolResult {
                content: if combined.is_empty() {
                    "No diagnostics".into()
                } else {
                    combined
                },
                is_error: !output.status.success(),
            })
        }
    }
}

// ─── Symbol Search ──────────────────────────────────────────────────────────

/// Searches project sources for symbol declarations.
pub struct LspSymbolSearchTool;

/// Implements the agent tool contract for symbol search.
#[async_trait::async_trait]
impl AgentTool for LspSymbolSearchTool {
    /// Returns the symbol-search tool's stable registry name.
    fn name(&self) -> &str {
        "lsp_symbol_search"
    }

    /// Describes the project symbol-search capability.
    fn description(&self) -> &str {
        "Search for symbol definitions across the project. For Rust uses `cargo doc` metadata \
         or grep-based fallback. Finds structs, functions, traits, types."
    }

    /// Returns the accepted symbol and optional declaration-kind parameters.
    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "symbol": { "type": "string", "description": "Symbol name to find (e.g. AgentTool, default_tools)." },
                "kind": { "type": "string", "description": "Symbol kind: fn, struct, trait, enum, type, impl, const. Optional." }
            },
            "required": ["symbol"]
        })
    }

    /// Searches source files with a restricted child-process environment.
    async fn execute(&self, params: Value, cwd: &Path) -> Result<ToolResult> {
        let symbol = match params.get("symbol").and_then(|v| v.as_str()) {
            Some(s) => s,
            None => {
                return Ok(ToolResult {
                    content: "Missing required parameter: symbol".into(),
                    is_error: true,
                });
            }
        };
        let kind = params.get("kind").and_then(|v| v.as_str()).unwrap_or("");

        // Build a regex pattern based on kind
        let pattern = if kind.is_empty() {
            format!(
                r"(pub\s+)?(fn|struct|trait|enum|type|const|static)\s+{}\b",
                regex::escape(symbol)
            )
        } else {
            format!(
                r"(pub\s+)?{}\s+{}\b",
                regex::escape(kind),
                regex::escape(symbol)
            )
        };

        let output = restricted_command("rg")
            .args([
                "--line-number",
                "--no-heading",
                "--color=never",
                "-e",
                &pattern,
                "--type=rust",
                "--type=go",
                "--type=ts",
            ])
            .current_dir(cwd)
            .output()
            .await;

        match output {
            Ok(out) => {
                let text = String::from_utf8_lossy(&out.stdout);
                if text.is_empty() {
                    Ok(ToolResult {
                        content: format!("No definitions found for '{symbol}'"),
                        is_error: false,
                    })
                } else {
                    let truncated: String = text.lines().take(30).collect::<Vec<_>>().join("\n");
                    Ok(ToolResult {
                        content: truncated,
                        is_error: false,
                    })
                }
            }
            Err(e) => Ok(ToolResult {
                content: format!("Symbol search failed: {e}"),
                is_error: true,
            }),
        }
    }
}

/// Constructs an agent-controlled command without inherited private signing secrets.
fn restricted_command(program: &str) -> Command {
    let mut command = Command::new(program);
    command.env_remove("PIV_PIN");
    command
}

/// Detects the diagnostics language from a source-file extension.
fn detect_language(file: &str) -> String {
    if file.ends_with(".rs") {
        "rust".into()
    } else if file.ends_with(".ts") || file.ends_with(".tsx") {
        "typescript".into()
    } else if file.ends_with(".js") || file.ends_with(".jsx") {
        "javascript".into()
    } else if file.ends_with(".go") {
        "go".into()
    } else {
        "unknown".into()
    }
}

/// Tests restrictions applied to agent-controlled LSP child processes.
#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    /// Verifies LSP children explicitly remove any inherited PIV PIN.
    #[test]
    fn restricted_commands_remove_piv_pin_from_environment() {
        let command = restricted_command("cargo");
        assert!(
            command
                .as_std()
                .get_envs()
                .any(|(name, value)| name == OsStr::new("PIV_PIN") && value.is_none())
        );
    }
}
