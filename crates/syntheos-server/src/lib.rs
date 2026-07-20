#![deny(missing_docs)]
#![warn(clippy::all)]
//! # syntheos-server
//!
//! The single HTTP entry point for the Henosis agent OS: Phase 0 unit 5 (the capstone).
//!
//! It boots the Phase 0 foundation -- the [`syntheos_axon`] bus, the
//! [`syntheos_identity`] principal directory, and the [`syntheos_dispatch`] dispatcher -- into
//! shared [`AppState`] and serves a small surface: `/health`, `/version`, `POST /enroll`, and
//! `POST /dispatch`. The dispatch route runs an action through the real gate chain, so the whole
//! stack is exercised fail-closed end-to-end over the wire.
//!
//! The gate chain ([`live_gate_chain`]) now runs REAL gates in all five slots: PistisGate
//! (capability/trust, Story 3.3), PlutusGate (org status, RBAC, daily quota, rate limit, Story
//! 6.x / row 1), EidolonGate (prompt-injection, scope-violation, and persona-drift policy read
//! from Thymus via the [`ThymusDriftSignal`] adapter, Story 2.6), HumanGate (human-in-the-loop
//! approvals over Rift, Story 4.6), and PhylaxGate (credential resolution, Story 3.6, when
//! `SYNTHEOS_PHYLAX_KEY` is set; boot fails when it is absent). All five gate slots are real -- no
//! deny-stubs remain. The
//! `EidolonOutputFilter` is wired into the dispatcher's output slot, scrubbing credential-bearing
//! fields from executor results.
//!
//! All five Phase 1 kernel services are wired (Story 1.7), each with a persistent store opened
//! at boot: `/chiasm/tasks` (+ `/chiasm/stats`), `/soma/agents`
//! (register/list/get/heartbeat/quality, + `/soma/stats`), `/broca/actions`
//! (log/feed/get/narrate, + `/broca/stats`), `/loom/workflows` + `/loom/runs` (+
//! steps/logs/cancel/complete/fail, `/loom/stats`) with the built-in transform executor, and
//! `/thymus/rubrics` + `/thymus/evaluations` (+ agent scores, metrics, drift, `/thymus/stats`)
//! with evaluations propagating into Soma presence through the [`SomaQualitySink`] adapter.
//! The directory is the persistent `SqliteDirectory`, so enrolled principals survive restarts
//! alongside the projections that reference them. Identity on these surfaces is caller-asserted
//! (`principal_id` in the body/query) until PistisGate lands in Phase 3 -- same posture as
//! `/dispatch`'s `RequestContext`.
//!
//! The surface is split into a library ([`router`] + [`AppState`]) so it can be unit-tested without
//! binding a socket; `main.rs` is the thin binary that wires state, initializes tracing, binds, and
//! serves with graceful shutdown.

pub mod app;

/// Projects dispatcher lifecycle events into Broca and task-scoped Chiasm activity.
pub mod action_reactor;

/// Production dispatcher executor for Hermes tools and Phylax operations.
pub mod henosis_executor;

/// The Stripe billing webhook: `POST /billing/stripe/webhook` (Story 6.4a).
///
/// Additive -- the default kernel server is unchanged when [`billing::BillingState`] is not
/// constructed (`SYNTHEOS_STRIPE_WEBHOOK_SECRET` unset), and the route 404s.
pub mod billing;

/// The operator API: JWT auth, RBAC extractor, dashboard, and WebSocket hub.
///
/// Additive -- the default kernel server is unchanged when [`operator::OperatorState`]
/// is not constructed. Routes mount conditionally (Task 7).
pub mod operator;

pub use action_reactor::spawn_action_reactor;
pub use app::{
    eidolon_gate, live_gate_chain, router, AppState, BrocaFeedQuery, BrocaLogRequest,
    BrocaTenantQuery, ChiasmCreateTask, ChiasmListQuery, ChiasmOwnerQuery, EnrollRequest,
    LoomCompleteStep, LoomCreateRun, LoomCreateWorkflow, LoomFailStep, LoomLogsQuery,
    LoomOwnerQuery, LoomRunsQuery, SomaHeartbeatRequest, SomaListQuery, SomaQualityRequest,
    SomaQualitySink, SomaRegisterRequest, SomaStatsQuery, ThymusCreateRubric, ThymusDriftQuery,
    ThymusDriftSignal, ThymusEvaluate, ThymusEvaluationsQuery, ThymusMetricSummaryQuery,
    ThymusOwnerQuery, ThymusRecordDrift, ThymusRecordMetric,
};
pub use henosis_executor::HenosisExecutor;
