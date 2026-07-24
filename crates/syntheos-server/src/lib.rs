#![deny(missing_docs)]
#![warn(clippy::all)]
//! # syntheos-server
//!
//! The HTTP entry point for the Henosis runtime.
//!
//! [`production_router`] exposes liveness, version, authenticated operator routes, authenticated
//! machine-control routes, and an optional billing webhook. The broader [`router`] retains
//! caller-asserted kernel routes for explicit loopback compatibility and tests; production boot
//! does not mount those routes.
//!
//! Production dispatch uses [`public_gate_chain`] for Pistis capability checks, Plutus policy,
//! Eidolon safety policy, and approval classification. Credential operations cross the external
//! authenticated `phylaxd` broker through the executor. The `EidolonOutputFilter` scrubs
//! credential-bearing fields before results become replayable.
//!
//! Chiasm, Soma, Broca, Loom, and Thymus keep their own persistent stores. Their compatibility
//! HTTP routes are intentionally absent from [`production_router`].
//!
//! The surface is split into a library ([`router`] + [`AppState`]) so it can be unit-tested without
//! binding a socket; `main.rs` is the thin binary that wires state, initializes tracing, binds, and
//! serves with graceful shutdown.

pub mod app;

/// Authenticated public dispatch, machine-token, approval, and audit authority surface.
pub mod authority;

/// Safe command parsing, local initialization, and configuration loading for the `henosis` binary.
pub mod cli;

/// Projects dispatcher lifecycle events into Broca and task-scoped Chiasm activity.
pub mod action_reactor;

/// Production dispatcher executor for Hermes tools and phylaxd operations.
pub mod henosis_executor;

/// The optional Stripe billing webhook at `POST /billing/stripe/webhook`.
///
/// The default kernel server is unchanged when [`billing::BillingState`] is not
/// constructed (`SYNTHEOS_STRIPE_WEBHOOK_SECRET` unset), and the route 404s.
pub mod billing;

/// The operator API: JWT auth, RBAC extractor, dashboard, and WebSocket hub.
///
/// The default kernel server is unchanged when [`operator::OperatorState`] is not constructed.
pub mod operator;

pub use action_reactor::spawn_action_reactor;
pub use app::{
    eidolon_gate, live_gate_chain, production_router, public_gate_chain, router, AppState,
    BrocaFeedQuery, BrocaLogRequest, BrocaTenantQuery, ChiasmCreateTask, ChiasmListQuery,
    ChiasmOwnerQuery, EnrollRequest, LiveGateDependencies, LoomCompleteStep, LoomCreateRun,
    LoomCreateWorkflow, LoomFailStep, LoomLogsQuery, LoomOwnerQuery, LoomRunsQuery,
    SomaHeartbeatRequest, SomaListQuery, SomaQualityRequest, SomaQualitySink, SomaRegisterRequest,
    SomaStatsQuery, ThymusCreateRubric, ThymusDriftQuery, ThymusDriftSignal, ThymusEvaluate,
    ThymusEvaluationsQuery, ThymusMetricSummaryQuery, ThymusOwnerQuery, ThymusRecordDrift,
    ThymusRecordMetric,
};
pub use henosis_executor::HenosisExecutor;
