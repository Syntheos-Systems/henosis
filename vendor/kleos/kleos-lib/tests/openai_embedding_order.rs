//! Regression for the OpenAI-compatible embeddings provider's response ordering.
//!
//! The `/v1/embeddings` API returns one object per input, each carrying an
//! explicit `index`, and does not guarantee they arrive in request order. Before
//! the fix, `embed_batch` mapped items by array position, so an out-of-order
//! response paired every embedding with the wrong text -- silent corruption that
//! poisons search and dedup with no error. These tests stand up a mock endpoint
//! (the established axum-on-a-random-port idiom) that deliberately returns items
//! reversed with correct indices, and assert the provider restores input order;
//! a response with the wrong item count is rejected rather than silently mapped.

use kleos_lib::embeddings::openai::OpenAiProvider;
use kleos_lib::embeddings::EmbeddingProvider;

const DIM: usize = 8;

/// Spawn a mock `/v1/embeddings` server. Each returned item encodes its input
/// index in embedding slot 0 (the rest zero) and is returned in REVERSE order
/// with its true `index` set, so a position-based mapping mis-pairs every
/// embedding. When `drop_last` is set the server returns one fewer item than
/// requested, to exercise the count-mismatch guard.
async fn spawn_mock_embeddings(drop_last: bool) -> String {
    use axum::{routing::post, Json, Router};
    use serde_json::{json, Value};

    let app = Router::new().route(
        "/v1/embeddings",
        post(move |Json(body): Json<Value>| async move {
            let inputs = body["input"].as_array().cloned().unwrap_or_default();
            let n = inputs.len();
            let take = if drop_last && n > 0 { n - 1 } else { n };
            let mut data: Vec<Value> = (0..take)
                .map(|i| {
                    let mut emb = vec![0.0f64; DIM];
                    emb[0] = i as f64;
                    json!({ "index": i, "embedding": emb })
                })
                .collect();
            // Deliberately out of order: the client must reorder by `index`.
            data.reverse();
            Json(json!({ "data": data }))
        }),
    );

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{}/v1/embeddings", addr)
}

/// An out-of-order response must be reordered by `index` so each embedding lines
/// up with its input text.
#[tokio::test]
async fn embed_batch_restores_input_order() {
    let url = spawn_mock_embeddings(false).await;
    let provider = OpenAiProvider::new(reqwest::Client::new(), url, None, None, DIM);

    let texts: Vec<String> = (0..5).map(|i| format!("text-{i}")).collect();
    let embeddings = provider.embed_batch(&texts).await.expect("embed_batch");

    assert_eq!(embeddings.len(), texts.len());
    for (i, emb) in embeddings.iter().enumerate() {
        assert_eq!(
            emb[0], i as f32,
            "embedding at position {i} must correspond to input {i} after reordering by index"
        );
    }
}

/// A response with fewer items than inputs must be rejected, not silently mapped
/// to a shorter or misaligned result.
#[tokio::test]
async fn embed_batch_rejects_count_mismatch() {
    let url = spawn_mock_embeddings(true).await;
    let provider = OpenAiProvider::new(reqwest::Client::new(), url, None, None, DIM);

    let texts: Vec<String> = (0..5).map(|i| format!("text-{i}")).collect();
    let result = provider.embed_batch(&texts).await;
    assert!(
        result.is_err(),
        "a response with fewer items than inputs must be rejected"
    );
}
