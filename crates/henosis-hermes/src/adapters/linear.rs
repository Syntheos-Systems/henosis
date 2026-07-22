//! Linear adapters: create_issue, list_issues, update_issue, search, create_webhook.
//!
//! All tools hit Linear's GraphQL API at `https://api.linear.app/graphql`.
//! Authentication is via an API key fetched from phylaxd under the `linear` provider.

use async_trait::async_trait;
use serde_json::{json, Value};
use tracing::warn;

use crate::adapters::common::{build_http, phylaxd_error_to_response, send_with_retry, truncate};
use crate::tool::{
    err, error_response, InvokeContext, InvokeRequest, InvokeResponse, Tool, ToolSchema,
};

/// Linear GraphQL API endpoint.
const GQL_URL: &str = "https://api.linear.app/graphql";
/// phylaxd provider tag for all Linear tools.
const PROVIDER: &str = "linear";

/// Create a Linear issue in the given team.
pub struct LinearCreateIssueTool;

#[async_trait]
/// Implements the Hermes tool contract for LinearCreateIssueTool.
impl Tool for LinearCreateIssueTool {
    /// Returns the public schema for this tool.
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            tool_id: "linear.create_issue".to_string(),
            name: "Create Linear Issue".to_string(),
            description: "Create a Linear issue in the given team.".to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["team_id", "title"],
                "properties": {
                    "team_id": { "type": "string", "description": "Linear team ID" },
                    "title": { "type": "string" },
                    "description": { "type": "string", "description": "Markdown body" },
                    "priority": { "type": "integer", "description": "0=none 1=urgent 2=high 3=medium 4=low" },
                    "assignee_id": { "type": "string" },
                    "label_ids": { "type": "array", "items": { "type": "string" } }
                }
            }),
            output_schema: json!({
                "type": "object",
                "properties": {
                    "id": { "type": "string" },
                    "identifier": { "type": "string" },
                    "url": { "type": "string" }
                }
            }),
            category: "project_management".to_string(),
            requires_auth: true,
        }
    }

    /// Returns the credential-provider identifier for this tool.
    fn provider(&self) -> &'static str {
        PROVIDER
    }

    /// Creating an issue is not idempotent; disable retries.
    fn retry_policy(&self) -> crate::tool::RetryPolicy {
        crate::tool::RetryPolicy::non_idempotent()
    }

    /// Validates and executes one tool invocation.
    async fn invoke(&self, ctx: &InvokeContext, req: InvokeRequest) -> InvokeResponse {
        let (tenant_id, obj) = match prep_request("linear.create_issue", &req) {
            Ok(r) => r,
            Err(r) => return r,
        };
        let team_id = match req_str(obj, "team_id", "linear.create_issue") {
            Ok(v) => v,
            Err(r) => return r,
        };
        let title = match req_str(obj, "title", "linear.create_issue") {
            Ok(v) => v,
            Err(r) => return r,
        };

        let mut vars = json!({ "teamId": team_id, "title": title });
        if let Some(d) = obj.get("description").and_then(|v| v.as_str()) {
            vars["description"] = json!(d);
        }
        if let Some(p) = obj.get("priority").and_then(|v| v.as_i64()) {
            vars["priority"] = json!(p);
        }
        if let Some(a) = opt_str(obj, "assignee_id") {
            vars["assigneeId"] = json!(a);
        }
        if let Some(labels) = obj.get("label_ids").and_then(|v| v.as_array()) {
            vars["labelIds"] = Value::Array(labels.clone());
        }

        let query = r#"
mutation CreateIssue($teamId:String! $title:String! $description:String $priority:Int $assigneeId:String $labelIds:[String!]) {
  issueCreate(input:{teamId:$teamId title:$title description:$description priority:$priority assigneeId:$assigneeId labelIds:$labelIds}) {
    success
    issue { id identifier url title }
  }
}"#;

        let token = match ctx.phylaxd.fetch_token(&tenant_id, PROVIDER).await {
            Ok(t) => t,
            Err(e) => return phylaxd_error_to_response("linear.create_issue", &e),
        };
        let outcome = match linear_gql(&token, query, &vars, &self.retry_policy()).await {
            Ok(o) => o,
            Err(r) => return r,
        };
        let issue = outcome
            .pointer("/data/issueCreate/issue")
            .cloned()
            .unwrap_or(Value::Null);
        InvokeResponse {
            tool_id: "linear.create_issue".into(),
            success: true,
            result: Some(json!({
                "id": issue.get("id").and_then(|v| v.as_str()).unwrap_or(""),
                "identifier": issue.get("identifier").and_then(|v| v.as_str()).unwrap_or(""),
                "url": issue.get("url").and_then(|v| v.as_str()).unwrap_or(""),
            })),
            error: None,
            duration_ms: 0,
        }
    }
}

// ---------------------------------------------------------------------------
// linear.list_issues -- list issues optionally filtered by team/state/assignee.
// ---------------------------------------------------------------------------

/// List Linear issues with optional filters.
pub struct LinearListIssuesTool;

#[async_trait]
/// Implements the Hermes tool contract for LinearListIssuesTool.
impl Tool for LinearListIssuesTool {
    /// Returns the public schema for this tool.
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            tool_id: "linear.list_issues".to_string(),
            name: "List Linear Issues".to_string(),
            description: "List Linear issues with optional filters.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "team_id": { "type": "string" },
                    "assignee_id": { "type": "string" },
                    "state": { "type": "string", "description": "State name filter (e.g. 'In Progress')" },
                    "first": { "type": "integer", "description": "Max issues to return (default 25, max 100)" }
                }
            }),
            output_schema: json!({
                "type": "object",
                "properties": {
                    "issues": { "type": "array" },
                    "total": { "type": "integer" }
                }
            }),
            category: "project_management".to_string(),
            requires_auth: true,
        }
    }

    /// Returns the credential-provider identifier for this tool.
    fn provider(&self) -> &'static str {
        PROVIDER
    }

    /// Validates and executes one tool invocation.
    async fn invoke(&self, ctx: &InvokeContext, req: InvokeRequest) -> InvokeResponse {
        let (tenant_id, obj) = match prep_request("linear.list_issues", &req) {
            Ok(r) => r,
            Err(r) => return r,
        };

        let first = obj
            .get("first")
            .and_then(|v| v.as_u64())
            .map(|n| n.clamp(1, 100))
            .unwrap_or(25);

        // Build a filter object for the GraphQL query.
        let mut filter = serde_json::Map::new();
        if let Some(team_id) = opt_str(obj, "team_id") {
            filter.insert("team".into(), json!({ "id": { "eq": team_id } }));
        }
        if let Some(assignee_id) = opt_str(obj, "assignee_id") {
            filter.insert("assignee".into(), json!({ "id": { "eq": assignee_id } }));
        }
        if let Some(state) = opt_str(obj, "state") {
            filter.insert("state".into(), json!({ "name": { "eq": state } }));
        }

        let vars = json!({ "filter": filter, "first": first });
        let query = r#"
query ListIssues($filter:IssueFilter $first:Int) {
  issues(filter:$filter first:$first) {
    totalCount
    nodes { id identifier title priority url state { name } assignee { name } }
  }
}"#;

        let token = match ctx.phylaxd.fetch_token(&tenant_id, PROVIDER).await {
            Ok(t) => t,
            Err(e) => return phylaxd_error_to_response("linear.list_issues", &e),
        };
        let outcome = match linear_gql(&token, query, &vars, &self.retry_policy()).await {
            Ok(o) => o,
            Err(r) => return r,
        };
        let issues_node = outcome
            .pointer("/data/issues")
            .cloned()
            .unwrap_or(Value::Null);
        let total = issues_node
            .get("totalCount")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let nodes = issues_node
            .get("nodes")
            .cloned()
            .unwrap_or_else(|| Value::Array(vec![]));
        let summarised: Vec<Value> = nodes
            .as_array()
            .map(|arr| arr.iter().map(issue_summary).collect())
            .unwrap_or_default();

        InvokeResponse {
            tool_id: "linear.list_issues".into(),
            success: true,
            result: Some(json!({ "issues": summarised, "total": total })),
            error: None,
            duration_ms: 0,
        }
    }
}

// ---------------------------------------------------------------------------
// linear.update_issue -- patch an existing issue.
// ---------------------------------------------------------------------------

/// Update a Linear issue; only supplied fields are changed.
pub struct LinearUpdateIssueTool;

#[async_trait]
/// Implements the Hermes tool contract for LinearUpdateIssueTool.
impl Tool for LinearUpdateIssueTool {
    /// Returns the public schema for this tool.
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            tool_id: "linear.update_issue".to_string(),
            name: "Update Linear Issue".to_string(),
            description: "Update a Linear issue; only supplied fields are changed.".to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["issue_id"],
                "properties": {
                    "issue_id": { "type": "string" },
                    "title": { "type": "string" },
                    "description": { "type": "string" },
                    "priority": { "type": "integer" },
                    "state_id": { "type": "string" },
                    "assignee_id": { "type": "string" }
                }
            }),
            output_schema: json!({ "type": "object" }),
            category: "project_management".to_string(),
            requires_auth: true,
        }
    }

    /// Returns the credential-provider identifier for this tool.
    fn provider(&self) -> &'static str {
        PROVIDER
    }

    /// Validates and executes one tool invocation.
    async fn invoke(&self, ctx: &InvokeContext, req: InvokeRequest) -> InvokeResponse {
        let (tenant_id, obj) = match prep_request("linear.update_issue", &req) {
            Ok(r) => r,
            Err(r) => return r,
        };
        let issue_id = match req_str(obj, "issue_id", "linear.update_issue") {
            Ok(v) => v,
            Err(r) => return r,
        };

        let mut input = serde_json::Map::new();
        if let Some(t) = opt_str(obj, "title") {
            input.insert("title".into(), json!(t));
        }
        if let Some(d) = obj.get("description").and_then(|v| v.as_str()) {
            input.insert("description".into(), json!(d));
        }
        if let Some(p) = obj.get("priority").and_then(|v| v.as_i64()) {
            input.insert("priority".into(), json!(p));
        }
        if let Some(s) = opt_str(obj, "state_id") {
            input.insert("stateId".into(), json!(s));
        }
        if let Some(a) = opt_str(obj, "assignee_id") {
            input.insert("assigneeId".into(), json!(a));
        }
        if input.is_empty() {
            return error_response(
                "linear.update_issue",
                "bad_request",
                "no updatable fields provided",
                Some("supply at least one of title/description/priority/state_id/assignee_id"),
            );
        }

        let vars = json!({ "id": issue_id, "input": input });
        let query = r#"
mutation UpdateIssue($id:String! $input:IssueUpdateInput!) {
  issueUpdate(id:$id input:$input) {
    success
    issue { id identifier title url state { name } }
  }
}"#;

        let token = match ctx.phylaxd.fetch_token(&tenant_id, PROVIDER).await {
            Ok(t) => t,
            Err(e) => return phylaxd_error_to_response("linear.update_issue", &e),
        };
        let outcome = match linear_gql(&token, query, &vars, &self.retry_policy()).await {
            Ok(o) => o,
            Err(r) => return r,
        };
        let issue = outcome
            .pointer("/data/issueUpdate/issue")
            .cloned()
            .unwrap_or(Value::Null);
        InvokeResponse {
            tool_id: "linear.update_issue".into(),
            success: true,
            result: Some(issue),
            error: None,
            duration_ms: 0,
        }
    }
}

// ---------------------------------------------------------------------------
// linear.search -- full-text search across issues.
// ---------------------------------------------------------------------------

/// Search Linear issues by free text.
pub struct LinearSearchTool;

#[async_trait]
/// Implements the Hermes tool contract for LinearSearchTool.
impl Tool for LinearSearchTool {
    /// Returns the public schema for this tool.
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            tool_id: "linear.search".to_string(),
            name: "Search Linear Issues".to_string(),
            description: "Search Linear issues by free text.".to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["query"],
                "properties": {
                    "query": { "type": "string", "description": "Search terms" },
                    "team_id": { "type": "string", "description": "Scope to a single team" },
                    "first": { "type": "integer", "description": "Max results (default 20, max 50)" }
                }
            }),
            output_schema: json!({
                "type": "object",
                "properties": { "issues": { "type": "array" }, "total": { "type": "integer" } }
            }),
            category: "project_management".to_string(),
            requires_auth: true,
        }
    }

    /// Returns the credential-provider identifier for this tool.
    fn provider(&self) -> &'static str {
        PROVIDER
    }

    /// Validates and executes one tool invocation.
    async fn invoke(&self, ctx: &InvokeContext, req: InvokeRequest) -> InvokeResponse {
        let (tenant_id, obj) = match prep_request("linear.search", &req) {
            Ok(r) => r,
            Err(r) => return r,
        };
        let query = match req_str(obj, "query", "linear.search") {
            Ok(v) => v,
            Err(r) => return r,
        };
        let first = obj
            .get("first")
            .and_then(|v| v.as_u64())
            .map(|n| n.clamp(1, 50))
            .unwrap_or(20);

        let mut filter = serde_json::Map::new();
        if let Some(team_id) = opt_str(obj, "team_id") {
            filter.insert("team".into(), json!({ "id": { "eq": team_id } }));
        }
        // Linear's search query uses a separate `term` field at root level.
        let vars = json!({
            "term": query,
            "filter": filter,
            "first": first,
        });
        let gql = r#"
query SearchIssues($term:String! $filter:IssueFilter $first:Int) {
  issueSearch(term:$term filter:$filter first:$first) {
    totalCount
    nodes { id identifier title priority url state { name } assignee { name } }
  }
}"#;

        let token = match ctx.phylaxd.fetch_token(&tenant_id, PROVIDER).await {
            Ok(t) => t,
            Err(e) => return phylaxd_error_to_response("linear.search", &e),
        };
        let outcome = match linear_gql(&token, gql, &vars, &self.retry_policy()).await {
            Ok(o) => o,
            Err(r) => return r,
        };
        let search_node = outcome
            .pointer("/data/issueSearch")
            .cloned()
            .unwrap_or(Value::Null);
        let total = search_node
            .get("totalCount")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let nodes = search_node
            .get("nodes")
            .cloned()
            .unwrap_or_else(|| Value::Array(vec![]));
        let summarised: Vec<Value> = nodes
            .as_array()
            .map(|arr| arr.iter().map(issue_summary).collect())
            .unwrap_or_default();

        InvokeResponse {
            tool_id: "linear.search".into(),
            success: true,
            result: Some(json!({ "issues": summarised, "total": total })),
            error: None,
            duration_ms: 0,
        }
    }
}

// ---------------------------------------------------------------------------
// linear.create_webhook -- register a webhook on a Linear team.
// ---------------------------------------------------------------------------

/// Register a Linear webhook for issue events on a team.
pub struct LinearCreateWebhookTool;

#[async_trait]
/// Implements the Hermes tool contract for LinearCreateWebhookTool.
impl Tool for LinearCreateWebhookTool {
    /// Returns the public schema for this tool.
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            tool_id: "linear.create_webhook".to_string(),
            name: "Create Linear Webhook".to_string(),
            description: "Register a Linear webhook for issue events on a team.".to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["team_id", "url"],
                "properties": {
                    "team_id": { "type": "string" },
                    "url": { "type": "string", "description": "HTTPS callback URL" },
                    "label": { "type": "string", "description": "Human-readable label" },
                    "resource_types": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Resource types to subscribe to (default [\"Issue\"])"
                    }
                }
            }),
            output_schema: json!({
                "type": "object",
                "properties": {
                    "id": { "type": "string" },
                    "url": { "type": "string" },
                    "enabled": { "type": "boolean" }
                }
            }),
            category: "project_management".to_string(),
            requires_auth: true,
        }
    }

    /// Returns the credential-provider identifier for this tool.
    fn provider(&self) -> &'static str {
        PROVIDER
    }

    /// Webhook registration is not idempotent; disable retries.
    fn retry_policy(&self) -> crate::tool::RetryPolicy {
        crate::tool::RetryPolicy::non_idempotent()
    }

    /// Validates and executes one tool invocation.
    async fn invoke(&self, ctx: &InvokeContext, req: InvokeRequest) -> InvokeResponse {
        let (tenant_id, obj) = match prep_request("linear.create_webhook", &req) {
            Ok(r) => r,
            Err(r) => return r,
        };
        let team_id = match req_str(obj, "team_id", "linear.create_webhook") {
            Ok(v) => v,
            Err(r) => return r,
        };
        let url = match req_str(obj, "url", "linear.create_webhook") {
            Ok(v) => v,
            Err(r) => return r,
        };
        let label = opt_str(obj, "label").unwrap_or_else(|| "hermes-webhook".to_string());
        let resource_types = obj
            .get("resource_types")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_else(|| vec![json!("Issue")]);

        let vars = json!({
            "teamId": team_id,
            "url": url,
            "label": label,
            "resourceTypes": resource_types,
        });
        let query = r#"
mutation CreateWebhook($teamId:String! $url:String! $label:String $resourceTypes:[String!]) {
  webhookCreate(input:{teamId:$teamId url:$url label:$label resourceTypes:$resourceTypes}) {
    success
    webhook { id url enabled label }
  }
}"#;

        let token = match ctx.phylaxd.fetch_token(&tenant_id, PROVIDER).await {
            Ok(t) => t,
            Err(e) => return phylaxd_error_to_response("linear.create_webhook", &e),
        };
        let outcome = match linear_gql(&token, query, &vars, &self.retry_policy()).await {
            Ok(o) => o,
            Err(r) => return r,
        };
        let webhook = outcome
            .pointer("/data/webhookCreate/webhook")
            .cloned()
            .unwrap_or(Value::Null);
        InvokeResponse {
            tool_id: "linear.create_webhook".into(),
            success: true,
            result: Some(json!({
                "id": webhook.get("id").and_then(|v| v.as_str()).unwrap_or(""),
                "url": webhook.get("url").and_then(|v| v.as_str()).unwrap_or(""),
                "enabled": webhook.get("enabled").and_then(|v| v.as_bool()).unwrap_or(false),
            })),
            error: None,
            duration_ms: 0,
        }
    }
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Send a GraphQL request to Linear and decode the JSON response.
///
/// Returns `Err(InvokeResponse)` on transport or API error so callers can
/// propagate cleanly with `?`-like ergonomics.
async fn linear_gql(
    token: &str,
    query: &str,
    variables: &Value,
    policy: &crate::tool::RetryPolicy,
) -> Result<Value, InvokeResponse> {
    let http = build_http().map_err(|e| linear_prep("linear", e.to_string()))?;
    let body = json!({ "query": query, "variables": variables });
    let request = http.post(GQL_URL).bearer_auth(token).json(&body);

    let outcome = send_with_retry(request, policy)
        .await
        .map_err(|e| InvokeResponse {
            tool_id: "linear".into(),
            success: false,
            result: None,
            error: Some(err(
                "linear_unreachable",
                format!("linear api request failed: {e}"),
                None,
            )),
            duration_ms: 0,
        })?;

    if !outcome.status.is_success() {
        warn!(status = %outcome.status, body = %truncate(&outcome.body, 256), "linear api error");
        return Err(InvokeResponse {
            tool_id: "linear".into(),
            success: false,
            result: None,
            error: Some(json!({
                "code": "linear_http_error",
                "message": format!("linear returned HTTP {}", outcome.status.as_u16()),
                "status": outcome.status.as_u16(),
                "body": truncate(&outcome.body, 512),
            })),
            duration_ms: 0,
        });
    }

    let parsed: Value = serde_json::from_str(&outcome.body).unwrap_or(Value::Null);

    // Linear embeds GraphQL errors in the JSON even on HTTP 200.
    if let Some(errors) = parsed.get("errors").and_then(|v| v.as_array()) {
        if !errors.is_empty() {
            let msg = errors
                .first()
                .and_then(|e| e.get("message"))
                .and_then(|v| v.as_str())
                .unwrap_or("unknown graphql error");
            warn!(error = msg, "linear graphql error");
            return Err(InvokeResponse {
                tool_id: "linear".into(),
                success: false,
                result: None,
                error: Some(json!({
                    "code": "linear_gql_error",
                    "message": msg,
                    "errors": errors,
                })),
                duration_ms: 0,
            });
        }
    }

    Ok(parsed)
}

/// Build a standard linear error response for transport failures.
fn linear_prep(tool_id: &str, msg: impl Into<String>) -> InvokeResponse {
    InvokeResponse {
        tool_id: tool_id.to_string(),
        success: false,
        result: None,
        error: Some(err("linear_error", msg.into(), None)),
        duration_ms: 0,
    }
}

/// Validate and extract the tenant_id + args object for a request.
fn prep_request<'a>(
    tool_id: &str,
    req: &'a InvokeRequest,
) -> Result<(String, &'a serde_json::Map<String, Value>), InvokeResponse> {
    let tenant_id = req
        .tenant_id
        .as_deref()
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .ok_or_else(|| error_response(tool_id, "bad_request", "tenant_id is required", None))?;
    let obj = req.args.as_object().ok_or_else(|| {
        error_response(tool_id, "bad_request", "args must be a JSON object", None)
    })?;
    Ok((tenant_id, obj))
}

/// Extract a required string field from an args object.
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

/// Extract an optional string field from an args object.
fn opt_str(obj: &serde_json::Map<String, Value>, key: &str) -> Option<String> {
    obj.get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Flatten a raw Linear issue node into a compact summary for list/search output.
fn issue_summary(node: &Value) -> Value {
    json!({
        "id": node.get("id").and_then(|v| v.as_str()).unwrap_or(""),
        "identifier": node.get("identifier").and_then(|v| v.as_str()).unwrap_or(""),
        "title": node.get("title").and_then(|v| v.as_str()).unwrap_or(""),
        "priority": node.get("priority").and_then(|v| v.as_i64()).unwrap_or(0),
        "url": node.get("url").and_then(|v| v.as_str()).unwrap_or(""),
        "state": node.pointer("/state/name").and_then(|v| v.as_str()).unwrap_or(""),
        "assignee": node.pointer("/assignee/name").and_then(|v| v.as_str()).unwrap_or(""),
    })
}

#[cfg(test)]
/// Contains focused unit tests for this module.
mod tests {
    use super::*;

    #[test]
    /// Verifies issue summary maps fields.
    fn issue_summary_maps_fields() {
        let node = json!({
            "id": "i1",
            "identifier": "ENG-42",
            "title": "Fix bug",
            "priority": 2,
            "url": "https://linear.app/x",
            "state": { "name": "In Progress" },
            "assignee": { "name": "Alice" }
        });
        let s = issue_summary(&node);
        assert_eq!(s["identifier"], "ENG-42");
        assert_eq!(s["state"], "In Progress");
        assert_eq!(s["assignee"], "Alice");
    }

    #[test]
    /// Verifies issue summary tolerates missing fields.
    fn issue_summary_tolerates_missing_fields() {
        let node = json!({ "id": "i2", "identifier": "ENG-1" });
        let s = issue_summary(&node);
        assert_eq!(s["state"], "");
        assert_eq!(s["assignee"], "");
    }

    #[test]
    /// Verifies opt str trims and rejects empty.
    fn opt_str_trims_and_rejects_empty() {
        let obj = json!({ "k": "  hello  ", "empty": "" });
        let m = obj.as_object().unwrap();
        assert_eq!(opt_str(m, "k").as_deref(), Some("hello"));
        assert!(opt_str(m, "empty").is_none());
        assert!(opt_str(m, "missing").is_none());
    }

    #[test]
    /// Verifies req str returns error on missing.
    fn req_str_returns_error_on_missing() {
        let obj = json!({ "x": 1 });
        let m = obj.as_object().unwrap();
        assert!(req_str(m, "title", "tool").is_err());
    }

    #[test]
    /// Verifies req str returns trimmed value.
    fn req_str_returns_trimmed_value() {
        let obj = json!({ "title": "  hi  " });
        let m = obj.as_object().unwrap();
        assert_eq!(req_str(m, "title", "tool").unwrap(), "hi");
    }

    #[test]
    /// Verifies prep request rejects missing tenant.
    fn prep_request_rejects_missing_tenant() {
        let req = InvokeRequest {
            tenant_id: None,
            args: json!({}),
        };
        let result = prep_request("t", &req);
        assert!(result.is_err());
    }

    #[test]
    /// Verifies prep request rejects non object args.
    fn prep_request_rejects_non_object_args() {
        let req = InvokeRequest {
            tenant_id: Some("t".into()),
            args: json!("bad"),
        };
        let result = prep_request("t", &req);
        assert!(result.is_err());
    }

    /// Adapter integration tests use wiremock to mock the Linear GraphQL endpoint.
    /// These tests are behind a separate feature to avoid the default test suite
    /// requiring network access. They are exercised by the workspace CI run.
    #[cfg(feature = "integration-tests")]
    mod integration {
        use super::*;
        use wiremock::{matchers::*, Mock, MockServer, ResponseTemplate};

        /// Verifies start linear mock.
        async fn start_linear_mock(response_body: Value) -> MockServer {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/graphql"))
                .respond_with(ResponseTemplate::new(200).set_body_json(response_body))
                .mount(&server)
                .await;
            server
        }
    }
}
