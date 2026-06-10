//! The workflow-lifecycle events Loom publishes onto the Axon bus.
//!
//! They live here (a service crate) rather than in `syntheos-contracts` because they are
//! Loom's domain events, but they implement the contracts' [`TypedEvent`] trait so any
//! in-process reactor (narration, evaluation) can subscribe without depending on Loom.
//! Payloads carry identifying strings only -- never step configs, inputs, or outputs, which
//! may hold detail that must not land on the ephemeral bus.

use serde::{Deserialize, Serialize};
use syntheos_contracts::TypedEvent;

/// The coarse channel every Loom workflow event travels on.
pub const WORKFLOW_CHANNEL: &str = "workflow";

/// A run was created (with its step instances).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunCreated {
    /// The new run's id.
    pub run_id: String,
    /// The workflow it executes.
    pub workflow_id: String,
    /// The workflow's name (for narration).
    pub workflow: String,
}

/// Emit `RunCreated` on the workflow channel.
impl TypedEvent for RunCreated {
    const CHANNEL: &'static str = WORKFLOW_CHANNEL;
    const KIND: &'static str = "workflow.run.created";
}

/// Every step finished and the run completed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunCompleted {
    /// The completed run's id.
    pub run_id: String,
}

/// Emit `RunCompleted` on the workflow channel.
impl TypedEvent for RunCompleted {
    const CHANNEL: &'static str = WORKFLOW_CHANNEL;
    const KIND: &'static str = "workflow.run.completed";
}

/// A step exhausted its retries and failed the run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunFailed {
    /// The failed run's id.
    pub run_id: String,
    /// The step whose failure was fatal.
    pub failed_step: String,
    /// The failure reason.
    pub error: String,
}

/// Emit `RunFailed` on the workflow channel.
impl TypedEvent for RunFailed {
    const CHANNEL: &'static str = WORKFLOW_CHANNEL;
    const KIND: &'static str = "workflow.run.failed";
}

/// A run was cancelled by its owner.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunCancelled {
    /// The cancelled run's id.
    pub run_id: String,
}

/// Emit `RunCancelled` on the workflow channel.
impl TypedEvent for RunCancelled {
    const CHANNEL: &'static str = WORKFLOW_CHANNEL;
    const KIND: &'static str = "workflow.run.cancelled";
}

/// A step became ready and started (inline or awaiting external completion).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StepStarted {
    /// The run the step belongs to.
    pub run_id: String,
    /// The step's run-internal id.
    pub step_id: i64,
    /// The step name.
    pub step: String,
}

/// Emit `StepStarted` on the workflow channel.
impl TypedEvent for StepStarted {
    const CHANNEL: &'static str = WORKFLOW_CHANNEL;
    const KIND: &'static str = "workflow.step.started";
}

/// A step completed with an output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StepCompleted {
    /// The run the step belongs to.
    pub run_id: String,
    /// The step's run-internal id.
    pub step_id: i64,
    /// The step name.
    pub step: String,
}

/// Emit `StepCompleted` on the workflow channel.
impl TypedEvent for StepCompleted {
    const CHANNEL: &'static str = WORKFLOW_CHANNEL;
    const KIND: &'static str = "workflow.step.completed";
}

/// A step attempt failed (it may retry; `will_retry` says which).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StepFailed {
    /// The run the step belongs to.
    pub run_id: String,
    /// The step's run-internal id.
    pub step_id: i64,
    /// The step name.
    pub step: String,
    /// The failure reason.
    pub error: String,
    /// Whether the step will be retried (false = the run failed).
    pub will_retry: bool,
}

/// Emit `StepFailed` on the workflow channel.
impl TypedEvent for StepFailed {
    const CHANNEL: &'static str = WORKFLOW_CHANNEL;
    const KIND: &'static str = "workflow.step.failed";
}
