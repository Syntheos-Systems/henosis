//! Optional text-embedding capability for semantic echo and loop detection.
//!
//! The 2026-07-17 design spec (P4) recorded embedding-based echo/loop
//! detection as future work behind the `echo::similarity` seam; this module
//! is that capability landing. The default build implements a thin
//! OpenAI-compatible `/v1/embeddings` client instead of pulling in the vendored
//! kleos-lib ML stack. The optional `cognition` feature adds an adapter around
//! Henosis's in-process provider, letting memory and room semantics share one
//! ONNX session without changing call sites.
//!
//! Everything remains optional at runtime: explicit HTTP configuration selects
//! the wire client, cognition plus in-process Kleos selects the shared local
//! provider, and every caller falls back to token overlap when neither exists.

use async_trait::async_trait;
use serde::Deserialize;
#[cfg(feature = "cognition")]
use std::sync::Arc;
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

/// Adapter that lets Rift semantic checks use Henosis's in-process vendored
/// Kleos embedding provider without creating another model session.
#[cfg(feature = "cognition")]
pub struct CognitionEmbedder {
    /// Provider shared with the `Cognition` memory facade.
    provider: Arc<dyn henosis_cognition::EmbeddingProvider>,
}

/// Constructs the Rift-facing adapter around a shared cognition provider.
#[cfg(feature = "cognition")]
impl CognitionEmbedder {
    /// Wrap an existing provider. Cloning the input `Arc` preserves one model
    /// instance across cognition and room semantics.
    pub fn new(provider: Arc<dyn henosis_cognition::EmbeddingProvider>) -> Self {
        Self { provider }
    }
}

/// Delegates Rift embedding requests into the shared Kleos provider.
#[cfg(feature = "cognition")]
#[async_trait]
impl Embedder for CognitionEmbedder {
    /// Embed one text through the in-process provider, translating the vendored
    /// Kleos error into the bridge's operational embedding error.
    async fn embed(&self, text: &str) -> Result<Vec<f32>, BridgeError> {
        self.provider
            .embed(text)
            .await
            .map_err(|error| BridgeError::Embedding(error.to_string()))
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
    #[cfg(feature = "cognition")]
    use super::{CognitionEmbedder, Embedder};
    #[cfg(feature = "cognition")]
    use std::sync::atomic::{AtomicUsize, Ordering};
    #[cfg(feature = "cognition")]
    use std::sync::Arc;

    /// Stub vendored provider used to prove the Rift adapter delegates through
    /// the exact shared `Arc` instead of constructing independent state.
    #[cfg(feature = "cognition")]
    struct StubCognitionProvider {
        /// Number of embed calls observed through every clone of the provider.
        calls: Arc<AtomicUsize>,
    }

    /// Returns a fixed vector while recording calls through shared state.
    #[cfg(feature = "cognition")]
    impl henosis_cognition::EmbeddingProvider for StubCognitionProvider {
        /// Record one call and return the deterministic test vector.
        fn embed<'a>(
            &'a self,
            _text: &'a str,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<Output = henosis_cognition::KleosResult<Vec<f32>>>
                    + Send
                    + 'a,
            >,
        > {
            Box::pin(async move {
                self.calls.fetch_add(1, Ordering::SeqCst);
                Ok(vec![0.25, 0.75])
            })
        }
    }

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

    /// Verifies the cognition adapter reaches the shared provider instance and
    /// returns its vector unchanged.
    #[cfg(feature = "cognition")]
    #[tokio::test]
    async fn test_cognition_adapter_delegates_to_shared_provider() {
        let calls = Arc::new(AtomicUsize::new(0));
        let provider: Arc<dyn henosis_cognition::EmbeddingProvider> =
            Arc::new(StubCognitionProvider {
                calls: Arc::clone(&calls),
            });
        let adapter = CognitionEmbedder::new(Arc::clone(&provider));

        assert_eq!(adapter.embed("shared").await.unwrap(), vec![0.25, 0.75]);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(Arc::strong_count(&provider), 2);
    }
}
