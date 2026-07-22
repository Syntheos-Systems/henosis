//! Azure OpenAI provider.
//! Same OpenAI wire format but with Azure-specific URL structure and api-key auth.

use std::pin::Pin;

use anyhow::{Context, Result, bail};
use async_stream::stream;
use async_trait::async_trait;
use futures::Stream;
use reqwest_eventsource::{Event, RequestBuilderExt};

use crate::proxy::{OaiResponse, build_request, to_chat_response};
use crate::streaming::parse_openai_sse;
use crate::types::{ChatRequest, ChatResponse, Provider, StreamEvent};

const DEFAULT_API_VERSION: &str = "2024-10-21";

/// Sends chat requests to an Azure OpenAI deployment.
pub struct AzureProvider {
    client: reqwest::Client,
    endpoint: String,
    deployment: String,
    api_key: String,
    api_version: String,
}

/// Provides construction and endpoint helpers for Azure OpenAI.
impl AzureProvider {
    /// Creates a provider for one Azure OpenAI deployment.
    pub fn new(
        client: reqwest::Client,
        endpoint: String,
        deployment: String,
        api_key: String,
    ) -> Self {
        Self {
            client,
            endpoint: endpoint.trim_end_matches('/').to_string(),
            deployment,
            api_key,
            api_version: DEFAULT_API_VERSION.to_string(),
        }
    }

    /// Overrides the Azure OpenAI API version used in requests.
    pub fn with_api_version(mut self, version: String) -> Self {
        self.api_version = version;
        self
    }

    /// Builds the deployment-specific chat completions URL.
    fn url(&self) -> String {
        format!(
            "{}/openai/deployments/{}/chat/completions?api-version={}",
            self.endpoint, self.deployment, self.api_version
        )
    }
}

/// Implements synchronous and streaming chat requests for Azure OpenAI.
#[async_trait]
impl Provider for AzureProvider {
    /// Sends a non-streaming chat completion request.
    async fn send(&self, request: &ChatRequest) -> Result<ChatResponse> {
        let oai = build_request(request);
        let body = serde_json::to_string(&oai)?;

        let resp = self
            .client
            .post(self.url())
            .header("api-key", &self.api_key)
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
            bail!("azure error {status}: {text}");
        }

        let oai_resp: OaiResponse = resp.json().await.context("parse azure response")?;
        Ok(to_chat_response(oai_resp))
    }

    /// Opens a streaming chat completion request and converts its SSE events.
    fn send_streaming(
        &self,
        request: &ChatRequest,
    ) -> Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>> {
        let client = self.client.clone();
        let url = self.url();
        let api_key = self.api_key.clone();
        let mut oai = build_request(request);
        oai.stream = true;

        Box::pin(stream! {
            let body = match serde_json::to_string(&oai) {
                Ok(b) => b,
                Err(e) => { yield Err(e.into()); return; }
            };

            let rb = client.post(&url)
                .header("api-key", &api_key)
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
                        yield Err(anyhow::anyhow!("sse error: {e}"));
                        break;
                    }
                }
            }
        })
    }

    /// Returns the stable provider identifier.
    fn name(&self) -> &str {
        "azure"
    }
}
