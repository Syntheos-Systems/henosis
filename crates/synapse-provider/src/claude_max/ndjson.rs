//! NDJSON line codec for the claude subprocess protocol.

use crate::claude_max::protocol::{self, IncomingMessage, OutgoingMessage};

/// Serialize an outgoing message to a single NDJSON line (with trailing newline).
pub(crate) fn serialize(msg: &OutgoingMessage) -> anyhow::Result<String> {
    let mut json = serde_json::to_string(msg)?;
    json.push('\n');
    Ok(json)
}

/// Parse a raw stdout line into a typed incoming message.
/// Trims whitespace before parsing. Returns an error for empty/blank lines.
pub(crate) fn parse_line(line: &str) -> anyhow::Result<IncomingMessage> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Err(anyhow::anyhow!("empty NDJSON line"));
    }
    protocol::parse_incoming(trimmed)
}

/// Groups `{` functionality.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::claude_max::protocol::{IncomingMessage, OutgoingMessage, UserMessagePayload};
    use serde_json::json;

    /// Handles `serialize_produces_single_line` behavior.
    #[test]
    fn serialize_produces_single_line() {
        let msg = OutgoingMessage::User {
            message: UserMessagePayload {
                role: "user".into(),
                content: json!("test prompt"),
            },
            session_id: "s1".into(),
            parent_tool_use_id: None,
        };
        let line = serialize(&msg).unwrap();
        assert!(
            !line.contains('\n') || line.ends_with('\n') && line.matches('\n').count() == 1,
            "NDJSON line must contain exactly one trailing newline"
        );
        assert!(line.ends_with('\n'), "NDJSON line must end with newline");
    }

    /// Handles `serialize_then_parse_round_trips_user_content` behavior.
    #[test]
    fn serialize_then_parse_round_trips_user_content() {
        let msg = OutgoingMessage::User {
            message: UserMessagePayload {
                role: "user".into(),
                content: json!([{"type": "text", "text": "hello"}]),
            },
            session_id: "s2".into(),
            parent_tool_use_id: None,
        };
        let line = serialize(&msg).unwrap();
        let value: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(value["type"], "user");
        assert_eq!(value["message"]["content"][0]["text"], "hello");
    }

    /// Handles `parse_line_handles_trailing_whitespace` behavior.
    #[test]
    fn parse_line_handles_trailing_whitespace() {
        let raw = r#"{"type":"system","subtype":"init","session_id":"s1","model":"claude-sonnet-4-6","tools":[]}"#;
        let padded = format!("  {}  \n", raw);
        let msg = parse_line(&padded).unwrap();
        assert!(matches!(msg, IncomingMessage::System(_)));
    }

    /// Handles `parse_line_rejects_empty_input` behavior.
    #[test]
    fn parse_line_rejects_empty_input() {
        let result = parse_line("");
        assert!(result.is_err());
    }
}
