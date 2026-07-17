//! Optional text-embedding capability for semantic echo and loop detection.
//!
//! The 2026-07-17 design spec (P4) recorded embedding-based echo/loop
//! detection as future work behind the `echo::similarity` seam; this module
//! is that capability landing. It deliberately implements a thin
//! OpenAI-compatible `/v1/embeddings` client instead of pulling the vendored
//! kleos-lib ML stack into the default bridge build: the same wire protocol
//! serves OpenAI, TEI, Ollama, and any locally hosted bge-class model, and
//! the [`Embedder`] trait keeps an in-process kleos-lib adapter possible
//! later without touching call sites.
//!
//! Everything here is optional at runtime: no `[embedding]` config block
//! means no embedder, and every caller falls back to the token-overlap path.

use async_trait::async_trait;
use serde::Deserialize;
use std::time::Duration;

use crate::error::BridgeError;

/// Ceiling on one embeddings request. Embed calls run inside the compose
/// floor and topic seeding; an unbounded call against a stalled endpoint
/// would freeze the whole room, not just this check (adversarial review
/// finding). A timeout surfaces as an error, and every caller degrades to
/// non-semantic behavior on error.
const EMBED_TIMEOUT: Duration = Duration::from_secs(10);

/// Turns text into a fixed-dimension vector. Implementations must be safe to
/// call concurrently; the bridge shares one embedder across echo detection
/// and topic-reignition checks.
#[async_trait]
pub trait Embedder: Send + Sync {
    /// Embed one text. Errors are expected operational events (endpoint down,
    /// model cold) and callers must degrade to non-semantic behavior rather
    /// than fail the surrounding operation.
    async fn embed(&self, text: &str) -> Result<Vec<f32>, BridgeError>;
}

/// One embedding row of an OpenAI-compatible embeddings response.
#[derive(Debug, Deserialize)]
struct EmbeddingRow {
    /// The vector itself.
    embedding: Vec<f32>,
}

/// Body shape of an OpenAI-compatible embeddings response; only the fields
/// the bridge reads.
#[derive(Debug, Deserialize)]
struct EmbeddingsResponse {
    /// One row per input; the bridge always sends a single input.
    data: Vec<EmbeddingRow>,
}

/// Embedder speaking the OpenAI `/v1/embeddings` wire protocol.
pub struct OpenAiEmbedder {
    /// Shared HTTP client.
    client: reqwest::Client,
    /// Full endpoint URL (e.g. `http://127.0.0.1:11434/v1/embeddings`).
    url: String,
    /// Model identifier passed through to the endpoint.
    model: String,
    /// Optional bearer token; local TEI/Ollama endpoints need none.
    api_key: Option<String>,
}

/// Construction and the single-request embed primitive.
impl OpenAiEmbedder {
    /// Build an embedder against a full endpoint URL and model name. The
    /// HTTP client carries [`EMBED_TIMEOUT`] so a stalled endpoint errors
    /// out instead of holding the compose floor indefinitely.
    pub fn new(url: String, model: String, api_key: Option<String>) -> Self {
        let client = reqwest::Client::builder()
            .timeout(EMBED_TIMEOUT)
            .build()
            .unwrap_or_else(|e| {
                tracing::warn!("embedder client build failed ({e}), using default client");
                reqwest::Client::new()
            });
        Self {
            client,
            url,
            model,
            api_key,
        }
    }
}

/// Implements the embed operation over the OpenAI-compatible wire protocol.
#[async_trait]
impl Embedder for OpenAiEmbedder {
    /// POST one input and return its vector. A well-formed response with no
    /// rows is an error: an empty vector must never score as similarity 0
    /// silently when the endpoint is misbehaving.
    async fn embed(&self, text: &str) -> Result<Vec<f32>, BridgeError> {
        let mut req = self.client.post(&self.url).json(&serde_json::json!({
            "model": self.model,
            "input": text,
        }));
        if let Some(key) = &self.api_key {
            req = req.bearer_auth(key);
        }
        let resp = req.send().await?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(BridgeError::Embedding(format!(
                "embeddings endpoint returned {status}: {body}"
            )));
        }
        let body: EmbeddingsResponse = resp.json().await?;
        body.data
            .into_iter()
            .next()
            .map(|row| row.embedding)
            .ok_or_else(|| BridgeError::Embedding("embeddings response had no rows".into()))
    }
}

/// Cosine similarity of two vectors. Dimension mismatches and zero-norm
/// vectors score 0.0 instead of panicking or producing NaN: a broken
/// embedding must never suppress a message on its own.
pub fn cosine(a: &[f32], b: &[f32]) -> f64 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let mut dot = 0.0f64;
    let mut norm_a = 0.0f64;
    let mut norm_b = 0.0f64;
    for (x, y) in a.iter().zip(b.iter()) {
        dot += f64::from(*x) * f64::from(*y);
        norm_a += f64::from(*x) * f64::from(*x);
        norm_b += f64::from(*y) * f64::from(*y);
    }
    if norm_a == 0.0 || norm_b == 0.0 {
        return 0.0;
    }
    dot / (norm_a.sqrt() * norm_b.sqrt())
}

/// Unit tests for the cosine guard rails and response parsing.
#[cfg(test)]
mod tests {
    use super::{cosine, EmbeddingsResponse};

    /// Verifies identical vectors score 1.0 and orthogonal vectors 0.0.
    #[test]
    fn test_cosine_identity_and_orthogonality() {
        assert!((cosine(&[1.0, 0.0], &[1.0, 0.0]) - 1.0).abs() < 1e-9);
        assert!(cosine(&[1.0, 0.0], &[0.0, 1.0]).abs() < 1e-9);
    }

    /// Verifies dimension mismatch and zero-norm vectors score 0.0, not NaN
    /// (spec edge case: a broken embedding must never suppress on its own).
    #[test]
    fn test_cosine_degenerate_inputs_score_zero() {
        assert_eq!(cosine(&[1.0, 2.0], &[1.0]), 0.0);
        assert_eq!(cosine(&[], &[]), 0.0);
        assert_eq!(cosine(&[0.0, 0.0], &[1.0, 1.0]), 0.0);
    }

    /// Verifies opposite vectors score -1.0 (callers threshold on >= so
    /// negatives never suppress).
    #[test]
    fn test_cosine_opposite_vectors() {
        assert!((cosine(&[1.0, 0.0], &[-1.0, 0.0]) + 1.0).abs() < 1e-9);
    }

    /// Verifies the OpenAI-compatible response shape parses down to the
    /// vector the bridge needs.
    #[test]
    fn test_embeddings_response_parses() {
        let json = r#"{"object":"list","data":[{"object":"embedding","index":0,"embedding":[0.1,0.2]}],"model":"m"}"#;
        let parsed: EmbeddingsResponse = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.data[0].embedding.len(), 2);
    }
}
