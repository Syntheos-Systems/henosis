//! The task-lifecycle events Chiasm publishes onto the Axon bus.
//!
//! They live here (a service crate) rather than in `syntheos-contracts` because they are Chiasm's
//! domain events, but they implement the contracts' [`TypedEvent`] trait so any in-process reactor
//! (narration, evaluation, the future durable audit path) can subscribe without depending on
//! Chiasm. Payloads carry identifying strings only -- never task bodies or outputs -- so nothing
//! sensitive lands on the ephemeral bus, matching the action-lifecycle convention.

use serde::{Deserialize, Serialize};
use syntheos_contracts::TypedEvent;

/// The coarse channel every Chiasm task event travels on.
pub const TASK_CHANNEL: &str = "task";

/// A task was created.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskCreated {
    /// The new task's id.
    pub task_id: String,
    /// The project it groups under.
    pub project: String,
    /// Its title.
    pub title: String,
    /// Its initial status token.
    pub status: String,
}

/// Emit `TaskCreated` on the task channel.
impl TypedEvent for TaskCreated {
    const CHANNEL: &'static str = TASK_CHANNEL;
    const KIND: &'static str = "task.created";
}

/// A task changed status or fields (but did not complete).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskUpdated {
    /// The task's id.
    pub task_id: String,
    /// Its status token after the update.
    pub status: String,
}

/// Emit `TaskUpdated` on the task channel.
impl TypedEvent for TaskUpdated {
    const CHANNEL: &'static str = TASK_CHANNEL;
    const KIND: &'static str = "task.updated";
}

/// A task reached the `completed` terminal state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskCompleted {
    /// The completed task's id.
    pub task_id: String,
}

/// Emit `TaskCompleted` on the task channel.
impl TypedEvent for TaskCompleted {
    const CHANNEL: &'static str = TASK_CHANNEL;
    const KIND: &'static str = "task.completed";
}

/// A task was deleted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskDeleted {
    /// The deleted task's id.
    pub task_id: String,
}

/// Emit `TaskDeleted` on the task channel.
impl TypedEvent for TaskDeleted {
    const CHANNEL: &'static str = TASK_CHANNEL;
    const KIND: &'static str = "task.deleted";
}

/// A task was enqueued, unassigned, for an agent to claim.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskQueued {
    /// The queued task's id.
    pub task_id: String,
    /// The project it groups under.
    pub project: String,
}

/// Emit `TaskQueued` on the task channel.
impl TypedEvent for TaskQueued {
    const CHANNEL: &'static str = TASK_CHANNEL;
    const KIND: &'static str = "task.queued";
}

/// A queued task was claimed by an agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskClaimed {
    /// The claimed task's id.
    pub task_id: String,
    /// The principal that claimed it.
    pub assignee: String,
}

/// Emit `TaskClaimed` on the task channel.
impl TypedEvent for TaskClaimed {
    const CHANNEL: &'static str = TASK_CHANNEL;
    const KIND: &'static str = "task.claimed";
}

/// A task was marked stale because its heartbeat lapsed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskStale {
    /// The staled task's id.
    pub task_id: String,
}

/// Emit `TaskStale` on the task channel.
impl TypedEvent for TaskStale {
    const CHANNEL: &'static str = TASK_CHANNEL;
    const KIND: &'static str = "task.stale";
}
