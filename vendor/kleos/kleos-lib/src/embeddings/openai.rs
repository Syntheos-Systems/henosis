use crate::embeddings::EmbeddingProvider;
use crate::{EngError, Result};

/// OpenAI-compatible embeddings provider.
///
/// Works with any `/v1/embeddings`-compatible endpoint: OpenAI, TEI,
/// Ollama, Azure OpenAI, and others. Validates that the returned
/// dimension matches the configured `dim`.
///
/// Configured via:
///   `KLEOS_EMBEDDING_URL`     — full endpoint URL (required)
///   `KLEOS_EMBEDDING_API_KEY` — Bearer token (optional; falls back to `LLM_API_KEY`)
///   `KLEOS_EMBEDDING_MODEL`   — model name (optional; omitted from request when unset)
///   `KLEOS_EMBEDDING_DIM`     — expected vector dimension (default: 1024)
pub struct OpenAiProvider {
    http: reqwest::Client,
    pub url: String,
    api_key: Option<String>,
    model: Option<String>,
    dim: usize,
}

impl OpenAiProvider {
    /// Create a provider for any OpenAI-compatible `/v1/embeddings` endpoint.
    ///
    /// - `url`: full endpoint URL (e.g. `http://tei:8080/v1/embeddings`)
    /// - `api_key`: optional Bearer token
    /// - `model`: optional model name; omitted from request when `None`
    /// - `dim`: expected embedding dimension for validation
    pub fn new(
        http: reqwest::Client,
        url: String,
        api_key: Option<String>,
        model: Option<String>,
        dim: usize,
    ) -> Self {
        OpenAiProvider {
            http,
            url,
            api_key,
            model,
            dim,
        }
    }

    /// Construct from environment variables.
    ///
    /// Returns `None` when `KLEOS_EMBEDDING_URL` is not set.
    pub fn from_env(http: reqwest::Client, dim: usize) -> Option<Self> {
        let url = std::env::var("KLEOS_EMBEDDING_URL").ok()?;
        let api_key = std::env::var("KLEOS_EMBEDDING_API_KEY")
            .ok()
            .or_else(|| std::env::var("KLEOS_LLM_API_KEY").ok())
            .or_else(|| std::env::var("LLM_API_KEY").ok());
        let model = std::env::var("KLEOS_EMBEDDING_MODEL").ok();
        Some(Self::new(http, url, api_key, model, dim))
    }
}

impl OpenAiProvider {
    fn build_request(&self, input: serde_json::Value) -> reqwest::RequestBuilder {
        let mut body = serde_json::json!({ "input": input });
        if let Some(ref model) = self.model {
            body["model"] = serde_json::Value::String(model.clone());
        }
        let mut req = self.http.post(&self.url).json(&body);
        if let Some(ref key) = self.api_key {
            req = req.header("Authorization", format!("Bearer {}", key));
        }
        req
    }

    fn parse_embedding(item: &serde_json::Value, dim: usize, ctx: &str) -> Result<Vec<f32>> {
        let embedding: Vec<f32> = item["embedding"]
            .as_array()
            .ok_or_else(|| EngError::Internal(format!("{}: missing embedding field", ctx)))?
            .iter()
            .filter_map(|v| v.as_f64().map(|f| f as f32))
            .collect();
        if embedding.len() != dim {
            return Err(EngError::Internal(format!(
                "{}: dimension mismatch: expected {}, got {}",
                ctx,
                dim,
                embedding.len()
            )));
        }
        Ok(embedding)
    }
}

impl EmbeddingProvider for OpenAiProvider {
    fn embed<'a>(
        &'a self,
        text: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<f32>>> + Send + 'a>> {
        use tracing::Instrument;
        let span = tracing::info_span!(
            "embed_openai_compat",
            url = %self.url,
            text_len = text.len(),
            dim = self.dim
        );
        Box::pin(
            async move {
                let resp = self
                    .build_request(serde_json::Value::String(text.to_string()))
                    .send()
                    .await
                    .map_err(|e| EngError::Internal(format!("embed network: {}", e)))?;

                if !resp.status().is_success() {
                    let status = resp.status();
                    let body = resp.text().await.unwrap_or_default();
                    return Err(EngError::Internal(format!("embed {}: {}", status, body)));
                }

                let body: serde_json::Value = resp
                    .json()
                    .await
                    .map_err(|e| EngError::Internal(format!("embed parse: {}", e)))?;

                Self::parse_embedding(&body["data"][0], self.dim, "embed")
            }
            .instrument(span),
        )
    }

    fn embed_batch<'a>(
        &'a self,
        texts: &'a [String],
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<Vec<f32>>>> + Send + 'a>>
    {
        use tracing::Instrument;
        let span = tracing::info_span!(
            "embed_openai_compat_batch",
            url = %self.url,
            batch_size = texts.len(),
            dim = self.dim
        );
        Box::pin(
            async move {
                if texts.is_empty() {
                    return Ok(vec![]);
                }
                let input = serde_json::Value::Array(
                    texts
                        .iter()
                        .map(|t| serde_json::Value::String(t.clone()))
                        .collect(),
                );
                let resp = self
                    .build_request(input)
                    .send()
                    .await
                    .map_err(|e| EngError::Internal(format!("embed_batch network: {}", e)))?;

                if !resp.status().is_success() {
                    let status = resp.status();
                    let body = resp.text().await.unwrap_or_default();
                    return Err(EngError::Internal(format!(
                        "embed_batch {}: {}",
                        status, body
                    )));
                }

                let body: serde_json::Value = resp
                    .json()
                    .await
                    .map_err(|e| EngError::Internal(format!("embed_batch parse: {}", e)))?;

                let data = body["data"].as_array().ok_or_else(|| {
                    EngError::Internal("embed_batch: missing data array".to_string())
                })?;

                data.iter()
                    .map(|item| Self::parse_embedding(item, self.dim, "embed_batch"))
                    .collect()
            }
            .instrument(span),
        )
    }
}
