//! Shared test scaffolding for the session runtime: a scriptable stub provider
//! and a manager builder. Compiled only under `cfg(test)`.

#![cfg(test)]

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use futures::stream;
use synapse_provider::{
    ChatRequest, ChatResponse, ContentBlock, Provider, StopReason, StreamEvent, Usage,
};
use synapse_tools::ToolRegistry;

use crate::cost::PricingTable;
use crate::types::AgentConfig;

use super::manager::SessionManager;

/// Provider that emits a fixed text delta then stops. No network.
pub struct StubProvider {
    pub reply: String,
}

#[async_trait::async_trait]
impl Provider for StubProvider {
    fn name(&self) -> &str {
        "stub"
    }

    async fn send(&self, _req: &ChatRequest) -> Result<ChatResponse> {
        Ok(ChatResponse {
            id: "stub".into(),
            content: vec![ContentBlock::Text {
                text: self.reply.clone(),
            }],
            stop_reason: StopReason::EndTurn,
            usage: Usage {
                input_tokens: 3,
                output_tokens: 2,
                ..Default::default()
            },
        })
    }

    fn send_streaming(
        &self,
        _req: &ChatRequest,
    ) -> std::pin::Pin<Box<dyn futures::Stream<Item = Result<StreamEvent>> + Send>> {
        let reply = self.reply.clone();
        Box::pin(stream::iter(vec![
            Ok(StreamEvent::ContentDelta(reply)),
            Ok(StreamEvent::Usage(Usage {
                input_tokens: 3,
                output_tokens: 2,
                ..Default::default()
            })),
            Ok(StreamEvent::MessageStop(StopReason::EndTurn)),
        ]))
    }
}

/// A base `AgentConfig` with everything off except model/system/limits.
pub fn base_config() -> AgentConfig {
    AgentConfig {
        model: "stub-model".into(),
        system_prompt: "test agent".into(),
        cwd: PathBuf::from("/tmp"),
        max_turns: 4,
        max_tokens: 256,
        session_store: None,
        session_id: None,
        depth: 0,
        compression: None,
        router: None,
        max_tool_result_tokens: 0,
        tool_gate: None,
        hooks: None,
    }
}

/// Build a manager backed by the stub provider and an empty tool registry.
pub fn stub_manager() -> Arc<SessionManager> {
    let provider: Arc<dyn Provider + Send + Sync> = Arc::new(StubProvider {
        reply: "hello from stub".into(),
    });
    let tools = Arc::new(ToolRegistry::new());
    let pricing = Arc::new(PricingTable::load());
    SessionManager::new(provider, tools, pricing, None, base_config())
}
