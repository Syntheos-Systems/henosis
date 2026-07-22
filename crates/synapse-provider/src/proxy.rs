//! OpenAI-compatible proxy provider.
//! Routes through any OpenAI-compatible endpoint using a static API key.

use std::pin::Pin;

use anyhow::{Context, Result, bail};
use async_stream::stream;
use async_trait::async_trait;
use futures::Stream;
use reqwest_eventsource::{Event, RequestBuilderExt};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::streaming::parse_openai_sse;
use crate::types::{
    ChatRequest, ChatResponse, ContentBlock, Provider, Role, StopReason, StreamEvent, Usage,
};

/// Serializes an OpenAI-compatible chat completion request.
#[derive(Debug, Serialize)]
pub(crate) struct OaiRequest {
    pub model: String,
    pub messages: Vec<OaiMessage>,
    /// Token cap for standard OpenAI-compatible servers (Ollama, vLLM, etc.).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    /// Token cap for the Foundry/Azure OpenAI backend, whose gpt-5-class models
    /// reject `max_tokens` with HTTP 400 and require `max_completion_tokens`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_completion_tokens: Option<u32>,
    pub stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<Value>>,
}

/// Serializes one OpenAI-compatible request message.
#[derive(Debug, Serialize)]
pub(crate) struct OaiMessage {
    pub role: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<OaiToolCallOut>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

/// Outbound tool call (request serialization).
#[derive(Debug, Serialize)]
pub(crate) struct OaiToolCallOut {
    pub id: String,
    #[serde(rename = "type")]
    pub call_type: &'static str,
    pub function: OaiFunctionOut,
}

/// Serializes the function payload inside an outbound tool call.
#[derive(Debug, Serialize)]
pub(crate) struct OaiFunctionOut {
    pub name: String,
    pub arguments: String,
}

/// Deserializes an OpenAI-compatible chat completion response.
#[derive(Debug, Deserialize)]
pub(crate) struct OaiResponse {
    pub id: String,
    pub choices: Vec<OaiChoice>,
    pub usage: Option<OaiUsage>,
}

/// Deserializes one response choice and its finish reason.
#[derive(Debug, Deserialize)]
pub(crate) struct OaiChoice {
    pub message: OaiChoiceMessage,
    pub finish_reason: Option<String>,
}

/// Deserializes content and tool calls from a response choice.
#[derive(Debug, Deserialize)]
pub(crate) struct OaiChoiceMessage {
    pub content: Option<String>,
    pub tool_calls: Option<Vec<OaiToolCall>>,
}

/// Deserializes one tool call returned by the provider.
#[derive(Debug, Deserialize)]
pub(crate) struct OaiToolCall {
    pub id: String,
    pub function: OaiFunction,
}

/// Deserializes a returned function name and JSON arguments.
#[derive(Debug, Deserialize)]
pub(crate) struct OaiFunction {
    pub name: String,
    pub arguments: String,
}

/// Deserializes token usage from an OpenAI-compatible response.
#[derive(Debug, Deserialize)]
pub(crate) struct OaiUsage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    #[serde(default)]
    pub prompt_tokens_details: Option<OaiPromptDetails>,
}

/// Prompt-token detail breakdown from an OpenAI-compatible `usage` object.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct OaiPromptDetails {
    /// Tokens served from the provider's automatic prompt cache.
    #[serde(default)]
    pub cached_tokens: u32,
}

/// Maps a provider-neutral role to the OpenAI wire role.
pub(crate) fn role_str(role: &Role) -> &'static str {
    match role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    }
}

/// Builds a standard OpenAI-compatible request body.
pub(crate) fn build_request(req: &ChatRequest) -> OaiRequest {
    build_request_with(req, false)
}

/// Build an OpenAI-compatible request body. When `max_completion_tokens` is
/// true, the token cap is emitted as `max_completion_tokens` instead of
/// `max_tokens` -- required by the Foundry/Azure OpenAI proxy, whose gpt-5-class
/// models reject `max_tokens` with HTTP 400.
pub(crate) fn build_request_with(req: &ChatRequest, max_completion_tokens: bool) -> OaiRequest {
    let mut messages = Vec::new();
    if let Some(ref sys) = req.system {
        messages.push(OaiMessage {
            role: "system",
            content: Some(sys.clone()),
            tool_calls: None,
            tool_call_id: None,
        });
    }
    for msg in &req.messages {
        let mut text_parts: Vec<String> = Vec::new();
        let mut tool_calls: Vec<OaiToolCallOut> = Vec::new();
        let mut tool_results: Vec<(String, String)> = Vec::new();

        for block in &msg.content {
            match block {
                ContentBlock::Text { text } => text_parts.push(text.clone()),
                ContentBlock::ToolUse { id, name, input } => {
                    tool_calls.push(OaiToolCallOut {
                        id: id.clone(),
                        call_type: "function",
                        function: OaiFunctionOut {
                            name: name.clone(),
                            arguments: serde_json::to_string(input).unwrap_or_default(),
                        },
                    });
                }
                ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    ..
                } => {
                    tool_results.push((tool_use_id.clone(), content.clone()));
                }
            }
        }

        if !tool_calls.is_empty() {
            // Assistant message with tool_calls field
            let content = if text_parts.is_empty() {
                None
            } else {
                Some(text_parts.join("\n"))
            };
            messages.push(OaiMessage {
                role: "assistant",
                content,
                tool_calls: Some(tool_calls),
                tool_call_id: None,
            });
        } else if !tool_results.is_empty() {
            // Each tool result becomes a separate role:"tool" message
            for (tool_use_id, content) in tool_results {
                messages.push(OaiMessage {
                    role: "tool",
                    content: Some(content),
                    tool_calls: None,
                    tool_call_id: Some(tool_use_id),
                });
            }
        } else {
            // Plain text message
            messages.push(OaiMessage {
                role: role_str(&msg.role),
                content: Some(text_parts.join("\n")),
                tool_calls: None,
                tool_call_id: None,
            });
        }
    }
    // Convert flat tool schemas to OpenAI format.
    // Anthropic shape:        { name, description, input_schema }
    // Flattened OpenAI shape: { name, description, parameters }
    //
    // Both reach here (callers that unwrap OpenAI `{type:"function",..}` tools
    // keep the key `parameters`). Reading only `input_schema` silently
    // substituted an empty `{}`, which strict validators (DeepSeek) reject
    // outright -- the 2026-07-16 silent-bots incident. Accept either key, and
    // fall back to a VALID empty object schema, never bare `{}`.
    let tools = req.tools.as_ref().map(|tools| {
        tools
            .iter()
            .map(|t| {
                let schema = t
                    .get("input_schema")
                    .or_else(|| t.get("parameters"))
                    .cloned()
                    .unwrap_or_else(|| serde_json::json!({"type": "object", "properties": {}}));
                serde_json::json!({
                    "type": "function",
                    "function": {
                        "name": t.get("name").cloned().unwrap_or_default(),
                        "description": t.get("description").cloned().unwrap_or_default(),
                        "parameters": schema,
                    }
                })
            })
            .collect::<Vec<_>>()
    });

    OaiRequest {
        model: req.model.clone(),
        messages,
        max_tokens: (!max_completion_tokens).then_some(req.max_tokens),
        max_completion_tokens: max_completion_tokens.then_some(req.max_tokens),
        stream: req.stream,
        tools,
    }
}

/// Converts an OpenAI-compatible response to the provider-neutral response.
pub(crate) fn to_chat_response(resp: OaiResponse) -> ChatResponse {
    let mut content = Vec::new();
    let mut stop = StopReason::EndTurn;

    // Process ALL choices -- some proxies split text and tool_calls into separate choices.
    for choice in &resp.choices {
        if let Some(ref reason) = choice.finish_reason {
            match reason.as_str() {
                "tool_calls" => stop = StopReason::ToolUse,
                "length" => stop = StopReason::MaxTokens,
                _ => {}
            }
        }
        if let Some(ref text) = choice.message.content
            && !text.is_empty()
        {
            content.push(ContentBlock::Text { text: text.clone() });
        }
        if let Some(ref tcs) = choice.message.tool_calls {
            for tc in tcs {
                let input: Value =
                    serde_json::from_str(&tc.function.arguments).unwrap_or(Value::Null);
                content.push(ContentBlock::ToolUse {
                    id: tc.id.clone(),
                    name: tc.function.name.clone(),
                    input,
                });
            }
        }
    }

    let usage = resp
        .usage
        .map(|u| Usage {
            input_tokens: u.prompt_tokens,
            output_tokens: u.completion_tokens,
            cache_read_tokens: u
                .prompt_tokens_details
                .as_ref()
                .map(|d| d.cached_tokens)
                .unwrap_or(0),
            cache_write_tokens: 0,
        })
        .unwrap_or_default();

    ChatResponse {
        id: resp.id,
        content,
        stop_reason: stop,
        usage,
    }
}

/// Sends chat requests to a configurable OpenAI-compatible endpoint.
pub struct ProxyProvider {
    client: reqwest::Client,
    base_url: String,
    api_key: String,
    name: &'static str,
    /// Emit the token cap as `max_completion_tokens` instead of `max_tokens`.
    /// Set for the Foundry/Azure OpenAI proxy, whose gpt-5-class models reject
    /// `max_tokens` with HTTP 400.
    max_completion_tokens: bool,
}

/// Provides construction and endpoint helpers for proxy providers.
impl ProxyProvider {
    /// Creates a provider targeting an OpenAI-compatible base URL.
    pub fn new(client: reqwest::Client, base_url: String, api_key: String) -> Self {
        Self {
            client,
            base_url,
            api_key,
            name: "proxy",
            max_completion_tokens: false,
        }
    }

    /// Override the provider name reported via `Provider::name()`. Useful when a
    /// preset (e.g. OpenCode Zen) wraps the proxy and wants its own identity for
    /// telemetry / pricing lookup.
    pub fn with_name(mut self, name: &'static str) -> Self {
        self.name = name;
        self
    }

    /// Emit the token cap as `max_completion_tokens` rather than `max_tokens`.
    /// Required by the Foundry/Azure OpenAI proxy (gpt-5-class models 400 on
    /// `max_tokens`).
    pub fn with_max_completion_tokens(mut self, enabled: bool) -> Self {
        self.max_completion_tokens = enabled;
        self
    }

    /// Returns the configured provider base URL.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Builds the proxy chat completions endpoint.
    fn endpoint(&self) -> String {
        format!("{}/chat/completions", self.base_url.trim_end_matches('/'))
    }
}

/// Implements synchronous and streaming chat requests for compatible proxies.
#[async_trait]
impl Provider for ProxyProvider {
    /// Sends a non-streaming chat completion request.
    async fn send(&self, request: &ChatRequest) -> Result<ChatResponse> {
        let oai = build_request_with(request, self.max_completion_tokens);
        let body = serde_json::to_string(&oai)?;

        let resp = self
            .client
            .post(self.endpoint())
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .body(body)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp
                .text()
                .await
                .unwrap_or_else(|e| format!("(failed to read body: {e})"));
            bail!("proxy error {status}: {text}");
        }

        let oai_resp: OaiResponse = resp.json().await.context("parse proxy response")?;
        Ok(to_chat_response(oai_resp))
    }

    /// Opens a streaming chat completion request and converts its SSE events.
    fn send_streaming(
        &self,
        request: &ChatRequest,
    ) -> Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>> {
        let client = self.client.clone();
        let endpoint = self.endpoint();
        let api_key = self.api_key.clone();
        let mut oai = build_request_with(request, self.max_completion_tokens);
        oai.stream = true;

        Box::pin(stream! {
            let body = match serde_json::to_string(&oai) {
                Ok(b) => b,
                Err(e) => { yield Err(e.into()); return; }
            };
            let rb = client.post(&endpoint)
                .header("Authorization", format!("Bearer {api_key}"))
                .header("Content-Type", "application/json")
                .body(body);

            let mut es = match rb.eventsource() {
                Ok(es) => es,
                Err(e) => { yield Err(anyhow::anyhow!("{e}")); return; }
            };

            while let Some(event) = {
                use futures::StreamExt;
                es.next().await
            } {
                match event {
                    Ok(Event::Message(msg)) => {
                        for ev in parse_openai_sse(&msg.data) {
                            yield Ok(ev);
                        }
                    }
                    Ok(Event::Open) => {}
                    Err(reqwest_eventsource::Error::StreamEnded) => {
                        break; // Normal: stream closed after [DONE]
                    }
                    Err(e) => {
                        yield Err(anyhow::anyhow!("sse error: {e}"));
                        break;
                    }
                }
            }
        })
    }

    /// Returns the configured provider identifier.
    fn name(&self) -> &str {
        self.name
    }
}

/// Tests OpenAI wire conversion and proxy request behavior.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ChatMessage, ChatRequest, ContentBlock, Role};
    use serde_json::json;

    /// Builds a minimal request shared by proxy conversion tests.
    fn make_request(messages: Vec<ChatMessage>) -> ChatRequest {
        ChatRequest {
            model: "test-model".into(),
            messages,
            max_tokens: 1024,
            system: None,
            tools: None,
            stream: false,
        }
    }

    /// Verifies plain text messages retain their OpenAI wire fields.
    #[test]
    fn build_request_simple_text() {
        let req = make_request(vec![ChatMessage {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: "hello".into(),
            }],
        }]);
        let oai = build_request(&req);
        assert_eq!(oai.messages.len(), 1);
        assert_eq!(oai.messages[0].role, "user");
        assert_eq!(oai.messages[0].content.as_deref(), Some("hello"));
        assert!(oai.messages[0].tool_calls.is_none());
        assert!(oai.messages[0].tool_call_id.is_none());
    }

    /// A tool carrying OpenAI's `parameters` key (rather than Anthropic's
    /// `input_schema`) must keep its schema on the wire: reading only
    /// `input_schema` emitted an empty `{}` and strict validators (DeepSeek)
    /// rejected the whole request, silencing every tool-carrying caller.
    #[test]
    fn build_request_keeps_schema_from_flattened_openai_tool() {
        let mut req = make_request(vec![ChatMessage {
            role: Role::User,
            content: vec![ContentBlock::Text { text: "hi".into() }],
        }]);
        req.tools = Some(vec![json!({
            "name": "search_memory",
            "description": "Search long-term memory.",
            "parameters": {
                "type": "object",
                "properties": { "query": { "type": "string" } },
                "required": ["query"],
            },
        })]);

        let oai = build_request(&req);
        let params = &oai.tools.as_ref().expect("tools present")[0]["function"]["parameters"];
        assert_eq!(params["type"], "object");
        assert_eq!(params["properties"]["query"]["type"], "string");
        assert_eq!(params["required"][0], "query");
    }

    /// Anthropic-native callers supply `input_schema`; that path must keep
    /// working unchanged.
    #[test]
    fn build_request_keeps_schema_from_anthropic_tool() {
        let mut req = make_request(vec![ChatMessage {
            role: Role::User,
            content: vec![ContentBlock::Text { text: "hi".into() }],
        }]);
        req.tools = Some(vec![json!({
            "name": "store_memory",
            "description": "Save a fact.",
            "input_schema": {
                "type": "object",
                "properties": { "content": { "type": "string" } },
            },
        })]);

        let oai = build_request(&req);
        let params = &oai.tools.as_ref().expect("tools present")[0]["function"]["parameters"];
        assert_eq!(params["type"], "object");
        assert_eq!(params["properties"]["content"]["type"], "string");
    }

    /// With neither key present, emit a VALID empty schema rather than `{}`,
    /// which strict validators reject.
    #[test]
    fn build_request_schemaless_tool_still_valid() {
        let mut req = make_request(vec![ChatMessage {
            role: Role::User,
            content: vec![ContentBlock::Text { text: "hi".into() }],
        }]);
        req.tools = Some(vec![json!({ "name": "ping", "description": "ping" })]);

        let oai = build_request(&req);
        let params = &oai.tools.as_ref().expect("tools present")[0]["function"]["parameters"];
        assert_eq!(params["type"], "object");
    }

    /// Verifies the system prompt becomes the first wire message.
    #[test]
    fn build_request_system_message() {
        let mut req = make_request(vec![ChatMessage {
            role: Role::User,
            content: vec![ContentBlock::Text { text: "hi".into() }],
        }]);
        req.system = Some("You are helpful.".into());
        let oai = build_request(&req);
        assert_eq!(oai.messages.len(), 2);
        assert_eq!(oai.messages[0].role, "system");
        assert_eq!(oai.messages[0].content.as_deref(), Some("You are helpful."));
    }

    /// Verifies assistant tool use becomes an OpenAI function call.
    #[test]
    fn build_request_assistant_tool_use() {
        let req = make_request(vec![ChatMessage {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: "call_1".into(),
                name: "bash".into(),
                input: json!({"command": "ls"}),
            }],
        }]);
        let oai = build_request(&req);
        assert_eq!(oai.messages.len(), 1);
        let msg = &oai.messages[0];
        assert_eq!(msg.role, "assistant");
        assert!(msg.content.is_none()); // no text, only tool_calls
        let tcs = msg.tool_calls.as_ref().unwrap();
        assert_eq!(tcs.len(), 1);
        assert_eq!(tcs[0].id, "call_1");
        assert_eq!(tcs[0].call_type, "function");
        assert_eq!(tcs[0].function.name, "bash");
        assert_eq!(tcs[0].function.arguments, r#"{"command":"ls"}"#);
    }

    /// Verifies assistant text and tool use coexist in one message.
    #[test]
    fn build_request_assistant_text_plus_tool_use() {
        let req = make_request(vec![ChatMessage {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Text {
                    text: "Let me check.".into(),
                },
                ContentBlock::ToolUse {
                    id: "call_2".into(),
                    name: "read".into(),
                    input: json!({"path": "/tmp/foo"}),
                },
            ],
        }]);
        let oai = build_request(&req);
        let msg = &oai.messages[0];
        assert_eq!(msg.role, "assistant");
        assert_eq!(msg.content.as_deref(), Some("Let me check."));
        assert_eq!(msg.tool_calls.as_ref().unwrap().len(), 1);
    }

    /// Verifies tool results become separate tool-role messages.
    #[test]
    fn build_request_tool_results() {
        let req = make_request(vec![ChatMessage {
            role: Role::User,
            content: vec![
                ContentBlock::ToolResult {
                    tool_use_id: "call_1".into(),
                    content: "file1.txt\nfile2.txt".into(),
                    is_error: false,
                },
                ContentBlock::ToolResult {
                    tool_use_id: "call_2".into(),
                    content: "contents here".into(),
                    is_error: false,
                },
            ],
        }]);
        let oai = build_request(&req);
        // Two tool results become two separate role:"tool" messages
        assert_eq!(oai.messages.len(), 2);
        assert_eq!(oai.messages[0].role, "tool");
        assert_eq!(oai.messages[0].tool_call_id.as_deref(), Some("call_1"));
        assert_eq!(
            oai.messages[0].content.as_deref(),
            Some("file1.txt\nfile2.txt")
        );
        assert_eq!(oai.messages[1].role, "tool");
        assert_eq!(oai.messages[1].tool_call_id.as_deref(), Some("call_2"));
    }

    /// Verifies a complete multi-turn tool exchange preserves ordering.
    #[test]
    fn build_request_multi_turn_tool_conversation() {
        // Full multi-turn: user -> assistant(tool_use) -> user(tool_result) -> assistant(text)
        let req = make_request(vec![
            ChatMessage {
                role: Role::User,
                content: vec![ContentBlock::Text {
                    text: "list files".into(),
                }],
            },
            ChatMessage {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolUse {
                    id: "call_1".into(),
                    name: "bash".into(),
                    input: json!({"command": "ls"}),
                }],
            },
            ChatMessage {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "call_1".into(),
                    content: "a.txt\nb.txt".into(),
                    is_error: false,
                }],
            },
            ChatMessage {
                role: Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: "Found 2 files.".into(),
                }],
            },
        ]);
        let oai = build_request(&req);
        assert_eq!(oai.messages.len(), 4);

        assert_eq!(oai.messages[0].role, "user");
        assert_eq!(oai.messages[0].content.as_deref(), Some("list files"));

        assert_eq!(oai.messages[1].role, "assistant");
        assert!(oai.messages[1].tool_calls.is_some());

        assert_eq!(oai.messages[2].role, "tool");
        assert_eq!(oai.messages[2].tool_call_id.as_deref(), Some("call_1"));

        assert_eq!(oai.messages[3].role, "assistant");
        assert_eq!(oai.messages[3].content.as_deref(), Some("Found 2 files."));
    }

    /// Verifies tool calls are serialized as JSON rather than legacy XML.
    #[test]
    fn build_request_no_xml_in_output() {
        // Verify the old XML format is gone
        let req = make_request(vec![
            ChatMessage {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolUse {
                    id: "call_x".into(),
                    name: "bash".into(),
                    input: json!({"cmd": "echo"}),
                }],
            },
            ChatMessage {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "call_x".into(),
                    content: "output".into(),
                    is_error: false,
                }],
            },
        ]);
        let oai = build_request(&req);
        let json = serde_json::to_string(&oai).unwrap();
        assert!(
            !json.contains("<tool_use"),
            "serialized request must not contain XML tool_use tags"
        );
        assert!(
            !json.contains("<tool_result"),
            "serialized request must not contain XML tool_result tags"
        );
    }

    /// Verifies cached prompt tokens are retained in response usage.
    #[test]
    fn to_chat_response_captures_cached_tokens() {
        let json = r#"{
            "id": "resp-1",
            "choices": [{"message": {"content": "hi", "tool_calls": null}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 100, "completion_tokens": 20, "prompt_tokens_details": {"cached_tokens": 64}}
        }"#;
        let resp: OaiResponse = serde_json::from_str(json).unwrap();
        let chat = to_chat_response(resp);
        assert_eq!(chat.usage.input_tokens, 100);
        assert_eq!(chat.usage.cache_read_tokens, 64);
        assert_eq!(chat.usage.cache_write_tokens, 0);
    }

    /// Verifies absent prompt details produce zero cached-token usage.
    #[test]
    fn to_chat_response_no_prompt_details_yields_zero_cache() {
        let json = r#"{
            "id": "resp-2",
            "choices": [{"message": {"content": "hi", "tool_calls": null}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 50, "completion_tokens": 10}
        }"#;
        let resp: OaiResponse = serde_json::from_str(json).unwrap();
        let chat = to_chat_response(resp);
        assert_eq!(chat.usage.cache_read_tokens, 0);
        assert_eq!(chat.usage.cache_write_tokens, 0);
    }

    /// Verifies serialized tool calls match the OpenAI structure.
    #[test]
    fn build_request_serialization_format() {
        // Verify the serialized JSON has correct OpenAI structure
        let req = make_request(vec![ChatMessage {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: "call_1".into(),
                name: "bash".into(),
                input: json!({"command": "ls"}),
            }],
        }]);
        let oai = build_request(&req);
        let val: Value = serde_json::to_value(&oai).unwrap();
        let msg = &val["messages"][0];
        assert_eq!(msg["role"], "assistant");
        assert!(msg.get("content").is_none() || msg["content"].is_null());
        assert_eq!(msg["tool_calls"][0]["type"], "function");
        assert_eq!(msg["tool_calls"][0]["function"]["name"], "bash");
    }

    /// Verifies the alternate token-limit field is emitted exclusively.
    #[test]
    fn build_request_with_emits_max_completion_tokens() {
        let req = make_request(vec![ChatMessage {
            role: Role::User,
            content: vec![ContentBlock::Text { text: "hi".into() }],
        }]);
        // Default path keeps `max_tokens` (plain OpenAI-compatible servers).
        let std = serde_json::to_value(build_request(&req)).unwrap();
        assert_eq!(std["max_tokens"], 1024);
        assert!(std.get("max_completion_tokens").is_none());
        // Foundry/Azure path renames to `max_completion_tokens` (gpt-5 rejects
        // `max_tokens` with HTTP 400) and omits `max_tokens` entirely.
        let foundry = serde_json::to_value(build_request_with(&req, true)).unwrap();
        assert_eq!(foundry["max_completion_tokens"], 1024);
        assert!(foundry.get("max_tokens").is_none());
    }
}
