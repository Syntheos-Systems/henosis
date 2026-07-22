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
    /// Build the POST request to the configured `/v1/embeddings` endpoint,
    /// attaching the model name when set and the Bearer token when present.
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

    /// Parse one response item's `embedding` array into a `Vec<f32>`, erroring
    /// when its length does not match the configured `dim`.
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

/// Embed text through an OpenAI-compatible `/v1/embeddings` endpoint, both the
/// single-text and batched paths.
impl EmbeddingProvider for OpenAiProvider {
    /// Embed a single text and return its dimension-checked vector.
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

    /// Embed a batch of texts in one request, returning vectors aligned to the
    /// input order (the response is reordered by each item's `index`).
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

                // The embeddings API returns one object per input, each carrying an
                // explicit `index`, and does not guarantee they arrive in request
                // order. Mapping by array position would silently pair an embedding
                // with the wrong text, so require exactly one item per input and
                // sort by `index` before parsing. Absent `index` falls back to array
                // position so a stricter-order provider still yields a stable result.
                if data.len() != texts.len() {
                    return Err(EngError::Internal(format!(
                        "embed_batch: expected {} embeddings, got {}",
                        texts.len(),
                        data.len()
                    )));
                }
                let mut indexed: Vec<(u64, &serde_json::Value)> = data
                    .iter()
                    .enumerate()
                    .map(|(pos, item)| (item["index"].as_u64().unwrap_or(pos as u64), item))
                    .collect();
                indexed.sort_by_key(|(idx, _)| *idx);
                indexed
                    .into_iter()
                    .map(|(_, item)| Self::parse_embedding(item, self.dim, "embed_batch"))
                    .collect()
            }
            .instrument(span),
        )
    }
}
