use serde::{Deserialize, Serialize};

/// Wire format selection for the HTTP reranker backend.
///
/// - `Cohere`: Cohere / Jina compatible. Request uses `documents` + `top_n`;
///   response is `{"results": [{"index": N, "relevance_score": F}]}`.
/// - `Tei`: Hugging Face Text Embeddings Inference compatible. Request uses
///   `texts`; response is `[{"index": N, "score": F}]` (no wrapper object).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RerankFormat {
    #[default]
    Cohere,
    Tei,
}

impl RerankFormat {
    /// Parse from `KLEOS_RERANKER_FORMAT` / `ENGRAM_RERANKER_FORMAT` env var value.
    pub fn parse(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "tei" | "huggingface" | "hf" => RerankFormat::Tei,
            _ => RerankFormat::Cohere,
        }
    }
}

// --- Cohere / Jina format ---

#[derive(Serialize)]
pub(super) struct CohereRerankRequest<'a> {
    #[serde(skip_serializing_if = "str::is_empty")]
    pub model: &'a str,
    pub query: &'a str,
    pub documents: Vec<&'a str>,
    pub top_n: usize,
}

#[derive(Deserialize)]
pub(super) struct CohereRerankResponse {
    pub results: Vec<CohereRerankResult>,
}

#[derive(Deserialize)]
pub(super) struct CohereRerankResult {
    pub index: usize,
    pub relevance_score: f64,
}

// --- TEI format ---

#[derive(Serialize)]
pub(super) struct TeiRerankRequest<'a> {
    pub query: &'a str,
    pub texts: Vec<&'a str>,
    pub truncate: bool,
}

/// TEI returns a flat array: `[{"index": N, "score": F}, ...]`
pub(super) type TeiRerankResponse = Vec<TeiRerankResult>;

#[derive(Deserialize)]
pub(super) struct TeiRerankResult {
    pub index: usize,
    pub score: f64,
}

// Unified result used internally after format-specific deserialization.
pub(super) struct RerankResult {
    pub index: usize,
    pub score: f64,
}

impl From<CohereRerankResult> for RerankResult {
    fn from(r: CohereRerankResult) -> Self {
        RerankResult {
            index: r.index,
            score: r.relevance_score,
        }
    }
}

impl From<TeiRerankResult> for RerankResult {
    fn from(r: TeiRerankResult) -> Self {
        RerankResult {
            index: r.index,
            score: r.score,
        }
    }
}
