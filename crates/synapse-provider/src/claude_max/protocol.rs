//! Wire types for the claude CLI NDJSON stream protocol.

use serde::{Deserialize, Serialize};

// -- Outgoing messages (Synapse -> subprocess stdin) --

/// Top-level outgoing NDJSON message envelope.
#[derive(Debug, Serialize)]
#[serde(tag = "type")]
pub(crate) enum OutgoingMessage {
    /// User turn sent to the subprocess.
    #[serde(rename = "user")]
    User {
        message: UserMessagePayload,
        session_id: String,
        parent_tool_use_id: Option<String>,
    },
    /// Init handshake declaring MCP capabilities.
    #[serde(rename = "initialize")]
    Initialize {
        protocol_version: String,
        capabilities: InitCapabilities,
    },
    /// Response to a control_request from the subprocess.
    #[serde(rename = "control_response")]
    ControlResponse { response: ControlResponsePayload },
}

/// Payload for a user message sent to the subprocess.
#[derive(Debug, Serialize)]
pub(crate) struct UserMessagePayload {
    /// Role of the message sender (always "user").
    pub role: String,
    /// Content as a JSON value (string or array of content blocks).
    pub content: serde_json::Value,
}

/// Capabilities declared during the init handshake.
#[derive(Debug, Serialize)]
pub(crate) struct InitCapabilities {
    /// Whether MCP tool support is enabled.
    pub mcp: bool,
}

/// Payload for a control_response message.
#[derive(Debug, Serialize)]
pub(crate) struct ControlResponsePayload {
    /// Response subtype (e.g. "success").
    pub subtype: String,
    /// Request ID echoed from the control_request.
    pub request_id: String,
    /// Response body (varies by request type).
    pub response: serde_json::Value,
}

// -- Incoming messages (subprocess stdout -> Synapse) --

/// Parsed incoming NDJSON message from the subprocess.
#[derive(Debug)]
pub(crate) enum IncomingMessage {
    /// System event (init, api_retry, etc).
    System(SystemMessage),
    /// Complete assistant response with content blocks.
    Assistant(AssistantData),
    /// Control request requiring a response (tool permission or MCP).
    ControlRequest(ControlRequestData),
    /// Turn result with usage statistics.
    Result(ResultData),
    /// Unrecognized message type (logged and skipped).
    Unknown(serde_json::Value),
}

/// System event from the subprocess.
#[allow(dead_code)]
#[derive(Debug, Deserialize)]
pub(crate) struct SystemMessage {
    /// Event subtype (e.g. "init", "api_retry").
    pub subtype: String,
    /// Session identifier (present on init).
    #[serde(default)]
    pub session_id: Option<String>,
    /// Model identifier (present on init).
    #[serde(default)]
    pub model: Option<String>,
    /// Tool list (present on init, typically empty when --tools "" is used).
    #[serde(default)]
    pub tools: Vec<serde_json::Value>,
}

/// Assistant message containing response content blocks.
#[derive(Debug, Deserialize)]
pub(crate) struct AssistantData {
    /// The assistant message body.
    pub message: AssistantMessage,
}

/// Body of an assistant message.
#[allow(dead_code)]
#[derive(Debug, Deserialize)]
pub(crate) struct AssistantMessage {
    /// Anthropic message ID.
    pub id: String,
    /// Role (always "assistant").
    pub role: String,
    /// Model that generated this response.
    pub model: String,
    /// Content blocks (text, tool_use, thinking).
    pub content: Vec<AssistantContentBlock>,
    /// Reason the model stopped generating.
    pub stop_reason: Option<String>,
    /// Token usage for this message.
    pub usage: Option<MessageUsage>,
}

/// Content block inside an assistant message.
#[allow(dead_code)]
#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
pub(crate) enum AssistantContentBlock {
    /// Text content.
    #[serde(rename = "text")]
    Text { text: String },
    /// Tool invocation (handled via MCP, not surfaced to agent loop).
    #[serde(rename = "tool_use")]
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    /// Thinking content (extended thinking).
    #[serde(rename = "thinking")]
    Thinking { thinking: String },
}

/// Token usage from an assistant message or result.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct MessageUsage {
    /// Number of (uncached) input tokens consumed.
    pub input_tokens: u32,
    /// Number of output tokens generated.
    pub output_tokens: u32,
    /// Tokens read from the prompt cache (discounted read rate).
    #[serde(default)]
    pub cache_read_input_tokens: u32,
    /// Tokens written to the prompt cache (creation premium).
    #[serde(default)]
    pub cache_creation_input_tokens: u32,
}

/// Control request from the subprocess (requires a control_response).
#[derive(Debug, Deserialize)]
pub(crate) struct ControlRequestData {
    /// Unique request ID (must be echoed in the control_response).
    pub request_id: String,
    /// Discriminated payload of the request.
    pub request: ControlRequestPayload,
}

/// Discriminated payload of a control request.
#[allow(dead_code)]
#[derive(Debug, Deserialize)]
#[serde(tag = "subtype")]
pub(crate) enum ControlRequestPayload {
    /// Permission check for a built-in tool (always denied).
    #[serde(rename = "can_use_tool")]
    CanUseTool {
        tool_name: String,
        input: serde_json::Value,
    },
    /// MCP JSONRPC message routed to the in-process bridge.
    #[serde(rename = "mcp_message")]
    McpMessage {
        server_name: String,
        message: serde_json::Value,
    },
}

/// Turn result from the subprocess.
#[allow(dead_code)]
#[derive(Debug, Deserialize)]
pub(crate) struct ResultData {
    /// Result subtype ("success" or "error").
    pub subtype: String,
    /// Result body (varies by subtype).
    #[serde(default)]
    pub result: Option<serde_json::Value>,
    /// Duration of the turn in milliseconds.
    #[serde(default)]
    pub duration_ms: Option<u64>,
    /// Number of turns completed.
    #[serde(default)]
    pub num_turns: Option<u32>,
    /// Aggregate token usage.
    #[serde(default)]
    pub usage: Option<MessageUsage>,
}

/// Parse a raw NDJSON line into a typed incoming message.
/// Unknown message types are returned as IncomingMessage::Unknown rather than errors.
pub(crate) fn parse_incoming(line: &str) -> anyhow::Result<IncomingMessage> {
    let value: serde_json::Value = serde_json::from_str(line)?;
    let msg_type = value.get("type").and_then(|t| t.as_str()).unwrap_or("");

    match msg_type {
        "system" => {
            let msg: SystemMessage = serde_json::from_value(value)?;
            Ok(IncomingMessage::System(msg))
        }
        "assistant" => {
            let msg: AssistantData = serde_json::from_value(value)?;
            Ok(IncomingMessage::Assistant(msg))
        }
        "control_request" => {
            let msg: ControlRequestData = serde_json::from_value(value)?;
            Ok(IncomingMessage::ControlRequest(msg))
        }
        "result" => {
            let msg: ResultData = serde_json::from_value(value)?;
            Ok(IncomingMessage::Result(msg))
        }
        _ => Ok(IncomingMessage::Unknown(value)),
    }
}

#[cfg(test)]
/// Exercises Claude Max protocol serialization and parsing behavior.
mod tests {
    use super::*;
    use serde_json::json;

    /// Verifies user messages serialize into the NDJSON user envelope.
    #[test]
    fn serialize_user_message() {
        let msg = OutgoingMessage::User {
            message: UserMessagePayload {
                role: "user".into(),
                content: serde_json::Value::String("hello".into()),
            },
            session_id: "test-session".into(),
            parent_tool_use_id: None,
        };
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["type"], "user");
        assert_eq!(json["message"]["role"], "user");
        assert_eq!(json["message"]["content"], "hello");
        assert_eq!(json["session_id"], "test-session");
        assert!(json["parent_tool_use_id"].is_null());
    }

    /// Verifies control responses serialize denial payloads in the expected envelope.
    #[test]
    fn serialize_control_response_deny() {
        let msg = OutgoingMessage::ControlResponse {
            response: ControlResponsePayload {
                subtype: "success".into(),
                request_id: "req-123".into(),
                response: json!({
                    "behavior": "deny",
                    "message": "built-in tools disabled"
                }),
            },
        };
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["type"], "control_response");
        assert_eq!(json["response"]["subtype"], "success");
        assert_eq!(json["response"]["request_id"], "req-123");
        assert_eq!(json["response"]["response"]["behavior"], "deny");
    }

    /// Verifies initialize messages advertise MCP capability state.
    #[test]
    fn serialize_initialize() {
        let msg = OutgoingMessage::Initialize {
            protocol_version: "1".into(),
            capabilities: InitCapabilities { mcp: true },
        };
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["type"], "initialize");
        assert_eq!(json["protocol_version"], "1");
        assert_eq!(json["capabilities"]["mcp"], true);
    }

    /// Verifies system init events parse session and model metadata.
    #[test]
    fn parse_system_init() {
        let json = json!({
            "type": "system",
            "subtype": "init",
            "session_id": "sess-abc",
            "model": "claude-sonnet-4-6",
            "tools": []
        });
        let line = serde_json::to_string(&json).unwrap();
        let msg = parse_incoming(&line).unwrap();
        match msg {
            IncomingMessage::System(s) => {
                assert_eq!(s.subtype, "init");
                assert_eq!(s.session_id.unwrap(), "sess-abc");
                assert_eq!(s.model.unwrap(), "claude-sonnet-4-6");
            }
            other => panic!("expected System, got {other:?}"),
        }
    }

    /// Verifies assistant text messages parse into text content blocks.
    #[test]
    fn parse_assistant_text() {
        let json = json!({
            "type": "assistant",
            "message": {
                "id": "msg_001",
                "role": "assistant",
                "model": "claude-sonnet-4-6",
                "content": [
                    {"type": "text", "text": "Hello world"}
                ],
                "stop_reason": "end_turn",
                "usage": {"input_tokens": 10, "output_tokens": 5}
            }
        });
        let line = serde_json::to_string(&json).unwrap();
        let msg = parse_incoming(&line).unwrap();
        match msg {
            IncomingMessage::Assistant(a) => {
                assert_eq!(a.message.id, "msg_001");
                assert_eq!(a.message.content.len(), 1);
                match &a.message.content[0] {
                    AssistantContentBlock::Text { text } => assert_eq!(text, "Hello world"),
                    other => panic!("expected Text, got {other:?}"),
                }
            }
            other => panic!("expected Assistant, got {other:?}"),
        }
    }

    /// Verifies built-in tool permission requests parse as denied-tool checks.
    #[test]
    fn parse_control_request_can_use_tool() {
        let json = json!({
            "type": "control_request",
            "request_id": "req-456",
            "request": {
                "subtype": "can_use_tool",
                "tool_name": "bash",
                "input": {"command": "ls"}
            }
        });
        let line = serde_json::to_string(&json).unwrap();
        let msg = parse_incoming(&line).unwrap();
        match msg {
            IncomingMessage::ControlRequest(cr) => {
                assert_eq!(cr.request_id, "req-456");
                match cr.request {
                    ControlRequestPayload::CanUseTool { tool_name, .. } => {
                        assert_eq!(tool_name, "bash");
                    }
                    other => panic!("expected CanUseTool, got {other:?}"),
                }
            }
            other => panic!("expected ControlRequest, got {other:?}"),
        }
    }

    /// Verifies MCP bridge requests preserve server name and JSONRPC payload.
    #[test]
    fn parse_control_request_mcp_message() {
        let json = json!({
            "type": "control_request",
            "request_id": "req-789",
            "request": {
                "subtype": "mcp_message",
                "server_name": "synapse-tools",
                "message": {
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "tools/list",
                    "params": {}
                }
            }
        });
        let line = serde_json::to_string(&json).unwrap();
        let msg = parse_incoming(&line).unwrap();
        match msg {
            IncomingMessage::ControlRequest(cr) => {
                assert_eq!(cr.request_id, "req-789");
                match cr.request {
                    ControlRequestPayload::McpMessage {
                        server_name,
                        message,
                    } => {
                        assert_eq!(server_name, "synapse-tools");
                        assert_eq!(message["method"], "tools/list");
                    }
                    other => panic!("expected McpMessage, got {other:?}"),
                }
            }
            other => panic!("expected ControlRequest, got {other:?}"),
        }
    }

    /// Verifies successful turn result messages parse usage and duration metadata.
    #[test]
    fn parse_result_success() {
        let json = json!({
            "type": "result",
            "subtype": "success",
            "result": "done",
            "duration_ms": 1234,
            "num_turns": 1,
            "usage": {"input_tokens": 100, "output_tokens": 50}
        });
        let line = serde_json::to_string(&json).unwrap();
        let msg = parse_incoming(&line).unwrap();
        match msg {
            IncomingMessage::Result(r) => {
                assert_eq!(r.subtype, "success");
                assert_eq!(r.duration_ms.unwrap(), 1234);
                assert_eq!(r.usage.as_ref().unwrap().input_tokens, 100);
            }
            other => panic!("expected Result, got {other:?}"),
        }
    }

    /// Verifies future unknown message types are preserved as unknown values.
    #[test]
    fn parse_unknown_type_returns_unknown() {
        let json = json!({
            "type": "some_future_type",
            "data": "whatever"
        });
        let line = serde_json::to_string(&json).unwrap();
        let msg = parse_incoming(&line).unwrap();
        assert!(matches!(msg, IncomingMessage::Unknown(_)));
    }

    /// Verifies explicit cache token fields deserialize into usage metadata.
    #[test]
    fn message_usage_deserializes_cache_fields() {
        let json = r#"{"input_tokens":10,"output_tokens":5,"cache_read_input_tokens":80,"cache_creation_input_tokens":12}"#;
        let usage: MessageUsage = serde_json::from_str(json).unwrap();
        assert_eq!(usage.input_tokens, 10);
        assert_eq!(usage.cache_read_input_tokens, 80);
        assert_eq!(usage.cache_creation_input_tokens, 12);
    }

    /// Verifies omitted cache token fields default to zero.
    #[test]
    fn message_usage_defaults_cache_fields_to_zero() {
        let json = r#"{"input_tokens":10,"output_tokens":5}"#;
        let usage: MessageUsage = serde_json::from_str(json).unwrap();
        assert_eq!(usage.cache_read_input_tokens, 0);
        assert_eq!(usage.cache_creation_input_tokens, 0);
    }
}
