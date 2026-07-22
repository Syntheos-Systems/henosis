//! Agent-forge tools -- direct HTTP calls to the forge HTTP server.
//! No MCP, no SSE. Just POST /tool/:name.

use crate::tool::{AgentTool, ToolResult};
use anyhow::Result;
use serde_json::Value;
use std::path::Path;

/// Returns the configured Agent-Forge service URL.
fn forge_url() -> String {
    std::env::var("FORGE_URL").unwrap_or_else(|_| "http://127.0.0.1:4201".to_string())
}

/// Invokes an Agent-Forge HTTP tool and extracts its textual response.
async fn forge_call(tool_name: &str, args: &Value) -> Result<String> {
    let url = format!("{}/tool/{}", forge_url(), tool_name);
    let client = reqwest::Client::new();
    let resp = client
        .post(&url)
        .header("Content-Type", "application/json")
        .json(args)
        .send()
        .await?;

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        anyhow::bail!("forge {tool_name} error {status}: {text}");
    }

    let body: Value = resp.json().await?;
    // Extract text from content array
    if let Some(content) = body.get("content").and_then(|c| c.as_array()) {
        let texts: Vec<&str> = content
            .iter()
            .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
            .collect();
        if !texts.is_empty() {
            return Ok(texts.join("\n"));
        }
    }
    Ok(serde_json::to_string_pretty(&body)?)
}

// ─── repo_map ───────────────────────────────────────────────────────────────

pub struct RepoMapTool;

/// Implements the agent tool contract for repository maps.
#[async_trait::async_trait]
impl AgentTool for RepoMapTool {
    /// Returns the repository-map tool's stable registry name.
    fn name(&self) -> &str {
        "repo_map"
    }

    /// Describes structural repository mapping.
    fn description(&self) -> &str {
        "Generate a structural map of a codebase showing files, functions, and classes \
         ranked by importance. Use before touching any unfamiliar codebase."
    }

    /// Returns the repository-map parameter schema.
    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Root path to map. Defaults to cwd." },
                "focus": { "type": "string", "description": "Focus on specific file or symbol." },
                "max_tokens": { "type": "number", "description": "Max output tokens. Default 4000." }
            }
        })
    }

    /// Requests a structural map for the supplied repository path.
    async fn execute(&self, params: Value, cwd: &Path) -> Result<ToolResult> {
        let mut args = params.clone();
        if args.get("path").is_none() {
            args["path"] = Value::String(cwd.to_string_lossy().to_string());
        }
        match forge_call("repo_map", &args).await {
            Ok(text) => Ok(ToolResult {
                content: text,
                is_error: false,
            }),
            Err(e) => Ok(ToolResult {
                content: format!("repo_map failed: {e}"),
                is_error: true,
            }),
        }
    }
}

// ─── search_code ────────────────────────────────────────────────────────────

pub struct SearchCodeTool;

/// Implements the agent tool contract for structural code search.
#[async_trait::async_trait]
impl AgentTool for SearchCodeTool {
    /// Returns the code-search tool's stable registry name.
    fn name(&self) -> &str {
        "search_code"
    }

    /// Describes structural symbol search.
    fn description(&self) -> &str {
        "Find functions, classes, or variables by name using AST-aware search. \
         More precise than grep for code structure."
    }

    /// Returns the code-search parameter schema.
    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "Name to search for." },
                "path": { "type": "string", "description": "Directory to search. Defaults to cwd." },
                "type": { "type": "string", "description": "Filter: function, class, variable, or all." },
                "language": { "type": "string", "description": "Filter by language: rust, js, python, etc." }
            },
            "required": ["query"]
        })
    }

    /// Searches the supplied repository for matching code symbols.
    async fn execute(&self, params: Value, cwd: &Path) -> Result<ToolResult> {
        let mut args = params.clone();
        if args.get("path").is_none() {
            args["path"] = Value::String(cwd.to_string_lossy().to_string());
        }
        match forge_call("search_code", &args).await {
            Ok(text) => Ok(ToolResult {
                content: text,
                is_error: false,
            }),
            Err(e) => Ok(ToolResult {
                content: format!("search_code failed: {e}"),
                is_error: true,
            }),
        }
    }
}

// ─── log_hypothesis ─────────────────────────────────────────────────────────

pub struct LogHypothesisTool;

/// Implements the agent tool contract for hypothesis logging.
#[async_trait::async_trait]
impl AgentTool for LogHypothesisTool {
    /// Returns the hypothesis tool's stable registry name.
    fn name(&self) -> &str {
        "log_hypothesis"
    }

    /// Describes pre-fix hypothesis logging.
    fn description(&self) -> &str {
        "Log a debugging hypothesis before attempting a fix. Required before editing code \
         to fix a bug. Forces structured reasoning about root cause."
    }

    /// Returns the hypothesis parameter schema.
    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "error_description": { "type": "string", "description": "What error or bug you observed." },
                "root_cause": { "type": "string", "description": "Your hypothesis for the root cause." },
                "planned_fix": { "type": "string", "description": "What you plan to do about it." },
                "files_to_touch": { "type": "array", "items": { "type": "string" }, "description": "Files you plan to modify." }
            },
            "required": ["error_description", "root_cause", "planned_fix"]
        })
    }

    /// Records a debugging hypothesis with Agent-Forge.
    async fn execute(&self, params: Value, _cwd: &Path) -> Result<ToolResult> {
        match forge_call("log_hypothesis", &params).await {
            Ok(text) => Ok(ToolResult {
                content: text,
                is_error: false,
            }),
            Err(e) => Ok(ToolResult {
                content: format!("log_hypothesis failed: {e}"),
                is_error: true,
            }),
        }
    }
}

// ─── log_outcome ────────────────────────────────────────────────────────────

pub struct LogOutcomeTool;

/// Implements the agent tool contract for hypothesis outcomes.
#[async_trait::async_trait]
impl AgentTool for LogOutcomeTool {
    /// Returns the outcome tool's stable registry name.
    fn name(&self) -> &str {
        "log_outcome"
    }

    /// Describes hypothesis outcome recording.
    fn description(&self) -> &str {
        "Log the outcome of a fix attempt. Required after every fix. Records whether \
         hypothesis was correct and what actually worked."
    }

    /// Returns the outcome parameter schema.
    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "hypothesis_id": { "type": "string", "description": "ID from log_hypothesis." },
                "correct": { "type": "boolean", "description": "Was the hypothesis correct?" },
                "actual_cause": { "type": "string", "description": "What the actual cause was (if different)." },
                "fix_description": { "type": "string", "description": "What fix was applied." }
            },
            "required": ["hypothesis_id", "correct"]
        })
    }

    /// Records the observed result of a prior hypothesis.
    async fn execute(&self, params: Value, _cwd: &Path) -> Result<ToolResult> {
        match forge_call("log_outcome", &params).await {
            Ok(text) => Ok(ToolResult {
                content: text,
                is_error: false,
            }),
            Err(e) => Ok(ToolResult {
                content: format!("log_outcome failed: {e}"),
                is_error: true,
            }),
        }
    }
}

// ─── recall_errors ──────────────────────────────────────────────────────────

pub struct RecallErrorsTool;

/// Implements the agent tool contract for prior-error recall.
#[async_trait::async_trait]
impl AgentTool for RecallErrorsTool {
    /// Returns the error-recall tool's stable registry name.
    fn name(&self) -> &str {
        "recall_errors"
    }

    /// Describes previous-error retrieval.
    fn description(&self) -> &str {
        "Search past agent mistakes and debugging outcomes. Check this before debugging \
         to avoid repeating known wrong hypotheses."
    }

    /// Returns the error-recall parameter schema.
    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "Error or topic to search for." },
                "limit": { "type": "number", "description": "Max results. Default 5." }
            },
            "required": ["query"]
        })
    }

    /// Searches Agent-Forge for relevant prior failures.
    async fn execute(&self, params: Value, _cwd: &Path) -> Result<ToolResult> {
        match forge_call("recall_errors", &params).await {
            Ok(text) => Ok(ToolResult {
                content: text,
                is_error: false,
            }),
            Err(e) => Ok(ToolResult {
                content: format!("recall_errors failed: {e}"),
                is_error: true,
            }),
        }
    }
}

// ─── test_impact ────────────────────────────────────────────────────────────

pub struct TestImpactTool;

/// Implements the agent tool contract for test-impact analysis.
#[async_trait::async_trait]
impl AgentTool for TestImpactTool {
    /// Returns the test-impact tool's stable registry name.
    fn name(&self) -> &str {
        "test_impact"
    }

    /// Describes test-impact analysis.
    fn description(&self) -> &str {
        "Given a list of changed files, returns which tests to run. Use instead of \
         running the full test suite after every change."
    }

    /// Returns the test-impact parameter schema.
    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "changed_files": { "type": "array", "items": { "type": "string" }, "description": "Files that were modified." },
                "path": { "type": "string", "description": "Project root. Defaults to cwd." }
            },
            "required": ["changed_files"]
        })
    }

    /// Identifies tests affected by the supplied code change.
    async fn execute(&self, params: Value, cwd: &Path) -> Result<ToolResult> {
        let mut args = params.clone();
        if args.get("path").is_none() {
            args["path"] = Value::String(cwd.to_string_lossy().to_string());
        }
        match forge_call("test_impact", &args).await {
            Ok(text) => Ok(ToolResult {
                content: text,
                is_error: false,
            }),
            Err(e) => Ok(ToolResult {
                content: format!("test_impact failed: {e}"),
                is_error: true,
            }),
        }
    }
}

// ─── execute ────────────────────────────────────────────────────────────────

pub struct ExecuteTool;

/// Implements the agent tool contract for Forge-managed command execution.
#[async_trait::async_trait]
impl AgentTool for ExecuteTool {
    /// Returns the execution tool's stable registry name.
    fn name(&self) -> &str {
        "execute"
    }

    /// Describes Forge-managed command execution.
    fn description(&self) -> &str {
        "Run a command and return structured output. Use for running code, builds, or scripts. \
         Returns exit code, stdout, stderr separately."
    }

    /// Returns the execution parameter schema.
    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "Command to run." },
                "cwd": { "type": "string", "description": "Working directory. Defaults to cwd." },
                "timeout": { "type": "number", "description": "Timeout in ms. Default 30000." }
            },
            "required": ["command"]
        })
    }

    /// Runs a command through the Agent-Forge service.
    async fn execute(&self, params: Value, cwd: &Path) -> Result<ToolResult> {
        let mut args = params.clone();
        if args.get("cwd").is_none() {
            args["cwd"] = Value::String(cwd.to_string_lossy().to_string());
        }
        match forge_call("execute", &args).await {
            Ok(text) => Ok(ToolResult {
                content: text,
                is_error: false,
            }),
            Err(e) => Ok(ToolResult {
                content: format!("execute failed: {e}"),
                is_error: true,
            }),
        }
    }
}

// ─── verify ─────────────────────────────────────────────────────────────────

pub struct VerifyTool;

/// Implements the agent tool contract for verification commands.
#[async_trait::async_trait]
impl AgentTool for VerifyTool {
    /// Returns the verification tool's stable registry name.
    fn name(&self) -> &str {
        "verify"
    }

    /// Describes evidence-producing verification.
    fn description(&self) -> &str {
        "Run a test command and return structured pass/fail results. \
         Parses test output to extract failures."
    }

    /// Returns the verification parameter schema.
    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "Test command to run." },
                "cwd": { "type": "string", "description": "Working directory. Defaults to cwd." },
                "timeout": { "type": "number", "description": "Timeout in ms. Default 60000." }
            },
            "required": ["command"]
        })
    }

    /// Runs a verification command through Agent-Forge.
    async fn execute(&self, params: Value, cwd: &Path) -> Result<ToolResult> {
        let mut args = params.clone();
        if args.get("cwd").is_none() {
            args["cwd"] = Value::String(cwd.to_string_lossy().to_string());
        }
        match forge_call("verify", &args).await {
            Ok(text) => Ok(ToolResult {
                content: text,
                is_error: false,
            }),
            Err(e) => Ok(ToolResult {
                content: format!("verify failed: {e}"),
                is_error: true,
            }),
        }
    }
}

// ─── ast_search ─────────────────────────────────────────────────────────────

pub struct AstSearchTool;

/// Implements the agent tool contract for AST-aware search.
#[async_trait::async_trait]
impl AgentTool for AstSearchTool {
    /// Returns the AST-search tool's stable registry name.
    fn name(&self) -> &str {
        "ast_search"
    }

    /// Describes syntax-tree-aware code search.
    fn description(&self) -> &str {
        "Structural code search using ast-grep patterns. Finds code by structure, not text. \
         Example: 'console.log($$$)' finds all console.log calls regardless of arguments."
    }

    /// Returns the AST-search parameter schema.
    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "ast-grep pattern to search for." },
                "path": { "type": "string", "description": "Directory to search. Defaults to cwd." },
                "language": { "type": "string", "description": "Language: rust, javascript, python, etc." }
            },
            "required": ["pattern"]
        })
    }

    /// Searches syntax trees within the supplied repository path.
    async fn execute(&self, params: Value, cwd: &Path) -> Result<ToolResult> {
        let mut args = params.clone();
        if args.get("path").is_none() {
            args["path"] = Value::String(cwd.to_string_lossy().to_string());
        }
        match forge_call("ast_search", &args).await {
            Ok(text) => Ok(ToolResult {
                content: text,
                is_error: false,
            }),
            Err(e) => Ok(ToolResult {
                content: format!("ast_search failed: {e}"),
                is_error: true,
            }),
        }
    }
}

// ─── prose_analyze ──────────────────────────────────────────────────────────

pub struct ProseAnalyzeTool;

/// Implements the agent tool contract for prose analysis.
#[async_trait::async_trait]
impl AgentTool for ProseAnalyzeTool {
    /// Returns the prose-analysis tool's stable registry name.
    fn name(&self) -> &str {
        "prose_analyze"
    }

    /// Describes prose-pattern analysis.
    fn description(&self) -> &str {
        "Analyze written text for AI-sounding patterns, readability, and voice consistency. \
         Required before finalizing any natural language content."
    }

    /// Returns the prose-analysis parameter schema.
    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "text": { "type": "string", "description": "Text to analyze." },
                "format": { "type": "string", "description": "One of: reddit, business, documentation, email, blog, general" }
            },
            "required": ["text", "format"]
        })
    }

    /// Analyzes supplied prose for learned writing patterns.
    async fn execute(&self, params: Value, _cwd: &Path) -> Result<ToolResult> {
        match forge_call("prose_analyze", &params).await {
            Ok(text) => Ok(ToolResult {
                content: text,
                is_error: false,
            }),
            Err(e) => Ok(ToolResult {
                content: format!("prose_analyze failed: {e}"),
                is_error: true,
            }),
        }
    }
}

// ─── prose_learn ────────────────────────────────────────────────────────────

pub struct ProseLearnTool;

/// Implements the agent tool contract for prose-pattern learning.
#[async_trait::async_trait]
impl AgentTool for ProseLearnTool {
    /// Returns the prose-learning tool's stable registry name.
    fn name(&self) -> &str {
        "prose_learn"
    }

    /// Describes prose-pattern learning.
    fn description(&self) -> &str {
        "Save approved text as a voice profile sample. Call when the operator approves written content \
         or submits their own text as reference."
    }

    /// Returns the prose-learning parameter schema.
    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "text": { "type": "string", "description": "Approved text to learn from." },
                "format": { "type": "string", "description": "One of: reddit, business, documentation, email, blog, general" }
            },
            "required": ["text", "format"]
        })
    }

    /// Records a prose pattern with Agent-Forge.
    async fn execute(&self, params: Value, _cwd: &Path) -> Result<ToolResult> {
        match forge_call("prose_learn", &params).await {
            Ok(text) => Ok(ToolResult {
                content: text,
                is_error: false,
            }),
            Err(e) => Ok(ToolResult {
                content: format!("prose_learn failed: {e}"),
                is_error: true,
            }),
        }
    }
}

// ─── session_diff ───────────────────────────────────────────────────────────

pub struct SessionDiffTool;

/// Implements the agent tool contract for session diff review.
#[async_trait::async_trait]
impl AgentTool for SessionDiffTool {
    /// Returns the session-diff tool's stable registry name.
    fn name(&self) -> &str {
        "session_diff"
    }

    /// Describes session-level change review.
    fn description(&self) -> &str {
        "Show all changes made in the current session. Use to audit work before declaring done."
    }

    /// Returns the session-diff parameter schema.
    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Repo path. Defaults to cwd." }
            }
        })
    }

    /// Audits repository changes accumulated during the current session.
    async fn execute(&self, params: Value, cwd: &Path) -> Result<ToolResult> {
        let mut args = params.clone();
        if args.get("path").is_none() {
            args["path"] = Value::String(cwd.to_string_lossy().to_string());
        }
        match forge_call("session_diff", &args).await {
            Ok(text) => Ok(ToolResult {
                content: text,
                is_error: false,
            }),
            Err(e) => Ok(ToolResult {
                content: format!("session_diff failed: {e}"),
                is_error: true,
            }),
        }
    }
}
