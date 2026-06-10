//! The HTTP surface and the shared application state every handler receives.

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use syntheos_axon::AxonBus;
use syntheos_contracts::{GateRequest, Principal, PrincipalKind};
use syntheos_dispatch::{DispatchOutcome, Dispatcher};
use syntheos_identity::PrincipalDirectory;

/// The foundation wired together once at boot and shared with every handler.
///
/// Cheap to clone (it is all `Arc`s), as the `axum` `State` extractor requires.
#[derive(Clone)]
pub struct AppState {
    /// The unified action dispatcher (deny-by-default gate chain in Phase 0).
    dispatcher: Arc<Dispatcher>,
    /// The principal directory actors are enrolled into and looked up from.
    directory: Arc<dyn PrincipalDirectory>,
    /// The in-process event bus (held for a future event-stream surface).
    bus: Arc<AxonBus>,
}

impl AppState {
    /// Wire the foundation into shared application state.
    pub fn new(
        dispatcher: Arc<Dispatcher>,
        directory: Arc<dyn PrincipalDirectory>,
        bus: Arc<AxonBus>,
    ) -> Self {
        Self {
            dispatcher,
            directory,
            bus,
        }
    }

    /// The shared event bus, for surfaces (e.g. a future event stream) that subscribe to it.
    pub fn bus(&self) -> &Arc<AxonBus> {
        &self.bus
    }
}

/// Build the router for the Phase 0 surface: health, version, enroll, dispatch.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/version", get(version))
        .route("/enroll", post(enroll))
        .route("/dispatch", post(dispatch))
        .with_state(state)
}

/// Liveness probe.
async fn health() -> &'static str {
    "ok"
}

/// Report the running crate's name and version.
async fn version() -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "name": env!("CARGO_PKG_NAME"),
        "version": env!("CARGO_PKG_VERSION"),
    }))
}

/// Body for [`enroll`]: which kind of actor to enroll, plus an optional label.
#[derive(Debug, Deserialize)]
pub struct EnrollRequest {
    /// The category of actor to enroll.
    pub kind: PrincipalKind,
    /// Optional human-readable label.
    pub display: Option<String>,
}

/// Enroll a new actor and return the created principal.
async fn enroll(
    State(state): State<AppState>,
    Json(req): Json<EnrollRequest>,
) -> Result<Json<Principal>, (StatusCode, String)> {
    state
        .directory
        .enroll(req.kind, req.display)
        .await
        .map(Json)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

/// Run an action through the full gate chain and (if allowed) execute it.
///
/// Authorization decisions (`Denied` / `RequiresApproval`) are normal `200` outcomes, not errors;
/// only a genuine execution failure returns `500`.
async fn dispatch(
    State(state): State<AppState>,
    Json(req): Json<GateRequest>,
) -> Result<Json<DispatchOutcome>, (StatusCode, String)> {
    state
        .dispatcher
        .dispatch(req)
        .await
        .map(Json)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{to_bytes, Body};
    use axum::http::Request;
    use syntheos_dispatch::deny::{deny_gate_chain, DenyExecutor};
    use syntheos_dispatch::stubs::{stub_gate_chain, EchoExecutor};
    use syntheos_identity::InMemoryDirectory;
    use tower::ServiceExt;

    /// Build app state over the real foundation (allow-all stub gates + echo executor, both from
    /// the test-only `stubs` feature).
    fn test_state() -> AppState {
        let bus = Arc::new(AxonBus::new());
        let directory: Arc<dyn PrincipalDirectory> = Arc::new(InMemoryDirectory::new());
        let dispatcher = Arc::new(
            Dispatcher::new(stub_gate_chain(), Box::new(EchoExecutor), bus.clone())
                .expect("canonical stub chain"),
        );
        AppState::new(dispatcher, directory, bus)
    }

    /// Build app state exactly as the live binary does: deny-by-default chain + deny executor.
    fn deny_state() -> AppState {
        let bus = Arc::new(AxonBus::new());
        let directory: Arc<dyn PrincipalDirectory> = Arc::new(InMemoryDirectory::new());
        let dispatcher = Arc::new(
            Dispatcher::new(deny_gate_chain(), Box::new(DenyExecutor), bus.clone())
                .expect("canonical deny chain"),
        );
        AppState::new(dispatcher, directory, bus)
    }

    /// Collect a response body into a UTF-8 string.
    async fn body_string(response: axum::response::Response) -> String {
        let bytes = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("collect body");
        String::from_utf8(bytes.to_vec()).expect("utf8 body")
    }

    #[tokio::test]
    async fn health_ok() {
        let response = router(test_state())
            .oneshot(Request::builder().uri("/health").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(body_string(response).await, "ok");
    }

    #[tokio::test]
    async fn version_reports_crate() {
        let response = router(test_state())
            .oneshot(
                Request::builder()
                    .uri("/version")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let v: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
        assert_eq!(v["name"], "syntheos-server");
    }

    #[tokio::test]
    async fn enroll_returns_principal() {
        let response = router(test_state())
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/enroll")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"kind":"agent","display":"eidolon"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let p: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
        assert_eq!(p["kind"], "agent");
        assert_eq!(p["display"], "eidolon");
        assert!(p["id"].is_string());
    }

    #[tokio::test]
    async fn dispatch_allow_executes() {
        use syntheos_contracts::{PrincipalId, RequestContext, TenantId, ToolInvocation};
        let req = GateRequest {
            context: RequestContext {
                tenant: TenantId::new(),
                principal: PrincipalId::new(),
                persona: None,
                session: None,
                room: None,
                task: None,
                workflow: None,
            },
            invocation: ToolInvocation {
                tool: "kleos".into(),
                action: "memory_store".into(),
                args: serde_json::json!({}),
            },
        };
        let body = serde_json::to_string(&req).unwrap();
        let response = router(test_state())
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/dispatch")
                    .header("content-type", "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let out: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
        // DispatchOutcome::Executed { result } serializes externally-tagged.
        assert_eq!(out["Executed"]["result"]["echoed"], true);
        assert_eq!(out["Executed"]["result"]["tool"], "kleos");
    }

    #[test]
    fn empty_gate_chain_cannot_become_a_dispatcher() {
        let bus = Arc::new(AxonBus::new());
        let result = Dispatcher::new(Vec::new(), Box::new(DenyExecutor), bus);
        assert!(
            result.is_err(),
            "an empty gate chain must never construct a runnable dispatcher"
        );
    }

    #[tokio::test]
    async fn dispatch_deny_chain_denies() {
        use syntheos_contracts::{PrincipalId, RequestContext, TenantId, ToolInvocation};
        let req = GateRequest {
            context: RequestContext {
                tenant: TenantId::new(),
                principal: PrincipalId::new(),
                persona: None,
                session: None,
                room: None,
                task: None,
                workflow: None,
            },
            invocation: ToolInvocation {
                tool: "kleos".into(),
                action: "memory_store".into(),
                args: serde_json::json!({}),
            },
        };
        let body = serde_json::to_string(&req).unwrap();
        // deny_state wires the DenyExecutor: if the chain failed to deny, execution would error
        // into a 500, so a 200 Denied outcome proves the executor never ran.
        let response = router(deny_state())
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/dispatch")
                    .header("content-type", "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let out: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
        // DispatchOutcome::Denied { gate, reason } serializes externally-tagged; the first
        // canonical authority denies.
        assert_eq!(out["Denied"]["gate"], "pistis");
        assert!(
            out["Denied"]["reason"]
                .as_str()
                .expect("reason string")
                .contains("fail-closed"),
            "deny reason should state the fail-closed posture: {out}"
        );
    }
}
