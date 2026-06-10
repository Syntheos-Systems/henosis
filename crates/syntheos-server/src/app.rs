//! The HTTP surface and the shared application state every handler receives.

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use henosis_chiasm::{
    ChiasmError, ChiasmStats, ChiasmStore, NewTask, Task, TaskFilter, TaskStatus,
};
use henosis_broca::{
    ActionEntry, ActionFilter, BrocaError, BrocaStats, BrocaStore, LogAction,
};
use henosis_soma::{
    AgentPresence, PresenceFilter, PresenceStatus, QualityPatch, RegisterAgent, SomaError,
    SomaStats, SomaStore,
};
use serde::Deserialize;
use syntheos_contracts::Timestamp;
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
    /// The Soma presence store (Story 1.2).
    soma: Arc<SomaStore>,
    /// The Broca narration log (Story 1.3).
    broca: Arc<BrocaStore>,
}

impl AppState {
    /// Wire the foundation into shared application state.
    pub fn new(
        dispatcher: Arc<Dispatcher>,
        directory: Arc<dyn PrincipalDirectory>,
        bus: Arc<AxonBus>,
        chiasm: Arc<ChiasmStore>,
        soma: Arc<SomaStore>,
        broca: Arc<BrocaStore>,
    ) -> Self {
        Self {
            dispatcher,
            directory,
            bus,
            chiasm,
            soma,
            broca,
        }
    }

    /// The shared event bus, for surfaces (e.g. a future event stream) that subscribe to it.
    pub fn bus(&self) -> &Arc<AxonBus> {
        &self.bus
    }
}

/// Build the router: the Phase 0 surface (health, version, enroll, dispatch) plus the Phase 1
/// Chiasm task and Soma presence surfaces.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/version", get(version))
        .route("/enroll", post(enroll))
        .route("/dispatch", post(dispatch))
        .route("/chiasm/tasks", post(chiasm_create_task).get(chiasm_list_tasks))
        .route("/chiasm/tasks/{id}", get(chiasm_get_task))
        .route("/chiasm/stats", get(chiasm_stats))
        .route("/soma/agents", post(soma_register).get(soma_list))
        .route("/soma/agents/{id}", get(soma_get))
        .route("/soma/agents/{id}/heartbeat", post(soma_heartbeat))
        .route("/soma/agents/{id}/quality", post(soma_quality))
        .route("/soma/stats", get(soma_stats))
        .route("/broca/actions", post(broca_log).get(broca_feed))
        .route("/broca/actions/{id}", get(broca_get))
        .route("/broca/actions/{id}/narrate", post(broca_narrate))
        .route("/broca/stats", get(broca_stats))
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

/// Map a [`SomaError`] onto an HTTP status + message.
fn soma_error(e: SomaError) -> (StatusCode, String) {
    let status = match &e {
        SomaError::NotFound(_) => StatusCode::NOT_FOUND,
        // The body referenced a principal that is not enrolled: the request is well-formed but
        // names an actor the directory does not know.
        SomaError::UnknownPrincipal(_) => StatusCode::UNPROCESSABLE_ENTITY,
        SomaError::NameTaken(_) => StatusCode::CONFLICT,
        SomaError::InvalidInput(_) | SomaError::InvalidStatus(_) => StatusCode::BAD_REQUEST,
        // Backend, directory, backfill, and any future variants are server-side failures.
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };
    (status, e.to_string())
}

/// Body for [`soma_register`].
///
/// `principal_id` must already be enrolled (via `POST /enroll` or the future Pistis admission
/// path); registration verifies and never mints. Identity is caller-asserted in Phase 1, the
/// same posture as the Chiasm surface.
#[derive(Debug, Deserialize)]
pub struct SomaRegisterRequest {
    /// The agent's canonical principal id.
    pub principal_id: PrincipalId,
    /// Tenant the registration belongs to.
    pub tenant: TenantId,
    /// Working label (unique per tenant).
    pub name: String,
    /// Coarse category (e.g. `coding`, `cli`).
    pub agent_type: String,
    /// Optional description.
    pub description: Option<String>,
    /// Capability strings (defaults to none).
    pub capabilities: Option<Vec<String>>,
    /// Agent-specific configuration object (defaults to `{}`).
    pub config: Option<serde_json::Value>,
}

/// Register (or re-register) an agent's presence.
async fn soma_register(
    State(state): State<AppState>,
    Json(req): Json<SomaRegisterRequest>,
) -> Result<Json<AgentPresence>, (StatusCode, String)> {
    state
        .soma
        .register(RegisterAgent {
            principal_id: req.principal_id,
            tenant: req.tenant,
            name: req.name,
            agent_type: req.agent_type,
            description: req.description,
            capabilities: req.capabilities,
            config: req.config,
        })
        .await
        .map(Json)
        .map_err(soma_error)
}

/// Query string for [`soma_list`]: optional AND-filters.
#[derive(Debug, Deserialize)]
pub struct SomaListQuery {
    /// Only agents of this type.
    pub agent_type: Option<String>,
    /// Only agents in this status.
    pub status: Option<PresenceStatus>,
    /// Maximum rows to return.
    pub limit: Option<usize>,
}

/// List registered agents, newest first.
async fn soma_list(
    State(state): State<AppState>,
    Query(q): Query<SomaListQuery>,
) -> Result<Json<Vec<AgentPresence>>, (StatusCode, String)> {
    state
        .soma
        .list(PresenceFilter {
            agent_type: q.agent_type,
            status: q.status,
            limit: q.limit,
        })
        .await
        .map(Json)
        .map_err(soma_error)
}

/// Fetch one agent's presence by its principal id.
async fn soma_get(
    State(state): State<AppState>,
    Path(id): Path<PrincipalId>,
) -> Result<Json<AgentPresence>, (StatusCode, String)> {
    state
        .soma
        .get(id)
        .await
        .map_err(soma_error)?
        .map(Json)
        .ok_or((StatusCode::NOT_FOUND, format!("agent not registered: {id}")))
}

/// Body for [`soma_heartbeat`]: an optional status override riding the liveness signal.
#[derive(Debug, Default, Deserialize)]
pub struct SomaHeartbeatRequest {
    /// When set, the agent's status becomes this value; when absent, `pending`/`offline`
    /// revive to `online` and `error` stays sticky.
    pub status: Option<PresenceStatus>,
}

/// Record an agent heartbeat; returns the status after the beat.
async fn soma_heartbeat(
    State(state): State<AppState>,
    Path(id): Path<PrincipalId>,
    Json(req): Json<SomaHeartbeatRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    state
        .soma
        .heartbeat(id, req.status)
        .await
        .map(|status| Json(serde_json::json!({ "status": status })))
        .map_err(soma_error)
}

/// Body for [`soma_quality`]: a partial quality-signal update (at least one field).
#[derive(Debug, Default, Deserialize)]
pub struct SomaQualityRequest {
    /// New quality score.
    pub quality_score: Option<f64>,
    /// Replacement drift-flag set.
    pub drift_flags: Option<Vec<String>>,
}

/// Apply a quality-signal update (Thymus evaluation / supervision path).
async fn soma_quality(
    State(state): State<AppState>,
    Path(id): Path<PrincipalId>,
    Json(req): Json<SomaQualityRequest>,
) -> Result<Json<AgentPresence>, (StatusCode, String)> {
    state
        .soma
        .update_quality(
            id,
            QualityPatch {
                quality_score: req.quality_score,
                drift_flags: req.drift_flags,
            },
        )
        .await
        .map(Json)
        .map_err(soma_error)
}

/// Query string asserting the tenant for presence stats.
#[derive(Debug, Deserialize)]
pub struct SomaStatsQuery {
    /// The tenant whose registry is aggregated.
    pub tenant: TenantId,
}

/// Aggregate presence counts for the asserted tenant.
async fn soma_stats(
    State(state): State<AppState>,
    Query(q): Query<SomaStatsQuery>,
) -> Result<Json<SomaStats>, (StatusCode, String)> {
    state.soma.stats(q.tenant).await.map(Json).map_err(soma_error)
}

/// Map a [`BrocaError`] onto an HTTP status + message.
fn broca_error(e: BrocaError) -> (StatusCode, String) {
    let status = match &e {
        BrocaError::NotFound(_) => StatusCode::NOT_FOUND,
        BrocaError::InvalidInput(_) => StatusCode::BAD_REQUEST,
        // The pluggable narrator failed upstream; the action row itself is fine.
        BrocaError::Narration(_) => StatusCode::BAD_GATEWAY,
        // Backend and any future variants are server-side failures.
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };
    (status, e.to_string())
}

/// Body for [`broca_log`]. Identity is caller-asserted in Phase 1, the established posture.
#[derive(Debug, Deserialize)]
pub struct BrocaLogRequest {
    /// Tenant the action belongs to.
    pub tenant: TenantId,
    /// The acting agent's principal.
    pub principal_id: PrincipalId,
    /// Originating service name (defaults to `henosis`).
    pub service: Option<String>,
    /// Action type token.
    pub action: String,
    /// Structured payload (must be a JSON object when supplied).
    pub payload: Option<serde_json::Value>,
    /// Pre-computed narrative; when absent the template renderer runs at log time.
    pub narrative: Option<String>,
}

/// Record an action in the narration log.
async fn broca_log(
    State(state): State<AppState>,
    Json(req): Json<BrocaLogRequest>,
) -> Result<Json<ActionEntry>, (StatusCode, String)> {
    state
        .broca
        .log(LogAction {
            tenant: req.tenant,
            principal_id: req.principal_id,
            service: req.service,
            action: req.action,
            payload: req.payload,
            narrative: req.narrative,
        })
        .await
        .map(Json)
        .map_err(broca_error)
}

/// Query string for [`broca_feed`]: the asserted tenant plus optional AND-filters.
#[derive(Debug, Deserialize)]
pub struct BrocaFeedQuery {
    /// Tenant whose feed is read.
    pub tenant: TenantId,
    /// Only actions by this principal.
    pub principal_id: Option<PrincipalId>,
    /// Only actions from this service.
    pub service: Option<String>,
    /// Only actions of this type.
    pub action: Option<String>,
    /// Only actions recorded at or after this RFC3339 instant.
    pub since: Option<Timestamp>,
    /// Maximum rows to return.
    pub limit: Option<usize>,
    /// Rows to skip (pagination).
    pub offset: Option<usize>,
}

/// Read a tenant's narration feed, newest first.
async fn broca_feed(
    State(state): State<AppState>,
    Query(q): Query<BrocaFeedQuery>,
) -> Result<Json<Vec<ActionEntry>>, (StatusCode, String)> {
    state
        .broca
        .query(
            q.tenant,
            ActionFilter {
                principal_id: q.principal_id,
                service: q.service,
                action: q.action,
                since: q.since,
                limit: q.limit,
                offset: q.offset,
            },
        )
        .await
        .map(Json)
        .map_err(broca_error)
}

/// Query string asserting the tenant for single-action reads, narration, and stats.
#[derive(Debug, Deserialize)]
pub struct BrocaTenantQuery {
    /// The asserted tenant.
    pub tenant: TenantId,
}

/// Fetch one action by id within the asserted tenant.
async fn broca_get(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Query(q): Query<BrocaTenantQuery>,
) -> Result<Json<ActionEntry>, (StatusCode, String)> {
    state
        .broca
        .get(q.tenant, id)
        .await
        .map_err(broca_error)?
        .map(Json)
        .ok_or((StatusCode::NOT_FOUND, format!("action not found: {id}")))
}

/// Ensure an action carries a narrative (template, then the attached narrator) and return it.
async fn broca_narrate(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Query(q): Query<BrocaTenantQuery>,
) -> Result<Json<ActionEntry>, (StatusCode, String)> {
    state
        .broca
        .get_or_narrate(q.tenant, id)
        .await
        .map(Json)
        .map_err(broca_error)
}

/// Aggregate action counts for the asserted tenant.
async fn broca_stats(
    State(state): State<AppState>,
    Query(q): Query<BrocaTenantQuery>,
) -> Result<Json<BrocaStats>, (StatusCode, String)> {
    state.broca.stats(q.tenant).await.map(Json).map_err(broca_error)
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
        let soma = Arc::new(
            SomaStore::open_in_memory(bus.clone(), directory.clone()).expect("soma store"),
        );
        let broca = Arc::new(BrocaStore::open_in_memory(bus.clone()).expect("broca store"));
        AppState::new(dispatcher, directory, bus, chiasm, soma, broca)
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
        let soma = Arc::new(
            SomaStore::open_in_memory(bus.clone(), directory.clone()).expect("soma store"),
        );
        let broca = Arc::new(BrocaStore::open_in_memory(bus.clone()).expect("broca store"));
        AppState::new(dispatcher, directory, bus, chiasm, soma, broca)
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

    /// Enroll a principal over HTTP and return its id string.
    async fn enroll_http(app: &Router, kind: &str, display: &str) -> String {
        let body = serde_json::json!({ "kind": kind, "display": display });
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/enroll")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let principal: serde_json::Value =
            serde_json::from_str(&body_string(response).await).unwrap();
        principal["id"].as_str().expect("principal id").to_string()
    }

    #[tokio::test]
    async fn soma_register_heartbeat_quality_roundtrip_over_http() {
        use syntheos_contracts::TenantId;
        let app = router(test_state());
        let tenant = TenantId::new().to_string();
        let agent = enroll_http(&app, "agent", "worker").await;

        // Register the enrolled principal's presence.
        let body = serde_json::json!({
            "principal_id": agent,
            "tenant": tenant,
            "name": "worker",
            "agent_type": "coding",
            "capabilities": ["rust"],
        });
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/soma/agents")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let registered: serde_json::Value =
            serde_json::from_str(&body_string(response).await).unwrap();
        assert_eq!(registered["status"], "pending");

        // A heartbeat revives it to online.
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/soma/agents/{agent}/heartbeat"))
                    .header("content-type", "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let beat: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
        assert_eq!(beat["status"], "online");

        // Quality lands and reads back through GET.
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/soma/agents/{agent}/quality"))
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::json!({"quality_score": 0.9}).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/soma/agents/{agent}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let got: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
        assert_eq!(got["quality_score"], 0.9);
        assert_eq!(got["status"], "online");

        // Stats for the tenant see one online agent.
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/soma/stats?tenant={tenant}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let stats: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
        assert_eq!(stats["total"], 1);
        assert_eq!(stats["online"], 1);
    }

    #[tokio::test]
    async fn broca_log_and_feed_over_http() {
        use syntheos_contracts::{PrincipalId, TenantId};
        let app = router(test_state());
        let tenant = TenantId::new().to_string();
        let actor = PrincipalId::new().to_string();

        // Log an action with a template-narratable type.
        let body = serde_json::json!({
            "tenant": tenant,
            "principal_id": actor,
            "service": "chiasm",
            "action": "task.completed",
            "payload": {"title": "wire broca", "agent": "claude"},
        });
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/broca/actions")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let logged: serde_json::Value =
            serde_json::from_str(&body_string(response).await).unwrap();
        assert_eq!(
            logged["narrative"], "\"wire broca\" was completed by claude",
            "template narrative derived at log time"
        );

        // The feed returns it, filtered by service; a stranger tenant sees nothing.
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/broca/actions?tenant={tenant}&service=chiasm"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let feed: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
        assert_eq!(feed.as_array().expect("array").len(), 1);
        let other = TenantId::new();
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/broca/actions?tenant={other}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let empty: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
        assert!(empty.as_array().expect("array").is_empty());

        // Stats count it.
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/broca/stats?tenant={tenant}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let stats: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
        assert_eq!(stats["total"], 1);
        assert_eq!(stats["by_service"]["chiasm"], 1);
    }

    #[tokio::test]
    async fn soma_register_rejects_unenrolled_principal() {
        use syntheos_contracts::{PrincipalId, TenantId};
        let app = router(test_state());
        let body = serde_json::json!({
            "principal_id": PrincipalId::new().to_string(), // never enrolled
            "tenant": TenantId::new().to_string(),
            "name": "ghost",
            "agent_type": "cli",
        });
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/soma/agents")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        // Registration verifies against the directory and never mints (projection convention).
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
