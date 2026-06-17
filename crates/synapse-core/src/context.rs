//! Conversation context management.

use serde_json::Value;
use synapse_provider::{ChatMessage, ChatRequest, ContentBlock, Role};

use crate::cost::SessionCost;

/// Maintains the rolling conversation history and builds API requests.
pub struct ConversationContext {
    system: String,
    messages: Vec<ChatMessage>,
    tools: Vec<Value>,
    /// Accumulated cost telemetry for this session.
    pub session_cost: SessionCost,
}

impl ConversationContext {
    pub fn new(system: String, tools: Vec<Value>) -> Self {
        Self {
            system,
            messages: Vec::new(),
            tools,
            session_cost: SessionCost::new(),
        }
    }

    /// Create a context pre-loaded with historical messages (for session resume).
    pub fn from_history(system: String, tools: Vec<Value>, messages: Vec<ChatMessage>) -> Self {
        Self {
            system,
            messages,
            tools,
            session_cost: SessionCost::new(),
        }
    }

    /// Append a user turn with plain text content.
    pub fn add_user(&mut self, text: &str) {
        self.messages.push(ChatMessage {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: text.to_string(),
            }],
        });
    }

    /// Append an assistant turn with the full content block list returned by the LLM.
    pub fn add_assistant(&mut self, content: Vec<ContentBlock>) {
        self.messages.push(ChatMessage {
            role: Role::Assistant,
            content,
        });
    }

    /// Append a user turn carrying one or more tool results.
    pub fn add_tool_result(&mut self, tool_use_id: &str, content: &str, is_error: bool) {
        // Anthropic requires tool results in a user message.
        // If the last message is already a user message containing only tool
        // results, append into it; otherwise open a new user message.
        let result_block = ContentBlock::ToolResult {
            tool_use_id: tool_use_id.to_string(),
            content: content.to_string(),
            is_error,
        };

        if let Some(last) = self.messages.last_mut()
            && matches!(last.role, Role::User)
            && last
                .content
                .iter()
                .all(|b| matches!(b, ContentBlock::ToolResult { .. }))
        {
            last.content.push(result_block);
            return;
        }

        self.messages.push(ChatMessage {
            role: Role::User,
            content: vec![result_block],
        });
    }

    /// Build a ChatRequest from the current context.
    pub fn to_request(&self, model: &str, max_tokens: u32, stream: bool) -> ChatRequest {
        ChatRequest {
            model: model.to_string(),
            messages: self.messages.clone(),
            max_tokens,
            system: Some(self.system.clone()),
            tools: if self.tools.is_empty() {
                None
            } else {
                Some(self.tools.clone())
            },
            stream,
        }
    }

    pub fn message_count(&self) -> usize {
        self.messages.len()
    }

    /// Rough token estimate: total characters / 4.
    pub fn estimate_tokens(&self) -> usize {
        let chars: usize = self
            .messages
            .iter()
            .flat_map(|m| m.content.iter())
            .map(|b| match b {
                ContentBlock::Text { text } => text.len(),
                ContentBlock::ToolUse { name, input, .. } => name.len() + input.to_string().len(),
                ContentBlock::ToolResult { content, .. } => content.len(),
            })
            .sum();
        chars / 4
    }

    /// Access raw messages (for compression inspection).
    pub fn messages(&self) -> &[ChatMessage] {
        &self.messages
    }

    /// Replace the system prompt mid-session. Used by the CLI's per-turn
    /// FSRS recall injector and by `/persona` so the persona swap takes
    /// effect on the very next turn rather than the next session.
    pub fn set_system(&mut self, system: String) {
        self.system = system;
    }

    /// Current system prompt. Mostly useful for diagnostics -- writing
    /// goes through `set_system` so callers can't accidentally mutate
    /// the field without committing to the update path.
    pub fn system(&self) -> &str {
        &self.system
    }

    /// Replace all messages except the most recent `keep_recent` with a single
    /// assistant summary message. Used by context compression.
    pub fn replace_with_summary(&mut self, summary: String, keep_recent: usize) {
        if self.messages.len() <= keep_recent {
            return;
        }
        let split_at = self.messages.len() - keep_recent;
        let recent = self.messages.split_off(split_at);
        self.messages.clear();
        self.messages.push(ChatMessage {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: "[Previous conversation summary]".to_string(),
            }],
        });
        self.messages.push(ChatMessage {
            role: Role::Assistant,
            content: vec![ContentBlock::Text { text: summary }],
        });
        self.messages.extend(recent);
    }
}
