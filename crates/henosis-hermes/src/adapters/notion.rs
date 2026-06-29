//! Notion adapters (REST API v1). All requests carry a bearer token plus the
//! pinned `Notion-Version` header. The base URL is read from the invoke
//! context so tests can redirect it to a mock server.

use async_trait::async_trait;
use serde_json::{json, Value};
use tracing::warn;

use crate::adapters::common::{build_http, credd_error_to_response, send_with_retry, truncate, HttpOutcome};
use crate::tool::{
    error_response, InvokeContext, InvokeRequest, InvokeResponse, RetryPolicy, Tool, ToolSchema,
};

/// credd provider tag for all Notion tools.
const PROVIDER: &str = "notion";
/// Notion API version pinned on all requests.
const NOTION_VERSION: &str = "2022-06-28";

/// Apply Notion auth and API-version headers to a request builder.
fn notion_headers(rb: reqwest::RequestBuilder, token: &str) -> reqwest::RequestBuilder {
    rb.bearer_auth(token)
        .header("Notion-Version", NOTION_VERSION)
        .header("Content-Type", "application/json")
}

/// Resolve tenant credentials and an HTTP client for a Notion tool.
async fn notion_prep(
    ctx: &InvokeContext,
    tenant_id: Option<&str>,
    tool_id: &str,
) -> Result<(String, reqwest::Client), InvokeResponse> {
    let tenant = tenant_id
        .filter(|s| !s.is_empty())
        .ok_or_else(|| error_response(tool_id, "bad_request", "tenant_id is required", None))?;
    let token = ctx
        .credd
        .fetch_token(tenant, PROVIDER)
        .await
        .map_err(|e| credd_error_to_response(tool_id, &e))?;
    let http = build_http().map_err(|e| error_response(tool_id, "internal_error", e.to_string(), None))?;
    Ok((token, http))
}

/// Map a Notion HTTP outcome to either parsed JSON or a structured error.
fn notion_finish(tool_id: &str, res: Result<HttpOutcome, reqwest::Error>) -> Result<Value, InvokeResponse> {
    match res {
        Err(e) => Err(error_response(
            tool_id,
            "notion_unreachable",
            format!("notion api request failed: {e}"),
            None,
        )),
        Ok(o) if o.status.is_success() => Ok(serde_json::from_str(&o.body).unwrap_or(Value::Null)),
        Ok(o) => {
            warn!(status = %o.status, body = %truncate(&o.body, 256), "notion api error");
            Err(InvokeResponse {
                tool_id: tool_id.to_string(),
                success: false,
                result: None,
                error: Some(json!({
                    "code": "notion_api_error",
                    "message": format!("notion returned HTTP {}", o.status.as_u16()),
                    "status": o.status.as_u16(),
                    "body": truncate(&o.body, 512),
                })),
                duration_ms: 0,
            })
        }
    }
}

/// Build a successful `InvokeResponse` for any Notion tool.
fn ok(tool_id: &str, result: Value) -> InvokeResponse {
    InvokeResponse {
        tool_id: tool_id.to_string(),
        success: true,
        result: Some(result),
        error: None,
        duration_ms: 0,
    }
}

/// Build the `title` property payload Notion expects for a page title.
fn title_property(title: &str) -> Value {
    json!({ "title": [ { "text": { "content": title } } ] })
}

// ===========================================================================
// notion.search
// ===========================================================================

/// Search pages and databases in a Notion workspace.
pub struct NotionSearchTool;

#[async_trait]
impl Tool for NotionSearchTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            tool_id: "notion.search".to_string(),
            name: "Search Notion".to_string(),
            description: "Search pages and databases in a Notion workspace.".to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["query"],
                "properties": {
                    "query": { "type": "string" },
                    "filter": { "type": "string", "enum": ["page", "database"], "description": "Restrict to pages or databases" },
                    "page_size": { "type": "integer", "description": "Default 10, max 100" }
                }
            }),
            output_schema: json!({
                "type": "object",
                "properties": { "results": { "type": "array" }, "next_cursor": { "type": "string" } }
            }),
            category: "docs".to_string(),
            requires_auth: true,
        }
    }

    fn provider(&self) -> &'static str {
        PROVIDER
    }

    async fn invoke(&self, ctx: &InvokeContext, req: InvokeRequest) -> InvokeResponse {
        let tool_id = "notion.search";
        let obj = req.args.as_object().cloned().unwrap_or_default();
        let query = obj.get("query").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let page_size = obj.get("page_size").and_then(|v| v.as_u64()).unwrap_or(10).clamp(1, 100);

        let mut payload = json!({ "query": query, "page_size": page_size });
        if let Some(f) = obj.get("filter").and_then(|v| v.as_str()) {
            payload["filter"] = json!({ "value": f, "property": "object" });
        }

        let (token, http) = match notion_prep(ctx, req.tenant_id.as_deref(), tool_id).await {
            Ok(v) => v,
            Err(r) => return r,
        };
        let url = format!("{}/v1/search", ctx.bases.notion.trim_end_matches('/'));
        let request = notion_headers(http.post(&url), &token).json(&payload);
        match notion_finish(tool_id, send_with_retry(request, &self.retry_policy()).await) {
            Ok(v) => ok(
                tool_id,
                json!({
                    "results": v.get("results").cloned().unwrap_or_else(|| Value::Array(vec![])),
                    "next_cursor": v.get("next_cursor").cloned().unwrap_or(Value::Null),
                }),
            ),
            Err(r) => r,
        }
    }
}

// ===========================================================================
// notion.get_page (page + first 100 child blocks)
// ===========================================================================

/// Fetch a Notion page's properties plus its first 100 child blocks.
pub struct NotionGetPageTool;

#[async_trait]
impl Tool for NotionGetPageTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            tool_id: "notion.get_page".to_string(),
            name: "Get Notion Page".to_string(),
            description: "Fetch a Notion page's properties plus its first 100 child blocks.".to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["page_id"],
                "properties": { "page_id": { "type": "string" } }
            }),
            output_schema: json!({
                "type": "object",
                "properties": { "page": { "type": "object" }, "blocks": { "type": "array" } }
            }),
            category: "docs".to_string(),
            requires_auth: true,
        }
    }

    fn provider(&self) -> &'static str {
        PROVIDER
    }

    async fn invoke(&self, ctx: &InvokeContext, req: InvokeRequest) -> InvokeResponse {
        let tool_id = "notion.get_page";
        let obj = req.args.as_object().cloned().unwrap_or_default();
        let page_id = obj.get("page_id").and_then(|v| v.as_str()).unwrap_or("").to_string();

        let (token, http) = match notion_prep(ctx, req.tenant_id.as_deref(), tool_id).await {
            Ok(v) => v,
            Err(r) => return r,
        };
        let base = ctx.bases.notion.trim_end_matches('/');

        let page_req = notion_headers(http.get(format!("{base}/v1/pages/{page_id}")), &token);
        let page = match notion_finish(tool_id, send_with_retry(page_req, &self.retry_policy()).await) {
            Ok(v) => v,
            Err(r) => return r,
        };

        let blocks_req = notion_headers(http.get(format!("{base}/v1/blocks/{page_id}/children")), &token)
            .query(&[("page_size", "100")]);
        let blocks = match notion_finish(tool_id, send_with_retry(blocks_req, &self.retry_policy()).await) {
            Ok(v) => v.get("results").cloned().unwrap_or_else(|| Value::Array(vec![])),
            Err(r) => return r,
        };

        ok(tool_id, json!({ "page": page, "blocks": blocks }))
    }
}

// ===========================================================================
// notion.create_page
// ===========================================================================

/// Create a page under a parent page or database in Notion.
pub struct NotionCreatePageTool;

#[async_trait]
impl Tool for NotionCreatePageTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            tool_id: "notion.create_page".to_string(),
            name: "Create Notion Page".to_string(),
            description: "Create a page under a parent page or database.".to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["parent_id", "title"],
                "properties": {
                    "parent_id": { "type": "string" },
                    "title": { "type": "string" },
                    "parent_type": { "type": "string", "enum": ["page", "database"], "description": "Default page" },
                    "properties": { "type": "object", "description": "Additional properties for database pages" },
                    "children": { "type": "array", "description": "Block content" }
                }
            }),
            output_schema: json!({
                "type": "object",
                "properties": { "id": { "type": "string" }, "url": { "type": "string" } }
            }),
            category: "docs".to_string(),
            requires_auth: true,
        }
    }

    fn provider(&self) -> &'static str {
        PROVIDER
    }

    fn retry_policy(&self) -> RetryPolicy {
        RetryPolicy::non_idempotent()
    }

    async fn invoke(&self, ctx: &InvokeContext, req: InvokeRequest) -> InvokeResponse {
        let tool_id = "notion.create_page";
        let obj = req.args.as_object().cloned().unwrap_or_default();
        let parent_id = obj.get("parent_id").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let title = obj.get("title").and_then(|v| v.as_str()).unwrap_or("");
        let parent_type = obj.get("parent_type").and_then(|v| v.as_str()).unwrap_or("page");

        let parent = if parent_type == "database" {
            json!({ "database_id": parent_id })
        } else {
            json!({ "page_id": parent_id })
        };

        // Start from the title property, then let caller-supplied properties
        // override (database pages often key the title differently).
        let mut properties = serde_json::Map::new();
        properties.insert("title".into(), title_property(title));
        if let Some(extra) = obj.get("properties").and_then(|v| v.as_object()) {
            for (k, v) in extra {
                properties.insert(k.clone(), v.clone());
            }
        }

        let mut payload = json!({ "parent": parent, "properties": Value::Object(properties) });
        if let Some(children) = obj.get("children").and_then(|v| v.as_array()) {
            payload["children"] = Value::Array(children.clone());
        }

        let (token, http) = match notion_prep(ctx, req.tenant_id.as_deref(), tool_id).await {
            Ok(v) => v,
            Err(r) => return r,
        };
        let url = format!("{}/v1/pages", ctx.bases.notion.trim_end_matches('/'));
        let request = notion_headers(http.post(&url), &token).json(&payload);
        match notion_finish(tool_id, send_with_retry(request, &self.retry_policy()).await) {
            Ok(v) => ok(
                tool_id,
                json!({
                    "id": v.get("id").and_then(|n| n.as_str()).unwrap_or(""),
                    "url": v.get("url").and_then(|n| n.as_str()).unwrap_or(""),
                }),
            ),
            Err(r) => r,
        }
    }
}

// ===========================================================================
// notion.append_blocks
// ===========================================================================

/// Append block children to a Notion page or block.
pub struct NotionAppendBlocksTool;

#[async_trait]
impl Tool for NotionAppendBlocksTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            tool_id: "notion.append_blocks".to_string(),
            name: "Append Notion Blocks".to_string(),
            description: "Append block children to a page or block.".to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["page_id", "children"],
                "properties": {
                    "page_id": { "type": "string", "description": "Parent page or block id" },
                    "children": { "type": "array", "description": "Blocks to append" }
                }
            }),
            output_schema: json!({ "type": "object", "properties": { "results": { "type": "array" } } }),
            category: "docs".to_string(),
            requires_auth: true,
        }
    }

    fn provider(&self) -> &'static str {
        PROVIDER
    }

    fn retry_policy(&self) -> RetryPolicy {
        RetryPolicy::non_idempotent()
    }

    async fn invoke(&self, ctx: &InvokeContext, req: InvokeRequest) -> InvokeResponse {
        let tool_id = "notion.append_blocks";
        let obj = req.args.as_object().cloned().unwrap_or_default();
        let page_id = obj.get("page_id").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let children = obj.get("children").cloned().unwrap_or_else(|| Value::Array(vec![]));

        let (token, http) = match notion_prep(ctx, req.tenant_id.as_deref(), tool_id).await {
            Ok(v) => v,
            Err(r) => return r,
        };
        let url = format!("{}/v1/blocks/{page_id}/children", ctx.bases.notion.trim_end_matches('/'));
        let request = notion_headers(http.patch(&url), &token).json(&json!({ "children": children }));
        match notion_finish(tool_id, send_with_retry(request, &self.retry_policy()).await) {
            Ok(v) => ok(
                tool_id,
                json!({ "results": v.get("results").cloned().unwrap_or_else(|| Value::Array(vec![])) }),
            ),
            Err(r) => r,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::test_support::test_ctx;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// Build a credd mock for Notion token resolution.
    fn credd_mock() -> Mock {
        Mock::given(method("POST")).and(path("/resolve/raw")).respond_with(
            ResponseTemplate::new(200).set_body_json(json!({
                "category": "notion_oauth", "name": "t", "value": { "access_token": "test-token" }
            })),
        )
    }

    #[test]
    fn title_property_shape() {
        let p = title_property("Hello");
        assert_eq!(p["title"][0]["text"]["content"], json!("Hello"));
    }

    #[test]
    fn mutating_tools_non_idempotent() {
        assert_eq!(NotionCreatePageTool.retry_policy().max_retries, 0);
        assert_eq!(NotionAppendBlocksTool.retry_policy().max_retries, 0);
        assert_eq!(NotionSearchTool.retry_policy().max_retries, 3);
    }

    test_adapter!(
        search_returns_results,
        tool: NotionSearchTool,
        method: "POST",
        path: "/v1/search",
        respond: json!({ "results": [ { "id": "p1" } ], "next_cursor": null }),
        args: json!({ "query": "notes" }),
        expect: { "results" => [ { "id": "p1" } ] }
    );

    test_adapter!(
        append_blocks_returns_results,
        tool: NotionAppendBlocksTool,
        method: "PATCH",
        path: "/v1/blocks/B1/children",
        respond: json!({ "results": [ { "id": "b1" } ] }),
        args: json!({ "page_id": "B1", "children": [ { "type": "paragraph" } ] }),
        expect: { "results" => [ { "id": "b1" } ] }
    );

    // get_page makes two calls (page + children); exercise both via a manual
    // mock since the single-shot macro cannot express it.
    #[tokio::test]
    async fn get_page_merges_page_and_blocks() {
        let server = MockServer::start().await;
        credd_mock().mount(&server).await;
        Mock::given(method("GET"))
            .and(path("/v1/pages/PG1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "id": "PG1", "object": "page" })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/blocks/PG1/children"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "results": [ { "id": "blk1" } ] })))
            .mount(&server)
            .await;

        let ctx = test_ctx(&server.uri());
        let resp = NotionGetPageTool
            .invoke(&ctx, InvokeRequest { tenant_id: Some("t".into()), args: json!({ "page_id": "PG1" }) })
            .await;
        assert!(resp.success, "error: {:?}", resp.error);
        let result = resp.result.unwrap();
        assert_eq!(result["page"]["id"], json!("PG1"));
        assert_eq!(result["blocks"][0]["id"], json!("blk1"));
    }
}
