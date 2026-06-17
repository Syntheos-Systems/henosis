//! Session search tool -- FTS5 search across persisted conversation sessions.

use crate::tool::{AgentTool, ToolResult};
use anyhow::Result;
use serde_json::Value;
use std::path::Path;
use synapse_session::SessionStore;

pub struct SessionSearchTool;

#[async_trait::async_trait]
impl AgentTool for SessionSearchTool {
    fn name(&self) -> &str {
        "session_search"
    }

    fn description(&self) -> &str {
        "Search across past conversation sessions using full-text search. \
         Returns matching turns with context snippets, session IDs, and projects. \
         Useful for recalling previous work, finding past solutions, or checking \
         if something was already discussed."
    }

    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Search query (natural language or keywords)."
                },
                "project": {
                    "type": "string",
                    "description": "Filter results to a specific project name. Omit to search all projects."
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum number of results to return (default 10, max 50)."
                }
            },
            "required": ["query"]
        })
    }

    async fn execute(&self, params: Value, _cwd: &Path) -> Result<ToolResult> {
        let query = match params.get("query").and_then(|v| v.as_str()) {
            Some(q) if !q.trim().is_empty() => q.trim(),
            _ => {
                return Ok(ToolResult {
                    content: "Missing or empty required parameter: query".to_string(),
                    is_error: true,
                });
            }
        };

        let project = params.get("project").and_then(|v| v.as_str());
        let limit = params
            .get("limit")
            .and_then(|v| v.as_u64())
            .unwrap_or(10)
            .min(50) as usize;

        let store = match SessionStore::open_default() {
            Ok(s) => s,
            Err(e) => {
                return Ok(ToolResult {
                    content: format!("Failed to open session store: {e}"),
                    is_error: true,
                });
            }
        };

        let results = if let Some(proj) = project {
            store.search_project(query, proj, limit)?
        } else {
            store.search(query, limit)?
        };

        if results.is_empty() {
            return Ok(ToolResult {
                content: format!("No results for \"{query}\"."),
                is_error: false,
            });
        }

        let mut output = format!("Found {} results for \"{}\":\n\n", results.len(), query);
        for r in &results {
            // Strip FTS5 highlight markers for plain text output
            let snippet = r.snippet.replace(">>>", "").replace("<<<", "");
            output.push_str(&format!(
                "Session #{} ({}, {}) [{}]\n  {}\n\n",
                r.session_id, r.project, r.model, r.role, snippet,
            ));
        }

        Ok(ToolResult {
            content: output,
            is_error: false,
        })
    }
}

pub struct SessionListTool;

#[async_trait::async_trait]
impl AgentTool for SessionListTool {
    fn name(&self) -> &str {
        "session_list"
    }

    fn description(&self) -> &str {
        "List recent conversation sessions with their IDs, projects, models, \
         turn counts, and summaries. Use to find session IDs for deeper inspection."
    }

    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "limit": {
                    "type": "integer",
                    "description": "Number of sessions to return (default 10, max 50)."
                }
            }
        })
    }

    async fn execute(&self, params: Value, _cwd: &Path) -> Result<ToolResult> {
        let limit = params
            .get("limit")
            .and_then(|v| v.as_u64())
            .unwrap_or(10)
            .min(50) as usize;

        let store = match SessionStore::open_default() {
            Ok(s) => s,
            Err(e) => {
                return Ok(ToolResult {
                    content: format!("Failed to open session store: {e}"),
                    is_error: true,
                });
            }
        };

        let sessions = store.list_sessions(limit, 0)?;
        if sessions.is_empty() {
            return Ok(ToolResult {
                content: "No sessions found.".to_string(),
                is_error: false,
            });
        }

        let mut output = format!("{} recent sessions:\n\n", sessions.len());
        for s in &sessions {
            let turns = store.turn_count(s.id).unwrap_or(0);
            let (inp, out) = store.session_token_counts(s.id).unwrap_or((0, 0));
            let summary = s.summary.as_deref().unwrap_or("-");
            output.push_str(&format!(
                "#{} | {} | {} | {turns} turns | tokens: in={inp} out={out}\n  {summary}\n\n",
                s.id, s.project, s.model,
            ));
        }

        Ok(ToolResult {
            content: output,
            is_error: false,
        })
    }
}
