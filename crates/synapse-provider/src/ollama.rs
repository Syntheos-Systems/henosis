//! Ollama local model provider.
//! Speaks OpenAI-compatible API at localhost:11434/v1. No auth required.

use std::pin::Pin;

use anyhow::{Context, Result, bail};
use async_stream::stream;
use async_trait::async_trait;
use futures::Stream;
use reqwest_eventsource::{Event, RequestBuilderExt};

use crate::proxy::{OaiResponse, build_request, to_chat_response};
use crate::streaming::parse_openai_sse;
use crate::types::{ChatRequest, ChatResponse, Provider, StreamEvent};

const DEFAULT_OLLAMA_URL: &str = "http://localhost:11434/v1";

/// Sends chat requests to an OpenAI-compatible Ollama server.
pub struct OllamaProvider {
    client: reqwest::Client,
    base_url: String,
}

/// Provides construction and endpoint helpers for Ollama.
impl OllamaProvider {
    /// Creates a provider targeting the default local Ollama endpoint.
    pub fn new(client: reqwest::Client) -> Self {
        Self {
            client,
            base_url: DEFAULT_OLLAMA_URL.to_string(),
        }
    }

    /// Creates a provider targeting a custom Ollama endpoint.
    pub fn with_url(client: reqwest::Client, base_url: String) -> Self {
        Self { client, base_url }
    }

    /// Builds the Ollama chat completions endpoint.
    fn endpoint(&self) -> String {
        format!("{}/chat/completions", self.base_url.trim_end_matches('/'))
    }
}

/// Implements synchronous and streaming chat requests for Ollama.
#[async_trait]
impl Provider for OllamaProvider {
    /// Sends a non-streaming chat completion request.
    async fn send(&self, request: &ChatRequest) -> Result<ChatResponse> {
        let oai = build_request(request);
        let body = serde_json::to_string(&oai)?;

        let resp = self
            .client
            .post(self.endpoint())
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
            bail!("ollama error {status}: {text}");
        }

        let oai_resp: OaiResponse = resp.json().await.context("parse ollama response")?;
        Ok(to_chat_response(oai_resp))
    }

    /// Opens a streaming chat completion request and converts its SSE events.
    fn send_streaming(
        &self,
        request: &ChatRequest,
    ) -> Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>> {
        let client = self.client.clone();
        let endpoint = self.endpoint();
        let mut oai = build_request(request);
        oai.stream = true;

        Box::pin(stream! {
            let body = match serde_json::to_string(&oai) {
                Ok(b) => b,
                Err(e) => { yield Err(e.into()); return; }
            };

            let rb = client.post(&endpoint)
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
                        break;
                    }
                    Err(e) => {
                        yield Err(anyhow::anyhow!("ollama sse error: {e}"));
                        break;
                    }
                }
            }
        })
    }

    /// Returns the stable provider identifier.
    fn name(&self) -> &str {
        "ollama"
    }
}
