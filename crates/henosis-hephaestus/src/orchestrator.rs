//! Provider-agnostic tool-use loop. This is the core of the Phase 1
//! refactor: the LLM loop takes `&dyn Provider` and never touches an HTTP
//! client, never knows about Anthropic-specific headers, and never parses
//! provider-specific JSON.
//!
//! Provider-specific concerns live in `providers/` and `provider.rs`. The
//! orchestrator only knows the generic surface: `ChatRequest`,
//! `ChatResponse`, `ContentBlock`, `StopReason`.
//!
//! Crash recovery contract preserved: checkpoint after every turn, same
//! `Checkpoint` schema, same `kleos_store_checkpoint` call. HITL
//! pause/resume contract preserved: same `OrchestratorResult::Paused` shape
//! with `messages: Vec<Value>` so `tasks.rs::run_task_loop` stays unchanged.
//!
//! The internal message representation remains `Vec<serde_json::Value>` so
//! the on-disk checkpoint schema stays identical. Conversion to/from
//! `Vec<ChatMessage>` happens only at the provider boundary.

use std::sync::Arc;

use chrono::Utc;
use serde_json::{Value, json};
use thiserror::Error;
use tracing::warn;

use crate::anthropic_auth::CLAUDE_CODE_IDENTITY;
use crate::checkpoint::Checkpoint;
use crate::config::Config;
use crate::gate::GateClient;
use crate::hermes_client::{HermesClient, ToolDef, ToolResult};
use crate::provider::{ChatMessage, ChatRequest, ContentBlock, Provider, Role, StopReason};
use crate::services::Services;
use crate::streaming::{StreamEventEnvelope, StreamSink};

/// Errors the orchestrator can surface. Provider-specific failures collapse
/// into `Provider(String)`; the underlying provider trait uses anyhow::Error
/// so the orchestrator just records the message text.
#[derive(Debug, Error)]
pub enum OrchestratorError {
    /// Provider call failed (network, 4xx/5xx, parse error, etc.).
    #[error("provider error: {0}")]
    Provider(String),
    /// The tool-use loop ran for more than `max_turns` iterations.
    #[error("tool-use loop exceeded max_turns ({0})")]
    LoopExhausted(usize),
}

/// Result of one orchestrator pass. Mirrors the legacy `AnthropicResult` so
/// `tasks.rs::run_task_loop` can switch over with no shape change.
pub enum OrchestratorResult {
    /// Model finished; contains concatenated assistant text from all turns.
    Complete(String),
    /// Model called ask_human. Caller pauses, collects input, appends a
    /// tool_result, and calls `resume()` to continue the loop.
    Paused {
        /// Text accumulated before the pause.
        accumulated_text: String,
        /// Full conversation history up to and including the assistant turn
        /// that contained the ask_human call. Does NOT include the
        /// tool_result yet -- the caller appends that before resuming.
        messages: Vec<Value>,
        /// The question the model wants to ask.
        question: String,
        /// tool_use_id for the ask_human block -- needed to build the
        /// tool_result.
        tool_use_id: String,
    },
}

/// Run a fresh orchestrator pass. Equivalent to the legacy
/// `anthropic_complete`: wraps the input prompt in a single user message and
/// drives the loop. `stream` is optional -- when provided, the orchestrator
/// emits per-turn StreamEventEnvelope events so SSE subscribers see live
/// progress.
#[allow(clippy::too_many_arguments)]
pub async fn run(
    provider: Arc<dyn Provider>,
    services: &Services,
    hermes: &HermesClient,
    gate: &GateClient,
    cfg: &Config,
    tenant_id: Option<&str>,
    task_id: Option<&str>,
    extra_system: Option<&str>,
    tools: &[ToolDef],
    prompt: &str,
    max_turns: usize,
    stream: Option<&StreamSink>,
) -> Result<OrchestratorResult, OrchestratorError> {
    let messages = vec![json!({
        "role": "user",
        "content": [{ "type": "text", "text": prompt }]
    })];
    loop_inner(
        provider,
        services,
        hermes,
        gate,
        cfg,
        tenant_id,
        task_id,
        extra_system,
        tools,
        messages,
        max_turns,
        0,
        stream,
    )
    .await
}

/// Resume the orchestrator from an existing message history. Equivalent to
/// the legacy `anthropic_resume`: caller supplies the conversation thus far
/// (typically with a fresh tool_result appended after a HITL pause or a
/// crash-recovery checkpoint), and the loop picks up from the next turn.
#[allow(clippy::too_many_arguments)]
pub async fn resume(
    provider: Arc<dyn Provider>,
    services: &Services,
    hermes: &HermesClient,
    gate: &GateClient,
    cfg: &Config,
    tenant_id: Option<&str>,
    task_id: Option<&str>,
    extra_system: Option<&str>,
    tools: &[ToolDef],
    messages: Vec<Value>,
    max_turns: usize,
    start_step: u32,
    stream: Option<&StreamSink>,
) -> Result<OrchestratorResult, OrchestratorError> {
    loop_inner(
        provider,
        services,
        hermes,
        gate,
        cfg,
        tenant_id,
        task_id,
        extra_system,
        tools,
        messages,
        max_turns,
        start_step,
        stream,
    )
    .await
}

/// Core tool-use loop. Both `run` and `resume` delegate here. Provider-agnostic:
/// it issues `provider.send(&ChatRequest)` and reads `ChatResponse` only.
#[allow(clippy::too_many_arguments)]
async fn loop_inner(
    provider: Arc<dyn Provider>,
    services: &Services,
    hermes: &HermesClient,
    gate: &GateClient,
    cfg: &Config,
    tenant_id: Option<&str>,
    task_id: Option<&str>,
    extra_system: Option<&str>,
    tools: &[ToolDef],
    mut messages: Vec<Value>,
    max_turns: usize,
    start_step: u32,
    stream: Option<&StreamSink>,
) -> Result<OrchestratorResult, OrchestratorError> {
    let tools_json: Option<Vec<Value>> = if tools.is_empty() {
        None
    } else {
        Some(
            tools
                .iter()
                .map(|t| {
                    json!({
                        "name": t.name,
                        "description": t.description,
                        "input_schema": t.input_schema,
                    })
                })
                .collect(),
        )
    };

    let mut accumulated_text = String::new();

    for turn in 0..max_turns {
        let step = start_step + turn as u32;

        // Persist a checkpoint of the state heading into this turn (best
        // effort). On crash recovery the latest checkpoint feeds back into
        // `resume()` so the loop picks up where it left off.
        if let Some(tid) = task_id {
            let cp = Checkpoint {
                task_id: tid.to_string(),
                step,
                messages: messages.clone(),
                accumulated_text: accumulated_text.clone(),
                tenant_id: tenant_id.map(String::from),
                system: extra_system.map(String::from),
                paused: None,
                created_at: Utc::now(),
            };
            services.kleos_store_checkpoint(&cp).await;
        }

        // Gate check before LLM call (best-effort).
        let gate_ctx = json!({ "turn": step, "action": "llm.call" });
        gate.check("llm.call", &gate_ctx).await;

        // Build the generic ChatRequest. System prompt is provider-agnostic:
        // we include the Hephaestus claude-code identity block alongside any
        // task-supplied system text. Providers may interpret this string
        // differently (Anthropic wraps it in a system block array; OpenAI
        // sends it as a `system` chat message) -- the orchestrator does not
        // care.
        let system = build_system_string(extra_system);
        let chat_messages = messages_value_to_chat(&messages);

        let request = ChatRequest {
            model: cfg.model.clone(),
            messages: chat_messages,
            max_tokens: cfg.max_tokens,
            system: Some(system),
            tools: tools_json.clone(),
            stream: false,
        };

        let response = provider
            .send(&request)
            .await
            .map_err(|e| OrchestratorError::Provider(e.to_string()))?;

        // Extract text + tool_use blocks from the typed response, and emit
        // the corresponding StreamEvents to any attached SSE sink.
        let mut turn_text = String::new();
        let mut tool_uses: Vec<ToolUseRef> = Vec::new();
        for block in &response.content {
            match block {
                ContentBlock::Text { text } => {
                    turn_text.push_str(text);
                    if let Some(s) = stream {
                        s.emit(StreamEventEnvelope::TextDelta { text: text.clone() });
                    }
                }
                ContentBlock::ToolUse { id, name, input } => {
                    if let Some(s) = stream {
                        s.emit(StreamEventEnvelope::ToolStart {
                            id: id.clone(),
                            name: name.clone(),
                        });
                    }
                    tool_uses.push(ToolUseRef {
                        id: id.clone(),
                        name: name.clone(),
                        input: input.clone(),
                    });
                }
                ContentBlock::ToolResult { .. } => {
                    // Providers never return tool_result blocks from `send`;
                    // this branch exists for completeness only.
                }
            }
        }
        accumulated_text.push_str(&turn_text);

        // Emit a turn_end event with the lowercased stop_reason. Synthetic
        // for now; once send_streaming is wired in this comes straight from
        // StreamEvent::MessageStop.
        if let Some(s) = stream {
            let stop_reason = match response.stop_reason {
                StopReason::EndTurn => "end_turn",
                StopReason::ToolUse => "tool_use",
                StopReason::MaxTokens => "max_tokens",
                StopReason::StopSequence => "stop_sequence",
            }
            .to_string();
            s.emit(StreamEventEnvelope::TurnEnd { stop_reason });
        }

        let final_stop = matches!(
            response.stop_reason,
            StopReason::EndTurn | StopReason::MaxTokens | StopReason::StopSequence
        );
        if final_stop || tool_uses.is_empty() {
            return Ok(OrchestratorResult::Complete(accumulated_text));
        }

        // Append the assistant turn to the message history. We rebuild the
        // wire-format JSON from the typed content so subsequent turns can
        // continue to thread `messages: Vec<Value>` through the checkpoint.
        let assistant_content = chat_content_to_value(&response.content);
        messages.push(json!({ "role": "assistant", "content": assistant_content }));

        // Dispatch tools. Built-ins handled locally; everything else goes to
        // Hermes. ask_human triggers an early Paused return.
        let mut result_blocks: Vec<Value> = Vec::new();
        for tu in &tool_uses {
            if tu.name == "ask_human" {
                let question = tu
                    .input
                    .get("question")
                    .and_then(|q| q.as_str())
                    .unwrap_or("(no question provided)")
                    .to_string();

                // If we have partial results from earlier tools in this turn,
                // append them as a user message so the conversation stays
                // structurally consistent.
                if !result_blocks.is_empty() {
                    messages.push(json!({ "role": "user", "content": result_blocks }));
                }

                return Ok(OrchestratorResult::Paused {
                    accumulated_text,
                    messages,
                    question,
                    tool_use_id: tu.id.clone(),
                });
            }

            let result: ToolResult = if tu.name == "code_exec" {
                let language = tu
                    .input
                    .get("language")
                    .and_then(|l| l.as_str())
                    .unwrap_or("bash");
                let code = tu
                    .input
                    .get("code")
                    .and_then(|c| c.as_str())
                    .unwrap_or("");
                match crate::sandbox::run_code(
                    language,
                    code,
                    cfg.sandbox_timeout,
                    &cfg.sandbox_memory,
                )
                .await
                {
                    Ok(output) => ToolResult {
                        tool_use_id: tu.id.clone(),
                        content: output,
                        is_error: false,
                    },
                    Err(e) => ToolResult {
                        tool_use_id: tu.id.clone(),
                        content: e,
                        is_error: true,
                    },
                }
            } else {
                hermes
                    .call_tool(&tu.name, &tu.id, tenant_id, &tu.input)
                    .await
            };

            // Mirror the tool_result to any attached stream subscriber.
            if let Some(s) = stream {
                s.emit(StreamEventEnvelope::ToolResult {
                    id: result.tool_use_id.clone(),
                    content: result.content.clone(),
                    is_error: result.is_error,
                });
            }

            result_blocks.push(json!({
                "type": "tool_result",
                "tool_use_id": result.tool_use_id,
                "content": result.content,
                "is_error": result.is_error,
            }));
        }

        // Post-tool gate check (best-effort).
        let gate_ctx = json!({ "action": "tool.results", "tool_count": result_blocks.len() });
        gate.check("tool.results", &gate_ctx).await;

        // Append tool results as a user turn and continue the loop.
        messages.push(json!({ "role": "user", "content": result_blocks }));
    }

    warn!(turns = max_turns, "orchestrator loop exhausted");
    Err(OrchestratorError::LoopExhausted(max_turns))
}

/// Build the system prompt string passed in `ChatRequest.system`. Always
/// prepends the Hephaestus claude-code identity; appends any extra
/// task-supplied system text separated by a blank line.
fn build_system_string(extra_system: Option<&str>) -> String {
    match extra_system {
        Some(extra) if !extra.trim().is_empty() => {
            format!("{CLAUDE_CODE_IDENTITY}\n\n{extra}")
        }
        _ => CLAUDE_CODE_IDENTITY.to_string(),
    }
}

/// Convert the orchestrator's wire-format message history into the typed
/// `Vec<ChatMessage>` the Provider trait consumes. Each message has a role
/// and a list of content blocks; the JSON shape closely mirrors Anthropic's
/// wire format, which is what Hephaestus has always used internally.
fn messages_value_to_chat(messages: &[Value]) -> Vec<ChatMessage> {
    let mut out = Vec::with_capacity(messages.len());
    for msg in messages {
        let role = match msg.get("role").and_then(|r| r.as_str()) {
            Some("assistant") => Role::Assistant,
            Some("system") => Role::System,
            Some("tool") => Role::Tool,
            _ => Role::User,
        };
        let content = parse_content_blocks(msg.get("content"));
        out.push(ChatMessage { role, content });
    }
    out
}

/// Parse a JSON `content` field into typed `ContentBlock`s. Handles both the
/// array-of-blocks shape (Anthropic's wire format) and the legacy
/// shorthand where `content` is a bare string -- that string becomes a
/// single `Text` block.
fn parse_content_blocks(content: Option<&Value>) -> Vec<ContentBlock> {
    let Some(content) = content else {
        return Vec::new();
    };
    if let Some(s) = content.as_str() {
        return vec![ContentBlock::Text {
            text: s.to_string(),
        }];
    }
    let Some(arr) = content.as_array() else {
        return Vec::new();
    };
    let mut blocks = Vec::with_capacity(arr.len());
    for block in arr {
        match block.get("type").and_then(|t| t.as_str()) {
            Some("text") => {
                let text = block
                    .get("text")
                    .and_then(|t| t.as_str())
                    .unwrap_or("")
                    .to_string();
                blocks.push(ContentBlock::Text { text });
            }
            Some("tool_use") => {
                let id = block
                    .get("id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let name = block
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let input = block.get("input").cloned().unwrap_or(Value::Null);
                blocks.push(ContentBlock::ToolUse { id, name, input });
            }
            Some("tool_result") => {
                let tool_use_id = block
                    .get("tool_use_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let content = block
                    .get("content")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let is_error = block
                    .get("is_error")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                blocks.push(ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                });
            }
            _ => {}
        }
    }
    blocks
}

/// Reverse of `parse_content_blocks`: typed `ContentBlock`s back to JSON
/// content array, used when appending an assistant turn to the message
/// history.
fn chat_content_to_value(blocks: &[ContentBlock]) -> Vec<Value> {
    blocks
        .iter()
        .map(|b| match b {
            ContentBlock::Text { text } => json!({ "type": "text", "text": text }),
            ContentBlock::ToolUse { id, name, input } => json!({
                "type": "tool_use",
                "id": id,
                "name": name,
                "input": input,
            }),
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => json!({
                "type": "tool_result",
                "tool_use_id": tool_use_id,
                "content": content,
                "is_error": is_error,
            }),
        })
        .collect()
}

/// Lightweight extraction of a tool_use block from the typed response. The
/// orchestrator uses this rather than working with `ContentBlock::ToolUse`
/// directly so the dispatch loop has a single small struct to thread.
struct ToolUseRef {
    /// Anthropic-assigned tool_use_id used in the tool_result response.
    id: String,
    /// Tool name as declared in the tools list.
    name: String,
    /// Parsed tool input arguments.
    input: Value,
}
