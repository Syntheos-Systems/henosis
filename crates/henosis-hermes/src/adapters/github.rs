//! GitHub REST API adapters.
//!
//! Covers issue management, pull request management, code search, repository
//! listing, and webhook registration. All tools share a common auth/prep
//! pattern (`gh_auth`, `gh_prep`, `gh_finish`).

use async_trait::async_trait;
use serde_json::{json, Value};
use tracing::warn;

use crate::adapters::common::{
    build_http, phylaxd_error_to_response, send_with_retry, truncate, HttpOutcome,
};
use crate::tool::{
    err, error_response, InvokeContext, InvokeRequest, InvokeResponse, RetryPolicy, Tool,
    ToolSchema,
};

/// Tool ID for the create-issue adapter.
const TOOL_ID: &str = "github.create_issue";
/// phylaxd provider tag for all GitHub tools.
const PROVIDER: &str = "github";
/// GitHub REST API base URL.
const GH_API: &str = "https://api.github.com";
/// User-Agent header value sent to GitHub.
const GH_UA: &str = "hermes-tool-gateway";

/// Create a new issue in a GitHub repository.
pub struct GitHubCreateIssueTool;

#[async_trait]
/// Implements the Hermes tool contract for GitHubCreateIssueTool.
impl Tool for GitHubCreateIssueTool {
    /// Returns the public schema for this tool.
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            tool_id: TOOL_ID.to_string(),
            name: "Create GitHub Issue".to_string(),
            description: "Create a new issue in a GitHub repository.".to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["owner", "repo", "title"],
                "properties": {
                    "owner": { "type": "string", "description": "Repository owner (user or org)" },
                    "repo": { "type": "string", "description": "Repository name" },
                    "title": { "type": "string", "description": "Issue title" },
                    "body": { "type": "string", "description": "Issue body (markdown)" },
                    "labels": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Labels to apply to the issue"
                    },
                    "assignees": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Logins to assign"
                    }
                }
            }),
            output_schema: json!({
                "type": "object",
                "properties": {
                    "issue_number": { "type": "integer" },
                    "html_url": { "type": "string" },
                    "node_id": { "type": "string" }
                }
            }),
            category: "development".to_string(),
            requires_auth: true,
        }
    }

    /// Returns the credential-provider identifier for this tool.
    fn provider(&self) -> &'static str {
        PROVIDER
    }

    /// Validates and executes one tool invocation.
    async fn invoke(&self, ctx: &InvokeContext, req: InvokeRequest) -> InvokeResponse {
        let tenant_id = match req.tenant_id.as_deref().filter(|s| !s.is_empty()) {
            Some(t) => t.to_string(),
            None => return error_response(TOOL_ID, "bad_request", "tenant_id is required", None),
        };

        let args = match parse_args(&req.args) {
            Ok(a) => a,
            Err(msg) => return error_response(TOOL_ID, "bad_request", msg, None),
        };

        let token = match ctx.phylaxd.fetch_token(&tenant_id, PROVIDER).await {
            Ok(t) => t,
            Err(e) => return phylaxd_error_to_response(TOOL_ID, &e),
        };

        let http = match build_http() {
            Ok(c) => c,
            Err(e) => return error_response(TOOL_ID, "internal_error", e.to_string(), None),
        };

        let url = format!(
            "https://api.github.com/repos/{}/{}/issues",
            args.owner, args.repo
        );

        let mut payload = json!({ "title": args.title });
        if let Some(b) = &args.body {
            payload["body"] = Value::String(b.clone());
        }
        if !args.labels.is_empty() {
            payload["labels"] =
                Value::Array(args.labels.iter().cloned().map(Value::String).collect());
        }
        if !args.assignees.is_empty() {
            payload["assignees"] =
                Value::Array(args.assignees.iter().cloned().map(Value::String).collect());
        }

        let request = http
            .post(&url)
            .bearer_auth(&token)
            .header("User-Agent", "hermes-tool-gateway")
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .json(&payload);

        let outcome = match send_with_retry(request, &self.retry_policy()).await {
            Ok(o) => o,
            Err(e) => {
                return InvokeResponse {
                    tool_id: TOOL_ID.into(),
                    success: false,
                    result: None,
                    error: Some(err(
                        "github_unreachable",
                        format!("github api request failed: {e}"),
                        None,
                    )),
                    duration_ms: 0,
                }
            }
        };

        let status = outcome.status;
        let body_text = outcome.body;

        if !status.is_success() {
            warn!(status = %status, body = %truncate(&body_text, 256), "github api error");
            return InvokeResponse {
                tool_id: TOOL_ID.into(),
                success: false,
                result: None,
                error: Some(json!({
                    "code": "github_api_error",
                    "message": format!("github returned HTTP {}", status.as_u16()),
                    "status": status.as_u16(),
                    "body": truncate(&body_text, 512),
                })),
                duration_ms: 0,
            };
        }

        let parsed: Value = serde_json::from_str(&body_text).unwrap_or(Value::Null);
        let number = parsed.get("number").and_then(|v| v.as_i64()).unwrap_or(0);
        let html_url = parsed
            .get("html_url")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let node_id = parsed
            .get("node_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        InvokeResponse {
            tool_id: TOOL_ID.into(),
            success: true,
            result: Some(json!({
                "issue_number": number,
                "html_url": html_url,
                "node_id": node_id,
            })),
            error: None,
            duration_ms: 0,
        }
    }
}

/// Parsed arguments for `github.create_issue`.
struct GitHubArgs {
    /// Repository owner login.
    owner: String,
    /// Repository name.
    repo: String,
    /// Issue title.
    title: String,
    /// Optional issue body (markdown).
    body: Option<String>,
    /// Labels to attach to the issue.
    labels: Vec<String>,
    /// GitHub logins to assign to the issue.
    assignees: Vec<String>,
}

/// Parse and validate `github.create_issue` arguments.
fn parse_args(args: &Value) -> Result<GitHubArgs, String> {
    let obj = args
        .as_object()
        .ok_or_else(|| "args must be a JSON object".to_string())?;
    let pull_required = |k: &str| -> Result<String, String> {
        obj.get(k)
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| format!("'{k}' is required and must be a non-empty string"))
    };
    let pull_optional = |k: &str| -> Option<String> {
        obj.get(k)
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    };
    let pull_array = |k: &str| -> Vec<String> {
        obj.get(k)
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .filter(|s| !s.is_empty())
                    .collect()
            })
            .unwrap_or_default()
    };
    Ok(GitHubArgs {
        owner: pull_required("owner")?,
        repo: pull_required("repo")?,
        title: pull_required("title")?,
        body: pull_optional("body"),
        labels: pull_array("labels"),
        assignees: pull_array("assignees"),
    })
}

// ===========================================================================
// Shared GitHub helpers
// ===========================================================================

/// Apply the standard GitHub auth + API headers to a request builder.
fn gh_auth(rb: reqwest::RequestBuilder, token: &str) -> reqwest::RequestBuilder {
    rb.bearer_auth(token)
        .header("User-Agent", GH_UA)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
}

/// Resolve tenant credentials and an HTTP client, or return a ready error.
async fn gh_prep(
    ctx: &InvokeContext,
    tenant_id: Option<&str>,
    tool_id: &str,
) -> Result<(String, reqwest::Client), InvokeResponse> {
    let tenant = tenant_id
        .filter(|s| !s.is_empty())
        .ok_or_else(|| error_response(tool_id, "bad_request", "tenant_id is required", None))?;
    let token = ctx
        .phylaxd
        .fetch_token(tenant, PROVIDER)
        .await
        .map_err(|e| phylaxd_error_to_response(tool_id, &e))?;
    let http =
        build_http().map_err(|e| error_response(tool_id, "internal_error", e.to_string(), None))?;
    Ok((token, http))
}

/// Turn a retry outcome into either the parsed JSON body or a ready error.
fn gh_finish(
    tool_id: &str,
    res: Result<HttpOutcome, reqwest::Error>,
) -> Result<Value, InvokeResponse> {
    match res {
        Err(e) => Err(error_response(
            tool_id,
            "github_unreachable",
            format!("github api request failed: {e}"),
            None,
        )),
        Ok(o) if o.status.is_success() => Ok(serde_json::from_str(&o.body).unwrap_or(Value::Null)),
        Ok(o) => {
            warn!(
                status = o.status.as_u16(),
                tool = tool_id,
                "github api error"
            );
            Err(InvokeResponse {
                tool_id: tool_id.to_string(),
                success: false,
                result: None,
                error: Some(json!({
                    "code": "github_api_error",
                    "message": format!("github returned HTTP {}", o.status.as_u16()),
                    "status": o.status.as_u16(),
                    "body": truncate(&o.body, 512),
                })),
                duration_ms: 0,
            })
        }
    }
}

/// Build a successful `InvokeResponse` for any GitHub tool.
fn gh_ok(tool_id: &str, result: Value) -> InvokeResponse {
    InvokeResponse {
        tool_id: tool_id.to_string(),
        success: true,
        result: Some(result),
        error: None,
        duration_ms: 0,
    }
}

/// Pull a required non-empty string field from an args map.
fn req_str(
    obj: &serde_json::Map<String, Value>,
    key: &str,
    tool_id: &str,
) -> Result<String, InvokeResponse> {
    obj.get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| error_response(tool_id, "bad_request", format!("'{key}' is required"), None))
}

/// Pull an optional trimmed non-empty string field.
fn opt_str(obj: &serde_json::Map<String, Value>, key: &str) -> Option<String> {
    obj.get(key)
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Require that `args` is a JSON object; return a structured error otherwise.
fn as_obj<'a>(
    args: &'a Value,
    tool_id: &str,
) -> Result<&'a serde_json::Map<String, Value>, InvokeResponse> {
    args.as_object()
        .ok_or_else(|| error_response(tool_id, "bad_request", "args must be a JSON object", None))
}

// ===========================================================================
// github.list_issues
// ===========================================================================

/// List issues in a GitHub repository with optional filters.
pub struct GitHubListIssuesTool;

#[async_trait]
/// Implements the Hermes tool contract for GitHubListIssuesTool.
impl Tool for GitHubListIssuesTool {
    /// Returns the public schema for this tool.
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            tool_id: "github.list_issues".to_string(),
            name: "List GitHub Issues".to_string(),
            description: "List issues in a repository, with optional filters.".to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["owner", "repo"],
                "properties": {
                    "owner": { "type": "string" },
                    "repo": { "type": "string" },
                    "state": { "type": "string", "enum": ["open", "closed", "all"] },
                    "labels": { "type": "string", "description": "Comma-separated label names" },
                    "assignee": { "type": "string" },
                    "since": { "type": "string", "description": "ISO 8601 timestamp" },
                    "per_page": { "type": "integer", "description": "Default 30, max 100" },
                    "page": { "type": "integer" }
                }
            }),
            output_schema: json!({ "type": "object", "properties": { "issues": { "type": "array" } } }),
            category: "development".to_string(),
            requires_auth: true,
        }
    }

    /// Returns the credential-provider identifier for this tool.
    fn provider(&self) -> &'static str {
        PROVIDER
    }

    /// Validates and executes one tool invocation.
    async fn invoke(&self, ctx: &InvokeContext, req: InvokeRequest) -> InvokeResponse {
        let tool_id = "github.list_issues";
        let obj = match as_obj(&req.args, tool_id) {
            Ok(o) => o,
            Err(r) => return r,
        };
        let owner = match req_str(obj, "owner", tool_id) {
            Ok(v) => v,
            Err(r) => return r,
        };
        let repo = match req_str(obj, "repo", tool_id) {
            Ok(v) => v,
            Err(r) => return r,
        };
        let (token, http) = match gh_prep(ctx, req.tenant_id.as_deref(), tool_id).await {
            Ok(v) => v,
            Err(r) => return r,
        };

        let mut params: Vec<(&str, String)> = Vec::new();
        if let Some(s) = opt_str(obj, "state") {
            params.push(("state", s));
        }
        if let Some(l) = opt_str(obj, "labels") {
            params.push(("labels", l));
        }
        if let Some(a) = opt_str(obj, "assignee") {
            params.push(("assignee", a));
        }
        if let Some(s) = opt_str(obj, "since") {
            params.push(("since", s));
        }
        if let Some(pp) = obj.get("per_page").and_then(|v| v.as_u64()) {
            params.push(("per_page", pp.clamp(1, 100).to_string()));
        }
        if let Some(p) = obj.get("page").and_then(|v| v.as_u64()) {
            params.push(("page", p.to_string()));
        }

        let url = format!("{GH_API}/repos/{owner}/{repo}/issues");
        let request = gh_auth(http.get(&url), &token).query(&params);
        match gh_finish(
            tool_id,
            send_with_retry(request, &self.retry_policy()).await,
        ) {
            Ok(v) => gh_ok(tool_id, json!({ "issues": v })),
            Err(r) => r,
        }
    }
}

// ===========================================================================
// github.get_issue (issue + comments)
// ===========================================================================

/// Fetch a single GitHub issue and its comments.
pub struct GitHubGetIssueTool;

#[async_trait]
/// Implements the Hermes tool contract for GitHubGetIssueTool.
impl Tool for GitHubGetIssueTool {
    /// Returns the public schema for this tool.
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            tool_id: "github.get_issue".to_string(),
            name: "Get GitHub Issue".to_string(),
            description: "Fetch a single issue along with its comments.".to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["owner", "repo", "issue_number"],
                "properties": {
                    "owner": { "type": "string" },
                    "repo": { "type": "string" },
                    "issue_number": { "type": "integer" }
                }
            }),
            output_schema: json!({
                "type": "object",
                "properties": { "issue": { "type": "object" }, "comments": { "type": "array" } }
            }),
            category: "development".to_string(),
            requires_auth: true,
        }
    }

    /// Returns the credential-provider identifier for this tool.
    fn provider(&self) -> &'static str {
        PROVIDER
    }

    /// Validates and executes one tool invocation.
    async fn invoke(&self, ctx: &InvokeContext, req: InvokeRequest) -> InvokeResponse {
        let tool_id = "github.get_issue";
        let obj = match as_obj(&req.args, tool_id) {
            Ok(o) => o,
            Err(r) => return r,
        };
        let owner = match req_str(obj, "owner", tool_id) {
            Ok(v) => v,
            Err(r) => return r,
        };
        let repo = match req_str(obj, "repo", tool_id) {
            Ok(v) => v,
            Err(r) => return r,
        };
        let number = match obj.get("issue_number").and_then(|v| v.as_u64()) {
            Some(n) => n,
            None => {
                return error_response(tool_id, "bad_request", "'issue_number' is required", None)
            }
        };
        let (token, http) = match gh_prep(ctx, req.tenant_id.as_deref(), tool_id).await {
            Ok(v) => v,
            Err(r) => return r,
        };

        let issue_url = format!("{GH_API}/repos/{owner}/{repo}/issues/{number}");
        let issue = match gh_finish(
            tool_id,
            send_with_retry(gh_auth(http.get(&issue_url), &token), &self.retry_policy()).await,
        ) {
            Ok(v) => v,
            Err(r) => return r,
        };

        let comments_url = format!("{GH_API}/repos/{owner}/{repo}/issues/{number}/comments");
        let comments = match gh_finish(
            tool_id,
            send_with_retry(
                gh_auth(http.get(&comments_url), &token),
                &self.retry_policy(),
            )
            .await,
        ) {
            Ok(v) => v,
            Err(r) => return r,
        };

        gh_ok(tool_id, json!({ "issue": issue, "comments": comments }))
    }
}

// ===========================================================================
// github.create_pr
// ===========================================================================

/// Open a pull request in a GitHub repository.
pub struct GitHubCreatePrTool;

#[async_trait]
/// Implements the Hermes tool contract for GitHubCreatePrTool.
impl Tool for GitHubCreatePrTool {
    /// Returns the public schema for this tool.
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            tool_id: "github.create_pr".to_string(),
            name: "Create Pull Request".to_string(),
            description: "Open a pull request in a repository.".to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["owner", "repo", "title", "head", "base"],
                "properties": {
                    "owner": { "type": "string" },
                    "repo": { "type": "string" },
                    "title": { "type": "string" },
                    "head": { "type": "string", "description": "Source branch" },
                    "base": { "type": "string", "description": "Target branch" },
                    "body": { "type": "string" },
                    "draft": { "type": "boolean" }
                }
            }),
            output_schema: json!({
                "type": "object",
                "properties": { "number": { "type": "integer" }, "html_url": { "type": "string" }, "node_id": { "type": "string" } }
            }),
            category: "development".to_string(),
            requires_auth: true,
        }
    }

    /// Returns the credential-provider identifier for this tool.
    fn provider(&self) -> &'static str {
        PROVIDER
    }

    /// Returns the retry policy for this tool operation.
    fn retry_policy(&self) -> RetryPolicy {
        RetryPolicy::non_idempotent()
    }

    /// Validates and executes one tool invocation.
    async fn invoke(&self, ctx: &InvokeContext, req: InvokeRequest) -> InvokeResponse {
        let tool_id = "github.create_pr";
        let obj = match as_obj(&req.args, tool_id) {
            Ok(o) => o,
            Err(r) => return r,
        };
        let owner = match req_str(obj, "owner", tool_id) {
            Ok(v) => v,
            Err(r) => return r,
        };
        let repo = match req_str(obj, "repo", tool_id) {
            Ok(v) => v,
            Err(r) => return r,
        };
        let title = match req_str(obj, "title", tool_id) {
            Ok(v) => v,
            Err(r) => return r,
        };
        let head = match req_str(obj, "head", tool_id) {
            Ok(v) => v,
            Err(r) => return r,
        };
        let base = match req_str(obj, "base", tool_id) {
            Ok(v) => v,
            Err(r) => return r,
        };
        let (token, http) = match gh_prep(ctx, req.tenant_id.as_deref(), tool_id).await {
            Ok(v) => v,
            Err(r) => return r,
        };

        let mut payload = json!({ "title": title, "head": head, "base": base });
        if let Some(b) = opt_str(obj, "body") {
            payload["body"] = Value::String(b);
        }
        if let Some(d) = obj.get("draft").and_then(|v| v.as_bool()) {
            payload["draft"] = Value::Bool(d);
        }

        let url = format!("{GH_API}/repos/{owner}/{repo}/pulls");
        let request = gh_auth(http.post(&url), &token).json(&payload);
        match gh_finish(
            tool_id,
            send_with_retry(request, &self.retry_policy()).await,
        ) {
            Ok(v) => gh_ok(
                tool_id,
                json!({
                    "number": v.get("number").and_then(|n| n.as_i64()).unwrap_or(0),
                    "html_url": v.get("html_url").and_then(|n| n.as_str()).unwrap_or(""),
                    "node_id": v.get("node_id").and_then(|n| n.as_str()).unwrap_or(""),
                }),
            ),
            Err(r) => r,
        }
    }
}

// ===========================================================================
// github.list_prs
// ===========================================================================

/// List pull requests in a GitHub repository.
pub struct GitHubListPrsTool;

#[async_trait]
/// Implements the Hermes tool contract for GitHubListPrsTool.
impl Tool for GitHubListPrsTool {
    /// Returns the public schema for this tool.
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            tool_id: "github.list_prs".to_string(),
            name: "List Pull Requests".to_string(),
            description: "List pull requests in a repository, with optional filters.".to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["owner", "repo"],
                "properties": {
                    "owner": { "type": "string" },
                    "repo": { "type": "string" },
                    "state": { "type": "string", "enum": ["open", "closed", "all"] },
                    "head": { "type": "string" },
                    "base": { "type": "string" },
                    "sort": { "type": "string" },
                    "direction": { "type": "string", "enum": ["asc", "desc"] },
                    "per_page": { "type": "integer" },
                    "page": { "type": "integer" }
                }
            }),
            output_schema: json!({ "type": "object", "properties": { "pull_requests": { "type": "array" } } }),
            category: "development".to_string(),
            requires_auth: true,
        }
    }

    /// Returns the credential-provider identifier for this tool.
    fn provider(&self) -> &'static str {
        PROVIDER
    }

    /// Validates and executes one tool invocation.
    async fn invoke(&self, ctx: &InvokeContext, req: InvokeRequest) -> InvokeResponse {
        let tool_id = "github.list_prs";
        let obj = match as_obj(&req.args, tool_id) {
            Ok(o) => o,
            Err(r) => return r,
        };
        let owner = match req_str(obj, "owner", tool_id) {
            Ok(v) => v,
            Err(r) => return r,
        };
        let repo = match req_str(obj, "repo", tool_id) {
            Ok(v) => v,
            Err(r) => return r,
        };
        let (token, http) = match gh_prep(ctx, req.tenant_id.as_deref(), tool_id).await {
            Ok(v) => v,
            Err(r) => return r,
        };

        let mut params: Vec<(&str, String)> = Vec::new();
        for key in ["state", "head", "base", "sort", "direction"] {
            if let Some(v) = opt_str(obj, key) {
                params.push((key, v));
            }
        }
        if let Some(pp) = obj.get("per_page").and_then(|v| v.as_u64()) {
            params.push(("per_page", pp.clamp(1, 100).to_string()));
        }
        if let Some(p) = obj.get("page").and_then(|v| v.as_u64()) {
            params.push(("page", p.to_string()));
        }

        let url = format!("{GH_API}/repos/{owner}/{repo}/pulls");
        let request = gh_auth(http.get(&url), &token).query(&params);
        match gh_finish(
            tool_id,
            send_with_retry(request, &self.retry_policy()).await,
        ) {
            Ok(v) => gh_ok(tool_id, json!({ "pull_requests": v })),
            Err(r) => r,
        }
    }
}

// ===========================================================================
// github.merge_pr
// ===========================================================================

/// Merge a pull request in a GitHub repository.
pub struct GitHubMergePrTool;

#[async_trait]
/// Implements the Hermes tool contract for GitHubMergePrTool.
impl Tool for GitHubMergePrTool {
    /// Returns the public schema for this tool.
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            tool_id: "github.merge_pr".to_string(),
            name: "Merge Pull Request".to_string(),
            description: "Merge a pull request.".to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["owner", "repo", "pull_number"],
                "properties": {
                    "owner": { "type": "string" },
                    "repo": { "type": "string" },
                    "pull_number": { "type": "integer" },
                    "merge_method": { "type": "string", "enum": ["merge", "squash", "rebase"] },
                    "commit_title": { "type": "string" },
                    "commit_message": { "type": "string" }
                }
            }),
            output_schema: json!({
                "type": "object",
                "properties": { "merged": { "type": "boolean" }, "sha": { "type": "string" }, "message": { "type": "string" } }
            }),
            category: "development".to_string(),
            requires_auth: true,
        }
    }

    /// Returns the credential-provider identifier for this tool.
    fn provider(&self) -> &'static str {
        PROVIDER
    }

    /// Returns the retry policy for this tool operation.
    fn retry_policy(&self) -> RetryPolicy {
        RetryPolicy::non_idempotent()
    }

    /// Validates and executes one tool invocation.
    async fn invoke(&self, ctx: &InvokeContext, req: InvokeRequest) -> InvokeResponse {
        let tool_id = "github.merge_pr";
        let obj = match as_obj(&req.args, tool_id) {
            Ok(o) => o,
            Err(r) => return r,
        };
        let owner = match req_str(obj, "owner", tool_id) {
            Ok(v) => v,
            Err(r) => return r,
        };
        let repo = match req_str(obj, "repo", tool_id) {
            Ok(v) => v,
            Err(r) => return r,
        };
        let number = match obj.get("pull_number").and_then(|v| v.as_u64()) {
            Some(n) => n,
            None => {
                return error_response(tool_id, "bad_request", "'pull_number' is required", None)
            }
        };
        let (token, http) = match gh_prep(ctx, req.tenant_id.as_deref(), tool_id).await {
            Ok(v) => v,
            Err(r) => return r,
        };

        let mut payload = json!({});
        if let Some(m) = opt_str(obj, "merge_method") {
            payload["merge_method"] = Value::String(m);
        }
        if let Some(t) = opt_str(obj, "commit_title") {
            payload["commit_title"] = Value::String(t);
        }
        if let Some(m) = opt_str(obj, "commit_message") {
            payload["commit_message"] = Value::String(m);
        }

        let url = format!("{GH_API}/repos/{owner}/{repo}/pulls/{number}/merge");
        let request = gh_auth(http.put(&url), &token).json(&payload);
        match gh_finish(
            tool_id,
            send_with_retry(request, &self.retry_policy()).await,
        ) {
            Ok(v) => gh_ok(
                tool_id,
                json!({
                    "merged": v.get("merged").and_then(|n| n.as_bool()).unwrap_or(false),
                    "sha": v.get("sha").and_then(|n| n.as_str()).unwrap_or(""),
                    "message": v.get("message").and_then(|n| n.as_str()).unwrap_or(""),
                }),
            ),
            Err(r) => r,
        }
    }
}

// ===========================================================================
// github.search_code
// ===========================================================================

/// Search code across GitHub using the code search API.
pub struct GitHubSearchCodeTool;

#[async_trait]
/// Implements the Hermes tool contract for GitHubSearchCodeTool.
impl Tool for GitHubSearchCodeTool {
    /// Returns the public schema for this tool.
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            tool_id: "github.search_code".to_string(),
            name: "Search Code".to_string(),
            description: "Search code across GitHub using the code search API.".to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["query"],
                "properties": {
                    "query": { "type": "string", "description": "GitHub code search query" },
                    "per_page": { "type": "integer" },
                    "page": { "type": "integer" }
                }
            }),
            output_schema: json!({
                "type": "object",
                "properties": { "total_count": { "type": "integer" }, "items": { "type": "array" } }
            }),
            category: "development".to_string(),
            requires_auth: true,
        }
    }

    /// Returns the credential-provider identifier for this tool.
    fn provider(&self) -> &'static str {
        PROVIDER
    }

    /// Validates and executes one tool invocation.
    async fn invoke(&self, ctx: &InvokeContext, req: InvokeRequest) -> InvokeResponse {
        let tool_id = "github.search_code";
        let obj = match as_obj(&req.args, tool_id) {
            Ok(o) => o,
            Err(r) => return r,
        };
        let query = match req_str(obj, "query", tool_id) {
            Ok(v) => v,
            Err(r) => return r,
        };
        let (token, http) = match gh_prep(ctx, req.tenant_id.as_deref(), tool_id).await {
            Ok(v) => v,
            Err(r) => return r,
        };

        let mut params: Vec<(&str, String)> = vec![("q", query)];
        if let Some(pp) = obj.get("per_page").and_then(|v| v.as_u64()) {
            params.push(("per_page", pp.clamp(1, 100).to_string()));
        }
        if let Some(p) = obj.get("page").and_then(|v| v.as_u64()) {
            params.push(("page", p.to_string()));
        }

        // Code search needs the text-match media type to return highlights.
        let request = http
            .get(format!("{GH_API}/search/code"))
            .bearer_auth(&token)
            .header("User-Agent", GH_UA)
            .header("Accept", "application/vnd.github.text-match+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .query(&params);
        match gh_finish(
            tool_id,
            send_with_retry(request, &self.retry_policy()).await,
        ) {
            Ok(v) => gh_ok(
                tool_id,
                json!({
                    "total_count": v.get("total_count").and_then(|n| n.as_i64()).unwrap_or(0),
                    "items": v.get("items").cloned().unwrap_or_else(|| Value::Array(vec![])),
                }),
            ),
            Err(r) => r,
        }
    }
}

// ===========================================================================
// github.list_repos (org or user)
// ===========================================================================

/// List repositories for an organization or user.
pub struct GitHubListReposTool;

#[async_trait]
/// Implements the Hermes tool contract for GitHubListReposTool.
impl Tool for GitHubListReposTool {
    /// Returns the public schema for this tool.
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            tool_id: "github.list_repos".to_string(),
            name: "List Repositories".to_string(),
            description: "List repositories for an organization or user (exactly one required)."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "org": { "type": "string", "description": "Organization login (mutually exclusive with user)" },
                    "user": { "type": "string", "description": "User login (mutually exclusive with org)" },
                    "type": { "type": "string", "enum": ["all", "public", "private", "forks", "sources", "member"] },
                    "sort": { "type": "string" },
                    "per_page": { "type": "integer" },
                    "page": { "type": "integer" }
                }
            }),
            output_schema: json!({ "type": "object", "properties": { "repos": { "type": "array" } } }),
            category: "development".to_string(),
            requires_auth: true,
        }
    }

    /// Returns the credential-provider identifier for this tool.
    fn provider(&self) -> &'static str {
        PROVIDER
    }

    /// Validates and executes one tool invocation.
    async fn invoke(&self, ctx: &InvokeContext, req: InvokeRequest) -> InvokeResponse {
        let tool_id = "github.list_repos";
        let obj = match as_obj(&req.args, tool_id) {
            Ok(o) => o,
            Err(r) => return r,
        };
        let org = opt_str(obj, "org");
        let user = opt_str(obj, "user");
        let base = match (org, user) {
            (Some(o), None) => format!("{GH_API}/orgs/{o}/repos"),
            (None, Some(u)) => format!("{GH_API}/users/{u}/repos"),
            (Some(_), Some(_)) => {
                return error_response(
                    tool_id,
                    "bad_request",
                    "provide either 'org' or 'user', not both",
                    None,
                )
            }
            (None, None) => {
                return error_response(
                    tool_id,
                    "bad_request",
                    "one of 'org' or 'user' is required",
                    None,
                )
            }
        };
        let (token, http) = match gh_prep(ctx, req.tenant_id.as_deref(), tool_id).await {
            Ok(v) => v,
            Err(r) => return r,
        };

        let mut params: Vec<(&str, String)> = Vec::new();
        if let Some(t) = opt_str(obj, "type") {
            params.push(("type", t));
        }
        if let Some(s) = opt_str(obj, "sort") {
            params.push(("sort", s));
        }
        if let Some(pp) = obj.get("per_page").and_then(|v| v.as_u64()) {
            params.push(("per_page", pp.clamp(1, 100).to_string()));
        }
        if let Some(p) = obj.get("page").and_then(|v| v.as_u64()) {
            params.push(("page", p.to_string()));
        }

        let request = gh_auth(http.get(&base), &token).query(&params);
        match gh_finish(
            tool_id,
            send_with_retry(request, &self.retry_policy()).await,
        ) {
            Ok(v) => gh_ok(tool_id, json!({ "repos": v })),
            Err(r) => r,
        }
    }
}

// ===========================================================================
// github.create_webhook
// ===========================================================================

/// Create a webhook on a GitHub repository.
pub struct GitHubCreateWebhookTool;

#[async_trait]
/// Implements the Hermes tool contract for GitHubCreateWebhookTool.
impl Tool for GitHubCreateWebhookTool {
    /// Returns the public schema for this tool.
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            tool_id: "github.create_webhook".to_string(),
            name: "Create Repository Webhook".to_string(),
            description: "Create a webhook on a repository.".to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["owner", "repo"],
                "properties": {
                    "owner": { "type": "string" },
                    "repo": { "type": "string" },
                    "url": { "type": "string", "description": "Payload delivery URL; defaults to this Hermes's /webhooks/github endpoint when omitted" },
                    "events": { "type": "array", "items": { "type": "string" }, "description": "Default [\"push\"]" },
                    "secret": { "type": "string" },
                    "content_type": { "type": "string", "enum": ["json", "form"] },
                    "active": { "type": "boolean" }
                }
            }),
            output_schema: json!({
                "type": "object",
                "properties": { "id": { "type": "integer" }, "url": { "type": "string" }, "active": { "type": "boolean" } }
            }),
            category: "development".to_string(),
            requires_auth: true,
        }
    }

    /// Returns the credential-provider identifier for this tool.
    fn provider(&self) -> &'static str {
        PROVIDER
    }

    /// Returns the retry policy for this tool operation.
    fn retry_policy(&self) -> RetryPolicy {
        RetryPolicy::non_idempotent()
    }

    /// Validates and executes one tool invocation.
    async fn invoke(&self, ctx: &InvokeContext, req: InvokeRequest) -> InvokeResponse {
        let tool_id = "github.create_webhook";
        let obj = match as_obj(&req.args, tool_id) {
            Ok(o) => o,
            Err(r) => return r,
        };
        let owner = match req_str(obj, "owner", tool_id) {
            Ok(v) => v,
            Err(r) => return r,
        };
        let repo = match req_str(obj, "repo", tool_id) {
            Ok(v) => v,
            Err(r) => return r,
        };
        // The delivery URL defaults to this Hermes's own webhook endpoint when
        // the caller omits it (registration convenience).
        let hook_url = match opt_str(obj, "url").or_else(|| {
            ctx.hermes_public_url
                .as_ref()
                .map(|base| format!("{}/webhooks/github", base.trim_end_matches('/')))
        }) {
            Some(u) => u,
            None => {
                return error_response(
                    tool_id,
                    "bad_request",
                    "no 'url' provided and HERMES_PUBLIC_URL is unset, so the webhook delivery URL cannot be determined",
                    None,
                )
            }
        };
        let (token, http) = match gh_prep(ctx, req.tenant_id.as_deref(), tool_id).await {
            Ok(v) => v,
            Err(r) => return r,
        };

        let events = obj
            .get("events")
            .and_then(|v| v.as_array())
            .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>())
            .filter(|v| !v.is_empty())
            .map(|v| {
                Value::Array(
                    v.into_iter()
                        .map(|s| Value::String(s.to_string()))
                        .collect(),
                )
            })
            .unwrap_or_else(|| json!(["push"]));
        let content_type = opt_str(obj, "content_type").unwrap_or_else(|| "json".to_string());
        let active = obj.get("active").and_then(|v| v.as_bool()).unwrap_or(true);

        let mut config = json!({ "url": hook_url, "content_type": content_type });
        if let Some(secret) = opt_str(obj, "secret") {
            config["secret"] = Value::String(secret);
        }
        let payload =
            json!({ "name": "web", "active": active, "events": events, "config": config });

        let url = format!("{GH_API}/repos/{owner}/{repo}/hooks");
        let request = gh_auth(http.post(&url), &token).json(&payload);
        match gh_finish(
            tool_id,
            send_with_retry(request, &self.retry_policy()).await,
        ) {
            Ok(v) => gh_ok(
                tool_id,
                json!({
                    "id": v.get("id").and_then(|n| n.as_i64()).unwrap_or(0),
                    "url": v.get("url").and_then(|n| n.as_str()).unwrap_or(""),
                    "active": v.get("active").and_then(|n| n.as_bool()).unwrap_or(false),
                }),
            ),
            Err(r) => r,
        }
    }
}

#[cfg(test)]
/// Contains focused unit tests for this module.
mod tests {
    use super::*;

    #[test]
    /// Verifies parse minimal.
    fn parse_minimal() {
        let v = json!({"owner":"a","repo":"b","title":"t"});
        let a = parse_args(&v).unwrap();
        assert_eq!(a.owner, "a");
        assert_eq!(a.repo, "b");
        assert_eq!(a.title, "t");
        assert!(a.labels.is_empty());
    }

    #[test]
    /// Verifies parse with labels.
    fn parse_with_labels() {
        let v = json!({"owner":"a","repo":"b","title":"t","labels":["bug","p1"]});
        let a = parse_args(&v).unwrap();
        assert_eq!(a.labels, vec!["bug".to_string(), "p1".to_string()]);
    }

    #[test]
    /// Verifies parse rejects missing title.
    fn parse_rejects_missing_title() {
        let v = json!({"owner":"a","repo":"b"});
        assert!(parse_args(&v).is_err());
    }

    #[test]
    /// Verifies parse rejects empty owner.
    fn parse_rejects_empty_owner() {
        let v = json!({"owner":"","repo":"b","title":"t"});
        assert!(parse_args(&v).is_err());
    }

    #[test]
    /// Verifies req str pulls and rejects.
    fn req_str_pulls_and_rejects() {
        let obj = json!({"owner": "octocat", "blank": "  "});
        let m = obj.as_object().unwrap();
        assert_eq!(req_str(m, "owner", "t").unwrap(), "octocat");
        assert!(req_str(m, "missing", "t").is_err());
        assert!(req_str(m, "blank", "t").is_err());
    }

    #[test]
    /// Verifies opt str trims and filters.
    fn opt_str_trims_and_filters() {
        let obj = json!({"a": "x", "b": "  ", "c": "  y  "});
        let m = obj.as_object().unwrap();
        assert_eq!(opt_str(m, "a").as_deref(), Some("x"));
        assert!(opt_str(m, "b").is_none());
        assert_eq!(opt_str(m, "c").as_deref(), Some("y"));
        assert!(opt_str(m, "missing").is_none());
    }

    #[test]
    /// Verifies non idempotent tools disable retry.
    fn non_idempotent_tools_disable_retry() {
        assert_eq!(GitHubCreatePrTool.retry_policy().max_retries, 0);
        assert_eq!(GitHubMergePrTool.retry_policy().max_retries, 0);
        assert_eq!(GitHubCreateWebhookTool.retry_policy().max_retries, 0);
        // Read-only tools keep the default retrying policy.
        assert_eq!(GitHubListIssuesTool.retry_policy().max_retries, 3);
    }

    #[test]
    /// Verifies all github tools report provider.
    fn all_github_tools_report_provider() {
        assert_eq!(GitHubListIssuesTool.provider(), "github");
        assert_eq!(GitHubGetIssueTool.provider(), "github");
        assert_eq!(GitHubSearchCodeTool.provider(), "github");
    }

    #[test]
    /// Verifies gh finish maps outcomes.
    fn gh_finish_maps_outcomes() {
        // Success body parses to JSON.
        let ok = gh_finish(
            "t",
            Ok(HttpOutcome {
                status: reqwest::StatusCode::OK,
                body: "{\"a\":1}".into(),
            }),
        );
        assert_eq!(ok.unwrap(), json!({"a": 1}));

        // 5xx becomes a structured api error carrying the status.
        let err = gh_finish(
            "t",
            Ok(HttpOutcome {
                status: reqwest::StatusCode::INTERNAL_SERVER_ERROR,
                body: "boom".into(),
            }),
        );
        let resp = err.unwrap_err();
        assert!(!resp.success);
        assert_eq!(
            resp.error.unwrap().get("status").and_then(|v| v.as_u64()),
            Some(500)
        );
    }
}
