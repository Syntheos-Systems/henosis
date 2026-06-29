//! Provider trait + types re-exported from synapse-provider. The orchestrator
//! and every Hephaestus consumer depends only on this surface; it must never
//! import `synapse_provider::*` directly so the alignment point is explicit
//! and a future swap (vendor, fork, version-pin) only touches this module.
//!
//! The trait, content blocks, request/response shapes, and stream events are
//! used verbatim from synapse-provider so Hephaestus and Synapse cannot drift
//! on provider semantics.

pub use synapse_provider::types::{
    ChatMessage, ChatRequest, ChatResponse, ContentBlock, Provider, Role, StopReason, StreamEvent,
    Usage,
};

// Re-export the SSE parsers as well: the orchestrator does not parse SSE
// directly (each provider does), but tests and the streaming task layer may
// need to assemble responses from a stream of events.
pub use synapse_provider::streaming::{events_to_response, parse_anthropic_sse, parse_openai_sse};
