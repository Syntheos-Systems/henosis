//! The HTTP surface and the shared application state every handler receives.

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use henosis_chiasm::{
    ChiasmError, ChiasmStats, ChiasmStore, NewTask, Task, TaskFilter, TaskStatus,
};
use serde::Deserialize;
use syntheos_axon::AxonBus;
use syntheos_contracts::{GateRequest, Principal, PrincipalId, PrincipalKind, TaskId, TenantId};
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
    /// The Chiasm task store (the first Phase 1 kernel service, Story 1.7).
    chiasm: Arc<ChiasmStore>,
}

impl AppState {
    /// Wire the foundation into shared application state.
    pub fn new(
        dispatcher: Arc<Dispatcher>,
        directory: Arc<dyn PrincipalDirectory>,
        bus: Arc<AxonBus>,
        chiasm: Arc<ChiasmStore>,
    ) -> Self {
        Self {
            dispatcher,
            directory,
            bus,
            chiasm,
        }
    }

    /// The shared event bus, for surfaces (e.g. a future event stream) that subscribe to it.
    pub fn bus(&self) -> &Arc<AxonBus> {
        &self.bus
    }
}

/// Build the router: the Phase 0 surface (health, version, enroll, dispatch) plus the Phase 1
/// Chiasm task surface.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/version", get(version))
        .route("/enroll", post(enroll))
        .route("/dispatch", post(dispatch))
        .route("/chiasm/tasks", post(chiasm_create_task).get(chiasm_list_tasks))
        .route("/chiasm/tasks/{id}", get(chiasm_get_task))
        .route("/chiasm/stats", get(chiasm_stats))
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

/// Map a [`ChiasmError`] onto an HTTP status + message.
fn chiasm_error(e: ChiasmError) -> (StatusCode, String) {
    let status = match &e {
        ChiasmError::NotFound(_) => StatusCode::NOT_FOUND,
        ChiasmError::InvalidStatus(_) => StatusCode::BAD_REQUEST,
        // Backend, backfill, and any future variants are server-side failures.
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };
    (status, e.to_string())
}

/// Body for [`chiasm_create_task`].
///
/// `tenant` and `principal_id` are caller-asserted in Phase 1: the server has no authentication
/// layer yet, so identity rides in the body the same way `/dispatch` carries a `RequestContext`.
/// PistisGate (Phase 3) replaces caller-asserted identity with verified capability checks.
#[derive(Debug, Deserialize)]
pub struct ChiasmCreateTask {
    /// Tenant the task belongs to.
    pub tenant: TenantId,
    /// Owner principal.
    pub principal_id: PrincipalId,
    /// Project the task groups under.
    pub project: String,
    /// Human-readable title.
    pub title: String,
    /// Initial status (defaults to `active`).
    pub status: Option<TaskStatus>,
    /// Optional progress note.
    pub summary: Option<String>,
    /// Optional description of the expected output.
    pub expected_output: Option<String>,
    /// Output format hint (defaults to `raw`).
    pub output_format: Option<String>,
    /// Optional assignee principal.
    pub assignee: Option<PrincipalId>,
    /// Heartbeat interval in seconds (defaults to 300).
    pub heartbeat_interval_secs: Option<i64>,
}

/// Create a Chiasm task owned by the asserted principal.
async fn chiasm_create_task(
    State(state): State<AppState>,
    Json(req): Json<ChiasmCreateTask>,
) -> Result<Json<Task>, (StatusCode, String)> {
    state
        .chiasm
        .create(NewTask {
            tenant: req.tenant,
            principal_id: req.principal_id,
            project: req.project,
            title: req.title,
            status: req.status,
            summary: req.summary,
            expected_output: req.expected_output,
            output_format: req.output_format,
            assignee: req.assignee,
            heartbeat_interval_secs: req.heartbeat_interval_secs,
        })
        .await
        .map(Json)
        .map_err(chiasm_error)
}

/// Query string for [`chiasm_list_tasks`]: the asserted owner plus optional AND-filters.
#[derive(Debug, Deserialize)]
pub struct ChiasmListQuery {
    /// Owner principal whose tasks are listed.
    pub principal_id: PrincipalId,
    /// Only tasks with this status.
    pub status: Option<TaskStatus>,
    /// Only tasks in this project.
    pub project: Option<String>,
    /// Maximum rows to return.
    pub limit: Option<usize>,
    /// Rows to skip (pagination).
    pub offset: Option<usize>,
}

/// List the asserted principal's tasks, newest-updated first.
async fn chiasm_list_tasks(
    State(state): State<AppState>,
    Query(q): Query<ChiasmListQuery>,
) -> Result<Json<Vec<Task>>, (StatusCode, String)> {
    state
        .chiasm
        .list(
            q.principal_id,
            TaskFilter {
                status: q.status,
                project: q.project,
                limit: q.limit,
                offset: q.offset,
            },
        )
        .await
        .map(Json)
        .map_err(chiasm_error)
}

/// Query string asserting the owner principal for single-task reads and stats.
#[derive(Debug, Deserialize)]
pub struct ChiasmOwnerQuery {
    /// The asserted owner principal.
    pub principal_id: PrincipalId,
}

/// Fetch one of the asserted principal's tasks by id. Owner-scoped: another principal's task is
/// indistinguishable from a missing one (404), never disclosed.
async fn chiasm_get_task(
    State(state): State<AppState>,
    Path(id): Path<TaskId>,
    Query(q): Query<ChiasmOwnerQuery>,
) -> Result<Json<Task>, (StatusCode, String)> {
    state
        .chiasm
        .get(q.principal_id, id)
        .await
        .map_err(chiasm_error)?
        .map(Json)
        .ok_or((StatusCode::NOT_FOUND, format!("task not found: {id}")))
}

/// Aggregate task counts for the asserted principal.
async fn chiasm_stats(
    State(state): State<AppState>,
    Query(q): Query<ChiasmOwnerQuery>,
) -> Result<Json<ChiasmStats>, (StatusCode, String)> {
    state
        .chiasm
        .stats(q.principal_id)
        .await
        .map(Json)
        .map_err(chiasm_error)
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
    /// the test-only `stubs` feature) with an in-memory Chiasm store.
    fn test_state() -> AppState {
        let bus = Arc::new(AxonBus::new());
        let directory: Arc<dyn PrincipalDirectory> = Arc::new(InMemoryDirectory::new());
        let dispatcher = Arc::new(
            Dispatcher::new(stub_gate_chain(), Box::new(EchoExecutor), bus.clone())
                .expect("canonical stub chain"),
        );
        let chiasm = Arc::new(ChiasmStore::open_in_memory(bus.clone()).expect("chiasm store"));
        AppState::new(dispatcher, directory, bus, chiasm)
    }

    /// Build app state exactly as the live binary does: deny-by-default chain + deny executor.
    fn deny_state() -> AppState {
        let bus = Arc::new(AxonBus::new());
        let directory: Arc<dyn PrincipalDirectory> = Arc::new(InMemoryDirectory::new());
        let dispatcher = Arc::new(
            Dispatcher::new(deny_gate_chain(), Box::new(DenyExecutor), bus.clone())
                .expect("canonical deny chain"),
        );
        let chiasm = Arc::new(ChiasmStore::open_in_memory(bus.clone()).expect("chiasm store"));
        AppState::new(dispatcher, directory, bus, chiasm)
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

    /// POST a Chiasm task creation and return the parsed response body (status must be 200).
    async fn create_task_http(
        app: &Router,
        tenant: &str,
        principal: &str,
        project: &str,
        title: &str,
    ) -> serde_json::Value {
        let body = serde_json::json!({
            "tenant": tenant,
            "principal_id": principal,
            "project": project,
            "title": title,
        });
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/chiasm/tasks")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        serde_json::from_str(&body_string(response).await).unwrap()
    }

    #[tokio::test]
    async fn chiasm_create_then_get_roundtrips_over_http() {
        use syntheos_contracts::{PrincipalId, TenantId};
        let app = router(test_state());
        let tenant = TenantId::new().to_string();
        let owner = PrincipalId::new().to_string();
        let created = create_task_http(&app, &tenant, &owner, "henosis", "wire chiasm").await;
        assert_eq!(created["status"], "active");
        assert_eq!(created["principal_id"], owner);
        let id = created["id"].as_str().expect("task id");

        // The owner reads it back.
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/chiasm/tasks/{id}?principal_id={owner}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let got: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
        assert_eq!(got["title"], "wire chiasm");

        // Another principal gets 404 -- owner-scoping does not disclose existence.
        let other = PrincipalId::new();
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/chiasm/tasks/{id}?principal_id={other}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn chiasm_list_and_stats_are_owner_scoped() {
        use syntheos_contracts::{PrincipalId, TenantId};
        let app = router(test_state());
        let tenant = TenantId::new().to_string();
        let owner = PrincipalId::new().to_string();
        create_task_http(&app, &tenant, &owner, "alpha", "a").await;
        create_task_http(&app, &tenant, &owner, "beta", "b").await;

        // Project filter narrows the list.
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/chiasm/tasks?principal_id={owner}&project=beta"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let tasks: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
        assert_eq!(tasks.as_array().expect("array").len(), 1);
        assert_eq!(tasks[0]["title"], "b");

        // Stats count the owner's tasks; a stranger sees zero.
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/chiasm/stats?principal_id={owner}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let stats: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
        assert_eq!(stats["total"], 2);
        let stranger = PrincipalId::new();
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/chiasm/stats?principal_id={stranger}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let stats: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
        assert_eq!(stats["total"], 0);
    }

    #[tokio::test]
    async fn chiasm_create_rejects_unknown_status() {
        use syntheos_contracts::{PrincipalId, TenantId};
        let app = router(test_state());
        let body = serde_json::json!({
            "tenant": TenantId::new().to_string(),
            "principal_id": PrincipalId::new().to_string(),
            "project": "p",
            "title": "t",
            "status": "definitely_not_a_status",
        });
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/chiasm/tasks")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        // Typed TaskStatus deserialization rejects the token before any handler runs.
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
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
