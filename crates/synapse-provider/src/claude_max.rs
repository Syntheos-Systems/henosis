//! ClaudeMaxProvider: routes LLM requests through a Claude Max subscription
//! by spawning the `claude` CLI as a persistent subprocess.

mod mcp_bridge;
mod ndjson;
mod protocol;
mod subprocess;

use anyhow::Result;
use async_trait::async_trait;

/// Result of executing a tool through the MCP bridge.
#[derive(Debug, Clone)]
pub struct ToolExecutionResult {
    /// Text output from the tool.
    pub output: String,
    /// Whether the tool execution resulted in an error.
    pub is_error: bool,
}

/// Abstraction over Synapse's ToolRegistry for the MCP bridge.
/// Implemented by the caller (agent loop) to decouple synapse-provider from synapse-tools.
#[async_trait]
pub trait ToolExecutor: Send + Sync {
    /// Returns tool schemas in Anthropic API format (name, description, input_schema).
    fn tool_schemas(&self) -> Vec<serde_json::Value>;
    /// Execute a tool by name with the given input arguments.
    async fn execute_tool(
        &self,
        name: &str,
        input: serde_json::Value,
    ) -> Result<ToolExecutionResult>;
}

use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use anyhow::Context;
use futures::Stream;

use crate::claude_max::mcp_bridge::McpBridge;
use crate::claude_max::protocol::{
    AssistantContentBlock, ControlRequestPayload, ControlResponsePayload, IncomingMessage,
    OutgoingMessage, UserMessagePayload,
};
use crate::claude_max::subprocess::SubprocessState;
use crate::{
    ChatMessage, ChatRequest, ChatResponse, ContentBlock, Role, StopReason, StreamEvent, Usage,
};

/// Default model used when none is specified.
const DEFAULT_MODEL: &str = "claude-sonnet-4-6";

/// Extract user messages that haven't been sent to the subprocess yet.
/// The subprocess maintains its own conversation history, so we only need to
/// send new user messages (the delta since the last send_streaming call).
fn extract_user_deltas(messages: &[ChatMessage], already_sent: usize) -> Vec<&ChatMessage> {
    messages
        .iter()
        .filter(|m| matches!(m.role, Role::User))
        .skip(already_sent)
        .collect()
}

/// Provider that routes LLM requests through a Claude Max subscription
/// by communicating with the `claude` CLI as a persistent subprocess.
///
/// # Concurrency
///
/// This provider is NOT safe for concurrent `send_streaming` calls. The
/// subprocess mutex is held for the entire stream lifetime (including all
/// `.await` points in the event loop). A second concurrent call will block
/// until the first stream is fully consumed or dropped. The agent loop is
/// sequential, so this is acceptable in practice.
///
/// If the subprocess crashes mid-session, the state is cleared and the next
/// call will re-spawn. However, `messages_sent` resets to 0, so the new
/// subprocess has no memory of prior turns.
pub struct ClaudeMaxProvider {
    /// Model identifier passed to the subprocess.
    model: String,
    /// Resolved path to the claude binary.
    cli_path: String,
    /// Cred namespace for the OAuth token.
    cred_namespace: String,
    /// Cred key for the OAuth token.
    cred_key: String,
    /// MCP bridge for tool schema conversion and execution.
    bridge: Arc<McpBridge>,
    /// Subprocess state, lazily initialized on first send_streaming() call.
    state: Arc<tokio::sync::Mutex<Option<SubprocessState>>>,
}

/// Adds inherent behavior for `ClaudeMaxProvider`.
impl ClaudeMaxProvider {
    /// Create a new ClaudeMaxProvider with lazy subprocess initialization.
    pub fn new(
        model: Option<String>,
        cli_path: Option<PathBuf>,
        cred_namespace: Option<String>,
        cred_key: Option<String>,
        tools: Arc<dyn ToolExecutor>,
    ) -> Self {
        let resolved_path = cli_path
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|| {
                which::which("claude")
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_else(|_| "claude".to_string())
            });

        Self {
            model: model.unwrap_or_else(|| DEFAULT_MODEL.to_string()),
            cli_path: resolved_path,
            cred_namespace: cred_namespace.unwrap_or_else(|| "anthropic".to_string()),
            cred_key: cred_key.unwrap_or_else(|| "claude-oauth-token".to_string()),
            bridge: Arc::new(McpBridge::new(tools)),
            state: Arc::new(tokio::sync::Mutex::new(None)),
        }
    }
}

/// Provider trait implementation that drives the subprocess event loop.
#[async_trait]
impl crate::Provider for ClaudeMaxProvider {
    /// Send a non-streaming request by collecting all stream events.
    async fn send(&self, request: &ChatRequest) -> Result<ChatResponse> {
        use futures::StreamExt;

        let mut stream = self.send_streaming(request);
        let mut text = String::new();
        let mut usage = Usage::default();
        let mut stop_reason = StopReason::EndTurn;

        while let Some(event) = stream.next().await {
            match event? {
                StreamEvent::ContentDelta(delta) => text.push_str(&delta),
                StreamEvent::Usage(u) => usage = u,
                StreamEvent::MessageStop(reason) => stop_reason = reason,
                _ => {}
            }
        }

        Ok(ChatResponse {
            id: uuid::Uuid::new_v4().to_string(),
            content: vec![ContentBlock::Text { text }],
            stop_reason,
            usage,
        })
    }

    /// Send a streaming request to the subprocess, returning events as they arrive.
    /// The subprocess mutex is held for the entire stream lifetime -- concurrent
    /// callers will block until this stream is consumed or dropped.
    fn send_streaming(
        &self,
        request: &ChatRequest,
    ) -> Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>> {
        let state = self.state.clone();
        let bridge = self.bridge.clone();
        let cli_path = self.cli_path.clone();
        let model = self.model.clone();
        let cred_namespace = self.cred_namespace.clone();
        let cred_key = self.cred_key.clone();
        let messages = request.messages.clone();
        let system = request.system.clone();

        // Capture self for token fetch (only needed if subprocess not yet spawned).
        let fetch_token = {
            let ns = cred_namespace.clone();
            let key = cred_key.clone();
            move || {
                let ns = ns.clone();
                let key = key.clone();
                async move {
                    let output = tokio::process::Command::new("cred")
                        .args(["get", "--raw", &ns, &key])
                        .output()
                        .await
                        .context("failed to run cred for OAuth token")?;
                    if !output.status.success() {
                        let stderr = String::from_utf8_lossy(&output.stderr);
                        anyhow::bail!("cred get failed: {}", stderr.trim());
                    }
                    let token = String::from_utf8(output.stdout)
                        .context("token not UTF-8")?
                        .trim()
                        .to_string();
                    if token.is_empty() {
                        anyhow::bail!("cred returned empty token");
                    }
                    Ok::<String, anyhow::Error>(token)
                }
            }
        };

        Box::pin(async_stream::stream! {
            let mut guard = state.lock().await;

            // Lazy init: spawn subprocess on first use.
            if guard.is_none() {
                let token = match fetch_token().await {
                    Ok(t) => t,
                    Err(e) => {
                        yield Err(e);
                        return;
                    }
                };
                match SubprocessState::spawn(&cli_path, &model, &token).await {
                    Ok(sub) => *guard = Some(sub),
                    Err(e) => {
                        yield Err(e.context("failed to spawn claude subprocess"));
                        return;
                    }
                }
            }

            let sub = match guard.as_mut() {
                Some(s) => s,
                None => {
                    yield Err(anyhow::anyhow!("subprocess state missing after init"));
                    return;
                }
            };

            // On first message, prepend system prompt as context if present.
            // The subprocess does not have a separate system prompt mechanism;
            // it must be embedded in the first user message content.
            let system_prefix = if sub.messages_sent == 0 {
                system.clone()
            } else {
                None
            };

            // Determine which user messages are new (delta since last call).
            let deltas = extract_user_deltas(&messages, sub.messages_sent);

            if deltas.is_empty() {
                yield Err(anyhow::anyhow!("no new user messages to send"));
                return;
            }

            // Send each delta user message.
            for (i, user_msg) in deltas.iter().enumerate() {
                // Filter content blocks: only send Text blocks to the subprocess.
                // ToolUse/ToolResult blocks belong to the agent loop's context
                // and are handled internally by the subprocess via MCP.
                let content_blocks: Vec<&ContentBlock> = user_msg
                    .content
                    .iter()
                    .filter(|b| matches!(b, ContentBlock::Text { .. }))
                    .collect();

                // Prepend system prompt to the first message if present.
                let content = if let Some(sys) = system_prefix.as_ref().filter(|_| i == 0) {
                    let mut blocks = vec![serde_json::json!({"type": "text", "text": sys})];
                    for b in &content_blocks {
                        match serde_json::to_value(b) {
                            Ok(v) => blocks.push(v),
                            Err(e) => {
                                log::warn!("failed to serialize content block: {e}");
                            }
                        }
                    }
                    serde_json::Value::Array(blocks)
                } else {
                    match serde_json::to_value(&content_blocks) {
                        Ok(v) => v,
                        Err(e) => {
                            yield Err(anyhow::anyhow!("failed to serialize content: {e}"));
                            return;
                        }
                    }
                };

                let outgoing = OutgoingMessage::User {
                    message: UserMessagePayload {
                        role: "user".into(),
                        content,
                    },
                    session_id: sub.session_id.clone(),
                    parent_tool_use_id: None,
                };

                if let Err(e) = sub.write_message(&outgoing).await {
                    // Subprocess is dead -- clear state so next call re-spawns.
                    *guard = None;
                    yield Err(e.context("failed to send user message"));
                    return;
                }
                sub.messages_sent += 1;
            }

            // Read events until we get a result message.
            loop {
                let msg = match sub.read_line().await {
                    Ok(Some(msg)) => msg,
                    Ok(None) => {
                        // Subprocess closed stdout -- clear state so next call re-spawns.
                        *guard = None;
                        yield Err(anyhow::anyhow!("subprocess closed stdout unexpectedly"));
                        return;
                    }
                    Err(e) => {
                        // Read error -- subprocess likely dead, clear for re-spawn.
                        *guard = None;
                        yield Err(e.context("failed to read from subprocess"));
                        return;
                    }
                };

                match msg {
                    IncomingMessage::Assistant(data) => {
                        // Yield text content as ContentDelta events.
                        // ToolUse blocks are NOT yielded -- they are handled
                        // internally via the MCP bridge.
                        for block in &data.message.content {
                            match block {
                                AssistantContentBlock::Text { text } => {
                                    yield Ok(StreamEvent::ContentDelta(text.clone()));
                                }
                                AssistantContentBlock::ToolUse { .. } => {
                                    // Handled via control_request/mcp_message.
                                }
                                AssistantContentBlock::Thinking { thinking } => {
                                    // Surface thinking as content delta for display.
                                    yield Ok(StreamEvent::ContentDelta(thinking.clone()));
                                }
                            }
                        }
                        // Yield usage if present on the assistant message.
                        if let Some(usage) = &data.message.usage {
                            yield Ok(StreamEvent::Usage(Usage {
                                input_tokens: usage.input_tokens
                                    + usage.cache_read_input_tokens
                                    + usage.cache_creation_input_tokens,
                                output_tokens: usage.output_tokens,
                                cache_read_tokens: usage.cache_read_input_tokens,
                                cache_write_tokens: usage.cache_creation_input_tokens,
                            }));
                        }
                    }

                    IncomingMessage::ControlRequest(cr) => {
                        let response = match cr.request {
                            ControlRequestPayload::CanUseTool { tool_name, .. } => {
                                log::debug!("denying built-in tool request: {tool_name}");
                                OutgoingMessage::ControlResponse {
                                    response: ControlResponsePayload {
                                        subtype: "success".into(),
                                        request_id: cr.request_id,
                                        response: serde_json::json!({
                                            "behavior": "deny",
                                            "message": "built-in tools disabled"
                                        }),
                                    },
                                }
                            }
                            ControlRequestPayload::McpMessage {
                                server_name,
                                message,
                            } => {
                                log::debug!("handling MCP message for server: {server_name}");
                                let mcp_response = bridge.handle_jsonrpc(message).await;
                                OutgoingMessage::ControlResponse {
                                    response: ControlResponsePayload {
                                        subtype: "success".into(),
                                        request_id: cr.request_id,
                                        response: mcp_response,
                                    },
                                }
                            }
                        };

                        if let Err(e) = sub.write_message(&response).await {
                            *guard = None;
                            yield Err(e.context("failed to send control response"));
                            return;
                        }
                    }

                    IncomingMessage::Result(result) => {
                        // Yield final usage from the result message.
                        if let Some(usage) = &result.usage {
                            yield Ok(StreamEvent::Usage(Usage {
                                input_tokens: usage.input_tokens
                                    + usage.cache_read_input_tokens
                                    + usage.cache_creation_input_tokens,
                                output_tokens: usage.output_tokens,
                                cache_read_tokens: usage.cache_read_input_tokens,
                                cache_write_tokens: usage.cache_creation_input_tokens,
                            }));
                        }

                        if result.subtype == "error" {
                            yield Ok(StreamEvent::Error(
                                result
                                    .result
                                    .as_ref()
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("unknown error")
                                    .to_string(),
                            ));
                        }

                        yield Ok(StreamEvent::MessageStop(StopReason::EndTurn));
                        break;
                    }

                    IncomingMessage::System(sys) => {
                        log::debug!(
                            "system event during stream: subtype={}",
                            sys.subtype
                        );
                    }

                    IncomingMessage::Unknown(val) => {
                        log::debug!(
                            "unknown NDJSON message type: {}",
                            val.get("type")
                                .and_then(|t| t.as_str())
                                .unwrap_or("(missing)")
                        );
                    }
                }
            }
        })
    }

    /// Provider name for logging and identification.
    fn name(&self) -> &str {
        "claude-max"
    }
}

/// Groups `{` functionality.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ChatMessage, ContentBlock, Role};
    use serde_json::json;

    /// Verify that extract_user_deltas returns only unsent messages.
    #[test]
    fn extract_delta_skips_already_sent() {
        let messages = vec![
            ChatMessage {
                role: Role::User,
                content: vec![ContentBlock::Text {
                    text: "first".into(),
                }],
            },
            ChatMessage {
                role: Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: "response".into(),
                }],
            },
            ChatMessage {
                role: Role::User,
                content: vec![ContentBlock::Text {
                    text: "second".into(),
                }],
            },
        ];

        // Already sent 1 user message. The assistant message is the subprocess's
        // own response, so we skip it. Only the second user message is new.
        let delta = extract_user_deltas(&messages, 1);
        assert_eq!(delta.len(), 1);
        match &delta[0].content[0] {
            ContentBlock::Text { text } => assert_eq!(text, "second"),
            other => panic!("expected Text, got {:?}", other),
        }
    }

    /// Handles `extract_delta_with_no_prior_sends_returns_first_user` behavior.
    #[test]
    fn extract_delta_with_no_prior_sends_returns_first_user() {
        let messages = vec![ChatMessage {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: "hello".into(),
            }],
        }];

        let delta = extract_user_deltas(&messages, 0);
        assert_eq!(delta.len(), 1);
    }

    /// Handles `extract_delta_skips_tool_and_assistant_messages` behavior.
    #[test]
    fn extract_delta_skips_tool_and_assistant_messages() {
        let messages = vec![
            ChatMessage {
                role: Role::User,
                content: vec![ContentBlock::Text {
                    text: "first".into(),
                }],
            },
            ChatMessage {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolUse {
                    id: "t1".into(),
                    name: "bash".into(),
                    input: json!({"command": "ls"}),
                }],
            },
            ChatMessage {
                role: Role::Tool,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "t1".into(),
                    content: "output".into(),
                    is_error: false,
                }],
            },
            ChatMessage {
                role: Role::User,
                content: vec![ContentBlock::Text {
                    text: "second".into(),
                }],
            },
        ];

        // After sending 1 user message, the new user messages are the delta.
        // Tool/assistant messages from the agent loop context are skipped
        // (subprocess has its own internal context for those).
        let delta = extract_user_deltas(&messages, 1);
        assert_eq!(delta.len(), 1);
    }
}
