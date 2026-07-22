pub mod chunking;
// Model staging (download.rs) and the local ONNX embedder exist only for the
// ml feature; the HTTP/OpenAI providers below cover remote-embedding builds.
#[cfg(feature = "ml")]
pub mod download;
pub mod http;
pub mod normalize;
#[cfg(feature = "ml")]
pub mod onnx;
pub mod openai;

use crate::Result;

/// Object-safe text-embedding backend (local ONNX, OpenAI-compatible HTTP,
/// or plain HTTP); consumers hold `Arc<dyn EmbeddingProvider>`.
#[allow(clippy::type_complexity)]
pub trait EmbeddingProvider: Send + Sync {
    /// Embed one text into the provider's vector space.
    fn embed<'a>(
        &'a self,
        text: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<f32>>> + Send + 'a>>;

    /// Embed a batch of texts. Default implementation calls `embed` sequentially.
    fn embed_batch<'a>(
        &'a self,
        texts: &'a [String],
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<Vec<f32>>>> + Send + 'a>>
    {
        Box::pin(async move {
            let mut results = Vec::with_capacity(texts.len());
            for text in texts {
                results.push(self.embed(text).await?);
            }
            Ok(results)
        })
    }
}
