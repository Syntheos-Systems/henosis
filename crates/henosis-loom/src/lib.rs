#![deny(missing_docs)]
#![warn(clippy::all)]
//! # henosis-loom
//!
//! Loom, the workflow-orchestration kernel service, extracted from `kleos-lib` onto the
//! Henosis substrate (Phase 1 Story 1.4, the fourth kernel service in the workspace).
//!
//! The Kleos loom service keyed workflows and runs on `i64` surrogate ids inside a
//! `user_id: i64` shard (hardcoded to 1 in row mapping). This extraction puts them on the
//! principal model: definitions and runs are owner-scoped projections keyed by
//! [`syntheos_contracts::WorkflowId`]/[`syntheos_contracts::RunId`], statuses and step types
//! are typed enums, and lifecycle changes publish typed events onto the in-process
//! [`syntheos_axon`] bus. No `user_id: i64` survives in any public type.
//!
//! The engine is dependency-driven: an advance pass starts every pending step whose
//! dependencies completed, feeding it the run input overlaid with dependency outputs, and
//! completes the run when nothing is pending or running. Execution goes through the
//! [`StepExecutor`] seam: the built-in [`TransformExecutor`] runs pure-JSON steps inline
//! today, Hephaestus provides the real executor in Phase 5 (story 5.5) with no API change,
//! and everything else completes externally via [`LoomStore::complete_step`].
//!
//! ## Scope
//!
//! Slice 1 (this commit): workflow CRUD with DAG validation, runs, the step engine, the
//! executor seam + transform executor, retry semantics, cancellation, run logs, timeout
//! enforcement ([`LoomStore::sweep_timeouts`]), and stats. NOT ported here: webhook/LLM step
//! executors (Hermes Phase 5 / Synapse Phase 4) and any legacy backfill (Kleos has no
//! production workflows worth absorbing; confirm against the prod DB before the cutover).

pub mod error;
pub mod events;
pub mod executor;
pub mod model;
pub mod store;

pub use error::LoomError;
pub use events::{
    RunCancelled, RunCompleted, RunCreated, RunFailed, StepCompleted, StepFailed, StepStarted,
    WORKFLOW_CHANNEL,
};
pub use executor::{
    interpolate, resolve_dot_path, set_dot_path, StepContext, StepExecutor, TransformExecutor,
};
pub use model::{
    LogEntry, LogLevel, LoomStats, NewWorkflow, Run, RunFilter, RunStatus, Step, StepDef,
    StepStatus, StepType, Workflow, WorkflowPatch,
};
pub use store::LoomStore;
