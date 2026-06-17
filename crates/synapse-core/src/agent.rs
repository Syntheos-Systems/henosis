//! The agent loop: context -> LLM -> tool calls -> loop.

use std::sync::Arc;

use async_stream::stream;
use futures::StreamExt;
use futures::future::join_all;
use synapse_provider::{ChatMessage, ContentBlock, Provider, Role, StopReason, StreamEvent};
use synapse_tools::tool::ToolRegistry;
use synapse_tools::{GateDecision, PermissiveGate, SharedGate};
use tokio_stream::Stream;

use crate::compression;
use crate::context::ConversationContext;
use crate::cost::PricingTable;
use crate::types::{AgentConfig, AgentEvent};

/// Truncate a tool result to fit within a token budget.
/// Keeps the first ~60% and last ~30% with a marker in between.
/// Uses char boundaries to avoid splitting multi-byte UTF-8 sequences.
fn truncate_tool_result(content: &str, max_tokens: usize) -> String {
    let max_chars = max_tokens * 4;
    if content.chars().count() <= max_chars {
        return content.to_string();
    }
    let head_chars = max_chars * 6 / 10;
    let tail_chars = max_chars * 3 / 10;
    let truncated_tokens = (content.len().saturating_sub(max_chars)) / 4;

    // Find safe byte offsets for head cut
    let head_byte = content
        .char_indices()
        .nth(head_chars)
        .map(|(i, _)| i)
        .unwrap_or(content.len());

    // Find safe byte offset for tail cut (last tail_chars chars)
    let total_chars = content.chars().count();
    let tail_start_char = total_chars.saturating_sub(tail_chars);
    let tail_byte = content
        .char_indices()
        .nth(tail_start_char)
        .map(|(i, _)| i)
        .unwrap_or(0);

    format!(
        "{}\n\n...[truncated ~{} tokens]...\n\n{}",
        &content[..head_byte],
        truncated_tokens,
        &content[tail_byte..]
    )
}

/// Persist a message to the session store if configured. Non-fatal on error.
fn persist_turn(
    config: &AgentConfig,
    message: &ChatMessage,
    input_tokens: u32,
    output_tokens: u32,
) {
    if let (Some(store), Some(session_id)) = (&config.session_store, config.session_id)
        && let Err(e) = store.insert_turn(session_id, message, input_tokens, output_tokens)
    {
        log::warn!("failed to persist turn to session {session_id}: {e}");
    }
}

/// Run one turn of the agent reasoning loop with streaming.
///
/// Text deltas are emitted in real-time as the LLM generates them.
/// Tool calls are collected from the stream, executed concurrently, then looped.
pub fn agent_turn(
    config: AgentConfig,
    provider: Arc<dyn Provider + Send + Sync>,
    tools: Arc<ToolRegistry>,
    ctx: Arc<tokio::sync::Mutex<ConversationContext>>,
    message: String,
) -> impl Stream<Item = AgentEvent> + Send {
    agent_turn_with_pricing(config, provider, tools, ctx, message, None)
}

/// Agent turn with optional pricing table for cost telemetry.
pub fn agent_turn_with_pricing(
    config: AgentConfig,
    provider: Arc<dyn Provider + Send + Sync>,
    tools: Arc<ToolRegistry>,
    ctx: Arc<tokio::sync::Mutex<ConversationContext>>,
    message: String,
    pricing: Option<Arc<PricingTable>>,
) -> impl Stream<Item = AgentEvent> + Send {
    stream! {
        // Persist and add the user message
        {
            let user_msg = ChatMessage {
                role: Role::User,
                content: vec![ContentBlock::Text { text: message.clone() }],
            };
            persist_turn(&config, &user_msg, 0, 0);

            let mut ctx = ctx.lock().await;
            ctx.add_user(&message);
        }

        let mut turns = 0usize;
        // Track the model used on the previous turn so we can emit ModelSwitch.
        let mut prev_model: Option<String> = None;

        loop {
            if turns >= config.max_turns {
                yield AgentEvent::Error(format!(
                    "max_turns ({}) exceeded", config.max_turns
                ));
                break;
            }
            turns += 1;

            // Select model for this turn: router overrides config.model.
            let turn_model = if let Some(ref router) = config.router {
                router.select(turns).to_string()
            } else {
                config.model.clone()
            };

            // Emit ModelSwitch when the model changes between turns.
            if let Some(ref prev) = prev_model
                && *prev != turn_model {
                    yield AgentEvent::ModelSwitch {
                        from: prev.clone(),
                        to: turn_model.clone(),
                    };
                }
            prev_model = Some(turn_model.clone());

            yield AgentEvent::TurnStart;

            // Check if context compression is needed
            if let Some(ref comp_config) = config.compression {
                let mut ctx_guard = ctx.lock().await;
                let compressed = compression::maybe_compress(
                    &mut ctx_guard,
                    comp_config,
                    provider.as_ref(),
                ).await;
                if compressed {
                    yield AgentEvent::Text(
                        "\n[context compressed]\n".to_string(),
                    );
                }
                drop(ctx_guard);
            }

            // Build request with stream=true, using the per-turn model
            let request = {
                let ctx = ctx.lock().await;
                ctx.to_request(&turn_model, config.max_tokens, true)
            };

            let mut event_stream = provider.send_streaming(&request);

            // Collect the full response from the stream
            let mut text_parts: Vec<String> = Vec::new();
            let mut tool_uses: Vec<(String, String, String)> = Vec::new(); // (id, name, args_json)
            let mut current_tool_id = String::new();
            let mut current_tool_name = String::new();
            let mut current_tool_args = String::new();
            let mut stop_reason = StopReason::EndTurn;
            let mut usage = synapse_provider::Usage::default();

            while let Some(result) = event_stream.next().await {
                match result {
                    Ok(event) => match event {
                        StreamEvent::ContentDelta(delta) => {
                            yield AgentEvent::Text(delta.clone());
                            text_parts.push(delta);
                        }
                        StreamEvent::ToolUseStart { id, name } => {
                            // Flush any previous tool
                            if !current_tool_id.is_empty() {
                                tool_uses.push((
                                    current_tool_id.clone(),
                                    current_tool_name.clone(),
                                    current_tool_args.clone(),
                                ));
                            }
                            current_tool_id = id.clone();
                            current_tool_name = name.clone();
                            current_tool_args = String::new();
                            yield AgentEvent::ToolStart { id, name };
                        }
                        StreamEvent::ToolUseInputDelta(delta) => {
                            current_tool_args.push_str(&delta);
                        }
                        StreamEvent::MessageStop(reason) => {
                            stop_reason = reason;
                        }
                        StreamEvent::Usage(u) => {
                            usage = u;
                        }
                        StreamEvent::Error(e) => {
                            yield AgentEvent::Error(format!("stream error: {e}"));
                            break;
                        }
                    },
                    Err(e) => {
                        yield AgentEvent::Error(format!("provider stream error: {e}"));
                        break;
                    }
                }
            }

            // Flush final tool if any
            if !current_tool_id.is_empty() {
                tool_uses.push((current_tool_id, current_tool_name, current_tool_args));
            }

            yield AgentEvent::Usage {
                input_tokens: usage.input_tokens,
                output_tokens: usage.output_tokens,
                cache_read_tokens: usage.cache_read_tokens,
                cache_write_tokens: usage.cache_write_tokens,
            };

            // Emit cost telemetry if pricing table provided
            if let Some(ref pt) = pricing {
                let turn_usd = pt.cost(
                    &turn_model,
                    usage.input_tokens,
                    usage.output_tokens,
                    usage.cache_read_tokens,
                    usage.cache_write_tokens,
                );
                let session_total_usd = {
                    let mut ctx_guard = ctx.lock().await;
                    ctx_guard.session_cost.record(
                        &turn_model,
                        usage.input_tokens,
                        usage.output_tokens,
                        usage.cache_read_tokens,
                        usage.cache_write_tokens,
                        turn_usd,
                    );
                    ctx_guard.session_cost.total_usd
                };
                yield AgentEvent::Cost {
                    model: turn_model.clone(),
                    input_tokens: usage.input_tokens,
                    output_tokens: usage.output_tokens,
                    cache_read_tokens: usage.cache_read_tokens,
                    cache_write_tokens: usage.cache_write_tokens,
                    turn_usd,
                    session_total_usd,
                };
            }

            // Reconstruct the assistant content blocks for context
            let mut content_blocks: Vec<ContentBlock> = Vec::new();
            let full_text: String = text_parts.join("");
            if !full_text.is_empty() {
                content_blocks.push(ContentBlock::Text { text: full_text });
            }
            for (id, name, args) in &tool_uses {
                let input: serde_json::Value = serde_json::from_str(args)
                    .unwrap_or_else(|e| {
                        log::warn!("failed to parse tool args for {}: {}", name, e);
                        serde_json::Value::Object(Default::default())
                    });
                content_blocks.push(ContentBlock::ToolUse {
                    id: id.clone(),
                    name: name.clone(),
                    input: input.clone(),
                });
            }

            // Execute tool calls
            if !tool_uses.is_empty() {
                let cwd = config.cwd.clone();
                // Resolve the gate once per turn so each tool execution shares
                // the same auditor/hook context. Default to PermissiveGate
                // when the host did not install one.
                let gate: SharedGate = config
                    .tool_gate
                    .clone()
                    .unwrap_or_else(|| Arc::new(PermissiveGate) as SharedGate);
                let futures: Vec<_> = tool_uses
                    .iter()
                    .map(|(id, name, args)| {
                        let tools = Arc::clone(&tools);
                        let gate = Arc::clone(&gate);
                        let id = id.clone();
                        let name = name.clone();
                        let input: serde_json::Value = serde_json::from_str(args)
                            .unwrap_or_else(|e| {
                                log::warn!("failed to parse tool args for {}: {}", name, e);
                                serde_json::Value::Object(Default::default())
                            });
                        let cwd = cwd.clone();
                        async move {
                            // Gate: before_execute may deny, in which case we
                            // synthesize an error ToolResult and skip the tool.
                            let decision = gate.before_execute(&name, &input, &cwd).await;
                            let exec_result = match decision {
                                GateDecision::Deny(reason) => {
                                    synapse_tools::ToolResult {
                                        content: format!("tool gate denied {}: {}", name, reason),
                                        is_error: true,
                                    }
                                }
                                GateDecision::Allow => match tools.get(&name) {
                                    Some(tool) => match tool.execute(input.clone(), &cwd).await {
                                        Ok(r) => r,
                                        Err(e) => synapse_tools::ToolResult {
                                            content: e.to_string(),
                                            is_error: true,
                                        },
                                    },
                                    None => synapse_tools::ToolResult {
                                        content: format!("unknown tool: {name}"),
                                        is_error: true,
                                    },
                                },
                            };
                            // Gate: after_execute is fire-and-forget. Even on
                            // denial we notify so audit/hook layers see the
                            // attempt + outcome.
                            gate.after_execute(&name, &input, &exec_result, &cwd).await;
                            (id, exec_result.content, exec_result.is_error)
                        }
                    })
                    .collect();

                let results = join_all(futures).await;

                // Persist assistant turn (with tool uses)
                let assistant_msg = ChatMessage {
                    role: Role::Assistant,
                    content: content_blocks.clone(),
                };
                persist_turn(&config, &assistant_msg, usage.input_tokens, usage.output_tokens);

                // Add to context with potentially truncated content, persist with full content
                {
                    let mut ctx = ctx.lock().await;
                    ctx.add_assistant(content_blocks.clone());
                    for (id, content, is_error) in &results {
                        let ctx_content = if config.max_tool_result_tokens > 0 {
                            truncate_tool_result(content, config.max_tool_result_tokens)
                        } else {
                            content.clone()
                        };
                        ctx.add_tool_result(id, &ctx_content, *is_error);
                    }
                }

                // Persist each tool result as a user turn with the full content
                for (id, content, is_error) in &results {
                    let tool_msg = ChatMessage {
                        role: Role::User,
                        content: vec![ContentBlock::ToolResult {
                            tool_use_id: id.clone(),
                            content: content.clone(),
                            is_error: *is_error,
                        }],
                    };
                    persist_turn(&config, &tool_msg, 0, 0);
                }

                // Yield the full (untruncated) content so the UI sees everything
                for (id, content, is_error) in results {
                    yield AgentEvent::ToolResult {
                        id: id.clone(),
                        content: content.clone(),
                        is_error,
                    };
                }
            }

            match stop_reason {
                StopReason::EndTurn | StopReason::MaxTokens | StopReason::StopSequence => {
                    if tool_uses.is_empty() {
                        // Persist final assistant text turn
                        let assistant_msg = ChatMessage {
                            role: Role::Assistant,
                            content: content_blocks.clone(),
                        };
                        persist_turn(&config, &assistant_msg, usage.input_tokens, usage.output_tokens);

                        let mut ctx = ctx.lock().await;
                        ctx.add_assistant(content_blocks);
                    }
                    yield AgentEvent::TurnEnd;
                    break;
                }
                StopReason::ToolUse => {
                    if tool_uses.is_empty() {
                        let assistant_msg = ChatMessage {
                            role: Role::Assistant,
                            content: content_blocks.clone(),
                        };
                        persist_turn(&config, &assistant_msg, usage.input_tokens, usage.output_tokens);

                        let mut ctx = ctx.lock().await;
                        ctx.add_assistant(content_blocks);
                        yield AgentEvent::TurnEnd;
                        break;
                    }
                }
            }
        }
    }
}

/// Single-shot agent loop (backwards compatible). Runs one message to completion.
pub fn agent_loop(
    config: AgentConfig,
    provider: Arc<dyn Provider + Send + Sync>,
    tools: Arc<ToolRegistry>,
    initial_message: String,
) -> impl Stream<Item = AgentEvent> + Send {
    agent_loop_with_pricing(config, provider, tools, initial_message, None)
}

/// Agent loop with optional pricing table for cost telemetry.
pub fn agent_loop_with_pricing(
    config: AgentConfig,
    provider: Arc<dyn Provider + Send + Sync>,
    tools: Arc<ToolRegistry>,
    initial_message: String,
    pricing: Option<Arc<PricingTable>>,
) -> impl Stream<Item = AgentEvent> + Send {
    let ctx = Arc::new(tokio::sync::Mutex::new(ConversationContext::new(
        config.system_prompt.clone(),
        tools.all_tool_schemas(),
    )));
    agent_turn_with_pricing(config, provider, tools, ctx, initial_message, pricing)
}
