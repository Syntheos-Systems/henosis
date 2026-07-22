//! Integration coverage for the HTTP (Cohere/Jina and TEI) reranker backend.
//!
//! Production's remote reranker is a Cohere/Jina/TEI service the test suite
//! cannot call, which is why the HTTP blend path had no automated coverage.
//! These tests stand up local mock rerank servers (the established
//! axum-on-a-random-port idiom) with controlled relevance scores and drive a
//! real `HttpReranker` through them against real `hybrid_search` results
//! (genuine RRF-scale fusion scores). This is the only automated exercise of
//! the remote blend, in both wire formats.

use kleos_lib::db::Database;
use kleos_lib::memory::search::hybrid_search;
use kleos_lib::memory::types::{SearchRequest, SearchResult, StoreRequest};
use kleos_lib::memory::{self};
use kleos_lib::reranker::{HttpReranker, RerankFormat, Reranker};

/// Spawn a mock Cohere `/v1/rerank` server on a free 127.0.0.1 port. Each
/// document scores `hi` when its text contains `marker`, otherwise `lo`. Returns
/// the bound endpoint URL. Mirrors the axum-receiver idiom in `webhooks.rs`.
async fn spawn_mock_rerank(marker: &'static str, hi: f64, lo: f64) -> String {
    use axum::{routing::post, Json, Router};
    use serde_json::{json, Value};

    let app = Router::new().route(
        "/v1/rerank",
        post(move |Json(body): Json<Value>| async move {
            let docs = body["documents"].as_array().cloned().unwrap_or_default();
            let results: Vec<Value> = docs
                .iter()
                .enumerate()
                .map(|(i, d)| {
                    let text = d.as_str().unwrap_or("");
                    let score = if text.contains(marker) { hi } else { lo };
                    json!({ "index": i, "relevance_score": score })
                })
                .collect();
            Json(json!({ "results": results }))
        }),
    );

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{}/v1/rerank", addr)
}

/// Spawn a mock TEI `/rerank` server on a free 127.0.0.1 port. TEI's wire
/// format differs from Cohere's in both directions: the request carries
/// `texts` (not `documents`), and the response is a bare array of
/// `{"index": N, "score": F}` where `score` is an unbounded logit (not a
/// [0,1] relevance score). Documents containing `marker` score `hi_logit`,
/// the rest `lo_logit`.
async fn spawn_mock_rerank_tei(marker: &'static str, hi_logit: f64, lo_logit: f64) -> String {
    use axum::{routing::post, Json, Router};
    use serde_json::{json, Value};

    let app = Router::new().route(
        "/rerank",
        post(move |Json(body): Json<Value>| async move {
            let texts = body["texts"].as_array().cloned().unwrap_or_default();
            let results: Vec<Value> = texts
                .iter()
                .enumerate()
                .map(|(i, d)| {
                    let text = d.as_str().unwrap_or("");
                    let score = if text.contains(marker) {
                        hi_logit
                    } else {
                        lo_logit
                    };
                    json!({ "index": i, "score": score })
                })
                .collect();
            Json(json!(results))
        }),
    );

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{}/rerank", addr)
}

/// Seed `contents` as FTS-only memories (no embeddings) and return the real
/// `hybrid_search` results for `query`. The results carry genuine RRF-scale
/// fusion scores in `SearchResult::score`, exactly what the reranker blends.
async fn seed_and_search(contents: &[&str], query: &str) -> Vec<SearchResult> {
    let db = Database::connect_memory().await.expect("in-memory db");
    for c in contents {
        memory::store(
            &db,
            StoreRequest {
                content: c.to_string(),
                user_id: Some(1),
                ..Default::default()
            },
            None,
            false,
        )
        .await
        .expect("seed store");
    }
    let hits = hybrid_search(
        &db,
        SearchRequest {
            query: query.to_string(),
            user_id: Some(1),
            limit: Some(20),
            ..Default::default()
        },
    )
    .await
    .expect("hybrid_search");
    // hybrid_search returns Arc<Vec<..>>; clone out an owned, mutable Vec so the
    // reranker can re-score and re-sort it in place.
    (*hits).clone()
}

/// End-to-end: the remote reranker promotes a fusion-buried true answer when the
/// cross-encoder scores it far above the distractors (parity with the ONNX
/// rescue canary). Exercises the full HTTP request/response + blend + re-sort.
#[tokio::test]
async fn http_reranker_rescues_fusion_buried_answer() {
    let endpoint = spawn_mock_rerank("ZARQUON", 0.95, 0.30).await;
    // Distractors repeat the query terms densely; the true answer mentions them
    // once plus a unique marker, so FTS fusion ranks it at or near the bottom.
    let contents = [
        "widget throughput widget throughput widget assembly cadence",
        "widget throughput widget throughput metrics dashboard",
        "widget throughput widget throughput pipeline latency",
        "a single widget throughput note about ZARQUON calibration",
    ];
    let mut results = seed_and_search(&contents, "widget throughput").await;
    assert!(results.len() >= 2, "need several candidates to rerank");

    let rr = HttpReranker::new(endpoint, None, "test-model".to_string(), 24);
    rr.rerank_results("widget throughput", &mut results)
        .await
        .expect("http rerank");

    assert!(
        results[0].memory.content.contains("ZARQUON"),
        "cross-encoder must rescue the marked answer to the top; got order: {:?}",
        results
            .iter()
            .map(|r| &r.memory.content)
            .collect::<Vec<_>>()
    );
}

/// With equal cross-encoder scores across candidates, the `(1-w)` fusion weight
/// must still spread the blended scores meaningfully -- proof that the RRF-scale
/// fusion score is normalized onto [0,1] before the blend. Without normalization
/// the spread is ~0.01 (RRF magnitudes) and this assertion fails; with it the
/// spread approaches the fusion weight (~0.3). Non-vacuous guardrail for the fix.
#[tokio::test]
async fn http_reranker_normalizes_fusion_before_blend() {
    // Marker matches nothing, so every document gets the same CE score.
    let endpoint = spawn_mock_rerank("NO_SUCH_MARKER", 0.5, 0.5).await;
    let contents = [
        "alpha beacon topic apple apple apple orchard",
        "alpha beacon topic banana banana grove",
        "alpha beacon topic cherry tree",
    ];
    let mut results = seed_and_search(&contents, "alpha beacon topic").await;
    assert!(
        results.len() >= 2,
        "need several candidates with distinct fusion scores"
    );

    let rr = HttpReranker::new(endpoint, None, "test-model".to_string(), 24);
    rr.rerank_results("alpha beacon topic", &mut results)
        .await
        .expect("http rerank");

    let max = results.iter().map(|r| r.score).fold(f64::MIN, f64::max);
    let min = results.iter().map(|r| r.score).fold(f64::MAX, f64::min);
    assert!(
        max - min >= 0.25,
        "equal CE + normalized fusion must spread blended scores by ~the fusion \
         weight; got spread {:.4} (scores {:?}) -- fusion is not being normalized",
        max - min,
        results.iter().map(|r| r.score).collect::<Vec<_>>()
    );
}

/// TEI returns unbounded logits, so the blend must use the sigmoid-normalized
/// CE confidence, not the raw wire score. With +2.0/-2.0 logits: pre-fix the
/// raw blend produces scores far outside [0,1] (e.g. -1.4 at w=0.7); post-fix
/// every blended score stays in [0,1] (the scale min-relevance gating and
/// abstain thresholds assume) while the cross-encoder still rescues the
/// marked answer to the top.
#[tokio::test]
async fn http_reranker_tei_logits_blend_via_sigmoid() {
    let endpoint = spawn_mock_rerank_tei("ZARQUON", 2.0, -2.0).await;
    let contents = [
        "widget throughput widget throughput widget assembly cadence",
        "widget throughput widget throughput metrics dashboard",
        "widget throughput widget throughput pipeline latency",
        "a single widget throughput note about ZARQUON calibration",
    ];
    let mut results = seed_and_search(&contents, "widget throughput").await;
    assert!(results.len() >= 2, "need several candidates to rerank");

    let rr = HttpReranker::new(endpoint, None, String::new(), 24).with_format(RerankFormat::Tei);
    rr.rerank_results("widget throughput", &mut results)
        .await
        .expect("http rerank (tei)");

    assert!(
        results[0].memory.content.contains("ZARQUON"),
        "cross-encoder must rescue the marked answer under TEI too; got order: {:?}",
        results
            .iter()
            .map(|r| &r.memory.content)
            .collect::<Vec<_>>()
    );
    for r in results.iter() {
        assert!(
            (0.0..=1.0).contains(&r.score),
            "blended score {} escaped [0,1]: TEI logits must pass through the \
             sigmoid before the blend (scores {:?})",
            r.score,
            results.iter().map(|r| r.score).collect::<Vec<_>>()
        );
    }
}
