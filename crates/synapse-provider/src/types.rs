use std::pin::Pin;

use anyhow::Result;
use async_trait::async_trait;
use futures::Stream;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ContentBlock {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "tool_use")]
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    #[serde(rename = "tool_result")]
    ToolResult {
        tool_use_id: String,
        content: String,
        is_error: bool,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: Role,
    pub content: Vec<ContentBlock>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<ChatMessage>,
    pub max_tokens: u32,
    pub system: Option<String>,
    pub tools: Option<Vec<serde_json::Value>>,
    pub stream: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatResponse {
    pub id: String,
    pub content: Vec<ContentBlock>,
    pub stop_reason: StopReason,
    pub usage: Usage,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    ToolUse,
    MaxTokens,
    StopSequence,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Usage {
    /// Total input tokens for the call, INCLUDING cache reads and cache writes.
    pub input_tokens: u32,
    /// Output (completion) tokens generated.
    pub output_tokens: u32,
    /// Subset of `input_tokens` served from cache at the discounted read rate.
    #[serde(default)]
    pub cache_read_tokens: u32,
    /// Subset of `input_tokens` written to cache at the (provider-specific) write
    /// rate. Zero for providers with no separate cache-write charge (e.g. OpenAI).
    #[serde(default)]
    pub cache_write_tokens: u32,
}

/// SSE stream event from a provider.
#[derive(Debug, Clone)]
pub enum StreamEvent {
    ContentDelta(String),
    ToolUseStart { id: String, name: String },
    ToolUseInputDelta(String),
    MessageStop(StopReason),
    Usage(Usage),
    Error(String),
}

/// Unified LLM provider interface.
#[async_trait]
pub trait Provider: Send + Sync {
    async fn send(&self, request: &ChatRequest) -> Result<ChatResponse>;
    fn send_streaming(
        &self,
        request: &ChatRequest,
    ) -> Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>;
    fn name(&self) -> &str;
}
