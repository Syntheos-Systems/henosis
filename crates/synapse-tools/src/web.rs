//! Web tools -- fetch URLs and search the web.

use crate::tool::{AgentTool, ToolResult};
use anyhow::Result;
use serde_json::Value;
use std::path::Path;

// ─── WebFetch ───────────────────────────────────────────────────────────────

pub struct WebFetchTool;

#[async_trait::async_trait]
impl AgentTool for WebFetchTool {
    fn name(&self) -> &str {
        "web_fetch"
    }

    fn description(&self) -> &str {
        "Fetch the content of a URL. Returns the page text. \
         Use for reading documentation, APIs, or any web content."
    }

    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "url": { "type": "string", "description": "URL to fetch." },
                "max_bytes": { "type": "number", "description": "Max response bytes. Default 100000." }
            },
            "required": ["url"]
        })
    }

    async fn execute(&self, params: Value, _cwd: &Path) -> Result<ToolResult> {
        let url = match params.get("url").and_then(|v| v.as_str()) {
            Some(u) => u,
            None => {
                return Ok(ToolResult {
                    content: "Missing required parameter: url".into(),
                    is_error: true,
                });
            }
        };
        let max_bytes = params
            .get("max_bytes")
            .and_then(|v| v.as_u64())
            .unwrap_or(100_000) as usize;

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()?;

        // Direct fetch
        let resp = client.get(url).send().await?;
        if !resp.status().is_success() {
            return Ok(ToolResult {
                content: format!("HTTP {}", resp.status()),
                is_error: true,
            });
        }

        let body = resp.text().await?;
        let truncated = if body.len() > max_bytes {
            format!(
                "{}...\n[truncated at {} bytes]",
                &body[..max_bytes],
                max_bytes
            )
        } else {
            body
        };
        Ok(ToolResult {
            content: truncated,
            is_error: false,
        })
    }
}

// ─── WebSearch ──────────────────────────────────────────────────────────────

pub struct WebSearchTool;

#[async_trait::async_trait]
impl AgentTool for WebSearchTool {
    fn name(&self) -> &str {
        "web_search"
    }

    fn description(&self) -> &str {
        "Search the web for information. Returns search results with titles, URLs, and snippets."
    }

    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "Search query." },
                "num_results": { "type": "number", "description": "Number of results. Default 5." }
            },
            "required": ["query"]
        })
    }

    async fn execute(&self, params: Value, _cwd: &Path) -> Result<ToolResult> {
        let query = match params.get("query").and_then(|v| v.as_str()) {
            Some(q) => q,
            None => {
                return Ok(ToolResult {
                    content: "Missing required parameter: query".into(),
                    is_error: true,
                });
            }
        };

        // Use DuckDuckGo HTML (no API key needed)
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .user_agent("Mozilla/5.0 (compatible; Synapse/1.0)")
            .build()?;

        let resp = client
            .get("https://html.duckduckgo.com/html/")
            .query(&[("q", query)])
            .send()
            .await?;

        if !resp.status().is_success() {
            return Ok(ToolResult {
                content: format!("Search failed: HTTP {}", resp.status()),
                is_error: true,
            });
        }

        let body = resp.text().await?;
        let num_results = params
            .get("num_results")
            .and_then(|v| v.as_u64())
            .unwrap_or(5) as usize;

        // Simple HTML parsing for DuckDuckGo results
        let mut results = Vec::new();
        for chunk in body.split("class=\"result__a\"").skip(1).take(num_results) {
            let title = chunk
                .split('>')
                .nth(1)
                .and_then(|s| s.split('<').next())
                .unwrap_or("")
                .trim();
            let url = chunk
                .split("href=\"")
                .nth(0)
                .and_then(|_| chunk.split("href=\"").nth(1))
                .and_then(|s| s.split('"').next())
                .unwrap_or("");
            let snippet = chunk
                .split("class=\"result__snippet\"")
                .nth(1)
                .and_then(|s| s.split('>').nth(1))
                .and_then(|s| s.split('<').next())
                .unwrap_or("")
                .trim();

            if !title.is_empty() {
                results.push(format!("• {}\n  {}\n  {}", title, url, snippet));
            }
        }

        if results.is_empty() {
            Ok(ToolResult {
                content: "No results found.".into(),
                is_error: false,
            })
        } else {
            Ok(ToolResult {
                content: results.join("\n\n"),
                is_error: false,
            })
        }
    }
}
