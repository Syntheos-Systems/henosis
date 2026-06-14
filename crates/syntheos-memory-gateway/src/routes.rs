//! Axum handlers implementing the `frameshift-memory-http` wire contract.
//! Each handler delegates translation to [`KleosClient`].

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;

use crate::dto::{
    HealthResponse, ListResponse, Memory, SearchRequest, SearchResponse, StoreRequest,
    StoreResponse,
};
use crate::error::GatewayError;
use crate::kleos::KleosClient;

/// Upper bound on caller-requested result counts (search `k`, list `limit`)
/// before they are forwarded to Kleos, so a caller cannot request billions of
/// rows and force unbounded allocation upstream and in this gateway.
const MAX_RESULTS: usize = 1000;

/// Query parameters for the list endpoint.
#[derive(Debug, Deserialize)]
struct ListParams {
    /// Maximum number of memories to return.
    limit: Option<usize>,
    /// Number of memories to skip.
    offset: Option<usize>,
}

/// Build the gateway router with the upstream client as shared state.
pub fn router(client: KleosClient) -> Router {
    Router::new()
        .route("/store", post(store_handler))
        .route("/search", post(search_handler))
        .route("/memories", get(list_handler))
        .route("/memories/{id}", get(recall_handler).delete(forget_handler))
        .route("/health", get(health_handler))
        .with_state(client)
}

/// `POST /store` -- store free text and return an opaque id.
async fn store_handler(
    State(client): State<KleosClient>,
    Json(req): Json<StoreRequest>,
) -> Result<(StatusCode, Json<StoreResponse>), GatewayError> {
    let (id, created_at) = client.store(&req.text, &req.tags, &req.metadata).await?;
    Ok((StatusCode::CREATED, Json(StoreResponse { id, created_at })))
}

/// `POST /search` -- semantic search, returning matching memories.
async fn search_handler(
    State(client): State<KleosClient>,
    Json(req): Json<SearchRequest>,
) -> Result<Json<SearchResponse>, GatewayError> {
    let k = req.k.min(MAX_RESULTS);
    let results = client.search(&req.query, k, &req.filters).await?;
    Ok(Json(SearchResponse { results }))
}

/// `GET /memories/{id}` -- recall one memory by id.
async fn recall_handler(
    State(client): State<KleosClient>,
    Path(id): Path<String>,
) -> Result<Json<Memory>, GatewayError> {
    Ok(Json(client.get(&id).await?))
}

/// `GET /memories` -- list memories with limit/offset paging.
async fn list_handler(
    State(client): State<KleosClient>,
    Query(params): Query<ListParams>,
) -> Result<Json<ListResponse>, GatewayError> {
    let limit = params.limit.unwrap_or(50).min(MAX_RESULTS);
    let items = client.list(limit, params.offset.unwrap_or(0)).await?;
    Ok(Json(ListResponse { items }))
}

/// `DELETE /memories/{id}` -- forget one memory by id.
async fn forget_handler(
    State(client): State<KleosClient>,
    Path(id): Path<String>,
) -> Result<StatusCode, GatewayError> {
    client.forget(&id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /health` -- report upstream Kleos reachability (always HTTP 200).
async fn health_handler(State(client): State<KleosClient>) -> Json<HealthResponse> {
    let healthy = client.health().await;
    let message = if healthy {
        "kleos reachable".to_string()
    } else {
        "kleos unreachable".to_string()
    };
    Json(HealthResponse { healthy, message })
}
