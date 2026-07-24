#![deny(missing_docs)]
#![warn(clippy::all)]
//! # henosis-loom
//!
//! Loom orchestrates tenant-scoped workflows and their dependency-driven steps.
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
//! today, and everything else completes externally via [`LoomStore::complete_step`].
//!
//! Loom provides workflow CRUD with DAG validation, runs, the step engine, an executor seam
//! with a transform executor, retry semantics, cancellation, run logs, timeout enforcement
//! ([`LoomStore::sweep_timeouts`]), and statistics. Webhook and LLM steps remain externally
//! completed until an executor claims them.

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
    interpolate, resolve_dot_path, set_dot_path, CompositeStepExecutor, HephaestusDispatch,
    HephaestusStepExecutor, StepContext, StepExecutor, TransformExecutor,
};
pub use model::{
    LogEntry, LogLevel, LoomStats, NewWorkflow, Run, RunFilter, RunStatus, Step, StepDef,
    StepStatus, StepType, Workflow, WorkflowPatch,
};
pub use store::LoomStore;
