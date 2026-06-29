//! Slack adapter: send_message via chat.postMessage.
//!
//! Slack returns HTTP 200 even on logical errors; the adapter inspects the
//! JSON `ok` field and surfaces the `error` string when `ok` is false.

use async_trait::async_trait;
use serde_json::{json, Value};
use tracing::warn;

use crate::adapters::common::{build_http, credd_error_to_response, send_with_retry, truncate};
use crate::tool::{
    err, error_response, InvokeContext, InvokeRequest, InvokeResponse, Tool, ToolSchema,
};

/// Tool ID for the send_message adapter.
const TOOL_ID: &str = "slack.send_message";
/// credd provider tag for Slack.
const PROVIDER: &str = "slack";
/// Slack chat.postMessage API endpoint.
const CHAT_POST_URL: &str = "https://slack.com/api/chat.postMessage";

/// Send a message to a Slack channel or thread.
pub struct SlackSendMessageTool;

#[async_trait]
impl Tool for SlackSendMessageTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            tool_id: TOOL_ID.to_string(),
            name: "Send Slack Message".to_string(),
            description: "Send a message to a Slack channel or thread.".to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["channel", "text"],
                "properties": {
                    "channel": { "type": "string", "description": "Channel ID or name (e.g. #general)" },
                    "text": { "type": "string", "description": "Message text (Slack markdown supported)" },
                    "thread_ts": { "type": "string", "description": "Thread timestamp to reply to" },
                    "blocks": {
                        "type": "array",
                        "description": "Block Kit payload (overrides text rendering)"
                    }
                }
            }),
            output_schema: json!({
                "type": "object",
                "properties": {
                    "ts": { "type": "string" },
                    "channel": { "type": "string" }
                }
            }),
            category: "messaging".to_string(),
            requires_auth: true,
        }
    }

    fn provider(&self) -> &'static str {
        PROVIDER
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

        let http = match build_http() {
            Ok(c) => c,
            Err(e) => return error_response(TOOL_ID, "internal_error", e.to_string(), None),
        };

        let mut payload = json!({
            "channel": args.channel,
            "text": args.text,
        });
        if let Some(t) = &args.thread_ts {
            payload["thread_ts"] = Value::String(t.clone());
        }
        if let Some(b) = &args.blocks {
            payload["blocks"] = b.clone();
        }

        let request = http
            .post(CHAT_POST_URL)
            .bearer_auth(&token)
            .header("Content-Type", "application/json; charset=utf-8")
            .json(&payload);

        let outcome = match send_with_retry(request, &self.retry_policy()).await {
            Ok(o) => o,
            Err(e) => {
                return InvokeResponse {
                    tool_id: TOOL_ID.into(),
                    success: false,
                    result: None,
                    error: Some(err(
                        "slack_unreachable",
                        format!("slack api request failed: {e}"),
                        None,
                    )),
                    duration_ms: 0,
                }
            }
        };

        let status = outcome.status;
        let body_text = outcome.body;

        // Slack returns HTTP 200 even on logical errors. Inspect the JSON
        // `ok` field and surface `error` (e.g. `channel_not_found`).
        if !status.is_success() {
            warn!(status = %status, body = %truncate(&body_text, 256), "slack http error");
            return InvokeResponse {
                tool_id: TOOL_ID.into(),
                success: false,
                result: None,
                error: Some(json!({
                    "code": "slack_http_error",
                    "message": format!("slack returned HTTP {}", status.as_u16()),
                    "status": status.as_u16(),
                    "body": truncate(&body_text, 512),
                })),
                duration_ms: 0,
            };
        }

        let parsed: Value = serde_json::from_str(&body_text).unwrap_or(Value::Null);
        let ok = parsed.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
        if !ok {
            let slack_err = parsed
                .get("error")
                .and_then(|v| v.as_str())
                .unwrap_or("slack_unknown_error")
                .to_string();
            return InvokeResponse {
                tool_id: TOOL_ID.into(),
                success: false,
                result: None,
                error: Some(json!({
                    "code": "slack_api_error",
                    "message": format!("slack returned ok=false: {slack_err}"),
                    "slack_error": slack_err,
                })),
                duration_ms: 0,
            };
        }

        let ts = parsed
            .get("ts")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let channel = parsed
            .get("channel")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        InvokeResponse {
            tool_id: TOOL_ID.into(),
            success: true,
            result: Some(json!({
                "ts": ts,
                "channel": channel,
            })),
            error: None,
            duration_ms: 0,
        }
    }
}

/// Parsed arguments for `slack.send_message`.
struct SlackArgs {
    /// Target channel ID or name.
    channel: String,
    /// Message text.
    text: String,
    /// Optional thread timestamp for replies.
    thread_ts: Option<String>,
    /// Optional Block Kit blocks array.
    blocks: Option<Value>,
}

/// Parse and validate `slack.send_message` arguments.
fn parse_args(args: &Value) -> Result<SlackArgs, String> {
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
    let blocks = obj
        .get("blocks")
        .filter(|v| v.is_array())
        .cloned();
    Ok(SlackArgs {
        channel: pull_required("channel")?,
        text: pull_required("text")?,
        thread_ts: pull_optional("thread_ts"),
        blocks,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_minimal() {
        let v = json!({"channel":"#general","text":"hi"});
        let a = parse_args(&v).unwrap();
        assert_eq!(a.channel, "#general");
        assert!(a.thread_ts.is_none());
    }

    #[test]
    fn parse_with_thread_and_blocks() {
        let v = json!({
            "channel": "C123",
            "text": "fallback",
            "thread_ts": "1700000000.000100",
            "blocks": [{"type":"section","text":{"type":"mrkdwn","text":"hi"}}]
        });
        let a = parse_args(&v).unwrap();
        assert!(a.thread_ts.is_some());
        assert!(a.blocks.is_some());
    }

    #[test]
    fn parse_rejects_missing_text() {
        let v = json!({"channel":"#general"});
        assert!(parse_args(&v).is_err());
    }

    #[test]
    fn parse_rejects_empty_channel() {
        let v = json!({"channel":"","text":"hi"});
        assert!(parse_args(&v).is_err());
    }
}
