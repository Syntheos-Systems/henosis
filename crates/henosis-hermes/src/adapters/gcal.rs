//! Google Calendar adapters: list_events, create_event, update_event, delete_event.
//!
//! All tools authenticate via Google OAuth tokens resolved from phylaxd under
//! the `google` provider tag.

use async_trait::async_trait;
use serde_json::{json, Value};
use tracing::warn;

use crate::adapters::common::{build_http, phylaxd_error_to_response, send_with_retry, truncate};
use crate::tool::{
    err, error_response, InvokeContext, InvokeRequest, InvokeResponse, Tool, ToolSchema,
};

/// Tool ID for the list_events adapter.
const TOOL_ID: &str = "gcal.list_events";
/// phylaxd provider tag for all Google Calendar tools.
const PROVIDER: &str = "google";
/// Google Calendar v3 calendars endpoint base URL.
const CALENDAR_BASE: &str = "https://www.googleapis.com/calendar/v3/calendars";

/// List Google Calendar events in a window for the authenticated tenant.
pub struct GCalListEventsTool;

#[async_trait]
/// Implements the Hermes tool contract for GCalListEventsTool.
impl Tool for GCalListEventsTool {
    /// Returns the public schema for this tool.
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            tool_id: TOOL_ID.to_string(),
            name: "List Calendar Events".to_string(),
            description: "List Google Calendar events in a window for the authenticated tenant."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "calendar_id": {
                        "type": "string",
                        "description": "Calendar ID (default 'primary')"
                    },
                    "time_min": {
                        "type": "string",
                        "description": "Lower bound (RFC3339 timestamp). Defaults to now."
                    },
                    "time_max": {
                        "type": "string",
                        "description": "Upper bound (RFC3339 timestamp)"
                    },
                    "max_results": {
                        "type": "integer",
                        "description": "Maximum number of events to return (default 25, max 2500)"
                    },
                    "query": {
                        "type": "string",
                        "description": "Free text search across summary/description/location"
                    }
                }
            }),
            output_schema: json!({
                "type": "object",
                "properties": {
                    "events": { "type": "array" },
                    "next_page_token": { "type": "string" },
                    "summary": { "type": "string" }
                }
            }),
            category: "calendar".to_string(),
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

        let calendar_id = args.calendar_id.as_deref().unwrap_or("primary");
        let url = format!("{}/{}/events", CALENDAR_BASE, urlencode_path(calendar_id));
        let max_results = args.max_results.unwrap_or(25).clamp(1, 2500);

        let mut request = http.get(&url).bearer_auth(&token).query(&[
            ("maxResults", max_results.to_string()),
            ("singleEvents", "true".into()),
            ("orderBy", "startTime".into()),
        ]);
        if let Some(t) = &args.time_min {
            request = request.query(&[("timeMin", t.clone())]);
        }
        if let Some(t) = &args.time_max {
            request = request.query(&[("timeMax", t.clone())]);
        }
        if let Some(q) = &args.query {
            request = request.query(&[("q", q.clone())]);
        }

        let outcome = match send_with_retry(request, &self.retry_policy()).await {
            Ok(o) => o,
            Err(e) => {
                return InvokeResponse {
                    tool_id: TOOL_ID.into(),
                    success: false,
                    result: None,
                    error: Some(err(
                        "gcal_unreachable",
                        format!("calendar api request failed: {e}"),
                        None,
                    )),
                    duration_ms: 0,
                }
            }
        };

        let status = outcome.status;
        let body_text = outcome.body;

        if !status.is_success() {
            warn!(status = %status, body = %truncate(&body_text, 256), "calendar api error");
            return InvokeResponse {
                tool_id: TOOL_ID.into(),
                success: false,
                result: None,
                error: Some(json!({
                    "code": "gcal_api_error",
                    "message": format!("calendar returned HTTP {}", status.as_u16()),
                    "status": status.as_u16(),
                    "body": truncate(&body_text, 512),
                })),
                duration_ms: 0,
            };
        }

        let parsed: Value = serde_json::from_str(&body_text).unwrap_or(Value::Null);
        let events = parsed
            .get("items")
            .cloned()
            .unwrap_or_else(|| Value::Array(vec![]));
        let next = parsed
            .get("nextPageToken")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let summary = parsed
            .get("summary")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        InvokeResponse {
            tool_id: TOOL_ID.into(),
            success: true,
            result: Some(json!({
                "events": events,
                "next_page_token": next,
                "summary": summary,
            })),
            error: None,
            duration_ms: 0,
        }
    }
}

/// Parsed arguments for `gcal.list_events`.
#[derive(Debug, Default)]
struct GCalArgs {
    /// Calendar to query; defaults to "primary".
    calendar_id: Option<String>,
    /// RFC3339 lower bound for the event window.
    time_min: Option<String>,
    /// RFC3339 upper bound for the event window.
    time_max: Option<String>,
    /// Maximum number of events to return.
    max_results: Option<u32>,
    /// Free-text search filter.
    query: Option<String>,
}

/// Parse and validate `gcal.list_events` arguments.
fn parse_args(args: &Value) -> Result<GCalArgs, String> {
    let obj = match args {
        Value::Null => return Ok(GCalArgs::default()),
        Value::Object(m) => m,
        _ => return Err("args must be a JSON object".to_string()),
    };
    let pull_str = |k: &str| -> Option<String> {
        obj.get(k)
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    };
    let max_results = obj
        .get("max_results")
        .and_then(|v| v.as_u64())
        .map(|n| n as u32);
    Ok(GCalArgs {
        calendar_id: pull_str("calendar_id"),
        time_min: pull_str("time_min"),
        time_max: pull_str("time_max"),
        max_results,
        query: pull_str("query"),
    })
}

// ---------------------------------------------------------------------------
// gcal.create_event -- create a calendar event.
// ---------------------------------------------------------------------------

/// Create a Google Calendar event for the authenticated tenant.
pub struct GCalCreateEventTool;

#[async_trait]
/// Implements the Hermes tool contract for GCalCreateEventTool.
impl Tool for GCalCreateEventTool {
    /// Returns the public schema for this tool.
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            tool_id: "gcal.create_event".to_string(),
            name: "Create Calendar Event".to_string(),
            description: "Create a Google Calendar event for the authenticated tenant.".to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["summary", "start", "end"],
                "properties": {
                    "summary": { "type": "string", "description": "Event title" },
                    "start": { "type": "string", "description": "Start (RFC3339 datetime or YYYY-MM-DD for all-day)" },
                    "end": { "type": "string", "description": "End (RFC3339 datetime or YYYY-MM-DD)" },
                    "calendar_id": { "type": "string", "description": "Calendar id (default 'primary')" },
                    "description": { "type": "string" },
                    "location": { "type": "string" },
                    "attendees": { "type": "array", "items": { "type": "string" }, "description": "Attendee email addresses" },
                    "reminders": { "type": "object", "description": "Google Calendar reminders object" }
                }
            }),
            output_schema: json!({
                "type": "object",
                "properties": {
                    "id": { "type": "string" },
                    "html_link": { "type": "string" },
                    "status": { "type": "string" }
                }
            }),
            category: "calendar".to_string(),
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
            None => {
                return error_response(
                    "gcal.create_event",
                    "bad_request",
                    "tenant_id is required",
                    None,
                )
            }
        };
        let obj = match req.args.as_object() {
            Some(o) => o,
            None => {
                return error_response(
                    "gcal.create_event",
                    "bad_request",
                    "args must be a JSON object",
                    None,
                )
            }
        };
        let summary = match obj
            .get("summary")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            Some(s) => s,
            None => {
                return error_response(
                    "gcal.create_event",
                    "bad_request",
                    "'summary' is required",
                    None,
                )
            }
        };
        let start = match obj
            .get("start")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            Some(s) => s,
            None => {
                return error_response(
                    "gcal.create_event",
                    "bad_request",
                    "'start' is required",
                    None,
                )
            }
        };
        let end = match obj
            .get("end")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            Some(s) => s,
            None => {
                return error_response(
                    "gcal.create_event",
                    "bad_request",
                    "'end' is required",
                    None,
                )
            }
        };
        let calendar_id = obj
            .get("calendar_id")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .unwrap_or("primary");

        let mut event = json!({
            "summary": summary,
            "start": time_field(start),
            "end": time_field(end),
        });
        if let Some(d) = obj.get("description").and_then(|v| v.as_str()) {
            event["description"] = Value::String(d.to_string());
        }
        if let Some(l) = obj.get("location").and_then(|v| v.as_str()) {
            event["location"] = Value::String(l.to_string());
        }
        if let Some(att) = attendees_field(obj.get("attendees")) {
            event["attendees"] = att;
        }
        if let Some(r) = obj.get("reminders").filter(|v| v.is_object()) {
            event["reminders"] = r.clone();
        }

        let token = match ctx.phylaxd.fetch_token(&tenant_id, PROVIDER).await {
            Ok(t) => t,
            Err(e) => return phylaxd_error_to_response("gcal.create_event", &e),
        };
        let http = match build_http() {
            Ok(c) => c,
            Err(e) => {
                return error_response("gcal.create_event", "internal_error", e.to_string(), None)
            }
        };

        let url = format!("{}/{}/events", CALENDAR_BASE, urlencode_path(calendar_id));
        let request = http.post(&url).bearer_auth(&token).json(&event);
        let outcome = match send_with_retry(request, &self.retry_policy()).await {
            Ok(o) => o,
            Err(e) => {
                return error_response(
                    "gcal.create_event",
                    "gcal_unreachable",
                    format!("calendar api request failed: {e}"),
                    None,
                )
            }
        };
        if !outcome.status.is_success() {
            warn!(status = %outcome.status, body = %truncate(&outcome.body, 256), "calendar create error");
            return cal_error("gcal.create_event", outcome.status.as_u16(), &outcome.body);
        }
        let parsed: Value = serde_json::from_str(&outcome.body).unwrap_or(Value::Null);
        InvokeResponse {
            tool_id: "gcal.create_event".into(),
            success: true,
            result: Some(json!({
                "id": parsed.get("id").and_then(|v| v.as_str()).unwrap_or(""),
                "html_link": parsed.get("htmlLink").and_then(|v| v.as_str()).unwrap_or(""),
                "status": parsed.get("status").and_then(|v| v.as_str()).unwrap_or(""),
            })),
            error: None,
            duration_ms: 0,
        }
    }
}

// ---------------------------------------------------------------------------
// gcal.update_event -- patch an existing event (only provided fields change).
// ---------------------------------------------------------------------------

/// Patch a Google Calendar event; only supplied fields are changed.
pub struct GCalUpdateEventTool;

#[async_trait]
/// Implements the Hermes tool contract for GCalUpdateEventTool.
impl Tool for GCalUpdateEventTool {
    /// Returns the public schema for this tool.
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            tool_id: "gcal.update_event".to_string(),
            name: "Update Calendar Event".to_string(),
            description: "Patch a Google Calendar event; only supplied fields are changed."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["event_id"],
                "properties": {
                    "event_id": { "type": "string", "description": "Event id to update" },
                    "calendar_id": { "type": "string", "description": "Calendar id (default 'primary')" },
                    "summary": { "type": "string" },
                    "start": { "type": "string", "description": "RFC3339 datetime or YYYY-MM-DD" },
                    "end": { "type": "string", "description": "RFC3339 datetime or YYYY-MM-DD" },
                    "description": { "type": "string" },
                    "location": { "type": "string" },
                    "attendees": { "type": "array", "items": { "type": "string" } }
                }
            }),
            output_schema: json!({ "type": "object" }),
            category: "calendar".to_string(),
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
            None => {
                return error_response(
                    "gcal.update_event",
                    "bad_request",
                    "tenant_id is required",
                    None,
                )
            }
        };
        let obj = match req.args.as_object() {
            Some(o) => o,
            None => {
                return error_response(
                    "gcal.update_event",
                    "bad_request",
                    "args must be a JSON object",
                    None,
                )
            }
        };
        let event_id = match obj
            .get("event_id")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            Some(e) => e.to_string(),
            None => {
                return error_response(
                    "gcal.update_event",
                    "bad_request",
                    "'event_id' is required",
                    None,
                )
            }
        };
        let calendar_id = obj
            .get("calendar_id")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .unwrap_or("primary");

        let patch = build_event_patch(obj);
        if patch.as_object().map(|m| m.is_empty()).unwrap_or(true) {
            return error_response(
                "gcal.update_event",
                "bad_request",
                "no updatable fields provided",
                Some("supply at least one of summary/start/end/description/location/attendees"),
            );
        }

        let token = match ctx.phylaxd.fetch_token(&tenant_id, PROVIDER).await {
            Ok(t) => t,
            Err(e) => return phylaxd_error_to_response("gcal.update_event", &e),
        };
        let http = match build_http() {
            Ok(c) => c,
            Err(e) => {
                return error_response("gcal.update_event", "internal_error", e.to_string(), None)
            }
        };

        let url = format!(
            "{}/{}/events/{}",
            CALENDAR_BASE,
            urlencode_path(calendar_id),
            urlencode_path(&event_id)
        );
        let request = http.patch(&url).bearer_auth(&token).json(&patch);
        let outcome = match send_with_retry(request, &self.retry_policy()).await {
            Ok(o) => o,
            Err(e) => {
                return error_response(
                    "gcal.update_event",
                    "gcal_unreachable",
                    format!("calendar api request failed: {e}"),
                    None,
                )
            }
        };
        if !outcome.status.is_success() {
            warn!(status = %outcome.status, body = %truncate(&outcome.body, 256), "calendar update error");
            return cal_error("gcal.update_event", outcome.status.as_u16(), &outcome.body);
        }
        let parsed: Value = serde_json::from_str(&outcome.body).unwrap_or(Value::Null);
        InvokeResponse {
            tool_id: "gcal.update_event".into(),
            success: true,
            result: Some(parsed),
            error: None,
            duration_ms: 0,
        }
    }
}

// ---------------------------------------------------------------------------
// gcal.delete_event -- remove an event.
// ---------------------------------------------------------------------------

/// Delete a Google Calendar event.
pub struct GCalDeleteEventTool;

#[async_trait]
/// Implements the Hermes tool contract for GCalDeleteEventTool.
impl Tool for GCalDeleteEventTool {
    /// Returns the public schema for this tool.
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            tool_id: "gcal.delete_event".to_string(),
            name: "Delete Calendar Event".to_string(),
            description: "Delete a Google Calendar event.".to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["event_id"],
                "properties": {
                    "event_id": { "type": "string", "description": "Event id to delete" },
                    "calendar_id": { "type": "string", "description": "Calendar id (default 'primary')" },
                    "send_updates": { "type": "string", "enum": ["all", "externalOnly", "none"], "description": "Who to notify (default none)" }
                }
            }),
            output_schema: json!({
                "type": "object",
                "properties": { "deleted": { "type": "boolean" }, "event_id": { "type": "string" } }
            }),
            category: "calendar".to_string(),
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
            None => {
                return error_response(
                    "gcal.delete_event",
                    "bad_request",
                    "tenant_id is required",
                    None,
                )
            }
        };
        let obj = match req.args.as_object() {
            Some(o) => o,
            None => {
                return error_response(
                    "gcal.delete_event",
                    "bad_request",
                    "args must be a JSON object",
                    None,
                )
            }
        };
        let event_id = match obj
            .get("event_id")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            Some(e) => e.to_string(),
            None => {
                return error_response(
                    "gcal.delete_event",
                    "bad_request",
                    "'event_id' is required",
                    None,
                )
            }
        };
        let calendar_id = obj
            .get("calendar_id")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .unwrap_or("primary");
        let send_updates = match obj.get("send_updates").and_then(|v| v.as_str()) {
            Some(s) if ["all", "externalOnly", "none"].contains(&s) => s,
            Some(s) => {
                return error_response(
                    "gcal.delete_event",
                    "bad_request",
                    format!("invalid send_updates '{s}'"),
                    None,
                )
            }
            None => "none",
        };

        let token = match ctx.phylaxd.fetch_token(&tenant_id, PROVIDER).await {
            Ok(t) => t,
            Err(e) => return phylaxd_error_to_response("gcal.delete_event", &e),
        };
        let http = match build_http() {
            Ok(c) => c,
            Err(e) => {
                return error_response("gcal.delete_event", "internal_error", e.to_string(), None)
            }
        };

        let url = format!(
            "{}/{}/events/{}",
            CALENDAR_BASE,
            urlencode_path(calendar_id),
            urlencode_path(&event_id)
        );
        let request = http
            .delete(&url)
            .bearer_auth(&token)
            .query(&[("sendUpdates", send_updates)]);
        let outcome = match send_with_retry(request, &self.retry_policy()).await {
            Ok(o) => o,
            Err(e) => {
                return error_response(
                    "gcal.delete_event",
                    "gcal_unreachable",
                    format!("calendar api request failed: {e}"),
                    None,
                )
            }
        };
        // Google returns 204 No Content (or 200) on success; 410 Gone if already deleted.
        if !outcome.status.is_success() {
            warn!(status = %outcome.status, body = %truncate(&outcome.body, 256), "calendar delete error");
            return cal_error("gcal.delete_event", outcome.status.as_u16(), &outcome.body);
        }
        InvokeResponse {
            tool_id: "gcal.delete_event".into(),
            success: true,
            result: Some(json!({ "deleted": true, "event_id": event_id })),
            error: None,
            duration_ms: 0,
        }
    }
}

/// Render a Calendar start/end field: a date-only value (no time component)
/// becomes an all-day `{ "date": ... }`, otherwise `{ "dateTime": ... }`.
fn time_field(value: &str) -> Value {
    if value.contains('T') {
        json!({ "dateTime": value })
    } else {
        json!({ "date": value })
    }
}

/// Convert an attendees string array into Calendar's `[{ "email": ... }]` form.
fn attendees_field(v: Option<&Value>) -> Option<Value> {
    let arr = v.and_then(|v| v.as_array())?;
    let list: Vec<Value> = arr
        .iter()
        .filter_map(|a| a.as_str())
        .filter(|s| !s.is_empty())
        .map(|email| json!({ "email": email }))
        .collect();
    if list.is_empty() {
        None
    } else {
        Some(Value::Array(list))
    }
}

/// Build a patch body for `update_event` containing only the supplied fields.
fn build_event_patch(obj: &serde_json::Map<String, Value>) -> Value {
    let mut patch = serde_json::Map::new();
    if let Some(s) = obj.get("summary").and_then(|v| v.as_str()) {
        patch.insert("summary".into(), Value::String(s.to_string()));
    }
    if let Some(s) = obj
        .get("start")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
    {
        patch.insert("start".into(), time_field(s));
    }
    if let Some(s) = obj
        .get("end")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
    {
        patch.insert("end".into(), time_field(s));
    }
    if let Some(s) = obj.get("description").and_then(|v| v.as_str()) {
        patch.insert("description".into(), Value::String(s.to_string()));
    }
    if let Some(s) = obj.get("location").and_then(|v| v.as_str()) {
        patch.insert("location".into(), Value::String(s.to_string()));
    }
    if let Some(att) = attendees_field(obj.get("attendees")) {
        patch.insert("attendees".into(), att);
    }
    Value::Object(patch)
}

/// Build a standard calendar API error response.
fn cal_error(tool_id: &str, status: u16, body: &str) -> InvokeResponse {
    InvokeResponse {
        tool_id: tool_id.to_string(),
        success: false,
        result: None,
        error: Some(json!({
            "code": "gcal_api_error",
            "message": format!("calendar returned HTTP {status}"),
            "status": status,
            "body": truncate(body, 512),
        })),
        duration_ms: 0,
    }
}

/// Minimal URL path encoder for calendarId. Calendar IDs frequently contain
/// `@` and `:`; we encode the few path-significant characters by hand to
/// avoid pulling in `urlencoding`.
fn urlencode_path(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'@' => out.push_str("%40"),
            b':' => out.push_str("%3A"),
            b'/' => out.push_str("%2F"),
            _ => out.push(b as char),
        }
    }
    out
}

#[cfg(test)]
/// Contains focused unit tests for this module.
mod tests {
    use super::*;

    #[test]
    /// Verifies parse empty.
    fn parse_empty() {
        let a = parse_args(&Value::Null).unwrap();
        assert!(a.calendar_id.is_none());
        assert!(a.max_results.is_none());
    }

    #[test]
    /// Verifies parse full.
    fn parse_full() {
        let v = json!({
            "calendar_id": "user@example.com",
            "time_min": "2026-05-01T00:00:00Z",
            "time_max": "2026-05-31T23:59:59Z",
            "max_results": 10,
            "query": "standup"
        });
        let a = parse_args(&v).unwrap();
        assert_eq!(a.calendar_id.as_deref(), Some("user@example.com"));
        assert_eq!(a.max_results, Some(10));
    }

    #[test]
    /// Verifies url encode handles at and colon.
    fn url_encode_handles_at_and_colon() {
        assert_eq!(urlencode_path("user@example.com"), "user%40example.com");
        assert_eq!(urlencode_path("a/b:c"), "a%2Fb%3Ac");
        assert_eq!(urlencode_path("primary"), "primary");
    }

    #[test]
    /// Verifies time field distinguishes datetime and date.
    fn time_field_distinguishes_datetime_and_date() {
        assert_eq!(
            time_field("2026-06-01T10:00:00Z"),
            json!({"dateTime": "2026-06-01T10:00:00Z"})
        );
        assert_eq!(time_field("2026-06-01"), json!({"date": "2026-06-01"}));
    }

    #[test]
    /// Verifies attendees field maps emails.
    fn attendees_field_maps_emails() {
        let v = json!(["a@b.com", "", "c@d.com"]);
        let out = attendees_field(Some(&v)).unwrap();
        assert_eq!(out, json!([{"email": "a@b.com"}, {"email": "c@d.com"}]));
    }

    #[test]
    /// Verifies attendees field empty is none.
    fn attendees_field_empty_is_none() {
        assert!(attendees_field(Some(&json!([]))).is_none());
        assert!(attendees_field(None).is_none());
    }

    #[test]
    /// Verifies event patch includes only present fields.
    fn event_patch_includes_only_present_fields() {
        let obj = json!({ "event_id": "e1", "summary": "new title", "location": "HQ" });
        let patch = build_event_patch(obj.as_object().unwrap());
        assert_eq!(
            patch.get("summary").and_then(|v| v.as_str()),
            Some("new title")
        );
        assert_eq!(patch.get("location").and_then(|v| v.as_str()), Some("HQ"));
        assert!(patch.get("start").is_none());
        assert!(patch.get("description").is_none());
    }

    #[test]
    /// Verifies event patch empty when no fields.
    fn event_patch_empty_when_no_fields() {
        let obj = json!({ "event_id": "e1" });
        let patch = build_event_patch(obj.as_object().unwrap());
        assert!(patch.as_object().unwrap().is_empty());
    }
}
