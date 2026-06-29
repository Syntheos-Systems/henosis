//! Gmail adapters: send, read, search, and list_labels.
//!
//! All tools authenticate via Google OAuth tokens resolved from credd.
//! The send adapter builds a full RFC2822 MIME message and base64url-encodes
//! it before posting to the Gmail API.

use async_trait::async_trait;
use base64::engine::general_purpose::{URL_SAFE, URL_SAFE_NO_PAD};
use base64::Engine as _;
use serde_json::{json, Value};
use tracing::warn;

use crate::adapters::common::{build_http, credd_error_to_response, send_with_retry, truncate};
use crate::tool::{
    err, error_response, InvokeContext, InvokeRequest, InvokeResponse, RetryPolicy, Tool, ToolSchema,
};

/// Tool ID for the send adapter.
const TOOL_ID: &str = "gmail.send";
/// credd provider tag for all Gmail/Google tools.
const PROVIDER: &str = "google";
/// Gmail API endpoint for sending messages.
const GMAIL_SEND_URL: &str = "https://gmail.googleapis.com/gmail/v1/users/me/messages/send";

/// Send an email via Gmail on behalf of the authenticated tenant.
pub struct GmailSendTool;

#[async_trait]
impl Tool for GmailSendTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            tool_id: TOOL_ID.to_string(),
            name: "Send Email".to_string(),
            description: "Send an email via Gmail on behalf of the authenticated tenant.".to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["to", "subject", "body"],
                "properties": {
                    "to": { "type": "string", "description": "Recipient email address" },
                    "subject": { "type": "string", "description": "Email subject line" },
                    "body": { "type": "string", "description": "Email body (plain text)" },
                    "cc": { "type": "string", "description": "CC recipients" },
                    "bcc": { "type": "string", "description": "BCC recipients" }
                }
            }),
            output_schema: json!({
                "type": "object",
                "properties": {
                    "message_id": { "type": "string" },
                    "thread_id": { "type": "string" }
                }
            }),
            category: "email".to_string(),
            requires_auth: true,
        }
    }

    fn provider(&self) -> &'static str {
        PROVIDER
    }

    /// Sending an email is not safe to replay: a transient error after the
    /// message was actually accepted could deliver duplicates.
    fn retry_policy(&self) -> RetryPolicy {
        RetryPolicy::non_idempotent()
    }

    async fn invoke(&self, ctx: &InvokeContext, req: InvokeRequest) -> InvokeResponse {
        let tenant_id = match req.tenant_id.as_deref().filter(|s| !s.is_empty()) {
            Some(t) => t.to_string(),
            None => return error_response(TOOL_ID, "bad_request", "tenant_id is required", None),
        };

        let args = match parse_args(&req.args) {
            Ok(a) => a,
            Err(msg) => return error_response(TOOL_ID, "bad_request", msg, None),
        };

        let token = match ctx.credd.fetch_token(&tenant_id, PROVIDER).await {
            Ok(t) => t,
            Err(e) => return credd_error_to_response(TOOL_ID, &e),
        };

        let raw_mime = build_mime(&args);
        let raw_b64 = URL_SAFE_NO_PAD.encode(raw_mime.as_bytes());

        let http = match build_http() {
            Ok(c) => c,
            Err(e) => {
                warn!(error = %e, "gmail http client build failed");
                return error_response(TOOL_ID, "internal_error", e.to_string(), None);
            }
        };

        let request = http
            .post(GMAIL_SEND_URL)
            .bearer_auth(&token)
            .json(&json!({ "raw": raw_b64 }));

        let outcome = match send_with_retry(request, &self.retry_policy()).await {
            Ok(o) => o,
            Err(e) => {
                return InvokeResponse {
                    tool_id: TOOL_ID.into(),
                    success: false,
                    result: None,
                    error: Some(err(
                        "gmail_unreachable",
                        format!("gmail api request failed: {e}"),
                        None,
                    )),
                    duration_ms: 0,
                }
            }
        };

        let status = outcome.status;
        let body_text = outcome.body;

        if !status.is_success() {
            return InvokeResponse {
                tool_id: TOOL_ID.into(),
                success: false,
                result: None,
                error: Some(json!({
                    "code": "gmail_api_error",
                    "message": format!("gmail returned HTTP {}", status.as_u16()),
                    "status": status.as_u16(),
                    "body": truncate(&body_text, 512),
                })),
                duration_ms: 0,
            };
        }

        let parsed: Value = serde_json::from_str(&body_text).unwrap_or(Value::Null);
        let message_id = parsed.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let thread_id = parsed.get("threadId").and_then(|v| v.as_str()).unwrap_or("").to_string();

        InvokeResponse {
            tool_id: TOOL_ID.into(),
            success: true,
            result: Some(json!({
                "message_id": message_id,
                "thread_id": thread_id,
            })),
            error: None,
            duration_ms: 0,
        }
    }
}

/// Parsed arguments for `gmail.send`.
struct GmailArgs {
    /// Primary recipient address.
    to: String,
    /// Email subject line.
    subject: String,
    /// Plain-text body.
    body: String,
    /// Optional CC addresses.
    cc: Option<String>,
    /// Optional BCC addresses.
    bcc: Option<String>,
}

/// Parse and validate `gmail.send` arguments.
fn parse_args(args: &Value) -> Result<GmailArgs, String> {
    let obj = args.as_object().ok_or_else(|| "args must be a JSON object".to_string())?;
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
    Ok(GmailArgs {
        to: pull_required("to")?,
        subject: pull_required("subject")?,
        body: pull_required("body")?,
        cc: pull_optional("cc"),
        bcc: pull_optional("bcc"),
    })
}

/// Build an RFC2822 MIME message string from the parsed args.
fn build_mime(a: &GmailArgs) -> String {
    let mut out = String::new();
    out.push_str(&format!("To: {}\r\n", a.to));
    if let Some(cc) = &a.cc {
        out.push_str(&format!("Cc: {cc}\r\n"));
    }
    if let Some(bcc) = &a.bcc {
        out.push_str(&format!("Bcc: {bcc}\r\n"));
    }
    out.push_str(&format!("Subject: {}\r\n", encode_subject(&a.subject)));
    out.push_str("MIME-Version: 1.0\r\n");
    out.push_str("Content-Type: text/plain; charset=\"UTF-8\"\r\n");
    out.push_str("Content-Transfer-Encoding: 8bit\r\n");
    out.push_str("\r\n");
    out.push_str(&a.body);
    out
}

/// Encode the subject as plain ASCII when possible, or as RFC2047
/// base64url when it contains non-ASCII characters.
fn encode_subject(s: &str) -> String {
    if s.is_ascii() {
        s.to_string()
    } else {
        format!("=?UTF-8?B?{}?=", URL_SAFE_NO_PAD.encode(s.as_bytes()))
    }
}

// ---------------------------------------------------------------------------
// gmail.read -- fetch a single message by id and parse it into a flat shape.
// ---------------------------------------------------------------------------

/// Gmail API endpoint for fetching individual messages.
const GMAIL_MESSAGES_URL: &str = "https://gmail.googleapis.com/gmail/v1/users/me/messages";

/// Fetch a Gmail message by id and return parsed headers, body, attachments, and labels.
pub struct GmailReadTool;

#[async_trait]
impl Tool for GmailReadTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            tool_id: "gmail.read".to_string(),
            name: "Read Email".to_string(),
            description: "Fetch a Gmail message by id and return parsed headers, body, attachments and labels.".to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["message_id"],
                "properties": {
                    "message_id": { "type": "string", "description": "Gmail message id" },
                    "format": {
                        "type": "string",
                        "enum": ["full", "metadata", "minimal"],
                        "description": "Response detail level (default full)"
                    }
                }
            }),
            output_schema: json!({
                "type": "object",
                "properties": {
                    "id": { "type": "string" },
                    "thread_id": { "type": "string" },
                    "snippet": { "type": "string" },
                    "headers": { "type": "object" },
                    "body": { "type": "string" },
                    "attachments": { "type": "array" },
                    "labels": { "type": "array" }
                }
            }),
            category: "email".to_string(),
            requires_auth: true,
        }
    }

    fn provider(&self) -> &'static str {
        PROVIDER
    }

    async fn invoke(&self, ctx: &InvokeContext, req: InvokeRequest) -> InvokeResponse {
        let tenant_id = match req.tenant_id.as_deref().filter(|s| !s.is_empty()) {
            Some(t) => t.to_string(),
            None => return error_response("gmail.read", "bad_request", "tenant_id is required", None),
        };
        let obj = match req.args.as_object() {
            Some(o) => o,
            None => return error_response("gmail.read", "bad_request", "args must be a JSON object", None),
        };
        let message_id = match obj.get("message_id").and_then(|v| v.as_str()).map(str::trim).filter(|s| !s.is_empty()) {
            Some(m) => m.to_string(),
            None => return error_response("gmail.read", "bad_request", "'message_id' is required", None),
        };
        let format = match obj.get("format").and_then(|v| v.as_str()) {
            Some(f) if ["full", "metadata", "minimal"].contains(&f) => f,
            Some(f) => return error_response("gmail.read", "bad_request", format!("invalid format '{f}'"), Some("use full, metadata, or minimal")),
            None => "full",
        };

        let token = match ctx.credd.fetch_token(&tenant_id, PROVIDER).await {
            Ok(t) => t,
            Err(e) => return credd_error_to_response("gmail.read", &e),
        };
        let http = match build_http() {
            Ok(c) => c,
            Err(e) => return error_response("gmail.read", "internal_error", e.to_string(), None),
        };

        let url = format!("{GMAIL_MESSAGES_URL}/{message_id}");
        let request = http.get(&url).bearer_auth(&token).query(&[("format", format)]);
        let outcome = match send_with_retry(request, &self.retry_policy()).await {
            Ok(o) => o,
            Err(e) => return error_response("gmail.read", "gmail_unreachable", format!("gmail api request failed: {e}"), None),
        };
        if !outcome.status.is_success() {
            warn!(status = %outcome.status, body = %truncate(&outcome.body, 256), "gmail read error");
            return InvokeResponse {
                tool_id: "gmail.read".into(),
                success: false,
                result: None,
                error: Some(json!({
                    "code": "gmail_api_error",
                    "message": format!("gmail returned HTTP {}", outcome.status.as_u16()),
                    "status": outcome.status.as_u16(),
                    "body": truncate(&outcome.body, 512),
                })),
                duration_ms: 0,
            };
        }

        let parsed: Value = serde_json::from_str(&outcome.body).unwrap_or(Value::Null);
        InvokeResponse {
            tool_id: "gmail.read".into(),
            success: true,
            result: Some(parse_message(&parsed)),
            error: None,
            duration_ms: 0,
        }
    }
}

/// Permissively decode Gmail's base64url body data (with or without padding).
fn decode_b64url(s: &str) -> Vec<u8> {
    URL_SAFE
        .decode(s)
        .or_else(|_| URL_SAFE_NO_PAD.decode(s))
        .unwrap_or_default()
}

/// Pull the standard headers (from/to/subject/date) out of a payload's header
/// array into a flat object keyed by lowercase header name.
fn extract_headers(payload: &Value) -> Value {
    let mut out = serde_json::Map::new();
    if let Some(arr) = payload.get("headers").and_then(|v| v.as_array()) {
        for h in arr {
            let name = h.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let value = h.get("value").and_then(|v| v.as_str()).unwrap_or("");
            if matches!(name.to_ascii_lowercase().as_str(), "from" | "to" | "subject" | "date" | "cc") {
                out.insert(name.to_ascii_lowercase(), Value::String(value.to_string()));
            }
        }
    }
    Value::Object(out)
}

/// Walk a MIME tree depth-first, preferring text/plain and falling back to
/// text/html, returning the first decoded body found.
fn extract_body(payload: &Value) -> String {
    fn find(part: &Value, want: &str) -> Option<String> {
        let mime = part.get("mimeType").and_then(|v| v.as_str()).unwrap_or("");
        if mime == want {
            if let Some(data) = part.get("body").and_then(|b| b.get("data")).and_then(|v| v.as_str()) {
                return Some(String::from_utf8_lossy(&decode_b64url(data)).into_owned());
            }
        }
        if let Some(parts) = part.get("parts").and_then(|v| v.as_array()) {
            for p in parts {
                if let Some(found) = find(p, want) {
                    return Some(found);
                }
            }
        }
        None
    }
    find(payload, "text/plain")
        .or_else(|| find(payload, "text/html"))
        .unwrap_or_default()
}

/// Collect attachment metadata (filename, mime, size) from a MIME tree.
fn extract_attachments(payload: &Value) -> Vec<Value> {
    fn walk(part: &Value, out: &mut Vec<Value>) {
        let filename = part.get("filename").and_then(|v| v.as_str()).unwrap_or("");
        let has_attach_id = part
            .get("body")
            .and_then(|b| b.get("attachmentId"))
            .is_some();
        if !filename.is_empty() && has_attach_id {
            out.push(json!({
                "filename": filename,
                "mime_type": part.get("mimeType").and_then(|v| v.as_str()).unwrap_or(""),
                "size": part.get("body").and_then(|b| b.get("size")).and_then(|v| v.as_i64()).unwrap_or(0),
            }));
        }
        if let Some(parts) = part.get("parts").and_then(|v| v.as_array()) {
            for p in parts {
                walk(p, out);
            }
        }
    }
    let mut out = Vec::new();
    walk(payload, &mut out);
    out
}

/// Flatten a raw Gmail message JSON into Hermes's read result shape.
fn parse_message(msg: &Value) -> Value {
    let payload = msg.get("payload").cloned().unwrap_or(Value::Null);
    json!({
        "id": msg.get("id").and_then(|v| v.as_str()).unwrap_or(""),
        "thread_id": msg.get("threadId").and_then(|v| v.as_str()).unwrap_or(""),
        "snippet": msg.get("snippet").and_then(|v| v.as_str()).unwrap_or(""),
        "headers": extract_headers(&payload),
        "body": extract_body(&payload),
        "attachments": extract_attachments(&payload),
        "labels": msg.get("labelIds").cloned().unwrap_or_else(|| Value::Array(vec![])),
    })
}

// ---------------------------------------------------------------------------
// gmail.search -- query messages, then fetch metadata for each hit.
// ---------------------------------------------------------------------------

/// Search Gmail messages with Gmail query syntax and return summarized hits.
pub struct GmailSearchTool;

#[async_trait]
impl Tool for GmailSearchTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            tool_id: "gmail.search".to_string(),
            name: "Search Email".to_string(),
            description: "Search Gmail messages with Gmail query syntax and return summarized hits.".to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["query"],
                "properties": {
                    "query": { "type": "string", "description": "Gmail search query (e.g. 'from:alice is:unread')" },
                    "max_results": { "type": "integer", "description": "Max messages to return (default 10, max 100)" },
                    "page_token": { "type": "string", "description": "Pagination token from a prior response" }
                }
            }),
            output_schema: json!({
                "type": "object",
                "properties": {
                    "messages": { "type": "array" },
                    "next_page_token": { "type": "string" }
                }
            }),
            category: "email".to_string(),
            requires_auth: true,
        }
    }

    fn provider(&self) -> &'static str {
        PROVIDER
    }

    async fn invoke(&self, ctx: &InvokeContext, req: InvokeRequest) -> InvokeResponse {
        let tenant_id = match req.tenant_id.as_deref().filter(|s| !s.is_empty()) {
            Some(t) => t.to_string(),
            None => return error_response("gmail.search", "bad_request", "tenant_id is required", None),
        };
        let obj = match req.args.as_object() {
            Some(o) => o,
            None => return error_response("gmail.search", "bad_request", "args must be a JSON object", None),
        };
        let query = match obj.get("query").and_then(|v| v.as_str()).map(str::trim).filter(|s| !s.is_empty()) {
            Some(q) => q.to_string(),
            None => return error_response("gmail.search", "bad_request", "'query' is required", None),
        };
        let max_results = obj.get("max_results").and_then(|v| v.as_u64()).unwrap_or(10).clamp(1, 100);
        let page_token = obj.get("page_token").and_then(|v| v.as_str()).map(str::to_string);

        let token = match ctx.credd.fetch_token(&tenant_id, PROVIDER).await {
            Ok(t) => t,
            Err(e) => return credd_error_to_response("gmail.search", &e),
        };
        let http = match build_http() {
            Ok(c) => c,
            Err(e) => return error_response("gmail.search", "internal_error", e.to_string(), None),
        };

        let mut list_req = http
            .get(GMAIL_MESSAGES_URL)
            .bearer_auth(&token)
            .query(&[("q", query), ("maxResults", max_results.to_string())]);
        if let Some(pt) = &page_token {
            list_req = list_req.query(&[("pageToken", pt.clone())]);
        }
        let outcome = match send_with_retry(list_req, &self.retry_policy()).await {
            Ok(o) => o,
            Err(e) => return error_response("gmail.search", "gmail_unreachable", format!("gmail api request failed: {e}"), None),
        };
        if !outcome.status.is_success() {
            warn!(status = %outcome.status, body = %truncate(&outcome.body, 256), "gmail search error");
            return InvokeResponse {
                tool_id: "gmail.search".into(),
                success: false,
                result: None,
                error: Some(json!({
                    "code": "gmail_api_error",
                    "message": format!("gmail returned HTTP {}", outcome.status.as_u16()),
                    "status": outcome.status.as_u16(),
                    "body": truncate(&outcome.body, 512),
                })),
                duration_ms: 0,
            };
        }
        let listing: Value = serde_json::from_str(&outcome.body).unwrap_or(Value::Null);
        let ids: Vec<String> = listing
            .get("messages")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|m| m.get("id").and_then(|v| v.as_str()).map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        let next = listing.get("nextPageToken").and_then(|v| v.as_str()).unwrap_or("").to_string();

        // Fetch lightweight metadata for each hit so the caller gets useful
        // context (from/subject/date) without a second round trip.
        let mut messages = Vec::with_capacity(ids.len());
        for id in ids {
            let meta_req = http
                .get(format!("{GMAIL_MESSAGES_URL}/{id}"))
                .bearer_auth(&token)
                .query(&[
                    ("format", "metadata"),
                    ("metadataHeaders", "From"),
                    ("metadataHeaders", "Subject"),
                    ("metadataHeaders", "Date"),
                ]);
            match send_with_retry(meta_req, &self.retry_policy()).await {
                Ok(o) if o.status.is_success() => {
                    let m: Value = serde_json::from_str(&o.body).unwrap_or(Value::Null);
                    messages.push(summarize_message(&m));
                }
                // Skip individual failures rather than failing the whole search.
                _ => continue,
            }
        }

        InvokeResponse {
            tool_id: "gmail.search".into(),
            success: true,
            result: Some(json!({ "messages": messages, "next_page_token": next })),
            error: None,
            duration_ms: 0,
        }
    }
}

/// Summarize a metadata-format message into id/thread/snippet/from/subject/date.
fn summarize_message(msg: &Value) -> Value {
    let payload = msg.get("payload").cloned().unwrap_or(Value::Null);
    let headers = extract_headers(&payload);
    json!({
        "id": msg.get("id").and_then(|v| v.as_str()).unwrap_or(""),
        "thread_id": msg.get("threadId").and_then(|v| v.as_str()).unwrap_or(""),
        "snippet": msg.get("snippet").and_then(|v| v.as_str()).unwrap_or(""),
        "from": headers.get("from").and_then(|v| v.as_str()).unwrap_or(""),
        "subject": headers.get("subject").and_then(|v| v.as_str()).unwrap_or(""),
        "date": headers.get("date").and_then(|v| v.as_str()).unwrap_or(""),
    })
}

// ---------------------------------------------------------------------------
// gmail.list_labels -- enumerate the mailbox's labels.
// ---------------------------------------------------------------------------

/// Gmail API endpoint for listing labels.
const GMAIL_LABELS_URL: &str = "https://gmail.googleapis.com/gmail/v1/users/me/labels";

/// List Gmail labels (system and user) for the authenticated tenant.
pub struct GmailListLabelsTool;

#[async_trait]
impl Tool for GmailListLabelsTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            tool_id: "gmail.list_labels".to_string(),
            name: "List Email Labels".to_string(),
            description: "List Gmail labels (system and user) for the authenticated tenant.".to_string(),
            input_schema: json!({ "type": "object", "properties": {} }),
            output_schema: json!({
                "type": "object",
                "properties": { "labels": { "type": "array" } }
            }),
            category: "email".to_string(),
            requires_auth: true,
        }
    }

    fn provider(&self) -> &'static str {
        PROVIDER
    }

    async fn invoke(&self, ctx: &InvokeContext, req: InvokeRequest) -> InvokeResponse {
        let tenant_id = match req.tenant_id.as_deref().filter(|s| !s.is_empty()) {
            Some(t) => t.to_string(),
            None => return error_response("gmail.list_labels", "bad_request", "tenant_id is required", None),
        };
        let token = match ctx.credd.fetch_token(&tenant_id, PROVIDER).await {
            Ok(t) => t,
            Err(e) => return credd_error_to_response("gmail.list_labels", &e),
        };
        let http = match build_http() {
            Ok(c) => c,
            Err(e) => return error_response("gmail.list_labels", "internal_error", e.to_string(), None),
        };

        let request = http.get(GMAIL_LABELS_URL).bearer_auth(&token);
        let outcome = match send_with_retry(request, &self.retry_policy()).await {
            Ok(o) => o,
            Err(e) => return error_response("gmail.list_labels", "gmail_unreachable", format!("gmail api request failed: {e}"), None),
        };
        if !outcome.status.is_success() {
            warn!(status = %outcome.status, body = %truncate(&outcome.body, 256), "gmail labels error");
            return InvokeResponse {
                tool_id: "gmail.list_labels".into(),
                success: false,
                result: None,
                error: Some(json!({
                    "code": "gmail_api_error",
                    "message": format!("gmail returned HTTP {}", outcome.status.as_u16()),
                    "status": outcome.status.as_u16(),
                    "body": truncate(&outcome.body, 512),
                })),
                duration_ms: 0,
            };
        }
        let parsed: Value = serde_json::from_str(&outcome.body).unwrap_or(Value::Null);
        InvokeResponse {
            tool_id: "gmail.list_labels".into(),
            success: true,
            result: Some(json!({ "labels": map_labels(&parsed) })),
            error: None,
            duration_ms: 0,
        }
    }
}

/// Map a Gmail labels.list response into a compact id/name/type array,
/// carrying through message counts when the API provides them.
fn map_labels(resp: &Value) -> Vec<Value> {
    resp.get("labels")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .map(|l| {
                    let mut obj = serde_json::Map::new();
                    obj.insert("id".into(), Value::String(l.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string()));
                    obj.insert("name".into(), Value::String(l.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string()));
                    obj.insert("type".into(), Value::String(l.get("type").and_then(|v| v.as_str()).unwrap_or("user").to_string()));
                    if let Some(c) = l.get("messagesTotal") {
                        obj.insert("messages_total".into(), c.clone());
                    }
                    if let Some(c) = l.get("messagesUnread") {
                        obj.insert("messages_unread".into(), c.clone());
                    }
                    Value::Object(obj)
                })
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mime_includes_required_headers() {
        let args = GmailArgs {
            to: "alice@example.com".into(),
            subject: "hi".into(),
            body: "hello".into(),
            cc: None,
            bcc: None,
        };
        let mime = build_mime(&args);
        assert!(mime.contains("To: alice@example.com\r\n"));
        assert!(mime.contains("Subject: hi\r\n"));
        assert!(mime.contains("MIME-Version: 1.0"));
        assert!(mime.ends_with("hello"));
    }

    #[test]
    fn base64url_strips_padding() {
        let encoded = URL_SAFE_NO_PAD.encode("abc".as_bytes());
        assert_eq!(encoded, "YWJj");
        assert!(!encoded.contains('='));
        let encoded2 = URL_SAFE_NO_PAD.encode("ab".as_bytes());
        assert!(!encoded2.contains('='));
    }

    #[test]
    fn parse_args_rejects_empty() {
        let v = json!({ "to": "", "subject": "s", "body": "b" });
        assert!(parse_args(&v).is_err());
    }

    #[test]
    fn parse_args_optional_cc_bcc() {
        let v = json!({ "to": "a@b", "subject": "s", "body": "b", "cc": "c@d" });
        let args = parse_args(&v).unwrap();
        assert_eq!(args.cc.as_deref(), Some("c@d"));
        assert!(args.bcc.is_none());
    }

    #[test]
    fn subject_encodes_non_ascii() {
        assert_eq!(encode_subject("hello"), "hello");
        let utf8 = encode_subject("héllo");
        assert!(utf8.starts_with("=?UTF-8?B?"));
        assert!(utf8.ends_with("?="));
    }

    #[test]
    fn decode_b64url_handles_padded_and_unpadded() {
        assert_eq!(decode_b64url("aGVsbG8"), b"hello"); // no padding
        assert_eq!(decode_b64url("aGVsbG8="), b"hello"); // padded
    }

    #[test]
    fn extract_headers_pulls_standard_fields() {
        let payload = json!({
            "headers": [
                {"name": "From", "value": "a@b.com"},
                {"name": "Subject", "value": "hi"},
                {"name": "X-Spam", "value": "ignored"}
            ]
        });
        let h = extract_headers(&payload);
        assert_eq!(h.get("from").and_then(|v| v.as_str()), Some("a@b.com"));
        assert_eq!(h.get("subject").and_then(|v| v.as_str()), Some("hi"));
        assert!(h.get("x-spam").is_none());
    }

    #[test]
    fn extract_body_prefers_plain_then_html() {
        // text/plain present at a nested part.
        let payload = json!({
            "mimeType": "multipart/alternative",
            "parts": [
                {"mimeType": "text/html", "body": {"data": URL_SAFE_NO_PAD.encode("<p>hi</p>")}},
                {"mimeType": "text/plain", "body": {"data": URL_SAFE_NO_PAD.encode("hi plain")}}
            ]
        });
        assert_eq!(extract_body(&payload), "hi plain");

        // Only html available -> falls back.
        let html_only = json!({
            "mimeType": "text/html",
            "body": {"data": URL_SAFE_NO_PAD.encode("<b>x</b>")}
        });
        assert_eq!(extract_body(&html_only), "<b>x</b>");
    }

    #[test]
    fn extract_attachments_finds_files() {
        let payload = json!({
            "parts": [
                {"mimeType": "text/plain", "body": {"data": "x"}},
                {"filename": "doc.pdf", "mimeType": "application/pdf", "body": {"attachmentId": "abc", "size": 1234}}
            ]
        });
        let att = extract_attachments(&payload);
        assert_eq!(att.len(), 1);
        assert_eq!(att[0].get("filename").and_then(|v| v.as_str()), Some("doc.pdf"));
        assert_eq!(att[0].get("size").and_then(|v| v.as_i64()), Some(1234));
    }

    #[test]
    fn parse_message_flattens_shape() {
        let msg = json!({
            "id": "m1",
            "threadId": "t1",
            "snippet": "preview",
            "labelIds": ["INBOX", "UNREAD"],
            "payload": {
                "headers": [{"name": "Subject", "value": "s"}],
                "mimeType": "text/plain",
                "body": {"data": URL_SAFE_NO_PAD.encode("body text")}
            }
        });
        let out = parse_message(&msg);
        assert_eq!(out.get("id").and_then(|v| v.as_str()), Some("m1"));
        assert_eq!(out.get("body").and_then(|v| v.as_str()), Some("body text"));
        assert_eq!(out.get("labels").and_then(|v| v.as_array()).map(|a| a.len()), Some(2));
    }

    #[test]
    fn summarize_message_extracts_meta() {
        let msg = json!({
            "id": "m2",
            "threadId": "t2",
            "snippet": "snip",
            "payload": {"headers": [{"name": "From", "value": "x@y.com"}, {"name": "Subject", "value": "hey"}]}
        });
        let s = summarize_message(&msg);
        assert_eq!(s.get("from").and_then(|v| v.as_str()), Some("x@y.com"));
        assert_eq!(s.get("subject").and_then(|v| v.as_str()), Some("hey"));
    }

    #[test]
    fn map_labels_compacts_and_keeps_counts() {
        let resp = json!({
            "labels": [
                {"id": "INBOX", "name": "INBOX", "type": "system", "messagesTotal": 42, "messagesUnread": 3},
                {"id": "Label_1", "name": "Work", "type": "user"}
            ]
        });
        let labels = map_labels(&resp);
        assert_eq!(labels.len(), 2);
        assert_eq!(labels[0].get("messages_total").and_then(|v| v.as_i64()), Some(42));
        assert!(labels[1].get("messages_total").is_none());
        assert_eq!(labels[1].get("type").and_then(|v| v.as_str()), Some("user"));
    }
}
