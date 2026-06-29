//! The Loom domain types, reshaped onto the Henosis principal model.
//!
//! The Kleos `Workflow`/`Run` carried `i64` surrogate keys and a `user_id: i64` owner (often
//! hardcoded to `1` in row mapping). Here workflows and runs are owner-scoped projections keyed
//! by [`WorkflowId`]/[`RunId`] (UUID v8, from `syntheos-contracts` -- they are referenced across
//! services from Phase 5 on), statuses and step types are typed enums, and timestamps are
//! [`Timestamp`] (UTC). Steps and logs keep `i64` keys: they are run-internal audit rows, the
//! `chiasm_task_updates` precedent. No `user_id: i64` survives the port.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use syntheos_contracts::{PrincipalId, RunId, TenantId, Timestamp, WorkflowId};

use crate::error::LoomError;

/// The kind of work a step performs.
///
/// Serializes snake_case, matching the Kleos type strings. `Transform` runs inline via the
/// built-in executor; `Hephaestus` dispatches in-process to the absorbed agent executor
/// (Phase 5, story 5.5) via the [`crate::HephaestusDispatch`] seam; `Webhook`/`Llm` wait
/// for their executors (Hermes/Synapse, Phases 4-5); the rest complete externally via
/// [`crate::LoomStore::complete_step`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepType {
    /// An externally executed action.
    Action,
    /// A decision point resolved by an external actor.
    Decision,
    /// A grouping step whose children run concurrently (externally coordinated).
    Parallel,
    /// A wait/delay step resolved externally.
    Wait,
    /// An HTTP webhook call (executor arrives with Hermes, Phase 5).
    Webhook,
    /// An LLM call (executor arrives with Synapse, Phase 4).
    Llm,
    /// A pure JSON transform, executed inline by [`crate::TransformExecutor`].
    Transform,
    /// An agent task dispatched in-process to the Hephaestus executor (Phase 5, story 5.5).
    ///
    /// The [`crate::HephaestusDispatch`] seam in the kernel crate keeps henosis-loom free of a
    /// runtime dependency on henosis-hephaestus; the real implementation lives in
    /// syntheos-server (the composition layer that depends on both).
    Hephaestus,
}

impl StepType {
    /// The canonical storage/wire token for this step type.
    pub fn as_str(&self) -> &'static str {
        match self {
            StepType::Action => "action",
            StepType::Decision => "decision",
            StepType::Parallel => "parallel",
            StepType::Wait => "wait",
            StepType::Webhook => "webhook",
            StepType::Llm => "llm",
            StepType::Transform => "transform",
            StepType::Hephaestus => "hephaestus",
        }
    }

    /// Parse a step-type token, rejecting anything unknown.
    pub fn parse(s: &str) -> Result<Self, LoomError> {
        match s {
            "action" => Ok(StepType::Action),
            "decision" => Ok(StepType::Decision),
            "parallel" => Ok(StepType::Parallel),
            "wait" => Ok(StepType::Wait),
            "webhook" => Ok(StepType::Webhook),
            "llm" => Ok(StepType::Llm),
            "transform" => Ok(StepType::Transform),
            "hephaestus" => Ok(StepType::Hephaestus),
            other => Err(LoomError::InvalidStatus(other.to_string())),
        }
    }
}

/// The lifecycle state of a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    /// Created, not yet advanced.
    Pending,
    /// At least one advance pass has happened and the run is not terminal.
    Running,
    /// Every step finished and the outputs merged.
    Completed,
    /// A step exhausted its retries (or timed out past them).
    Failed,
    /// Cancelled by the owner; unfinished steps were skipped.
    Cancelled,
}

impl RunStatus {
    /// The canonical storage/wire token for this status.
    pub fn as_str(&self) -> &'static str {
        match self {
            RunStatus::Pending => "pending",
            RunStatus::Running => "running",
            RunStatus::Completed => "completed",
            RunStatus::Failed => "failed",
            RunStatus::Cancelled => "cancelled",
        }
    }

    /// Parse a run-status token, rejecting anything unknown.
    pub fn parse(s: &str) -> Result<Self, LoomError> {
        match s {
            "pending" => Ok(RunStatus::Pending),
            "running" => Ok(RunStatus::Running),
            "completed" => Ok(RunStatus::Completed),
            "failed" => Ok(RunStatus::Failed),
            "cancelled" => Ok(RunStatus::Cancelled),
            other => Err(LoomError::InvalidStatus(other.to_string())),
        }
    }

    /// Whether this is a terminal state (no further transitions).
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            RunStatus::Completed | RunStatus::Failed | RunStatus::Cancelled
        )
    }
}

/// The lifecycle state of a step within a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepStatus {
    /// Waiting for its dependencies (or for an advance pass).
    Pending,
    /// Started; an executor or external actor owns it.
    Running,
    /// Finished with an output.
    Completed,
    /// Exhausted its retries.
    Failed,
    /// Abandoned because the run was cancelled.
    Skipped,
}

impl StepStatus {
    /// The canonical storage/wire token for this status.
    pub fn as_str(&self) -> &'static str {
        match self {
            StepStatus::Pending => "pending",
            StepStatus::Running => "running",
            StepStatus::Completed => "completed",
            StepStatus::Failed => "failed",
            StepStatus::Skipped => "skipped",
        }
    }

    /// Parse a step-status token, rejecting anything unknown.
    pub fn parse(s: &str) -> Result<Self, LoomError> {
        match s {
            "pending" => Ok(StepStatus::Pending),
            "running" => Ok(StepStatus::Running),
            "completed" => Ok(StepStatus::Completed),
            "failed" => Ok(StepStatus::Failed),
            "skipped" => Ok(StepStatus::Skipped),
            other => Err(LoomError::InvalidStatus(other.to_string())),
        }
    }
}

/// One step in a workflow definition (stored as a JSON array on the workflow).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepDef {
    /// Step name, unique within the workflow; `depends_on` refers to these.
    pub name: String,
    /// What kind of work the step performs.
    #[serde(rename = "type")]
    pub step_type: StepType,
    /// Executor-specific configuration (defaults to `{}`).
    #[serde(default)]
    pub config: Option<serde_json::Value>,
    /// Names of steps that must complete first (defaults to none).
    #[serde(default)]
    pub depends_on: Option<Vec<String>>,
    /// Retry budget before the step (and run) fail (defaults to 3).
    #[serde(default)]
    pub max_retries: Option<i32>,
    /// Per-attempt timeout in milliseconds (defaults to 30000).
    #[serde(default)]
    pub timeout_ms: Option<i64>,
}

/// A workflow definition: a named, owner-scoped DAG of [`StepDef`]s.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Workflow {
    /// Stable workflow identity.
    pub id: WorkflowId,
    /// Tenant the workflow belongs to.
    pub tenant: TenantId,
    /// Owner principal. All reads/writes scope on this.
    pub principal_id: PrincipalId,
    /// Workflow name, unique per owner.
    pub name: String,
    /// Optional human-readable description.
    pub description: Option<String>,
    /// The validated step definitions.
    pub steps: Vec<StepDef>,
    /// Creation time.
    pub created_at: Timestamp,
    /// Last-modification time.
    pub updated_at: Timestamp,
}

/// The fields required to define a new workflow. Ids and timestamps are minted by the store.
#[derive(Debug, Clone)]
pub struct NewWorkflow {
    /// Tenant the workflow belongs to.
    pub tenant: TenantId,
    /// Owner principal.
    pub principal_id: PrincipalId,
    /// Workflow name, unique per owner.
    pub name: String,
    /// Optional description.
    pub description: Option<String>,
    /// Step definitions (validated: unique names, known deps, acyclic).
    pub steps: Vec<StepDef>,
}

/// A partial update to a workflow definition. `None` leaves that field unchanged.
#[derive(Debug, Clone, Default)]
pub struct WorkflowPatch {
    /// New name.
    pub name: Option<String>,
    /// New description.
    pub description: Option<String>,
    /// Replacement step definitions (re-validated).
    pub steps: Option<Vec<StepDef>>,
}

/// One run of a workflow.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Run {
    /// Stable run identity.
    pub id: RunId,
    /// The workflow this run executes.
    pub workflow_id: WorkflowId,
    /// Tenant the run belongs to.
    pub tenant: TenantId,
    /// Owner principal (the runner).
    pub principal_id: PrincipalId,
    /// Current lifecycle state.
    pub status: RunStatus,
    /// The run input object.
    pub input: serde_json::Value,
    /// Merged step outputs once completed (`{}` until then).
    pub output: serde_json::Value,
    /// Failure reason, when failed.
    pub error: Option<String>,
    /// When the first advance pass started the run.
    pub started_at: Option<Timestamp>,
    /// When the run reached a terminal state.
    pub completed_at: Option<Timestamp>,
    /// Creation time.
    pub created_at: Timestamp,
    /// Last-modification time.
    pub updated_at: Timestamp,
}

/// One step instance within a run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Step {
    /// Run-internal step id (audit-style surrogate; steps are not cross-service projections).
    pub id: i64,
    /// The run this step belongs to.
    pub run_id: RunId,
    /// Step name from the definition.
    pub name: String,
    /// What kind of work the step performs.
    #[serde(rename = "type")]
    pub step_type: StepType,
    /// Executor-specific configuration.
    pub config: serde_json::Value,
    /// Current lifecycle state.
    pub status: StepStatus,
    /// The merged input handed to the step when it started (`{}` until then).
    pub input: serde_json::Value,
    /// The step output once completed (`{}` until then).
    pub output: serde_json::Value,
    /// Last failure message, if any (survives a retry reset).
    pub error: Option<String>,
    /// Names of steps that must complete first.
    pub depends_on: Vec<String>,
    /// How many times the step has been retried.
    pub retry_count: i32,
    /// Retry budget before the step fails the run.
    pub max_retries: i32,
    /// Per-attempt timeout in milliseconds (enforced by [`crate::LoomStore::sweep_timeouts`]).
    pub timeout_ms: i64,
    /// When the current attempt started.
    pub started_at: Option<Timestamp>,
    /// When the step reached a terminal state.
    pub completed_at: Option<Timestamp>,
    /// Creation time.
    pub created_at: Timestamp,
}

/// Severity of a run log line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogLevel {
    /// Normal progress.
    Info,
    /// Recoverable trouble (e.g. a retry).
    Warn,
    /// Failure.
    Error,
}

impl LogLevel {
    /// The canonical storage/wire token for this level.
    pub fn as_str(&self) -> &'static str {
        match self {
            LogLevel::Info => "info",
            LogLevel::Warn => "warn",
            LogLevel::Error => "error",
        }
    }

    /// Parse a level token, rejecting anything unknown.
    pub fn parse(s: &str) -> Result<Self, LoomError> {
        match s {
            "info" => Ok(LogLevel::Info),
            "warn" => Ok(LogLevel::Warn),
            "error" => Ok(LogLevel::Error),
            other => Err(LoomError::InvalidStatus(other.to_string())),
        }
    }
}

/// One line of a run's execution log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogEntry {
    /// Append-only log id.
    pub id: i64,
    /// The run this line belongs to.
    pub run_id: RunId,
    /// The step it concerns, when step-scoped.
    pub step_id: Option<i64>,
    /// Severity.
    pub level: LogLevel,
    /// The log message.
    pub message: String,
    /// Structured detail (`{}` when none).
    pub data: serde_json::Value,
    /// When the line was recorded.
    pub created_at: Timestamp,
}

/// Filters for [`crate::LoomStore::list_runs`]. All filters are AND-combined; `None` = no
/// constraint. Results are newest-first.
#[derive(Debug, Clone, Default)]
pub struct RunFilter {
    /// Only runs of this workflow.
    pub workflow_id: Option<WorkflowId>,
    /// Only runs in this status.
    pub status: Option<RunStatus>,
    /// Maximum rows to return (`None` = no limit).
    pub limit: Option<usize>,
    /// Rows to skip (for pagination).
    pub offset: Option<usize>,
}

/// Aggregate workflow/run counts for one principal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoomStats {
    /// Workflow definitions owned by the principal.
    pub workflows: i64,
    /// Total runs.
    pub runs: i64,
    /// Runs currently pending or running.
    pub active_runs: i64,
    /// Count per run-status token.
    pub runs_by_status: BTreeMap<String, i64>,
}
