//! SSE stream parsing shared between OpenAI-compatible and Anthropic providers.

use anyhow::Result;
use serde::Deserialize;
use serde_json::Value;

use crate::types::{StopReason, StreamEvent, Usage};

// ──────────────────────────────────────────────────────────────────────────────
// OpenAI-compatible SSE format
// ──────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct OaiChunk {
    choices: Option<Vec<OaiChoice>>,
    usage: Option<OaiUsage>,
}

/// Deserializes one OpenAI streaming choice.
#[derive(Debug, Deserialize)]
struct OaiChoice {
    delta: Option<OaiDelta>,
    finish_reason: Option<String>,
}

/// Deserializes incremental text and tool-call content.
#[derive(Debug, Deserialize)]
struct OaiDelta {
    content: Option<String>,
    tool_calls: Option<Vec<OaiToolCallDelta>>,
}

/// Deserializes one incremental OpenAI tool call.
#[derive(Debug, Deserialize)]
struct OaiToolCallDelta {
    id: Option<String>,
    function: Option<OaiFunction>,
}

/// Deserializes incremental function name and arguments.
#[derive(Debug, Deserialize)]
struct OaiFunction {
    name: Option<String>,
    arguments: Option<String>,
}

/// Deserializes token usage carried by an OpenAI stream.
#[derive(Debug, Deserialize)]
struct OaiUsage {
    prompt_tokens: Option<u32>,
    completion_tokens: Option<u32>,
    #[serde(default)]
    prompt_tokens_details: Option<OaiPromptDetails>,
}

/// Prompt-token detail breakdown from an OpenAI-compatible `usage` object.
#[derive(Debug, Default, Deserialize)]
struct OaiPromptDetails {
    /// Tokens served from the provider's automatic prompt cache.
    #[serde(default)]
    cached_tokens: u32,
}

/// Parse a single OpenAI-compatible SSE `data:` payload into zero or more events.
pub fn parse_openai_sse(data: &str) -> Vec<StreamEvent> {
    if data.trim() == "[DONE]" {
        return vec![]; // Stream terminator, not a stop reason. Actual stop comes from finish_reason.
    }

    let chunk: OaiChunk = match serde_json::from_str(data) {
        Ok(c) => c,
        Err(e) => return vec![StreamEvent::Error(format!("parse_openai_sse: {e}: {data}"))],
    };

    let mut events = Vec::new();

    if let Some(usage) = chunk.usage {
        let cache_read = usage
            .prompt_tokens_details
            .as_ref()
            .map(|d| d.cached_tokens)
            .unwrap_or(0);
        events.push(StreamEvent::Usage(Usage {
            input_tokens: usage.prompt_tokens.unwrap_or(0),
            output_tokens: usage.completion_tokens.unwrap_or(0),
            cache_read_tokens: cache_read,
            cache_write_tokens: 0,
        }));
    }

    if let Some(choices) = chunk.choices {
        for choice in choices {
            // finish_reason wins
            if let Some(ref reason) = choice.finish_reason {
                let stop = match reason.as_str() {
                    "tool_calls" => StopReason::ToolUse,
                    "length" => StopReason::MaxTokens,
                    _ => StopReason::EndTurn,
                };
                events.push(StreamEvent::MessageStop(stop));
                continue;
            }

            if let Some(delta) = choice.delta {
                // Text content
                if let Some(text) = delta.content
                    && !text.is_empty()
                {
                    events.push(StreamEvent::ContentDelta(text));
                }

                // Tool calls
                if let Some(tool_calls) = delta.tool_calls {
                    for tc in tool_calls {
                        if let Some(func) = tc.function {
                            if let Some(id) = &tc.id
                                && let Some(name) = &func.name
                                && !name.is_empty()
                            {
                                events.push(StreamEvent::ToolUseStart {
                                    id: id.clone(),
                                    name: name.clone(),
                                });
                            }
                            if let Some(args) = func.arguments
                                && !args.is_empty()
                            {
                                events.push(StreamEvent::ToolUseInputDelta(args));
                            }
                        }
                    }
                }
            }
        }
    }

    events
}

// ──────────────────────────────────────────────────────────────────────────────
// Anthropic SSE format
// ──────────────────────────────────────────────────────────────────────────────

/// Parse a single Anthropic SSE event (event_type + data payload).
///
/// Anthropic sends:  `event: <type>\ndata: <json>\n\n`
/// eventsource-stream gives us the event type and the data separately.
pub fn parse_anthropic_sse(event_type: &str, data: &str) -> Vec<StreamEvent> {
    match event_type {
        "content_block_delta" => parse_anthropic_content_block_delta(data),
        "content_block_start" => parse_anthropic_content_block_start(data),
        "message_delta" => parse_anthropic_message_delta(data),
        "message_stop" => vec![], // handled by message_delta's stop_reason
        "ping" => vec![],
        "error" => {
            let msg = serde_json::from_str::<Value>(data)
                .ok()
                .and_then(|v| v["error"]["message"].as_str().map(str::to_owned))
                .unwrap_or_else(|| data.to_owned());
            vec![StreamEvent::Error(msg)]
        }
        _ => vec![],
    }
}

/// Parses an Anthropic content-block start event.
fn parse_anthropic_content_block_start(data: &str) -> Vec<StreamEvent> {
    let v: Value = match serde_json::from_str(data) {
        Ok(v) => v,
        Err(_) => return vec![],
    };
    let block = &v["content_block"];
    if block["type"].as_str() == Some("tool_use") {
        let id = block["id"].as_str().unwrap_or("").to_owned();
        let name = block["name"].as_str().unwrap_or("").to_owned();
        return vec![StreamEvent::ToolUseStart { id, name }];
    }
    vec![]
}

/// Parses incremental Anthropic text or tool arguments.
fn parse_anthropic_content_block_delta(data: &str) -> Vec<StreamEvent> {
    let v: Value = match serde_json::from_str(data) {
        Ok(v) => v,
        Err(_) => return vec![],
    };
    let delta = &v["delta"];
    match delta["type"].as_str() {
        Some("text_delta") => {
            let text = delta["text"].as_str().unwrap_or("").to_owned();
            if text.is_empty() {
                vec![]
            } else {
                vec![StreamEvent::ContentDelta(text)]
            }
        }
        Some("input_json_delta") => {
            let partial = delta["partial_json"].as_str().unwrap_or("").to_owned();
            if partial.is_empty() {
                vec![]
            } else {
                vec![StreamEvent::ToolUseInputDelta(partial)]
            }
        }
        _ => vec![],
    }
}

/// Parses Anthropic usage and stop-reason message deltas.
fn parse_anthropic_message_delta(data: &str) -> Vec<StreamEvent> {
    let v: Value = match serde_json::from_str(data) {
        Ok(v) => v,
        Err(_) => return vec![],
    };

    let mut events = Vec::new();

    // Usage
    if let Some(usage) = v.get("usage") {
        let uncached = usage["input_tokens"].as_u64().unwrap_or(0) as u32;
        let output = usage["output_tokens"].as_u64().unwrap_or(0) as u32;
        let cache_read = usage["cache_read_input_tokens"].as_u64().unwrap_or(0) as u32;
        let cache_write = usage["cache_creation_input_tokens"].as_u64().unwrap_or(0) as u32;
        events.push(StreamEvent::Usage(Usage {
            input_tokens: uncached + cache_read + cache_write,
            output_tokens: output,
            cache_read_tokens: cache_read,
            cache_write_tokens: cache_write,
        }));
    }

    // Stop reason
    if let Some(delta) = v.get("delta")
        && let Some(reason) = delta["stop_reason"].as_str()
    {
        let stop = match reason {
            "tool_use" => StopReason::ToolUse,
            "max_tokens" => StopReason::MaxTokens,
            "stop_sequence" => StopReason::StopSequence,
            _ => StopReason::EndTurn,
        };
        events.push(StreamEvent::MessageStop(stop));
    }

    events
}

// ──────────────────────────────────────────────────────────────────────────────
// Helper to collect a stream into a ChatResponse (non-streaming fallback path)
// ──────────────────────────────────────────────────────────────────────────────

use crate::types::{ChatResponse, ContentBlock};

/// Drain stream events into a `ChatResponse`. Used by non-streaming callers
/// that want to fall back to streaming internally.
pub fn events_to_response(events: Vec<StreamEvent>, id: &str) -> Result<ChatResponse> {
    let mut text = String::new();
    let mut stop_reason = StopReason::EndTurn;
    let mut usage = Usage::default();

    // Track current tool accumulation
    let mut tool_id = String::new();
    let mut tool_name = String::new();
    let mut tool_args = String::new();
    let mut content: Vec<ContentBlock> = Vec::new();

    for event in events {
        match event {
            StreamEvent::ContentDelta(t) => text.push_str(&t),
            StreamEvent::ToolUseStart { id, name } => {
                // Flush previous text if any
                if !text.is_empty() {
                    content.push(ContentBlock::Text {
                        text: std::mem::take(&mut text),
                    });
                }
                // Flush previous tool if any
                if !tool_id.is_empty() {
                    let input: serde_json::Value =
                        serde_json::from_str(&tool_args).unwrap_or(serde_json::Value::Null);
                    content.push(ContentBlock::ToolUse {
                        id: std::mem::take(&mut tool_id),
                        name: std::mem::take(&mut tool_name),
                        input,
                    });
                    tool_args.clear();
                }
                tool_id = id;
                tool_name = name;
            }
            StreamEvent::ToolUseInputDelta(partial) => {
                tool_args.push_str(&partial);
            }
            StreamEvent::MessageStop(r) => stop_reason = r,
            StreamEvent::Usage(u) => usage = u,
            StreamEvent::Error(e) => {
                return Err(anyhow::anyhow!("stream error: {e}"));
            }
        }
    }

    // Flush remaining
    if !text.is_empty() {
        content.push(ContentBlock::Text { text });
    }
    if !tool_id.is_empty() {
        let input: serde_json::Value =
            serde_json::from_str(&tool_args).unwrap_or(serde_json::Value::Null);
        content.push(ContentBlock::ToolUse {
            id: tool_id,
            name: tool_name,
            input,
        });
    }

    Ok(ChatResponse {
        id: id.to_owned(),
        content,
        stop_reason,
        usage,
    })
}

/// Tests OpenAI and Anthropic stream parsing and assembly.
#[cfg(test)]
mod tests {
    use super::*;

    /// Verifies the OpenAI terminator does not emit an event.
    #[test]
    fn openai_done_returns_empty() {
        // [DONE] is a stream terminator, not a stop reason.
        // The actual stop comes from finish_reason in a prior chunk.
        let events = parse_openai_sse("[DONE]");
        assert!(events.is_empty());
    }

    /// Verifies an OpenAI text delta becomes a content event.
    #[test]
    fn openai_content_delta() {
        let data = r#"{"choices":[{"delta":{"content":"hello"},"finish_reason":null}]}"#;
        let events = parse_openai_sse(data);
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], StreamEvent::ContentDelta(t) if t == "hello"));
    }

    /// Verifies an OpenAI function delta starts a tool-use event.
    #[test]
    fn openai_tool_call_start() {
        let data = r#"{"choices":[{"delta":{"tool_calls":[{"id":"call_1","function":{"name":"bash","arguments":""}}]},"finish_reason":null}]}"#;
        let events = parse_openai_sse(data);
        assert!(
            events
                .iter()
                .any(|e| matches!(e, StreamEvent::ToolUseStart { name, .. } if name == "bash"))
        );
    }

    /// Verifies the tool-calls finish reason maps to tool use.
    #[test]
    fn openai_finish_reason_tool_calls() {
        let data = r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#;
        let events = parse_openai_sse(data);
        assert!(matches!(
            &events[0],
            StreamEvent::MessageStop(StopReason::ToolUse)
        ));
    }

    /// Verifies an Anthropic text delta becomes a content event.
    #[test]
    fn anthropic_text_delta() {
        let data = r#"{"delta":{"type":"text_delta","text":"hi"}}"#;
        let events = parse_anthropic_sse("content_block_delta", data);
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], StreamEvent::ContentDelta(t) if t == "hi"));
    }

    /// Verifies an Anthropic tool block starts a tool-use event.
    #[test]
    fn anthropic_tool_use_start() {
        let data = r#"{"content_block":{"type":"tool_use","id":"t1","name":"read"}}"#;
        let events = parse_anthropic_sse("content_block_start", data);
        assert!(matches!(&events[0], StreamEvent::ToolUseStart { name, .. } if name == "read"));
    }

    /// Verifies Anthropic stop and usage data become stream events.
    #[test]
    fn anthropic_message_delta_stop_reason() {
        let data = r#"{"delta":{"stop_reason":"end_turn"},"usage":{"input_tokens":100,"output_tokens":50}}"#;
        let events = parse_anthropic_sse("message_delta", data);
        assert!(
            events
                .iter()
                .any(|e| matches!(e, StreamEvent::MessageStop(StopReason::EndTurn)))
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, StreamEvent::Usage(u) if u.input_tokens == 100))
        );
    }

    /// Verifies text stream events assemble into one response.
    #[test]
    fn events_to_response_assembles_text() {
        let events = vec![
            StreamEvent::ContentDelta("hello ".to_string()),
            StreamEvent::ContentDelta("world".to_string()),
            StreamEvent::MessageStop(StopReason::EndTurn),
            StreamEvent::Usage(Usage {
                input_tokens: 10,
                output_tokens: 5,
                ..Default::default()
            }),
        ];
        let resp = events_to_response(events, "test-id").unwrap();
        assert_eq!(resp.id, "test-id");
        assert_eq!(resp.content.len(), 1);
        assert!(matches!(&resp.content[0], ContentBlock::Text { text } if text == "hello world"));
        assert_eq!(resp.stop_reason, StopReason::EndTurn);
        assert_eq!(resp.usage.input_tokens, 10);
    }

    /// Verifies OpenAI cached-token usage survives SSE parsing.
    #[test]
    fn openai_sse_usage_captures_cached_tokens() {
        let data = r#"{"choices":[],"usage":{"prompt_tokens":100,"completion_tokens":20,"prompt_tokens_details":{"cached_tokens":80}}}"#;
        let events = parse_openai_sse(data);
        let usage = events
            .iter()
            .find_map(|e| match e {
                StreamEvent::Usage(u) => Some(u),
                _ => None,
            })
            .expect("usage event present");
        assert_eq!(usage.input_tokens, 100);
        assert_eq!(usage.output_tokens, 20);
        assert_eq!(usage.cache_read_tokens, 80);
        assert_eq!(usage.cache_write_tokens, 0);
    }

    /// Verifies missing OpenAI cache details produce zero cache usage.
    #[test]
    fn openai_sse_usage_without_cache_is_zero() {
        let data = r#"{"choices":[],"usage":{"prompt_tokens":100,"completion_tokens":20}}"#;
        let events = parse_openai_sse(data);
        let usage = events
            .iter()
            .find_map(|e| match e {
                StreamEvent::Usage(u) => Some(u),
                _ => None,
            })
            .expect("usage event present");
        assert_eq!(usage.cache_read_tokens, 0);
    }

    /// Verifies tool stream events assemble into one tool-use block.
    #[test]
    fn events_to_response_assembles_tool_use() {
        let events = vec![
            StreamEvent::ToolUseStart {
                id: "t1".into(),
                name: "bash".into(),
            },
            StreamEvent::ToolUseInputDelta(r#"{"command":"ls"}"#.to_string()),
            StreamEvent::MessageStop(StopReason::ToolUse),
        ];
        let resp = events_to_response(events, "test-id").unwrap();
        assert_eq!(resp.content.len(), 1);
        assert!(matches!(&resp.content[0], ContentBlock::ToolUse { name, .. } if name == "bash"));
    }

    /// Verifies Anthropic cache usage contributes to normalized input tokens.
    #[test]
    fn anthropic_message_delta_normalizes_total_and_captures_cache() {
        let data = r#"{"usage":{"input_tokens":10,"output_tokens":5,"cache_read_input_tokens":80,"cache_creation_input_tokens":12}}"#;
        let events = parse_anthropic_message_delta(data);
        let usage = events
            .iter()
            .find_map(|e| match e {
                StreamEvent::Usage(u) => Some(u),
                _ => None,
            })
            .expect("usage event present");
        assert_eq!(usage.input_tokens, 102);
        assert_eq!(usage.cache_read_tokens, 80);
        assert_eq!(usage.cache_write_tokens, 12);
    }
}
