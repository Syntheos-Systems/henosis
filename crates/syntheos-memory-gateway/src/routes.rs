//! Axum handlers implementing the `frameshift-memory-http` wire contract.
//! Each handler delegates translation to [`KleosClient`].

use axum::extract::{Path, Query, Request, State};
use axum::http::header::{AUTHORIZATION, WWW_AUTHENTICATE};
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

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

/// Digest-backed authentication state shared by the protected route layer.
#[derive(Clone)]
struct InboundAuth {
    /// SHA-256 digest of the configured gateway bearer token.
    expected_digest: [u8; 32],
}

/// Query parameters for the list endpoint.
#[derive(Debug, Deserialize)]
struct ListParams {
    /// Maximum number of memories to return.
    limit: Option<usize>,
    /// Number of memories to skip.
    offset: Option<usize>,
}

/// Build the gateway router with a public health route and authenticated memory routes.
pub fn router(client: KleosClient, inbound_token_digest: [u8; 32]) -> Router {
    let protected = Router::new()
        .route("/store", post(store_handler))
        .route("/search", post(search_handler))
        .route("/memories", get(list_handler))
        .route("/memories/{id}", get(recall_handler).delete(forget_handler))
        .route_layer(middleware::from_fn_with_state(
            InboundAuth {
                expected_digest: inbound_token_digest,
            },
            require_inbound_auth,
        ));

    Router::new()
        .merge(protected)
        .route("/health", get(health_handler))
        .with_state(client)
}

/// Require exactly one valid Bearer credential before a memory handler executes.
async fn require_inbound_auth(
    State(auth): State<InboundAuth>,
    request: Request,
    next: Next,
) -> Response {
    if authorize_gateway_request(request.headers(), &auth.expected_digest) {
        return next.run(request).await;
    }
    (StatusCode::UNAUTHORIZED, [(WWW_AUTHENTICATE, "Bearer")]).into_response()
}

/// Validate an unambiguous Authorization header against the configured digest.
fn authorize_gateway_request(headers: &HeaderMap, expected_digest: &[u8; 32]) -> bool {
    let mut values = headers.get_all(AUTHORIZATION).iter();
    let Some(value) = values.next() else {
        return false;
    };
    if values.next().is_some() {
        return false;
    }
    let Ok(value) = value.to_str() else {
        return false;
    };
    let Some(token) = value.strip_prefix("Bearer ") else {
        return false;
    };
    if token.is_empty() {
        return false;
    }
    let candidate_digest: [u8; 32] = Sha256::digest(token.as_bytes()).into();
    expected_digest
        .as_slice()
        .ct_eq(candidate_digest.as_slice())
        .into()
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

#[cfg(test)]
/// Exercises the complete inbound authentication boundary.
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Method, Request};
    use tower::ServiceExt;

    use crate::config::Config;

    /// Stable test credential used only to exercise request authentication.
    const TEST_TOKEN: &str = "gateway-api-token-that-is-at-least-thirty-two-bytes";

    /// Compute the expected digest for one test credential.
    fn token_digest(token: &str) -> [u8; 32] {
        Sha256::digest(token.as_bytes()).into()
    }

    /// Build an isolated router whose upstream port rejects connections immediately.
    fn test_router() -> Router {
        let config = Config {
            bind_addr: "127.0.0.1:4510".to_string(),
            kleos_base_url: "http://127.0.0.1:1".to_string(),
            signing_host: "test-host".to_string(),
            signing_agent: "test-agent".to_string(),
            signing_model: "test-model".to_string(),
            inbound_token_digest: token_digest(TEST_TOKEN),
        };
        let client = KleosClient::new(&config, None);
        router(client, config.inbound_token_digest)
    }

    /// Build one empty request for an in-process router call.
    fn request(method: Method, uri: &str, authorization: Option<&str>) -> Request<Body> {
        let mut builder = Request::builder().method(method).uri(uri);
        if let Some(value) = authorization {
            builder = builder.header(AUTHORIZATION, value);
        }
        builder.body(Body::empty()).expect("request")
    }

    /// Missing credentials are rejected before every memory handler can parse or dispatch.
    #[tokio::test]
    async fn every_memory_route_requires_authentication() {
        let app = test_router();
        for (method, uri) in [
            (Method::POST, "/store"),
            (Method::POST, "/search"),
            (Method::GET, "/memories"),
            (Method::GET, "/memories/not-an-id"),
            (Method::DELETE, "/memories/not-an-id"),
        ] {
            let response = app
                .clone()
                .oneshot(request(method, uri, None))
                .await
                .expect("response");
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
            assert_eq!(
                response
                    .headers()
                    .get(WWW_AUTHENTICATE)
                    .and_then(|value| value.to_str().ok()),
                Some("Bearer")
            );
        }
    }

    /// Correct credentials pass while malformed, duplicate, and wrong values fail closed.
    #[tokio::test]
    async fn bearer_validation_is_strict_and_unambiguous() {
        let expected = token_digest(TEST_TOKEN);
        let mut headers = HeaderMap::new();
        assert!(!authorize_gateway_request(&headers, &expected));

        headers.insert(AUTHORIZATION, "Basic ignored".parse().expect("header"));
        assert!(!authorize_gateway_request(&headers, &expected));

        headers.insert(AUTHORIZATION, "Bearer wrong-token".parse().expect("header"));
        assert!(!authorize_gateway_request(&headers, &expected));

        headers.insert(
            AUTHORIZATION,
            format!("Bearer {TEST_TOKEN}").parse().expect("header"),
        );
        assert!(authorize_gateway_request(&headers, &expected));

        headers.append(
            AUTHORIZATION,
            format!("Bearer {TEST_TOKEN}").parse().expect("header"),
        );
        assert!(!authorize_gateway_request(&headers, &expected));

        let response = test_router()
            .oneshot(request(
                Method::GET,
                "/memories/not-an-id",
                Some(&format!("Bearer {TEST_TOKEN}")),
            ))
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    /// Health remains public and returns its contract response without a credential.
    #[tokio::test]
    async fn health_remains_public() {
        let response = test_router()
            .oneshot(request(Method::GET, "/health", None))
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::OK);
    }
}
