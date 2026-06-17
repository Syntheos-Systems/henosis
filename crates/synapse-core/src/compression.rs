//! Context compression: threshold-triggered LLM summarization of old turns.
//!
//! When the conversation exceeds a token threshold, older messages are replaced
//! with a concise LLM-generated summary, preserving recent context intact.

use crate::context::ConversationContext;
use synapse_provider::{ChatMessage, ChatRequest, ContentBlock, Provider, Role};

/// Configuration for context compression.
#[derive(Debug, Clone)]
pub struct CompressionConfig {
    /// Compress when estimated tokens exceed this fraction of context_window (default 0.5).
    pub threshold_ratio: f32,
    /// Number of recent messages to always preserve (default 6).
    pub preserve_recent: usize,
    /// Maximum context window in tokens for the model (default 200_000).
    pub context_window: usize,
    /// Model to use for summarization (uses the same model as the agent).
    pub model: String,
    /// Max tokens for the summary response (default 1024).
    pub summary_max_tokens: u32,
}

impl Default for CompressionConfig {
    fn default() -> Self {
        Self {
            threshold_ratio: 0.5,
            preserve_recent: 6,
            context_window: 200_000,
            model: "claude-sonnet-4-20250514".to_string(),
            summary_max_tokens: 1024,
        }
    }
}

impl CompressionConfig {
    /// Token threshold that triggers compression.
    pub fn threshold_tokens(&self) -> usize {
        (self.context_window as f32 * self.threshold_ratio) as usize
    }
}

const SUMMARIZE_PROMPT: &str = "\
Summarize the following conversation concisely, preserving:
- Key decisions made and their rationale
- Important facts discovered (file paths, function names, error messages, config values)
- Current task state (what was done, what remains)
- Any constraints or rules established

Be factual and dense. No filler. Prioritize technical details over narrative.
Output only the summary, no preamble.";

/// Check if compression is needed and perform it if so.
///
/// Returns `true` if compression was performed, `false` if not needed.
/// Errors are logged but not propagated -- the agent continues either way.
pub async fn maybe_compress(
    ctx: &mut ConversationContext,
    config: &CompressionConfig,
    provider: &(dyn Provider + Send + Sync),
) -> bool {
    let current_tokens = ctx.estimate_tokens();
    let threshold = config.threshold_tokens();

    if current_tokens < threshold {
        return false;
    }

    let msg_count = ctx.message_count();
    if msg_count <= config.preserve_recent + 2 {
        // Not enough messages to compress (need at least some old + preserved recent)
        return false;
    }

    log::info!(
        "context compression triggered: ~{current_tokens} tokens > {threshold} threshold, \
         {msg_count} messages (preserving last {})",
        config.preserve_recent,
    );

    // Extract old messages (everything except the most recent preserve_recent)
    let messages = ctx.messages();
    let split_at = messages.len().saturating_sub(config.preserve_recent);
    let old_messages = &messages[..split_at];

    // Build the content to summarize
    let mut conversation_text = String::new();
    for msg in old_messages {
        let role = match msg.role {
            Role::User => "User",
            Role::Assistant => "Assistant",
            Role::System => "System",
            Role::Tool => "Tool",
        };
        for block in &msg.content {
            match block {
                ContentBlock::Text { text } => {
                    conversation_text.push_str(&format!("[{role}]: {text}\n\n"));
                }
                ContentBlock::ToolUse { name, input, .. } => {
                    // Truncate large tool inputs
                    let input_str = input.to_string();
                    let truncated = if input_str.len() > 500 {
                        format!("{}...", &input_str[..500])
                    } else {
                        input_str
                    };
                    conversation_text.push_str(&format!("[{role} -> {name}]: {truncated}\n\n"));
                }
                ContentBlock::ToolResult {
                    content, is_error, ..
                } => {
                    let prefix = if *is_error { "ERROR" } else { "Result" };
                    // Truncate large tool results
                    let truncated = if content.len() > 1000 {
                        format!("{}...", &content[..1000])
                    } else {
                        content.clone()
                    };
                    conversation_text.push_str(&format!("[{prefix}]: {truncated}\n\n"));
                }
            }
        }
    }

    // Build summarization request
    let request = ChatRequest {
        model: config.model.clone(),
        messages: vec![ChatMessage {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: format!("{SUMMARIZE_PROMPT}\n\n---\n\n{conversation_text}"),
            }],
        }],
        max_tokens: config.summary_max_tokens,
        system: None,
        tools: None,
        stream: false,
    };

    match provider.send(&request).await {
        Ok(response) => {
            // Extract text from response content blocks
            let summary: String = response
                .content
                .iter()
                .filter_map(|b| {
                    if let ContentBlock::Text { text } = b {
                        Some(text.as_str())
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>()
                .join("\n");

            if summary.is_empty() {
                log::warn!("compression produced empty summary, skipping");
                return false;
            }

            let old_tokens = ctx.estimate_tokens();
            ctx.replace_with_summary(summary, config.preserve_recent);
            let new_tokens = ctx.estimate_tokens();

            log::info!(
                "context compressed: ~{old_tokens} -> ~{new_tokens} tokens \
                 ({} messages -> {})",
                msg_count,
                ctx.message_count(),
            );
            true
        }
        Err(e) => {
            log::warn!("context compression failed (non-fatal): {e}");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_context(n_messages: usize) -> ConversationContext {
        let mut ctx = ConversationContext::new("system".into(), vec![]);
        for i in 0..n_messages {
            if i % 2 == 0 {
                ctx.add_user(&format!(
                    "User message {i} with some content to pad token count: {}",
                    "x".repeat(100)
                ));
            } else {
                ctx.add_assistant(vec![ContentBlock::Text {
                    text: format!("Assistant response {i} with padding: {}", "y".repeat(100)),
                }]);
            }
        }
        ctx
    }

    #[test]
    fn replace_with_summary_preserves_recent() {
        let mut ctx = make_context(10);
        assert_eq!(ctx.message_count(), 10);

        ctx.replace_with_summary("Summary of old conversation.".into(), 4);

        // 2 (summary pair) + 4 (preserved) = 6
        assert_eq!(ctx.message_count(), 6);

        // First message should be the summary prompt
        let msgs = ctx.messages();
        assert!(matches!(msgs[0].role, Role::User));
        assert!(matches!(msgs[1].role, Role::Assistant));

        // Check summary content
        if let ContentBlock::Text { text } = &msgs[1].content[0] {
            assert_eq!(text, "Summary of old conversation.");
        } else {
            panic!("expected text block");
        }
    }

    #[test]
    fn replace_with_summary_noop_when_few_messages() {
        let mut ctx = make_context(3);
        ctx.replace_with_summary("ignored".into(), 5);
        // Should not compress since we have fewer messages than keep_recent
        assert_eq!(ctx.message_count(), 3);
    }

    #[test]
    fn threshold_tokens_calculation() {
        let config = CompressionConfig {
            threshold_ratio: 0.5,
            context_window: 200_000,
            ..Default::default()
        };
        assert_eq!(config.threshold_tokens(), 100_000);

        let config2 = CompressionConfig {
            threshold_ratio: 0.3,
            context_window: 128_000,
            ..Default::default()
        };
        assert_eq!(config2.threshold_tokens(), 38_400);
    }
}
