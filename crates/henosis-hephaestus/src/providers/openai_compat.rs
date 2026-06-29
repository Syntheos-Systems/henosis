//! OpenAI-compatible provider wrapper. Delegates entirely to Synapse's
//! `ProxyProvider`, which already speaks the OpenAI chat-completions wire
//! shape (used by OpenAI proper, Ollama, Azure, OpenRouter, and most
//! aggregator endpoints). The Hephaestus wrapper exists so the orchestrator
//! sees a `Provider` resolved through Hephaestus's own factory, with a
//! Hephaestus-stable name reported via `Provider::name()`.
//!
//! Auth is intentionally simple: a static API key resolved at construction
//! time, either from the env var `HEPHAESTUS_PROVIDER_KEY` or from a credd
//! slot named in `HEPHAESTUS_PROVIDER_KEY_SLOT`. There is no token refresh
//! because the supported endpoints all use long-lived keys.

use std::pin::Pin;

use anyhow::Result;
use async_trait::async_trait;
use futures::Stream;
use reqwest::Client;
use synapse_provider::proxy::ProxyProvider;

use crate::provider::{ChatRequest, ChatResponse, Provider, StreamEvent};

/// Thin wrapper around `synapse_provider::proxy::ProxyProvider`. Exists so
/// Hephaestus controls the public name and constructor surface; behavior is
/// identical to the upstream proxy provider.
pub struct HephaestusProxyProvider {
    /// The upstream Synapse proxy provider that handles all wire-level logic.
    inner: ProxyProvider,
    /// Stable display name returned by `Provider::name()`.
    name: &'static str,
}

/// Constructor for the OpenAI-compatible proxy wrapper.
impl HephaestusProxyProvider {
    /// Build a proxy provider that POSTs to `{base_url}/chat/completions`
    /// with `Authorization: Bearer {api_key}`. The `display_name` is what
    /// `Provider::name()` returns; pick something stable for telemetry.
    pub fn new(
        http: Client,
        base_url: String,
        api_key: String,
        display_name: &'static str,
    ) -> Self {
        let inner = ProxyProvider::new(http, base_url, api_key).with_name(display_name);
        Self {
            inner,
            name: display_name,
        }
    }
}

/// Implementation of the generic `Provider` trait. Every method delegates
/// to the wrapped Synapse `ProxyProvider`.
#[async_trait]
impl Provider for HephaestusProxyProvider {
    /// Forward to the upstream proxy provider's non-streaming send.
    async fn send(&self, request: &ChatRequest) -> Result<ChatResponse> {
        self.inner.send(request).await
    }

    /// Forward to the upstream proxy provider's streaming send.
    fn send_streaming(
        &self,
        request: &ChatRequest,
    ) -> Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>> {
        self.inner.send_streaming(request)
    }

    /// Stable display name. Intentionally returns the wrapper-configured
    /// name rather than `inner.name()` so Hephaestus telemetry can choose
    /// e.g. `openai-compat` regardless of how the proxy presents itself.
    fn name(&self) -> &str {
        self.name
    }
}
