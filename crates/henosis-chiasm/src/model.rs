//! The Chiasm domain types, reshaped onto the Henosis principal model.
//!
//! The Kleos `Task` carried `user_id: i64` and a stringly `agent` field. Here ownership is a
//! [`PrincipalId`] (`principal_id`), the assignee is an `Option<PrincipalId>`, the task's identity
//! is a [`TaskId`] (UUID v8), status is a typed [`TaskStatus`] (not a free string), and timestamps
//! are [`Timestamp`] (UTC). No `user_id: i64` survives the port.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use syntheos_contracts::{PrincipalId, TaskId, TenantId, Timestamp};

use crate::error::ChiasmError;

/// The lifecycle state of a task.
///
/// Serializes snake_case (`active`, `blocked_on_human`, ...), matching the Kleos status strings so
/// the legacy backfill maps one-to-one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    /// Executing.
    Active,
    /// Paused by a user or the system.
    Paused,
    /// Waiting on a dependency.
    Blocked,
    /// Finished.
    Completed,
    /// Awaiting human input (e.g. feedback).
    BlockedOnHuman,
    /// Marked overdue by the heartbeat sweep.
    Stale,
    /// Unassigned, waiting for an agent to claim it.
    Queued,
}

/// Wire conversion and lifecycle helpers for task statuses.
impl TaskStatus {
    /// The canonical storage/wire token for this status.
    pub fn as_str(&self) -> &'static str {
        match self {
            TaskStatus::Active => "active",
            TaskStatus::Paused => "paused",
            TaskStatus::Blocked => "blocked",
            TaskStatus::Completed => "completed",
            TaskStatus::BlockedOnHuman => "blocked_on_human",
            TaskStatus::Stale => "stale",
            TaskStatus::Queued => "queued",
        }
    }

    /// Parse a status token, rejecting anything unknown ([`ChiasmError::InvalidStatus`]).
    pub fn parse(s: &str) -> Result<Self, ChiasmError> {
        match s {
            "active" => Ok(TaskStatus::Active),
            "paused" => Ok(TaskStatus::Paused),
            "blocked" => Ok(TaskStatus::Blocked),
            "completed" => Ok(TaskStatus::Completed),
            "blocked_on_human" => Ok(TaskStatus::BlockedOnHuman),
            "stale" => Ok(TaskStatus::Stale),
            "queued" => Ok(TaskStatus::Queued),
            other => Err(ChiasmError::InvalidStatus(other.to_string())),
        }
    }

    /// Whether this is a terminal state (no further transitions expected).
    pub fn is_terminal(&self) -> bool {
        matches!(self, TaskStatus::Completed)
    }
}

/// A coordination task: the unit Chiasm tracks through its lifecycle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Task {
    /// Stable task identity.
    pub id: TaskId,
    /// Tenant the task belongs to.
    pub tenant: TenantId,
    /// Owner principal (replaces Kleos `user_id`). All reads/writes scope on this.
    pub principal_id: PrincipalId,
    /// Assignee principal, or `None` when unassigned.
    pub assignee: Option<PrincipalId>,
    /// Project the task groups under.
    pub project: String,
    /// Human-readable title.
    pub title: String,
    /// Current lifecycle state.
    pub status: TaskStatus,
    /// Progress note / description.
    pub summary: Option<String>,
    /// Description of the expected output.
    pub expected_output: Option<String>,
    /// Output format hint (defaults to `raw`).
    pub output_format: String,
    /// Submitted output, once produced.
    pub output: Option<String>,
    /// Plan text (LLM generation deferred to the Broca extraction).
    pub plan: Option<String>,
    /// Reviewer feedback.
    pub feedback: Option<String>,
    /// Last heartbeat, or `None` if never beaten.
    pub last_heartbeat: Option<Timestamp>,
    /// Seconds between expected heartbeats before the task is stale.
    pub heartbeat_interval_secs: i64,
    /// Creation time.
    pub created_at: Timestamp,
    /// Last-modification time.
    pub updated_at: Timestamp,
}

/// The fields required to enroll a new task. Ids and timestamps are minted by the store.
#[derive(Debug, Clone)]
pub struct NewTask {
    /// Tenant the task belongs to.
    pub tenant: TenantId,
    /// Owner principal.
    pub principal_id: PrincipalId,
    /// Project the task groups under.
    pub project: String,
    /// Human-readable title.
    pub title: String,
    /// Initial status (defaults to [`TaskStatus::Active`]).
    pub status: Option<TaskStatus>,
    /// Optional progress note.
    pub summary: Option<String>,
    /// Optional description of the expected output.
    pub expected_output: Option<String>,
    /// Output format hint (defaults to `raw`).
    pub output_format: Option<String>,
    /// Optional assignee.
    pub assignee: Option<PrincipalId>,
    /// Heartbeat interval in seconds (defaults to 300).
    pub heartbeat_interval_secs: Option<i64>,
}

/// The fields required to enqueue an unassigned task for an agent to claim. A queued task is
/// minted with [`TaskStatus::Queued`] and no assignee; [`crate::ChiasmStore::claim_next`] later
/// assigns it.
#[derive(Debug, Clone)]
pub struct EnqueueTask {
    /// Tenant the task belongs to.
    pub tenant: TenantId,
    /// Owner principal (the enqueuer / queue scope).
    pub principal_id: PrincipalId,
    /// Project the task groups under.
    pub project: String,
    /// Human-readable title.
    pub title: String,
    /// Optional summary.
    pub summary: Option<String>,
}

/// A partial update to a task. Every field is optional; `None` leaves that column unchanged.
#[derive(Debug, Clone, Default)]
pub struct TaskPatch {
    /// New title.
    pub title: Option<String>,
    /// New status.
    pub status: Option<TaskStatus>,
    /// New summary.
    pub summary: Option<String>,
    /// New assignee (set only; clearing an assignee is not a slice-1 operation).
    pub assignee: Option<PrincipalId>,
}

/// Inspection helpers for partial task updates.
impl TaskPatch {
    /// Whether the patch carries no changes.
    pub fn is_empty(&self) -> bool {
        self.title.is_none()
            && self.status.is_none()
            && self.summary.is_none()
            && self.assignee.is_none()
    }
}

/// Filters for [`crate::ChiasmStore::list`]. All filters are AND-combined; `None` = no constraint.
#[derive(Debug, Clone, Default)]
pub struct TaskFilter {
    /// Only tasks with this status.
    pub status: Option<TaskStatus>,
    /// Only tasks in this project.
    pub project: Option<String>,
    /// Maximum rows to return (`None` = no limit).
    pub limit: Option<usize>,
    /// Rows to skip (for pagination).
    pub offset: Option<usize>,
}

/// One row of a task's change history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskUpdate {
    /// Append-only log id.
    pub id: i64,
    /// The task this entry belongs to.
    pub task_id: TaskId,
    /// Status recorded at this point.
    pub status: TaskStatus,
    /// Summary recorded at this point.
    pub summary: Option<String>,
    /// When the change was recorded.
    pub created_at: Timestamp,
}

/// One append-only dispatcher lifecycle event correlated with a Chiasm task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskActivity {
    /// Append-only log id.
    pub id: i64,
    /// The task this activity belongs to.
    pub task_id: TaskId,
    /// Tenant that dispatched the action.
    pub tenant: TenantId,
    /// Principal that dispatched the action and owns the task.
    pub principal_id: PrincipalId,
    /// Axon lifecycle kind such as `action.invoked` or `action.completed`.
    pub kind: String,
    /// Structured lifecycle payload with arguments and results deliberately omitted.
    pub payload: serde_json::Value,
    /// When the projection was recorded.
    pub created_at: Timestamp,
}

/// Aggregate task counts for one principal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChiasmStats {
    /// Total tasks owned by the principal.
    pub total: i64,
    /// Count per status token.
    pub by_status: BTreeMap<String, i64>,
}

/// A path claim: a TTL lease a task holds on a file path while an agent works it.
///
/// The Kleos claim was held by a stringly `agent` within a `user_id` shard. Here the claim is
/// held by its [`TaskId`] and scoped to the task's owner [`PrincipalId`]; heartbeats on the task
/// push `expires_at` forward, and the stale sweep releases the leases of any task it stales.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PathClaim {
    /// Lease log id (storage surrogate, not a projection key).
    pub id: i64,
    /// The task that holds this claim.
    pub task_id: TaskId,
    /// Owner principal of the claiming task. All claim reads/writes scope on this.
    pub principal_id: PrincipalId,
    /// Project the claimed path belongs to.
    pub project: String,
    /// The file path being claimed.
    pub path: String,
    /// When the claim was created.
    pub claimed_at: Timestamp,
    /// When the lease expires (heartbeats refresh this).
    pub expires_at: Timestamp,
    /// Whether the claim has been explicitly released.
    pub released: bool,
}

/// Lease-state helpers for path claims.
impl PathClaim {
    /// Whether this claim is active at `now`: not released and not yet expired. Expiry is
    /// compared here in Rust because the stored nanosecond RFC3339 strings do not order
    /// reliably in SQL.
    pub fn is_active_at(&self, now: &Timestamp) -> bool {
        !self.released && self.expires_at.as_offset_date_time() > now.as_offset_date_time()
    }
}

/// A conflict between a requested path and another task's active claim on it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PathConflict {
    /// The path that is already claimed.
    pub path: String,
    /// The task holding the conflicting claim.
    pub claimed_by_task: TaskId,
    /// Owner principal of the conflicting claim (the scope conflicts are checked within).
    pub claimed_by_principal: PrincipalId,
    /// When the conflicting lease expires.
    pub expires_at: Timestamp,
}

/// A dependency edge: the task `task_id` depends on `depends_on` completing first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Dependency {
    /// Edge log id (storage surrogate, not a projection key).
    pub id: i64,
    /// The dependent (downstream) task.
    pub task_id: TaskId,
    /// The task that must complete first.
    pub depends_on: TaskId,
    /// Title of the depended-on task (joined at read time; `None` if it was deleted mid-read).
    pub depends_on_title: Option<String>,
    /// Current status of the depended-on task (joined at read time).
    pub depends_on_status: Option<TaskStatus>,
    /// When the edge was created.
    pub created_at: Timestamp,
}
