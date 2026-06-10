#![deny(missing_docs)]
#![warn(clippy::all)]
//! # syntheos-server
//!
//! The single HTTP entry point for the Henosis agent OS: Phase 0 unit 5 (the capstone).
//!
//! It boots the Phase 0 foundation -- the [`syntheos_axon`] bus, the
//! [`syntheos_identity`] principal directory, and the [`syntheos_dispatch`] dispatcher (running
//! the canonical deny-by-default gate chain, so every action is denied until real authorities
//! land) -- into shared [`AppState`] and serves a small surface: `/health`, `/version`,
//! `POST /enroll`, and `POST /dispatch`. The dispatch route runs an action through the real gate
//! chain, so the whole Phase 0 stack is exercised fail-closed end-to-end over the wire.
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

pub use app::{
    router, AppState, BrocaFeedQuery, BrocaLogRequest, BrocaTenantQuery, ChiasmCreateTask,
    ChiasmListQuery, ChiasmOwnerQuery, EnrollRequest, LoomCompleteStep, LoomCreateRun,
    LoomCreateWorkflow, LoomFailStep, LoomLogsQuery, LoomOwnerQuery, LoomRunsQuery,
    SomaHeartbeatRequest, SomaListQuery, SomaQualityRequest, SomaQualitySink, SomaRegisterRequest,
    SomaStatsQuery, ThymusCreateRubric, ThymusDriftQuery, ThymusEvaluate,
    ThymusEvaluationsQuery, ThymusMetricSummaryQuery, ThymusOwnerQuery, ThymusRecordDrift,
    ThymusRecordMetric,
};
