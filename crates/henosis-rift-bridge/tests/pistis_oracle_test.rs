//! Integration tests for the HTTP-backed Pistis capability oracle.

use std::sync::Arc;

use axum::{
    extract::{Path, State},
    http::{header, HeaderMap, StatusCode},
    routing::post,
    Json, Router,
};
use henosis_rift_bridge::capability::{CapabilityDecision, CapabilityOracle, PistisOracle};
use henosis_rift_bridge::executor::Capability;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

/// One capability requirement recorded by the fake Pistis server.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
struct RecordedRequirement {
    /// Capability namespace the oracle requested.
    name: String,
    /// Pistis action kind serialized into the request body.
    action_kind: String,
}

/// One capability-check request captured by the fake Pistis server.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
struct RecordedCheckRequest {
    /// Principal UUID under evaluation (was `agent_id` in standalone bridge).
    principal: String,
    /// Required capabilities forwarded by the bridge oracle.
    required: Vec<RecordedRequirement>,
}

/// One HTTP call captured by the fake Pistis server.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RecordedCall {
    /// Decoded Matrix room identifier from the request path.
    room: String,
    /// Authorization header observed on the request, if present.
    authorization: Option<String>,
    /// JSON request body sent by the oracle.
    body: RecordedCheckRequest,
}

/// Response payload emitted by the fake Pistis server.
#[derive(Debug, Clone, PartialEq, Serialize)]
struct FakeDecision {
    /// Whether the requested capabilities are allowed.
    allowed: bool,
    /// Requirements the fake server rejects.
    missing: Vec<RecordedRequirement>,
    /// Placeholder trust score matching the planned Pistis response shape.
    trust_score: f64,
    /// Optional denial reason.
    reason: Option<String>,
}

/// Static behavior for one fake Pistis server instance.
#[derive(Debug, Clone)]
struct FakeResponse {
    /// HTTP status returned to the oracle.
    status: StatusCode,
    /// JSON body returned to the oracle.
    body: FakeDecision,
}

/// Shared state for the fake Pistis server.
#[derive(Debug, Clone)]
struct FakeServerState {
    /// Calls captured for later assertions.
    calls: Arc<Mutex<Vec<RecordedCall>>>,
    /// Response emitted by the fake server.
    response: FakeResponse,
}

/// Handle for a running fake Pistis server.
struct FakeServerHandle {
    /// Base URL exposed to the oracle under test.
    base_url: String,
    /// Recorded requests captured by the fake server.
    calls: Arc<Mutex<Vec<RecordedCall>>>,
    /// Background task serving HTTP requests.
    task: tokio::task::JoinHandle<()>,
}

impl FakeServerHandle {
    /// Return the single recorded call for a test that expects exactly one request.
    async fn one_call(&self) -> RecordedCall {
        let calls = self.calls.lock().await;
        assert_eq!(calls.len(), 1, "expected exactly one capability-check call");
        calls[0].clone()
    }
}

impl Drop for FakeServerHandle {
    /// Stop the background fake server when the test handle goes out of scope.
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Start a local fake Pistis server with the supplied static response.
async fn start_fake_server(response: FakeResponse) -> FakeServerHandle {
    async fn capability_check(
        State(state): State<FakeServerState>,
        Path(room): Path<String>,
        headers: HeaderMap,
        Json(body): Json<RecordedCheckRequest>,
    ) -> (StatusCode, Json<FakeDecision>) {
        let authorization = headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        state.calls.lock().await.push(RecordedCall {
            room,
            authorization,
            body,
        });
        (state.response.status, Json(state.response.body.clone()))
    }

    let calls = Arc::new(Mutex::new(Vec::new()));
    let state = FakeServerState {
        calls: calls.clone(),
        response,
    };
    let app = Router::new()
        .route("/api/v1/rooms/{room}/capabilities/check", post(capability_check))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind fake Pistis server");
    let addr = listener.local_addr().expect("fake Pistis server address");
    let task = tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("run fake Pistis server");
    });
    FakeServerHandle {
        base_url: format!("http://{addr}"),
        calls,
        task,
    }
}

/// Build a fake success response with the provided missing requirements.
fn fake_decision(allowed: bool, missing: Vec<RecordedRequirement>) -> FakeDecision {
    FakeDecision {
        allowed,
        missing,
        trust_score: 0.75,
        reason: (!allowed).then(|| "missing capability".to_owned()),
    }
}

/// Verify that an allowed Pistis response grants the requested capabilities.
#[tokio::test]
async fn test_pistis_oracle_allows_capabilities() {
    let server = start_fake_server(FakeResponse {
        status: StatusCode::OK,
        body: fake_decision(true, Vec::new()),
    })
    .await;
    let oracle = PistisOracle::new(
        server.base_url.clone(),
        "bridge-token".to_owned(),
        "!rift:host".to_owned(),
    );

    let decision = oracle
        .check(
            "architect",
            &[
                Capability::new(Capability::FS_READ),
                Capability::new(Capability::FS_WRITE),
            ],
        )
        .await
        .expect("allow decision");

    assert_eq!(
        decision,
        CapabilityDecision::Granted(vec![
            Capability::new(Capability::FS_READ),
            Capability::new(Capability::FS_WRITE),
        ])
    );
}

/// Verify that a denied Pistis response returns the missing capability subset.
#[tokio::test]
async fn test_pistis_oracle_denies_missing_capabilities() {
    let server = start_fake_server(FakeResponse {
        status: StatusCode::OK,
        body: fake_decision(
            false,
            vec![RecordedRequirement {
                name: Capability::FS_WRITE.to_owned(),
                action_kind: "Commit".to_owned(),
            }],
        ),
    })
    .await;
    let oracle = PistisOracle::new(
        server.base_url.clone(),
        "bridge-token".to_owned(),
        "!rift:host".to_owned(),
    );

    let decision = oracle
        .check(
            "architect",
            &[
                Capability::new(Capability::FS_READ),
                Capability::new(Capability::FS_WRITE),
            ],
        )
        .await
        .expect("deny decision");

    assert_eq!(
        decision,
        CapabilityDecision::Denied(vec![Capability::new(Capability::FS_WRITE)])
    );
}

/// Verify that the oracle forwards the bearer token and deterministic mapping.
#[tokio::test]
async fn test_pistis_oracle_sends_auth_header_and_mapped_body() {
    let server = start_fake_server(FakeResponse {
        status: StatusCode::OK,
        body: fake_decision(true, Vec::new()),
    })
    .await;
    let oracle = PistisOracle::new(
        server.base_url.clone(),
        "secret-bridge-token".to_owned(),
        "!room:host".to_owned(),
    );

    oracle
        .check(
            "architect",
            &[
                Capability::new(Capability::FS_READ),
                Capability::new(Capability::BASH),
                Capability::new(Capability::NETWORK),
            ],
        )
        .await
        .expect("mapping request");

    let call = server.one_call().await;
    assert_eq!(call.room, "!room:host");
    assert_eq!(
        call.authorization.as_deref(),
        Some("Bearer secret-bridge-token")
    );
    assert_eq!(
        call.body.required,
        vec![
            RecordedRequirement {
                name: Capability::FS_READ.to_owned(),
                action_kind: "Message".to_owned(),
            },
            RecordedRequirement {
                name: Capability::BASH.to_owned(),
                action_kind: "Commit".to_owned(),
            },
            RecordedRequirement {
                name: Capability::NETWORK.to_owned(),
                action_kind: "Message".to_owned(),
            },
        ]
    );
}

/// Verify that non-success Pistis responses surface as execution errors.
#[tokio::test]
async fn test_pistis_oracle_reports_server_errors() {
    let server = start_fake_server(FakeResponse {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        body: fake_decision(false, Vec::new()),
    })
    .await;
    let oracle = PistisOracle::new(
        server.base_url.clone(),
        "bridge-token".to_owned(),
        "!rift:host".to_owned(),
    );

    let error = oracle
        .check("architect", &[Capability::new(Capability::FS_READ)])
        .await
        .expect_err("server error should bubble up");

    assert!(
        error
            .to_string()
            .contains("Pistis check rejected"),
        "unexpected error: {error}"
    );
}
