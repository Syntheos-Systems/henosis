//! `GET /api/dashboard` -- in-process composition of all five kernel stores.
//!
//! [`compose_dashboard`] is the pure composition function: it calls each
//! kernel store's stats surface, maps the results to the documented response
//! shape, and returns a [`DashboardResponse`]. Every store call is independent;
//! a failing store is marked `status: "error"` in the `services` list and its
//! section carries zero/empty data rather than aborting the whole response.
//!
//! [`dashboard`] is the thin axum handler that:
//! 1. Validates the `Authorization: Bearer` JWT via the [`OperatorAuth`] extractor.
//! 2. Checks `Permission::OrgRead` via [`OperatorAuth::require`].
//! 3. Delegates to [`compose_dashboard`] with the JWT's org and principal.
//!
//! The `usage` section is always null per the spec's documented honesty rule:
//! Plutus usage metering is not yet wired into the dashboard (Phase 2 billing).
//! This is not a stub -- it is the spec's explicit placeholder.

use std::collections::BTreeMap;

use axum::extract::State;
use axum::Json;
use henosis_broca::{ActionFilter, BrocaStore};
use henosis_chiasm::ChiasmStore;
use henosis_loom::LoomStore;
use henosis_plutus::Permission;
use henosis_soma::SomaStore;
use henosis_thymus::ThymusStore;
use serde::Serialize;
use syntheos_contracts::{PrincipalId, TenantId};

use super::auth::OperatorError;
use super::rbac::OperatorAuth;
use super::OperatorState;

/// The health status of one kernel store as seen from the dashboard composition.
///
/// Each store that the composition calls contributes one `ServiceHealth` entry
/// to `DashboardResponse::services`. `status` is `"ok"` when the call succeeded
/// and `"error"` when it failed. A status of `"ok"` is NEVER fabricated for a
/// store that was not actually called.
#[derive(Debug, Clone, Serialize)]
pub struct ServiceHealth {
    /// The kernel store name (e.g. `"soma"`, `"chiasm"`, `"broca"`).
    pub name: String,
    /// `"ok"` when the store's stats call succeeded; `"error"` otherwise.
    pub status: String,
}

/// Agent presence aggregate from Soma, scoped by org (tenant).
#[derive(Debug, Clone, Serialize)]
pub struct AgentStats {
    /// Total registered agents for the org.
    pub total: i64,
    /// Agents currently reporting `online` status.
    pub online: i64,
}

/// Task aggregate from Chiasm, scoped by the authenticated principal.
#[derive(Debug, Clone, Serialize)]
pub struct TaskStats {
    /// Tasks in a non-terminal state (all statuses except `completed`).
    pub active: i64,
    /// Raw count per status token, passed through from [`henosis_chiasm::ChiasmStats`].
    pub by_status: BTreeMap<String, i64>,
}

/// Workflow and run aggregate from Loom, scoped by the authenticated principal.
#[derive(Debug, Clone, Serialize)]
pub struct WorkflowStats {
    /// Runs currently pending or actively executing (from [`henosis_loom::LoomStats::active_runs`]).
    pub runs_active: i64,
}

/// Quality measurement aggregate from Thymus, scoped by the authenticated principal.
#[derive(Debug, Clone, Serialize)]
pub struct QualityStats {
    /// Total evaluations recorded for this principal.
    pub evaluations: i64,
}

/// A compact representation of one Broca `ActionEntry` for the activity feed.
///
/// Maps from [`henosis_broca::ActionEntry`] to a smaller wire shape that
/// carries only the fields the dashboard UI needs. The full entry is still
/// available via the `/broca/actions` surface.
#[derive(Debug, Clone, Serialize)]
pub struct ActivityEntry {
    /// Broca row id (monotonically increasing, useful for polling).
    pub id: i64,
    /// The action type token (e.g. `"task.created"`, `"agent.heartbeat"`).
    pub action: String,
    /// The originating service name (e.g. `"henosis"`, `"synapse"`).
    pub service: String,
    /// The acting principal's UUID string.
    pub principal_id: String,
    /// The pre-rendered natural-language narrative, when present.
    pub narrative: Option<String>,
    /// RFC3339 timestamp when the action was recorded (serialized by [`syntheos_contracts::Timestamp`]).
    pub created_at: syntheos_contracts::Timestamp,
}

/// Usage metering placeholder per the spec's honesty rule (spec section 5).
///
/// Plutus usage metering is not yet integrated into the dashboard. These fields
/// are explicitly `None` with a human-readable `note` so clients know the gap is
/// acknowledged and tracked. This is the spec's documented design decision, not
/// a stub -- no fabricated values are ever returned here.
#[derive(Debug, Clone, Serialize)]
pub struct UsageStats {
    /// Token spend today. Always `null` until Plutus metering is wired into the dashboard.
    pub tokens_today: Option<f64>,
    /// USD cost today. Always `null` until Plutus metering is wired into the dashboard.
    pub cost_today: Option<f64>,
    /// Explains why the above fields are null.
    pub note: String,
}

/// The full payload returned by `GET /api/dashboard`.
///
/// Produced by [`compose_dashboard`] from all five kernel stores.
/// Organized into sections matching the spec section 5 shape.
#[derive(Debug, Clone, Serialize)]
pub struct DashboardResponse {
    /// One entry per kernel store that was called; `status` reflects whether the call succeeded.
    pub services: Vec<ServiceHealth>,
    /// Agent presence aggregate from Soma (org-scoped).
    pub agents: AgentStats,
    /// Task aggregate from Chiasm (principal-scoped).
    pub tasks: TaskStats,
    /// Workflow/run aggregate from Loom (principal-scoped).
    pub workflows: WorkflowStats,
    /// Quality aggregate from Thymus (principal-scoped).
    pub quality: QualityStats,
    /// Newest 20 Broca action entries for the org, newest first.
    pub activity: Vec<ActivityEntry>,
    /// Usage metering placeholder -- always null fields with an explanatory note.
    pub usage: UsageStats,
}

/// Compose a [`DashboardResponse`] from the five kernel stores.
///
/// This function has no HTTP concerns: it is a pure async composition that
/// calls each store, maps the results, and returns the dashboard payload.
/// The [`dashboard`] handler is the thin HTTP wrapper.
///
/// `org` comes from the JWT's `org` claim and scopes the Soma and Broca calls.
/// `principal` comes from the JWT's `sub` claim and scopes the Chiasm, Loom,
/// and Thymus calls.
///
/// Store failures do not abort the composition. Each failing store is marked
/// `status: "error"` in `services` and its section carries zero/empty data.
/// A `Backend` error is returned only when the function itself fails structurally
/// (which cannot happen with the current independent-call design, but the
/// `Result` return type is preserved for forward compatibility).
#[allow(clippy::too_many_arguments)]
pub async fn compose_dashboard(
    soma: &SomaStore,
    chiasm: &ChiasmStore,
    broca: &BrocaStore,
    thymus: &ThymusStore,
    loom: &LoomStore,
    org: TenantId,
    principal: PrincipalId,
) -> Result<DashboardResponse, OperatorError> {
    // -- Soma: agent presence aggregate, scoped by org/tenant. --
    let (soma_health, agents) = match soma.stats(org).await {
        Ok(stats) => (
            ServiceHealth { name: "soma".into(), status: "ok".into() },
            AgentStats { total: stats.total, online: stats.online },
        ),
        Err(e) => {
            tracing::warn!("dashboard: soma.stats failed: {e}");
            (
                ServiceHealth { name: "soma".into(), status: "error".into() },
                AgentStats { total: 0, online: 0 },
            )
        }
    };

    // -- Chiasm: task aggregate, scoped by principal. --
    // "active" = total minus completed: covers active, queued, paused, blocked,
    // blocked_on_human, and stale statuses without hard-coding each name.
    let (chiasm_health, tasks) = match chiasm.stats(principal).await {
        Ok(stats) => {
            let completed = stats.by_status.get("completed").copied().unwrap_or(0);
            let active = stats.total - completed;
            (
                ServiceHealth { name: "chiasm".into(), status: "ok".into() },
                TaskStats { active, by_status: stats.by_status },
            )
        }
        Err(e) => {
            tracing::warn!("dashboard: chiasm.stats failed: {e}");
            (
                ServiceHealth { name: "chiasm".into(), status: "error".into() },
                TaskStats { active: 0, by_status: BTreeMap::new() },
            )
        }
    };

    // -- Loom: workflow/run aggregate, scoped by principal. --
    let (loom_health, workflows) = match loom.stats(principal).await {
        Ok(stats) => (
            ServiceHealth { name: "loom".into(), status: "ok".into() },
            WorkflowStats { runs_active: stats.active_runs },
        ),
        Err(e) => {
            tracing::warn!("dashboard: loom.stats failed: {e}");
            (
                ServiceHealth { name: "loom".into(), status: "error".into() },
                WorkflowStats { runs_active: 0 },
            )
        }
    };

    // -- Thymus: quality aggregate, scoped by principal. --
    let (thymus_health, quality) = match thymus.stats(principal).await {
        Ok(stats) => (
            ServiceHealth { name: "thymus".into(), status: "ok".into() },
            QualityStats { evaluations: stats.evaluations },
        ),
        Err(e) => {
            tracing::warn!("dashboard: thymus.stats failed: {e}");
            (
                ServiceHealth { name: "thymus".into(), status: "error".into() },
                QualityStats { evaluations: 0 },
            )
        }
    };

    // -- Broca: newest 20 action entries, scoped by org/tenant. --
    let (broca_health, activity) = match broca
        .query(org, ActionFilter { limit: Some(20), ..Default::default() })
        .await
    {
        Ok(entries) => {
            let activity = entries
                .into_iter()
                .map(|e| ActivityEntry {
                    id: e.id,
                    action: e.action,
                    service: e.service,
                    principal_id: e.principal_id.to_string(),
                    narrative: e.narrative,
                    created_at: e.created_at,
                })
                .collect();
            (ServiceHealth { name: "broca".into(), status: "ok".into() }, activity)
        }
        Err(e) => {
            tracing::warn!("dashboard: broca.query failed: {e}");
            (
                ServiceHealth { name: "broca".into(), status: "error".into() },
                Vec::new(),
            )
        }
    };

    // Usage: always null per the spec's honesty rule (Phase 2 billing metering, not yet wired).
    let usage = UsageStats {
        tokens_today: None,
        cost_today: None,
        note: "pending Plutus usage metering".into(),
    };

    Ok(DashboardResponse {
        services: vec![soma_health, chiasm_health, loom_health, thymus_health, broca_health],
        agents,
        tasks,
        workflows,
        quality,
        activity,
        usage,
    })
}

/// Handle `GET /api/dashboard`: compose a live snapshot of the authenticated operator's org.
///
/// The handler is a thin wrapper over [`compose_dashboard`]:
/// 1. The [`OperatorAuth`] extractor validates the Bearer JWT and extracts the
///    principal, org, and role. Produces 401 on any JWT failure.
/// 2. `auth.require(Permission::OrgRead)` enforces the RBAC gate. Produces 403
///    when the caller's role does not hold `OrgRead`.
/// 3. Delegates to [`compose_dashboard`] and returns the result as JSON.
pub async fn dashboard(
    State(state): State<OperatorState>,
    auth: OperatorAuth,
) -> Result<Json<DashboardResponse>, OperatorError> {
    // Gate: OrgRead permission is required for all dashboard access.
    auth.require(Permission::OrgRead)?;
    let resp = compose_dashboard(
        &state.soma,
        &state.chiasm,
        &state.broca,
        &state.thymus,
        &state.loom,
        auth.org,
        auth.principal,
    )
    .await?;
    Ok(Json(resp))
}

#[cfg(test)]
/// Tests for the dashboard composition and handler.
mod tests {
    use std::sync::Arc;

    use axum::body::{to_bytes, Body};
    use axum::http::{Request, StatusCode};
    use axum::routing::get;
    use axum::Router;
    use henosis_broca::{BrocaStore, LogAction};
    use henosis_chiasm::{ChiasmStore, NewTask};
    use henosis_loom::LoomStore;
    use henosis_plutus::MockPolicyBackend;
    use henosis_soma::{RegisterAgent, SomaStore};
    use henosis_thymus::ThymusStore;
    use syntheos_axon::AxonBus;
    use syntheos_contracts::{PrincipalId, PrincipalKind, TenantId};
    use syntheos_identity::{InMemoryDirectory, PrincipalDirectory};
    use tower::ServiceExt;

    use super::super::auth::{sign, OperatorClaims};
    use super::super::OperatorState;
    use super::dashboard;

    /// Build all in-memory stores and a seeded `OperatorState` for use in dashboard tests.
    ///
    /// Seeds:
    /// - Soma: one registered agent (`online` status).
    /// - Chiasm: one `active` task owned by `principal`.
    /// - Broca: one action logged for `org`.
    /// - Thymus and Loom: empty (stats will show zeroes).
    ///
    /// Returns `(state, org, principal, jwt_secret)` so the caller can mint JWTs.
    async fn seeded_state() -> (OperatorState, TenantId, PrincipalId, Arc<Vec<u8>>) {
        let bus = Arc::new(AxonBus::new());

        // The inner directory is kept as a concrete Arc so we can call enroll directly.
        let inner_dir = Arc::new(InMemoryDirectory::new());
        let directory: Arc<dyn PrincipalDirectory> = inner_dir.clone();

        let soma = Arc::new(
            SomaStore::open_in_memory(bus.clone(), directory.clone()).expect("soma store"),
        );
        let chiasm = Arc::new(ChiasmStore::open_in_memory(bus.clone()).expect("chiasm store"));
        let broca = Arc::new(BrocaStore::open_in_memory(bus.clone()).expect("broca store"));
        let loom =
            Arc::new(LoomStore::open_in_memory(bus.clone()).expect("loom store"));
        let thymus =
            Arc::new(ThymusStore::open_in_memory(bus.clone()).expect("thymus store"));

        // Pick fixed identifiers for deterministic assertions.
        let org = TenantId::new();
        // Enroll a principal in the directory so Soma accepts the registration.
        let principal = inner_dir
            .enroll(PrincipalKind::Agent, Some("test-agent".to_string()))
            .await
            .expect("enroll principal")
            .id;

        // -- Seed Soma: register the agent so soma.stats(org).total == 1. --
        soma.register(RegisterAgent {
            principal_id: principal,
            tenant: org,
            name: "test-agent".to_string(),
            agent_type: "test".to_string(),
            description: None,
            capabilities: None,
            config: None,
        })
        .await
        .expect("soma register");

        // -- Seed Chiasm: one active task for the principal. --
        chiasm
            .create(NewTask {
                tenant: org,
                principal_id: principal,
                project: "dashboard-test".to_string(),
                title: "seed task".to_string(),
                status: None, // defaults to Active
                summary: None,
                expected_output: None,
                output_format: None,
                assignee: None,
                heartbeat_interval_secs: None,
            })
            .await
            .expect("chiasm create task");

        // -- Seed Broca: one action for the org so activity.len() == 1. --
        broca
            .log(LogAction {
                tenant: org,
                principal_id: principal,
                service: None,
                action: "dashboard.test_action".to_string(),
                payload: None,
                narrative: None,
            })
            .await
            .expect("broca log");

        // Operator accounts store (the login flow; not tested here, just required by OperatorState).
        let accounts = Arc::new(
            syntheos_identity::SqliteDirectory::open_in_memory().expect("accounts store"),
        );
        // MockPolicyBackend::allow_all() covers OrgRead permission for a Viewer role JWT.
        let plutus: Arc<dyn henosis_plutus::PolicyBackend> =
            Arc::new(MockPolicyBackend::allow_all());
        let jwt_secret: Arc<Vec<u8>> =
            Arc::new(b"test-secret-32bytes-for-dashboard!".to_vec());

        let state = OperatorState {
            accounts,
            plutus,
            jwt_secret: jwt_secret.clone(),
            soma,
            chiasm,
            broca,
            thymus,
            loom,
            axon: bus,
            // Not exercised by these handler-level tests (no CORS preflight involved).
            cors_origins: Arc::new(vec![]),
        };

        (state, org, principal, jwt_secret)
    }

    /// Dashboard composes seeded stores and returns the documented null usage placeholder.
    ///
    /// Verifies:
    /// - 200 with a valid Viewer Bearer token.
    /// - `agents.total` == 1 (the one registered soma agent).
    /// - `tasks.active` == 1 (one active chiasm task).
    /// - `activity.len()` == 1 (one broca action logged).
    /// - `usage.tokens_today` is JSON null (honesty placeholder, not a stub).
    /// - All five service names appear in `services`.
    ///
    /// Also verifies that a request WITHOUT a Bearer token receives 401.
    #[tokio::test]
    async fn dashboard_composes_seeded_stores() {
        let (state, org, principal, jwt_secret) = seeded_state().await;

        // Mint a far-future Viewer JWT so it never expires during the test.
        let iat: i64 = 9_000_000_000;
        let claims = OperatorClaims::new(
            &principal.to_string(),
            &org.to_string(),
            "viewer",
            iat,
            86_400,
        );
        let token = sign(&claims, &jwt_secret).expect("sign token");

        let make_router = || {
            Router::new()
                .route("/api/dashboard", get(dashboard))
                .with_state(state.clone())
        };

        // -- Authenticated request: expect 200 with correct data. --
        let response = make_router()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/api/dashboard")
                    .header("Authorization", format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .expect("oneshot");

        assert_eq!(response.status(), StatusCode::OK, "authenticated GET /api/dashboard must be 200");

        let bytes = to_bytes(response.into_body(), 1 << 20).await.expect("body bytes");
        let body: serde_json::Value = serde_json::from_slice(&bytes).expect("body json");

        // Agent counts from Soma.
        assert_eq!(
            body["agents"]["total"],
            serde_json::json!(1),
            "agents.total must equal the one seeded agent: body = {body}"
        );

        // Task active count from Chiasm (one active task).
        assert_eq!(
            body["tasks"]["active"],
            serde_json::json!(1),
            "tasks.active must equal the one seeded active task: body = {body}"
        );

        // Activity from Broca (one logged action).
        assert_eq!(
            body["activity"].as_array().expect("activity is an array").len(),
            1,
            "activity must contain the one seeded broca action: body = {body}"
        );

        // Usage placeholder is always null per the spec's honesty rule.
        assert!(
            body["usage"]["tokens_today"].is_null(),
            "usage.tokens_today must be JSON null (documented honesty placeholder): body = {body}"
        );
        assert!(
            body["usage"]["cost_today"].is_null(),
            "usage.cost_today must be JSON null: body = {body}"
        );
        assert_eq!(
            body["usage"]["note"],
            serde_json::json!("pending Plutus usage metering"),
            "usage.note must match the spec string: body = {body}"
        );

        // All five kernel stores must appear in services.
        let services: Vec<String> = body["services"]
            .as_array()
            .expect("services is an array")
            .iter()
            .map(|s| s["name"].as_str().expect("name").to_string())
            .collect();
        for store in &["soma", "chiasm", "loom", "thymus", "broca"] {
            assert!(
                services.contains(&store.to_string()),
                "services must include {store}: services = {services:?}"
            );
        }

        // -- Unauthenticated request: expect 401. --
        let unauth_response = make_router()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/api/dashboard")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .expect("oneshot");

        assert_eq!(
            unauth_response.status(),
            StatusCode::UNAUTHORIZED,
            "GET /api/dashboard without a token must be 401"
        );
    }
}
