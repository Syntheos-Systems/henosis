//! The SQLite-backed Chiasm task store.
//!
//! Ownership is a [`PrincipalId`], lifecycle events are typed and published to the in-process
//! [`AxonBus`], and schema versioning uses `PRAGMA user_version` with `migrations/Vn__*.sql`.
//! One `Connection` is serialized behind a `Mutex`.
//!
//! The store supports task CRUD/history/statistics, the work queue (enqueue/claim), heartbeat
//! and stale sweep, path claims (TTL leases), dependency-DAG cycle checks and auto-unblock, and
//! legacy `user_id -> PrincipalId` imports.

use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use henosis_sqlite::OpenedDatabase;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior};
use syntheos_axon::AxonBus;
use syntheos_contracts::{PrincipalId, TaskId, TenantId, Timestamp, TypedEvent};

use crate::error::ChiasmError;
use crate::events::{
    ClaimCreated, ClaimReleased, TaskClaimed, TaskCompleted, TaskCreated, TaskDeleted, TaskQueued,
    TaskStale, TaskUnblocked, TaskUpdated,
};
use crate::model::{
    ChiasmStats, Dependency, EnqueueTask, NewTask, PathClaim, PathConflict, Task, TaskActivity,
    TaskFilter, TaskPatch, TaskStatus, TaskUpdate,
};

/// Maximum rows one list or query call may return.
///
/// Enforced by the store rather than trusted from the caller, so an omitted or
/// oversized limit cannot turn a listing into an unbounded scan.
const MAX_LIST_LIMIT: usize = 500;

/// Ordered schema migrations, applied by `PRAGMA user_version`. Append-only (see the DB convention).
const MIGRATIONS: &[(i64, &str)] = &[
    (1, include_str!("../migrations/V1__chiasm_tasks.sql")),
    (2, include_str!("../migrations/V2__chiasm_claims_deps.sql")),
    (3, include_str!("../migrations/V3__chiasm_legacy_maps.sql")),
    (
        4,
        include_str!("../migrations/V4__chiasm_legacy_map_source.sql"),
    ),
    (
        5,
        include_str!("../migrations/V5__chiasm_task_activity.sql"),
    ),
    (6, include_str!("../migrations/V6__chiasm_tenant_scope.sql")),
];

/// Seconds a heartbeat extends a task's unreleased path-claim leases by (Kleos parity: 600).
const HEARTBEAT_CLAIM_REFRESH_SECS: i64 = 600;

/// The columns of `chiasm_tasks`, in the order [`read_raw`] reads them.
const TASK_COLUMNS: &str = "id, tenant, principal_id, assignee, project, title, status, summary, \
    expected_output, output_format, output, plan, feedback, last_heartbeat, \
    heartbeat_interval_secs, created_at, updated_at";

/// The task-coordination store.
///
/// Share it as `Arc<ChiasmStore>`; all methods take `&self`.
pub struct ChiasmStore {
    /// The database and its path guard, serialized by a `Mutex`.
    conn: Mutex<OpenedDatabase>,
    /// The bus task-lifecycle events are published onto.
    bus: Arc<AxonBus>,
}

/// Map a generic rusqlite error to an opaque backend error.
pub(crate) fn berr(e: rusqlite::Error) -> ChiasmError {
    ChiasmError::Backend(e.to_string())
}

/// Serialize a [`Timestamp`] to its stored RFC3339-UTC string (via the contracts wire form).
pub(crate) fn ts_to_db(ts: &Timestamp) -> Result<String, ChiasmError> {
    serde_json::to_value(ts)
        .ok()
        .and_then(|v| v.as_str().map(String::from))
        .ok_or_else(|| ChiasmError::Backend("timestamp serialize".to_string()))
}

/// Parse a stored RFC3339 string back into a UTC-normalized [`Timestamp`].
pub(crate) fn ts_from_db(s: &str) -> Result<Timestamp, ChiasmError> {
    serde_json::from_value(serde_json::Value::String(s.to_string()))
        .map_err(|e| ChiasmError::Backend(format!("timestamp parse {s:?}: {e}")))
}

/// The instant `secs` seconds after `ts`. Used for path-claim lease expiry.
///
/// `OffsetDateTime`'s `Add` panics on overflow, and `secs` comes from a
/// caller-controlled `ttl_seconds`, so clamp the offset to +/-100 years (far
/// beyond any real lease, always representable from a current timestamp) and use
/// `checked_add` as a belt-and-suspenders no-op on any residual overflow. This
/// keeps a hostile or buggy ttl from panicking the request thread.
fn ts_plus(ts: &Timestamp, secs: i64) -> Timestamp {
    const MAX_OFFSET_SECS: i64 = 100 * 365 * 24 * 3600;
    let clamped = secs.clamp(-MAX_OFFSET_SECS, MAX_OFFSET_SECS);
    let base = ts.as_offset_date_time();
    let result = base
        .checked_add(time::Duration::seconds(clamped))
        .unwrap_or(base);
    Timestamp::from_utc(result)
}

/// The columns of `chiasm_path_claims`, in the order [`read_raw_claim`] reads them.
const CLAIM_COLUMNS: &str =
    "id, task_id, principal_id, project, path, claimed_at, expires_at, released";

/// The raw column values of one `chiasm_path_claims` row, before parsing into a [`PathClaim`].
struct RawClaim {
    /// Lease log id.
    id: i64,
    /// Holding task id (TaskId string).
    task_id: String,
    /// Owner principal (PrincipalId string).
    principal_id: String,
    /// Project the path belongs to.
    project: String,
    /// The claimed path.
    path: String,
    /// Claim creation time (RFC3339).
    claimed_at: String,
    /// Lease expiry time (RFC3339).
    expires_at: String,
    /// 0 = live lease, 1 = released.
    released: i64,
}

/// Read a `chiasm_path_claims` row positionally into a [`RawClaim`] (column order = [`CLAIM_COLUMNS`]).
fn read_raw_claim(row: &rusqlite::Row) -> rusqlite::Result<RawClaim> {
    Ok(RawClaim {
        id: row.get(0)?,
        task_id: row.get(1)?,
        principal_id: row.get(2)?,
        project: row.get(3)?,
        path: row.get(4)?,
        claimed_at: row.get(5)?,
        expires_at: row.get(6)?,
        released: row.get(7)?,
    })
}

/// Methods for `RawClaim`.
impl RawClaim {
    /// Parse raw columns into a typed [`PathClaim`], surfacing any corrupt value as a backend error.
    fn into_claim(self) -> Result<PathClaim, ChiasmError> {
        Ok(PathClaim {
            id: self.id,
            task_id: self.task_id.parse::<TaskId>().map_err(|e| {
                ChiasmError::Backend(format!("corrupt task_id {:?}: {e}", self.task_id))
            })?,
            principal_id: self.principal_id.parse::<PrincipalId>().map_err(|e| {
                ChiasmError::Backend(format!("corrupt principal_id {:?}: {e}", self.principal_id))
            })?,
            project: self.project,
            path: self.path,
            claimed_at: ts_from_db(&self.claimed_at)?,
            expires_at: ts_from_db(&self.expires_at)?,
            released: self.released != 0,
        })
    }
}

/// The raw column values of one `chiasm_tasks` row, before parsing into typed [`Task`] fields.
struct RawTask {
    id: String,
    tenant: String,
    principal_id: String,
    assignee: Option<String>,
    project: String,
    title: String,
    status: String,
    summary: Option<String>,
    expected_output: Option<String>,
    output_format: String,
    output: Option<String>,
    plan: Option<String>,
    feedback: Option<String>,
    last_heartbeat: Option<String>,
    heartbeat_interval_secs: i64,
    created_at: String,
    updated_at: String,
}

/// Read a `chiasm_tasks` row positionally into a [`RawTask`] (column order = [`TASK_COLUMNS`]).
fn read_raw(row: &rusqlite::Row) -> rusqlite::Result<RawTask> {
    Ok(RawTask {
        id: row.get(0)?,
        tenant: row.get(1)?,
        principal_id: row.get(2)?,
        assignee: row.get(3)?,
        project: row.get(4)?,
        title: row.get(5)?,
        status: row.get(6)?,
        summary: row.get(7)?,
        expected_output: row.get(8)?,
        output_format: row.get(9)?,
        output: row.get(10)?,
        plan: row.get(11)?,
        feedback: row.get(12)?,
        last_heartbeat: row.get(13)?,
        heartbeat_interval_secs: row.get(14)?,
        created_at: row.get(15)?,
        updated_at: row.get(16)?,
    })
}

/// Methods for `RawTask`.
impl RawTask {
    /// Parse raw columns into a typed [`Task`], surfacing any corrupt value as a backend error.
    fn into_task(self) -> Result<Task, ChiasmError> {
        let parse_id = |s: &str, what: &str| -> Result<PrincipalId, ChiasmError> {
            s.parse::<PrincipalId>()
                .map_err(|e| ChiasmError::Backend(format!("corrupt {what} {s:?}: {e}")))
        };
        Ok(Task {
            id: self
                .id
                .parse::<TaskId>()
                .map_err(|e| ChiasmError::Backend(format!("corrupt task id {:?}: {e}", self.id)))?,
            tenant: self.tenant.parse::<TenantId>().map_err(|e| {
                ChiasmError::Backend(format!("corrupt tenant {:?}: {e}", self.tenant))
            })?,
            principal_id: parse_id(&self.principal_id, "principal_id")?,
            assignee: self
                .assignee
                .as_deref()
                .map(|s| parse_id(s, "assignee"))
                .transpose()?,
            project: self.project,
            title: self.title,
            status: TaskStatus::parse(&self.status)?,
            summary: self.summary,
            expected_output: self.expected_output,
            output_format: self.output_format,
            output: self.output,
            plan: self.plan,
            feedback: self.feedback,
            last_heartbeat: self.last_heartbeat.as_deref().map(ts_from_db).transpose()?,
            heartbeat_interval_secs: self.heartbeat_interval_secs,
            created_at: ts_from_db(&self.created_at)?,
            updated_at: ts_from_db(&self.updated_at)?,
        })
    }
}

/// Methods for `ChiasmStore`.
impl ChiasmStore {
    /// Open (creating the file if absent) a store at `path`, applying any pending migrations.
    pub fn open(path: impl AsRef<Path>, bus: Arc<AxonBus>) -> Result<Self, ChiasmError> {
        let database = henosis_sqlite::open_database(path)
            .map_err(|error| ChiasmError::Backend(error.to_string()))?;
        Self::from_database(database, bus)
    }

    /// Open an ephemeral in-memory store. For tests and throwaway use.
    pub fn open_in_memory(bus: Arc<AxonBus>) -> Result<Self, ChiasmError> {
        let database = OpenedDatabase::open_in_memory().map_err(berr)?;
        Self::from_database(database, bus)
    }

    /// Configure and migrate the database while it retains its path guard.
    fn from_database(mut database: OpenedDatabase, bus: Arc<AxonBus>) -> Result<Self, ChiasmError> {
        database
            .busy_timeout(Duration::from_secs(5))
            .map_err(berr)?;
        database
            .pragma_update(None, "foreign_keys", true)
            .map_err(berr)?;
        apply_migrations(&mut database)?;
        Ok(Self {
            conn: Mutex::new(database),
            bus,
        })
    }

    /// Lock the connection, recovering from a poisoned mutex.
    fn lock(&self) -> MutexGuard<'_, OpenedDatabase> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Publish a task event, fire-and-forget. A publish failure is logged, never fatal -- telemetry
    /// must not change a task operation's outcome.
    fn emit<E: TypedEvent>(&self, event: &E, tenant: TenantId, principal: PrincipalId) {
        if let Err(e) = self.bus.publish_event(event, tenant, principal) {
            tracing::warn!(error = %e, kind = E::KIND, "failed to publish chiasm task event");
        }
    }

    /// Create a new task, minting its id and timestamps, and emit `task.created`.
    pub async fn create(&self, new: NewTask) -> Result<Task, ChiasmError> {
        let now = Timestamp::now();
        let task = Task {
            id: TaskId::new(),
            tenant: new.tenant,
            principal_id: new.principal_id,
            assignee: new.assignee,
            project: new.project,
            title: new.title,
            status: new.status.unwrap_or(TaskStatus::Active),
            summary: new.summary,
            expected_output: new.expected_output,
            output_format: new.output_format.unwrap_or_else(|| "raw".to_string()),
            output: None,
            plan: None,
            feedback: None,
            last_heartbeat: None,
            heartbeat_interval_secs: new.heartbeat_interval_secs.unwrap_or(300),
            created_at: now,
            updated_at: now,
        };
        {
            let conn = self.lock();
            insert_task(&conn, &task)?;
        }
        self.emit(
            &TaskCreated {
                task_id: task.id.to_string(),
                project: task.project.clone(),
                title: task.title.clone(),
                status: task.status.as_str().to_string(),
            },
            task.tenant,
            task.principal_id,
        );
        Ok(task)
    }

    /// Look up a task by id, scoped to its tenant and owner.
    ///
    /// Returns `Ok(None)` if absent or outside the supplied identity boundary.
    pub async fn get(
        &self,
        tenant: TenantId,
        principal: PrincipalId,
        id: TaskId,
    ) -> Result<Option<Task>, ChiasmError> {
        let conn = self.lock();
        Self::get_in(&conn, tenant, principal, id)
    }

    /// Tenant-and-owner-scoped lookup against an arbitrary connection.
    fn get_in(
        conn: &Connection,
        tenant: TenantId,
        principal: PrincipalId,
        id: TaskId,
    ) -> Result<Option<Task>, ChiasmError> {
        let raw = conn
            .query_row(
                &format!(
                    "SELECT {TASK_COLUMNS} FROM chiasm_tasks \
                     WHERE id = ?1 AND tenant = ?2 AND principal_id = ?3"
                ),
                rusqlite::params![id.to_string(), tenant.to_string(), principal.to_string()],
                read_raw,
            )
            .optional()
            .map_err(berr)?;
        raw.map(RawTask::into_task).transpose()
    }

    /// List a tenant-bound principal's tasks, newest-updated first, AND-filtered by [`TaskFilter`].
    pub async fn list(
        &self,
        tenant: TenantId,
        principal: PrincipalId,
        filter: TaskFilter,
    ) -> Result<Vec<Task>, ChiasmError> {
        let mut sql = format!(
            "SELECT {TASK_COLUMNS} FROM chiasm_tasks WHERE tenant = ?1 AND principal_id = ?2"
        );
        let mut args: Vec<rusqlite::types::Value> =
            vec![tenant.to_string().into(), principal.to_string().into()];
        let mut n = 2;
        if let Some(status) = &filter.status {
            n += 1;
            sql.push_str(&format!(" AND status = ?{n}"));
            args.push(status.as_str().to_string().into());
        }
        if let Some(project) = &filter.project {
            n += 1;
            sql.push_str(&format!(" AND project = ?{n}"));
            args.push(project.clone().into());
        }
        sql.push_str(" ORDER BY updated_at DESC");
        // A caller-supplied limit is advisory. An omitted or oversized value would
        // otherwise scan and serialize every row this tenant has accumulated while
        // holding the process-wide connection mutex, stalling every other tenant.
        let limit = filter.limit.unwrap_or(MAX_LIST_LIMIT).min(MAX_LIST_LIMIT);
        match filter.offset {
            Some(offset) => sql.push_str(&format!(" LIMIT {limit} OFFSET {offset}")),
            None => sql.push_str(&format!(" LIMIT {limit}")),
        }
        let conn = self.lock();
        let mut stmt = conn.prepare(&sql).map_err(berr)?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(args.iter()), read_raw)
            .map_err(berr)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(berr)?.into_task()?);
        }
        Ok(out)
    }

    /// Apply a partial update to a tenant-bound task and append its history transactionally.
    pub async fn update(
        &self,
        tenant: TenantId,
        principal: PrincipalId,
        id: TaskId,
        patch: TaskPatch,
    ) -> Result<Task, ChiasmError> {
        let task = {
            let mut conn = self.lock();
            let tx = conn.transaction().map_err(berr)?;
            let mut task =
                Self::get_in(&tx, tenant, principal, id)?.ok_or(ChiasmError::NotFound(id))?;
            if let Some(title) = patch.title {
                task.title = title;
            }
            if let Some(status) = patch.status {
                task.status = status;
            }
            if let Some(summary) = patch.summary {
                task.summary = Some(summary);
            }
            if let Some(assignee) = patch.assignee {
                task.assignee = Some(assignee);
            }
            task.updated_at = Timestamp::now();
            tx.execute(
                "UPDATE chiasm_tasks SET title = ?1, status = ?2, summary = ?3, assignee = ?4, \
                 updated_at = ?5 WHERE id = ?6 AND tenant = ?7 AND principal_id = ?8",
                rusqlite::params![
                    &task.title,
                    task.status.as_str(),
                    &task.summary,
                    task.assignee.map(|a| a.to_string()),
                    ts_to_db(&task.updated_at)?,
                    task.id.to_string(),
                    tenant.to_string(),
                    principal.to_string(),
                ],
            )
            .map_err(berr)?;
            tx.execute(
                "INSERT INTO chiasm_task_updates (task_id, status, summary, created_at) \
                 VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![
                    task.id.to_string(),
                    task.status.as_str(),
                    &task.summary,
                    ts_to_db(&task.updated_at)?,
                ],
            )
            .map_err(berr)?;
            tx.commit().map_err(berr)?;
            task
        };
        if task.status.is_terminal() {
            self.emit(
                &TaskCompleted {
                    task_id: task.id.to_string(),
                },
                task.tenant,
                task.principal_id,
            );
        } else {
            self.emit(
                &TaskUpdated {
                    task_id: task.id.to_string(),
                    status: task.status.as_str().to_string(),
                },
                task.tenant,
                task.principal_id,
            );
        }
        Ok(task)
    }

    /// Delete a tenant-bound task and emit `task.deleted` when a row was removed.
    pub async fn delete(
        &self,
        tenant: TenantId,
        principal: PrincipalId,
        id: TaskId,
    ) -> Result<bool, ChiasmError> {
        // Fetch first (scoped) so the event can carry the task's tenant/principal.
        let Some(task) = self.get(tenant, principal, id).await? else {
            return Ok(false);
        };
        let removed = {
            let conn = self.lock();
            conn.execute(
                "DELETE FROM chiasm_tasks WHERE id = ?1 AND tenant = ?2 AND principal_id = ?3",
                rusqlite::params![id.to_string(), tenant.to_string(), principal.to_string()],
            )
            .map_err(berr)?
        };
        if removed > 0 {
            self.emit(
                &TaskDeleted {
                    task_id: id.to_string(),
                },
                task.tenant,
                task.principal_id,
            );
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Return a tenant-bound task's change history, newest first, capped at `limit`.
    pub async fn history(
        &self,
        tenant: TenantId,
        principal: PrincipalId,
        id: TaskId,
        limit: usize,
    ) -> Result<Vec<TaskUpdate>, ChiasmError> {
        let conn = self.lock();
        let mut stmt = conn
            .prepare(
                "SELECT u.id, u.task_id, u.status, u.summary, u.created_at \
                 FROM chiasm_task_updates u JOIN chiasm_tasks t ON t.id = u.task_id \
                 WHERE u.task_id = ?1 AND t.tenant = ?2 AND t.principal_id = ?3 \
                 ORDER BY u.id DESC LIMIT ?4",
            )
            .map_err(berr)?;
        let rows = stmt
            .query_map(
                rusqlite::params![
                    id.to_string(),
                    tenant.to_string(),
                    principal.to_string(),
                    limit as i64
                ],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, Option<String>>(3)?,
                        r.get::<_, String>(4)?,
                    ))
                },
            )
            .map_err(berr)?;
        let mut out = Vec::new();
        for row in rows {
            let (uid, task_id, status, summary, created_at) = row.map_err(berr)?;
            out.push(TaskUpdate {
                id: uid,
                task_id: task_id.parse::<TaskId>().map_err(|e| {
                    ChiasmError::Backend(format!("corrupt task_id {task_id:?}: {e}"))
                })?,
                status: TaskStatus::parse(&status)?,
                summary,
                created_at: ts_from_db(&created_at)?,
            });
        }
        Ok(out)
    }

    /// Append one dispatcher lifecycle event to a tenant-bound task without changing task state.
    ///
    /// The insert is tenant- and principal-scoped in the same SQL statement. A missing task or
    /// identity mismatch returns [`ChiasmError::NotFound`] and records nothing.
    pub async fn record_activity(
        &self,
        tenant: TenantId,
        principal: PrincipalId,
        id: TaskId,
        kind: impl Into<String>,
        payload: serde_json::Value,
    ) -> Result<TaskActivity, ChiasmError> {
        if !payload.is_object() {
            return Err(ChiasmError::Backend(
                "task activity payload must be a JSON object".to_string(),
            ));
        }
        let kind = kind.into();
        let created_at = Timestamp::now();
        let conn = self.lock();
        let inserted = conn
            .execute(
                "INSERT INTO chiasm_task_activity \
                 (task_id, tenant, principal_id, kind, payload, created_at) \
                 SELECT ?1, ?2, ?3, ?4, ?5, ?6 \
                 WHERE EXISTS (SELECT 1 FROM chiasm_tasks \
                 WHERE id = ?1 AND tenant = ?2 AND principal_id = ?3)",
                rusqlite::params![
                    id.to_string(),
                    tenant.to_string(),
                    principal.to_string(),
                    &kind,
                    payload.to_string(),
                    ts_to_db(&created_at)?,
                ],
            )
            .map_err(berr)?;
        if inserted == 0 {
            return Err(ChiasmError::NotFound(id));
        }
        Ok(TaskActivity {
            id: conn.last_insert_rowid(),
            task_id: id,
            tenant,
            principal_id: principal,
            kind,
            payload,
            created_at,
        })
    }

    /// Return a tenant-bound task's dispatcher activity, newest first, capped at `limit`.
    pub async fn activity(
        &self,
        tenant: TenantId,
        principal: PrincipalId,
        id: TaskId,
        limit: usize,
    ) -> Result<Vec<TaskActivity>, ChiasmError> {
        let conn = self.lock();
        let mut stmt = conn
            .prepare(
                "SELECT a.id, a.task_id, a.tenant, a.principal_id, a.kind, a.payload, a.created_at \
                 FROM chiasm_task_activity a JOIN chiasm_tasks t ON t.id = a.task_id \
                 WHERE a.task_id = ?1 AND a.tenant = ?2 AND t.tenant = ?2 \
                 AND a.principal_id = ?3 AND t.principal_id = ?3 \
                 ORDER BY a.id DESC LIMIT ?4",
            )
            .map_err(berr)?;
        let rows = stmt
            .query_map(
                rusqlite::params![
                    id.to_string(),
                    tenant.to_string(),
                    principal.to_string(),
                    limit as i64
                ],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, String>(5)?,
                        row.get::<_, String>(6)?,
                    ))
                },
            )
            .map_err(berr)?;
        let mut out = Vec::new();
        for row in rows {
            let (activity_id, task_id, tenant, principal_id, kind, payload, created_at) =
                row.map_err(berr)?;
            out.push(TaskActivity {
                id: activity_id,
                task_id: task_id.parse::<TaskId>().map_err(|error| {
                    ChiasmError::Backend(format!("corrupt task_id {task_id:?}: {error}"))
                })?,
                tenant: tenant.parse::<TenantId>().map_err(|error| {
                    ChiasmError::Backend(format!("corrupt tenant {tenant:?}: {error}"))
                })?,
                principal_id: principal_id.parse::<PrincipalId>().map_err(|error| {
                    ChiasmError::Backend(format!("corrupt principal_id {principal_id:?}: {error}"))
                })?,
                kind,
                payload: serde_json::from_str(&payload).map_err(|error| {
                    ChiasmError::Backend(format!("corrupt activity payload: {error}"))
                })?,
                created_at: ts_from_db(&created_at)?,
            });
        }
        Ok(out)
    }

    /// Aggregate task counts for a tenant-bound principal.
    pub async fn stats(
        &self,
        tenant: TenantId,
        principal: PrincipalId,
    ) -> Result<ChiasmStats, ChiasmError> {
        let conn = self.lock();
        let mut stmt = conn
            .prepare(
                "SELECT status, COUNT(*) FROM chiasm_tasks \
                 WHERE tenant = ?1 AND principal_id = ?2 GROUP BY status",
            )
            .map_err(berr)?;
        let rows = stmt
            .query_map(
                rusqlite::params![tenant.to_string(), principal.to_string()],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
            )
            .map_err(berr)?;
        let mut by_status = std::collections::BTreeMap::new();
        let mut total = 0;
        for row in rows {
            let (status, count) = row.map_err(berr)?;
            total += count;
            by_status.insert(status, count);
        }
        Ok(ChiasmStats { total, by_status })
    }

    /// Enqueue an unassigned task ([`TaskStatus::Queued`], no assignee) for an agent to claim
    /// later, and emit `task.queued`.
    pub async fn enqueue(&self, new: EnqueueTask) -> Result<Task, ChiasmError> {
        let now = Timestamp::now();
        let task = Task {
            id: TaskId::new(),
            tenant: new.tenant,
            principal_id: new.principal_id,
            assignee: None,
            project: new.project,
            title: new.title,
            status: TaskStatus::Queued,
            summary: new.summary,
            expected_output: None,
            output_format: "raw".to_string(),
            output: None,
            plan: None,
            feedback: None,
            last_heartbeat: None,
            heartbeat_interval_secs: 300,
            created_at: now,
            updated_at: now,
        };
        {
            let conn = self.lock();
            insert_task(&conn, &task)?;
        }
        self.emit(
            &TaskQueued {
                task_id: task.id.to_string(),
                project: task.project.clone(),
            },
            task.tenant,
            task.principal_id,
        );
        Ok(task)
    }

    /// Atomically claim the oldest queued, unassigned task owned by `owner` (optionally filtered to
    /// `project`), assigning it to `claimer`, flipping it to [`TaskStatus::Active`], and stamping a
    /// first heartbeat. `Ok(None)` when the queue is empty. Emits `task.claimed` on success.
    ///
    /// The claim is a single `UPDATE ... WHERE id = (SELECT ... LIMIT 1) RETURNING`, so two
    /// concurrent claimers can never grab the same task even without the connection `Mutex`.
    pub async fn claim_next(
        &self,
        tenant: TenantId,
        owner: PrincipalId,
        claimer: PrincipalId,
        project: Option<&str>,
    ) -> Result<Option<Task>, ChiasmError> {
        let now = ts_to_db(&Timestamp::now())?;
        let project_clause = if project.is_some() {
            "AND project = ?5"
        } else {
            ""
        };
        let sql = format!(
            "UPDATE chiasm_tasks SET assignee = ?3, status = 'active', last_heartbeat = ?4, \
             updated_at = ?4 WHERE id = (SELECT id FROM chiasm_tasks \
             WHERE tenant = ?1 AND principal_id = ?2 \
             AND assignee IS NULL AND status = 'queued' {project_clause} \
             ORDER BY rowid ASC LIMIT 1) AND tenant = ?1 AND principal_id = ?2 \
             RETURNING {TASK_COLUMNS}"
        );
        let raw = {
            let conn = self.lock();
            let result = match project {
                Some(p) => conn.query_row(
                    &sql,
                    rusqlite::params![
                        tenant.to_string(),
                        owner.to_string(),
                        claimer.to_string(),
                        now,
                        p
                    ],
                    read_raw,
                ),
                None => conn.query_row(
                    &sql,
                    rusqlite::params![
                        tenant.to_string(),
                        owner.to_string(),
                        claimer.to_string(),
                        now
                    ],
                    read_raw,
                ),
            };
            result.optional().map_err(berr)?
        };
        let task = raw.map(RawTask::into_task).transpose()?;
        if let Some(task) = &task {
            self.emit(
                &TaskClaimed {
                    task_id: task.id.to_string(),
                    assignee: claimer.to_string(),
                },
                task.tenant,
                task.principal_id,
            );
        }
        Ok(task)
    }

    /// Record a liveness heartbeat for a tenant-bound task, refreshing `last_heartbeat` and
    /// extending every unreleased path-claim lease the task holds to now + 600s. The refresh is
    /// fire-and-forget (Kleos parity): a refresh failure is logged, never fatal, because task
    /// liveness must not depend on the claims table. Like Kleos, the refresh also revives an
    /// unreleased lease that had already lapsed -- the heartbeat proves the holder is still
    /// alive and working. Returns [`ChiasmError::NotFound`] if the task does not exist or is
    /// outside the supplied tenant-and-owner boundary.
    pub async fn record_heartbeat(
        &self,
        tenant: TenantId,
        principal: PrincipalId,
        id: TaskId,
    ) -> Result<(), ChiasmError> {
        let now_ts = Timestamp::now();
        let now = ts_to_db(&now_ts)?;
        let lease = ts_to_db(&ts_plus(&now_ts, HEARTBEAT_CLAIM_REFRESH_SECS))?;
        let updated = {
            let conn = self.lock();
            let updated = conn
                .execute(
                    "UPDATE chiasm_tasks SET last_heartbeat = ?1, updated_at = ?1 \
                     WHERE id = ?2 AND tenant = ?3 AND principal_id = ?4",
                    rusqlite::params![
                        now,
                        id.to_string(),
                        tenant.to_string(),
                        principal.to_string()
                    ],
                )
                .map_err(berr)?;
            if updated > 0 {
                if let Err(e) = conn.execute(
                    "UPDATE chiasm_path_claims SET expires_at = ?1 \
                     WHERE task_id = ?2 AND released = 0 \
                     AND EXISTS (SELECT 1 FROM chiasm_tasks \
                     WHERE id = ?2 AND tenant = ?3 AND principal_id = ?4)",
                    rusqlite::params![
                        lease,
                        id.to_string(),
                        tenant.to_string(),
                        principal.to_string()
                    ],
                ) {
                    tracing::warn!(error = %e, task = %id, "failed to refresh path-claim leases on heartbeat");
                }
            }
            updated
        };
        if updated == 0 {
            return Err(ChiasmError::NotFound(id));
        }
        Ok(())
    }

    /// System-wide sweep (NOT owner-scoped -- a maintenance task) that marks every active/paused
    /// task whose heartbeat is overdue as [`TaskStatus::Stale`], appends a history row, releases
    /// the task's path-claim leases (a stale task forfeits its claims, Kleos parity), and emits
    /// `task.stale` (plus `claim.released` when leases were forfeited). A task is overdue when
    /// `now - last_heartbeat > heartbeat_interval_secs * grace_multiplier`. Returns the tasks it
    /// staled.
    ///
    /// The overdue comparison is computed in Rust rather than SQL so it does not depend on SQLite
    /// parsing nanosecond-precision RFC3339 timestamps.
    pub async fn mark_stale(&self, grace_multiplier: f64) -> Result<Vec<Task>, ChiasmError> {
        let candidates = self.stale_candidates()?;
        self.mark_stale_candidates(
            candidates,
            grace_multiplier,
            Timestamp::now().as_offset_date_time(),
        )
    }

    /// Load active or paused heartbeat-bearing tasks for a stale-sweep attempt.
    fn stale_candidates(&self) -> Result<Vec<Task>, ChiasmError> {
        let conn = self.lock();
        let mut stmt = conn
            .prepare(&format!(
                "SELECT {TASK_COLUMNS} FROM chiasm_tasks \
                 WHERE status IN ('active', 'paused') AND last_heartbeat IS NOT NULL"
            ))
            .map_err(berr)?;
        let rows = stmt.query_map([], read_raw).map_err(berr)?;
        let mut candidates = Vec::new();
        for row in rows {
            candidates.push(row.map_err(berr)?.into_task()?);
        }
        Ok(candidates)
    }

    /// Compare-and-set stale candidates and emit events only for rows that remain unchanged.
    fn mark_stale_candidates(
        &self,
        candidates: Vec<Task>,
        grace_multiplier: f64,
        now_odt: time::OffsetDateTime,
    ) -> Result<Vec<Task>, ChiasmError> {
        const STALE_SUMMARY: &str = "marked stale: heartbeat overdue";
        let mut staled = Vec::new();
        for mut task in candidates {
            let Some(selected_heartbeat) = task.last_heartbeat.as_ref() else {
                continue;
            };
            let hb = selected_heartbeat.as_offset_date_time();
            let elapsed = (now_odt - hb).as_seconds_f64();
            let threshold = task.heartbeat_interval_secs as f64 * grace_multiplier;
            if elapsed <= threshold {
                continue;
            }
            let now_ts = Timestamp::now();
            let now_db = ts_to_db(&now_ts)?;
            let released = {
                let mut conn = self.lock();
                let tx = conn.transaction().map_err(berr)?;
                let changed = tx
                    .execute(
                        "UPDATE chiasm_tasks SET status = 'stale', summary = ?2, updated_at = ?3 \
                     WHERE id = ?1 AND tenant = ?4 AND principal_id = ?5 \
                     AND status = ?6 AND last_heartbeat = ?7",
                        rusqlite::params![
                            task.id.to_string(),
                            STALE_SUMMARY,
                            now_db,
                            task.tenant.to_string(),
                            task.principal_id.to_string(),
                            task.status.as_str(),
                            ts_to_db(selected_heartbeat)?,
                        ],
                    )
                    .map_err(berr)?;
                if changed != 1 {
                    tx.commit().map_err(berr)?;
                    continue;
                }
                tx.execute(
                    "INSERT INTO chiasm_task_updates (task_id, status, summary, created_at) \
                     VALUES (?1, 'stale', ?2, ?3)",
                    rusqlite::params![task.id.to_string(), STALE_SUMMARY, now_db],
                )
                .map_err(berr)?;
                let released = tx
                    .execute(
                        "UPDATE chiasm_path_claims SET released = 1 \
                         WHERE task_id = ?1 AND released = 0 \
                         AND EXISTS (SELECT 1 FROM chiasm_tasks \
                         WHERE id = ?1 AND tenant = ?2 AND principal_id = ?3)",
                        rusqlite::params![
                            task.id.to_string(),
                            task.tenant.to_string(),
                            task.principal_id.to_string()
                        ],
                    )
                    .map_err(berr)?;
                tx.commit().map_err(berr)?;
                released
            };
            task.status = TaskStatus::Stale;
            task.summary = Some(STALE_SUMMARY.to_string());
            task.updated_at = now_ts;
            self.emit(
                &TaskStale {
                    task_id: task.id.to_string(),
                },
                task.tenant,
                task.principal_id,
            );
            if released > 0 {
                self.emit(
                    &ClaimReleased {
                        task_id: task.id.to_string(),
                        count: released as u64,
                    },
                    task.tenant,
                    task.principal_id,
                );
            }
            staled.push(task);
        }
        Ok(staled)
    }

    /// Create TTL path-claim leases on `paths` for a tenant-bound task and emit `claim.created`.
    ///
    /// Each lease is created at now and expires at now + `ttl_seconds`; heartbeats on the task
    /// extend unreleased leases. The project is taken from the task itself (Kleos accepted it
    /// as a separate argument, which let a claim land in a different project than its task).
    /// Returns the new claims, in path order. An empty `paths` creates nothing and emits
    /// nothing. [`ChiasmError::NotFound`] if the task is outside the supplied identity boundary.
    pub async fn create_claims(
        &self,
        tenant: TenantId,
        principal: PrincipalId,
        task_id: TaskId,
        paths: &[&str],
        ttl_seconds: i64,
    ) -> Result<Vec<PathClaim>, ChiasmError> {
        let now = Timestamp::now();
        let expires = ts_plus(&now, ttl_seconds);
        let (task, claims) = {
            let mut conn = self.lock();
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(berr)?;
            let task = Self::get_in(&tx, tenant, principal, task_id)?
                .ok_or(ChiasmError::NotFound(task_id))?;
            let mut claims = Vec::with_capacity(paths.len());
            for &path in paths {
                let existing = {
                    let mut stmt = tx
                        .prepare(&format!(
                            "SELECT {CLAIM_COLUMNS} FROM chiasm_path_claims \
                             WHERE principal_id = ?1 AND project = ?2 AND path = ?3 \
                             AND released = 0 AND task_id != ?4 \
                             AND task_id IN (SELECT id FROM chiasm_tasks \
                             WHERE tenant = ?5 AND principal_id = ?1) \
                             ORDER BY id ASC"
                        ))
                        .map_err(berr)?;
                    let rows = stmt
                        .query_map(
                            rusqlite::params![
                                principal.to_string(),
                                &task.project,
                                path,
                                task_id.to_string(),
                                tenant.to_string()
                            ],
                            read_raw_claim,
                        )
                        .map_err(berr)?;
                    let mut active = None;
                    for row in rows {
                        let claim = row.map_err(berr)?.into_claim()?;
                        if claim.is_active_at(&now) {
                            active = Some(claim);
                            break;
                        }
                    }
                    active
                };
                if let Some(conflict) = existing {
                    return Err(ChiasmError::ClaimConflict {
                        path: path.to_string(),
                        claimed_by_task: conflict.task_id,
                    });
                }
                tx.execute(
                    "INSERT INTO chiasm_path_claims \
                     (task_id, principal_id, project, path, claimed_at, expires_at, released) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0)",
                    rusqlite::params![
                        task_id.to_string(),
                        principal.to_string(),
                        &task.project,
                        path,
                        ts_to_db(&now)?,
                        ts_to_db(&expires)?,
                    ],
                )
                .map_err(berr)?;
                claims.push(PathClaim {
                    id: tx.last_insert_rowid(),
                    task_id,
                    principal_id: principal,
                    project: task.project.clone(),
                    path: path.to_string(),
                    claimed_at: now,
                    expires_at: expires,
                    released: false,
                });
            }
            tx.commit().map_err(berr)?;
            (task, claims)
        };
        if !claims.is_empty() {
            self.emit(
                &ClaimCreated {
                    task_id: task_id.to_string(),
                    project: task.project.clone(),
                    count: claims.len() as u64,
                },
                task.tenant,
                task.principal_id,
            );
        }
        Ok(claims)
    }

    /// Check `paths` in a project for conflicts with active claims held by OTHER tasks, within
    /// one owner's coordination space. Pass `exclude_task_id` so a task re-claiming its own
    /// paths does not self-block. Active = unreleased and unexpired; expiry is compared in Rust
    /// (see [`PathClaim::is_active_at`]).
    pub async fn check_conflicts(
        &self,
        tenant: TenantId,
        principal: PrincipalId,
        project: &str,
        paths: &[&str],
        exclude_task_id: Option<TaskId>,
    ) -> Result<Vec<PathConflict>, ChiasmError> {
        let now = Timestamp::now();
        let conn = self.lock();
        let mut stmt = conn
            .prepare(&format!(
                "SELECT {CLAIM_COLUMNS} FROM chiasm_path_claims \
                 WHERE principal_id = ?1 AND project = ?2 AND path = ?3 AND released = 0 \
                 AND task_id IN (SELECT id FROM chiasm_tasks \
                 WHERE tenant = ?4 AND principal_id = ?1) \
                 ORDER BY id ASC"
            ))
            .map_err(berr)?;
        let mut conflicts = Vec::new();
        for &path in paths {
            let rows = stmt
                .query_map(
                    rusqlite::params![principal.to_string(), project, path, tenant.to_string()],
                    read_raw_claim,
                )
                .map_err(berr)?;
            for row in rows {
                let claim = row.map_err(berr)?.into_claim()?;
                if exclude_task_id == Some(claim.task_id) || !claim.is_active_at(&now) {
                    continue;
                }
                conflicts.push(PathConflict {
                    path: claim.path.clone(),
                    claimed_by_task: claim.task_id,
                    claimed_by_principal: claim.principal_id,
                    expires_at: claim.expires_at,
                });
            }
        }
        Ok(conflicts)
    }

    /// List a task's active (unreleased, unexpired) claims, oldest first.
    ///
    /// A task outside the supplied tenant-and-owner boundary yields an empty list.
    pub async fn get_claims_for_task(
        &self,
        tenant: TenantId,
        principal: PrincipalId,
        task_id: TaskId,
    ) -> Result<Vec<PathClaim>, ChiasmError> {
        let now = Timestamp::now();
        let conn = self.lock();
        let mut stmt = conn
            .prepare(&format!(
                "SELECT {CLAIM_COLUMNS} FROM chiasm_path_claims \
                 WHERE task_id = ?1 AND principal_id = ?2 AND released = 0 \
                 AND task_id IN (SELECT id FROM chiasm_tasks \
                 WHERE tenant = ?3 AND principal_id = ?2) ORDER BY id ASC"
            ))
            .map_err(berr)?;
        let rows = stmt
            .query_map(
                rusqlite::params![
                    task_id.to_string(),
                    principal.to_string(),
                    tenant.to_string()
                ],
                read_raw_claim,
            )
            .map_err(berr)?;
        let mut out = Vec::new();
        for row in rows {
            let claim = row.map_err(berr)?.into_claim()?;
            if claim.is_active_at(&now) {
                out.push(claim);
            }
        }
        Ok(out)
    }

    /// List every active claim in one tenant-bound principal's project, oldest first.
    pub async fn get_claims_for_project(
        &self,
        tenant: TenantId,
        principal: PrincipalId,
        project: &str,
    ) -> Result<Vec<PathClaim>, ChiasmError> {
        let now = Timestamp::now();
        let conn = self.lock();
        let mut stmt = conn
            .prepare(&format!(
                "SELECT {CLAIM_COLUMNS} FROM chiasm_path_claims \
                 WHERE principal_id = ?1 AND project = ?2 AND released = 0 \
                 AND task_id IN (SELECT id FROM chiasm_tasks \
                 WHERE tenant = ?3 AND principal_id = ?1) ORDER BY id ASC"
            ))
            .map_err(berr)?;
        let rows = stmt
            .query_map(
                rusqlite::params![principal.to_string(), project, tenant.to_string()],
                read_raw_claim,
            )
            .map_err(berr)?;
        let mut out = Vec::new();
        for row in rows {
            let claim = row.map_err(berr)?.into_claim()?;
            if claim.is_active_at(&now) {
                out.push(claim);
            }
        }
        Ok(out)
    }

    /// Release every unreleased claim a tenant-bound task holds and return the changed row count.
    ///
    /// A repeated release, a task without live claims, or a task outside the identity boundary
    /// releases zero and emits nothing.
    pub async fn release_claims(
        &self,
        tenant: TenantId,
        principal: PrincipalId,
        task_id: TaskId,
    ) -> Result<usize, ChiasmError> {
        let Some(task) = self.get(tenant, principal, task_id).await? else {
            return Ok(0);
        };
        let count = {
            let conn = self.lock();
            conn.execute(
                "UPDATE chiasm_path_claims SET released = 1 \
                 WHERE task_id = ?1 AND principal_id = ?2 AND released = 0 \
                 AND task_id IN (SELECT id FROM chiasm_tasks \
                 WHERE tenant = ?3 AND principal_id = ?2)",
                rusqlite::params![
                    task_id.to_string(),
                    principal.to_string(),
                    tenant.to_string()
                ],
            )
            .map_err(berr)?
        };
        if count > 0 {
            self.emit(
                &ClaimReleased {
                    task_id: task_id.to_string(),
                    count: count as u64,
                },
                task.tenant,
                task.principal_id,
            );
        }
        Ok(count)
    }

    /// Whether `needle` is reachable from `start` by walking tenant-bound dependency edges.
    fn reaches(
        conn: &Connection,
        tenant: TenantId,
        principal: PrincipalId,
        start: TaskId,
        needle: TaskId,
    ) -> Result<bool, ChiasmError> {
        let mut visited = std::collections::HashSet::new();
        let mut queue = std::collections::VecDeque::new();
        queue.push_back(start);
        let mut stmt = conn
            .prepare(
                "SELECT d.depends_on FROM chiasm_task_dependencies d \
                 JOIN chiasm_tasks source ON source.id = d.task_id \
                 JOIN chiasm_tasks target ON target.id = d.depends_on \
                 WHERE d.task_id = ?1 AND source.tenant = ?2 AND source.principal_id = ?3 \
                 AND target.tenant = ?2 AND target.principal_id = ?3",
            )
            .map_err(berr)?;
        while let Some(current) = queue.pop_front() {
            if current == needle {
                return Ok(true);
            }
            if !visited.insert(current) {
                continue;
            }
            let rows = stmt
                .query_map(
                    rusqlite::params![
                        current.to_string(),
                        tenant.to_string(),
                        principal.to_string()
                    ],
                    |r| r.get::<_, String>(0),
                )
                .map_err(berr)?;
            for row in rows {
                let dep = row.map_err(berr)?;
                queue.push_back(dep.parse::<TaskId>().map_err(|e| {
                    ChiasmError::Backend(format!("corrupt depends_on {dep:?}: {e}"))
                })?);
            }
        }
        Ok(false)
    }

    /// Add dependency edges `task_id -> depends_on[i]`, validating each target against
    /// self-reference ([`ChiasmError::SelfDependency`]) and cycles
    /// ([`ChiasmError::DependencyCycle`], BFS over existing edges). Both endpoints must be inside
    /// the same tenant-and-owner boundary. A missing or foreign target is
    /// [`ChiasmError::NotFound`], so cross-boundary edges cannot exist by construction.
    /// Duplicate edges are ignored. The whole batch is one transaction: any rejection inserts
    /// nothing, and the cycle check sees edges added earlier in the same batch.
    pub async fn add_dependencies(
        &self,
        tenant: TenantId,
        principal: PrincipalId,
        task_id: TaskId,
        depends_on: &[TaskId],
    ) -> Result<(), ChiasmError> {
        let now = ts_to_db(&Timestamp::now())?;
        let mut conn = self.lock();
        let tx = conn.transaction().map_err(berr)?;
        Self::get_in(&tx, tenant, principal, task_id)?.ok_or(ChiasmError::NotFound(task_id))?;
        for &dep in depends_on {
            if dep == task_id {
                return Err(ChiasmError::SelfDependency(task_id));
            }
            Self::get_in(&tx, tenant, principal, dep)?.ok_or(ChiasmError::NotFound(dep))?;
            if Self::reaches(&tx, tenant, principal, dep, task_id)? {
                return Err(ChiasmError::DependencyCycle {
                    task_id,
                    depends_on: dep,
                });
            }
            tx.execute(
                "INSERT OR IGNORE INTO chiasm_task_dependencies (task_id, depends_on, created_at) \
                 VALUES (?1, ?2, ?3)",
                rusqlite::params![task_id.to_string(), dep.to_string(), now],
            )
            .map_err(berr)?;
        }
        tx.commit().map_err(berr)?;
        Ok(())
    }

    /// List a tenant-bound task's dependency edges, oldest first.
    ///
    /// Each edge is joined with the depended-on task's current title and status.
    pub async fn get_dependencies(
        &self,
        tenant: TenantId,
        principal: PrincipalId,
        task_id: TaskId,
    ) -> Result<Vec<Dependency>, ChiasmError> {
        let conn = self.lock();
        let mut stmt = conn
            .prepare(
                "SELECT d.id, d.task_id, d.depends_on, dt.title, dt.status, d.created_at \
                 FROM chiasm_task_dependencies d \
                 JOIN chiasm_tasks t ON t.id = d.task_id \
                 JOIN chiasm_tasks dt ON dt.id = d.depends_on \
                 WHERE d.task_id = ?1 AND t.tenant = ?2 AND t.principal_id = ?3 \
                 AND dt.tenant = ?2 AND dt.principal_id = ?3 ORDER BY d.id ASC",
            )
            .map_err(berr)?;
        let rows = stmt
            .query_map(
                rusqlite::params![
                    task_id.to_string(),
                    tenant.to_string(),
                    principal.to_string()
                ],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, Option<String>>(3)?,
                        r.get::<_, Option<String>>(4)?,
                        r.get::<_, String>(5)?,
                    ))
                },
            )
            .map_err(berr)?;
        let mut out = Vec::new();
        for row in rows {
            let (id, edge_task, depends_on, title, status, created_at) = row.map_err(berr)?;
            out.push(Dependency {
                id,
                task_id: edge_task.parse::<TaskId>().map_err(|e| {
                    ChiasmError::Backend(format!("corrupt task_id {edge_task:?}: {e}"))
                })?,
                depends_on: depends_on.parse::<TaskId>().map_err(|e| {
                    ChiasmError::Backend(format!("corrupt depends_on {depends_on:?}: {e}"))
                })?,
                depends_on_title: title,
                depends_on_status: status.as_deref().map(TaskStatus::parse).transpose()?,
                created_at: ts_from_db(&created_at)?,
            });
        }
        Ok(out)
    }

    /// Remove one dependency edge from a tenant-bound task.
    ///
    /// Returns `false` for a missing edge or a task outside the supplied identity boundary.
    pub async fn remove_dependency(
        &self,
        tenant: TenantId,
        principal: PrincipalId,
        task_id: TaskId,
        depends_on: TaskId,
    ) -> Result<bool, ChiasmError> {
        let conn = self.lock();
        let n = conn
            .execute(
                "DELETE FROM chiasm_task_dependencies WHERE task_id = ?1 AND depends_on = ?2 \
                 AND task_id IN (SELECT id FROM chiasm_tasks \
                 WHERE tenant = ?3 AND principal_id = ?4) \
                 AND depends_on IN (SELECT id FROM chiasm_tasks \
                 WHERE tenant = ?3 AND principal_id = ?4)",
                rusqlite::params![
                    task_id.to_string(),
                    depends_on.to_string(),
                    tenant.to_string(),
                    principal.to_string()
                ],
            )
            .map_err(berr)?;
        Ok(n > 0)
    }

    /// Activate blocked dependents after `completed_task_id` is durably completed.
    ///
    /// Trigger validation, dependency checks, compare-and-set updates, and history writes share
    /// one immediate transaction. A concurrent status or dependency change therefore cannot
    /// prematurely unblock or resurrect a task. Returns the tasks activated and
    /// [`ChiasmError::NotFound`] when the trigger is outside the supplied identity boundary.
    ///
    /// The unblock is scoped to the dependent's tenant and owner. Only `blocked` dependents are
    /// activated; completed or stale tasks are never resurrected.
    pub async fn check_and_unblock(
        &self,
        tenant: TenantId,
        principal: PrincipalId,
        completed_task_id: TaskId,
    ) -> Result<Vec<Task>, ChiasmError> {
        const UNBLOCKED_SUMMARY: &str = "auto-unblocked: all dependencies completed";
        let now = Timestamp::now();
        let now_db = ts_to_db(&now)?;
        let unblocked = {
            let mut conn = self.lock();
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(berr)?;
            let trigger = Self::get_in(&tx, tenant, principal, completed_task_id)?
                .ok_or(ChiasmError::NotFound(completed_task_id))?;
            if trigger.status != TaskStatus::Completed {
                return Ok(Vec::new());
            }
            let tasks = {
                let mut stmt = tx
                    .prepare(&format!(
                        "UPDATE chiasm_tasks SET status = 'active', summary = ?4, updated_at = ?5 \
                         WHERE id IN (\
                           SELECT d.task_id FROM chiasm_task_dependencies d \
                           JOIN chiasm_tasks dependent ON dependent.id = d.task_id \
                           WHERE d.depends_on = ?1 \
                             AND dependent.tenant = ?2 AND dependent.principal_id = ?3 \
                             AND dependent.status = 'blocked' \
                             AND NOT EXISTS (\
                               SELECT 1 FROM chiasm_task_dependencies d2 \
                               LEFT JOIN chiasm_tasks required ON required.id = d2.depends_on \
                               WHERE d2.task_id = d.task_id \
                                 AND (required.id IS NULL OR required.tenant != ?2 \
                                   OR required.principal_id != ?3 \
                                   OR required.status != 'completed'))) \
                         AND tenant = ?2 AND principal_id = ?3 AND status = 'blocked' \
                         RETURNING {TASK_COLUMNS}"
                    ))
                    .map_err(berr)?;
                let rows = stmt
                    .query_map(
                        rusqlite::params![
                            completed_task_id.to_string(),
                            tenant.to_string(),
                            principal.to_string(),
                            UNBLOCKED_SUMMARY,
                            now_db,
                        ],
                        read_raw,
                    )
                    .map_err(berr)?;
                let mut tasks = Vec::new();
                for row in rows {
                    tasks.push(row.map_err(berr)?.into_task()?);
                }
                tasks
            };
            for task in &tasks {
                tx.execute(
                    "INSERT INTO chiasm_task_updates (task_id, status, summary, created_at) \
                     VALUES (?1, 'active', ?2, ?3)",
                    rusqlite::params![task.id.to_string(), UNBLOCKED_SUMMARY, now_db],
                )
                .map_err(berr)?;
            }
            tx.commit().map_err(berr)?;
            tasks
        };
        for task in &unblocked {
            self.emit(
                &TaskUpdated {
                    task_id: task.id.to_string(),
                    status: TaskStatus::Active.as_str().to_string(),
                },
                task.tenant,
                task.principal_id,
            );
            self.emit(
                &TaskUnblocked {
                    task_id: task.id.to_string(),
                    completed_dependency: completed_task_id.to_string(),
                },
                task.tenant,
                task.principal_id,
            );
        }
        Ok(unblocked)
    }
}

/// Insert a fully-formed [`Task`] row. Shared by `create`, `enqueue`, and the legacy backfill.
pub(crate) fn insert_task(conn: &Connection, task: &Task) -> Result<(), ChiasmError> {
    conn.execute(
        "INSERT INTO chiasm_tasks (id, tenant, principal_id, assignee, project, title, status, \
         summary, expected_output, output_format, output, plan, feedback, last_heartbeat, \
         heartbeat_interval_secs, created_at, updated_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)",
        rusqlite::params![
            task.id.to_string(),
            task.tenant.to_string(),
            task.principal_id.to_string(),
            task.assignee.map(|a| a.to_string()),
            &task.project,
            &task.title,
            task.status.as_str(),
            &task.summary,
            &task.expected_output,
            &task.output_format,
            &task.output,
            &task.plan,
            &task.feedback,
            task.last_heartbeat.as_ref().map(ts_to_db).transpose()?,
            task.heartbeat_interval_secs,
            ts_to_db(&task.created_at)?,
            ts_to_db(&task.updated_at)?,
        ],
    )
    .map_err(berr)?;
    Ok(())
}

/// Apply every migration whose version exceeds `PRAGMA user_version`, each in its own transaction,
/// bumping `user_version` as it goes. Idempotent: an up-to-date database applies nothing.
pub(crate) fn apply_migrations(conn: &mut OpenedDatabase) -> Result<(), ChiasmError> {
    let mut version: i64 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .map_err(berr)?;
    for (v, sql) in MIGRATIONS {
        if *v > version {
            let tx = conn.transaction().map_err(berr)?;
            tx.execute_batch(sql)
                .map_err(|e| ChiasmError::Backend(format!("migration V{v} failed: {e}")))?;
            tx.pragma_update(None, "user_version", *v).map_err(berr)?;
            tx.commit().map_err(berr)?;
            version = *v;
        }
    }
    Ok(())
}

#[cfg(test)]
/// Unit tests for this module.
mod tests {
    use super::*;
    use syntheos_axon::AxonBus;

    /// A store on a fresh in-memory db plus the bus it publishes to.
    fn store() -> (ChiasmStore, Arc<AxonBus>) {
        let bus = Arc::new(AxonBus::new());
        let store = ChiasmStore::open_in_memory(bus.clone()).expect("open");
        (store, bus)
    }

    /// A minimal NewTask for `principal` in `tenant`.
    fn new_task(tenant: TenantId, principal: PrincipalId, title: &str) -> NewTask {
        NewTask {
            tenant,
            principal_id: principal,
            project: "henosis".to_string(),
            title: title.to_string(),
            status: None,
            summary: None,
            expected_output: None,
            output_format: None,
            assignee: None,
            heartbeat_interval_secs: None,
        }
    }

    /// Drain the kind strings currently buffered on a raw subscriber.
    fn drain_kinds(
        rx: &mut tokio::sync::broadcast::Receiver<syntheos_contracts::AxonEnvelope>,
    ) -> Vec<String> {
        let mut kinds = Vec::new();
        while let Ok(env) = rx.try_recv() {
            kinds.push(env.kind);
        }
        kinds
    }

    #[tokio::test]
    /// Create then get roundtrips.
    async fn create_then_get_roundtrips() {
        let (store, _bus) = store();
        let (tenant, principal) = (TenantId::new(), PrincipalId::new());
        let made = store
            .create(new_task(tenant, principal, "ship chiasm"))
            .await
            .expect("create");
        assert_eq!(made.status, TaskStatus::Active);
        assert_eq!(made.output_format, "raw");
        let got = store
            .get(tenant, principal, made.id)
            .await
            .expect("get")
            .expect("present");
        assert_eq!(got, made);
    }

    #[tokio::test]
    /// Create emits task created.
    async fn create_emits_task_created() {
        let (store, bus) = store();
        let mut rx = bus.subscribe("task");
        store
            .create(new_task(TenantId::new(), PrincipalId::new(), "x"))
            .await
            .expect("create");
        assert_eq!(drain_kinds(&mut rx), ["task.created"]);
    }

    #[tokio::test]
    /// Get is tenant-and-owner scoped.
    async fn get_is_tenant_and_owner_scoped() {
        let (store, _bus) = store();
        let tenant = TenantId::new();
        let other_tenant = TenantId::new();
        let owner = PrincipalId::new();
        let other = PrincipalId::new();
        let task = store
            .create(new_task(tenant, owner, "secret"))
            .await
            .expect("create");
        // The owner sees it; a different principal does not.
        assert!(store
            .get(tenant, owner, task.id)
            .await
            .expect("get")
            .is_some());
        assert!(store
            .get(tenant, other, task.id)
            .await
            .expect("get")
            .is_none());
        assert!(store
            .get(other_tenant, owner, task.id)
            .await
            .expect("get")
            .is_none());
    }

    #[tokio::test]
    /// Every coordination API rejects cross-tenant access for the same principal.
    async fn tenant_boundary_covers_task_claim_queue_and_dependency_apis() {
        let (store, _bus) = store();
        let tenant_a = TenantId::new();
        let tenant_b = TenantId::new();
        let principal = PrincipalId::new();
        let task_b = store
            .create(new_task(tenant_b, principal, "tenant b task"))
            .await
            .expect("create tenant b task");
        let dependency_b = store
            .create(new_task(tenant_b, principal, "tenant b dependency"))
            .await
            .expect("create tenant b dependency");
        store
            .update(
                tenant_b,
                principal,
                task_b.id,
                TaskPatch {
                    summary: Some("tenant b history".to_string()),
                    ..Default::default()
                },
            )
            .await
            .expect("seed history");
        store
            .record_activity(
                tenant_b,
                principal,
                task_b.id,
                "action.completed",
                serde_json::json!({"tenant": "b"}),
            )
            .await
            .expect("seed activity");
        store
            .create_claims(tenant_b, principal, task_b.id, &["shared.rs"], 1800)
            .await
            .expect("seed claim");
        store
            .add_dependencies(tenant_b, principal, task_b.id, &[dependency_b.id])
            .await
            .expect("seed dependency");
        let queued_b = store
            .enqueue(EnqueueTask {
                tenant: tenant_b,
                principal_id: principal,
                project: "henosis".to_string(),
                title: "tenant b queued".to_string(),
                summary: None,
            })
            .await
            .expect("enqueue tenant b task");

        assert!(store
            .get(tenant_a, principal, task_b.id)
            .await
            .expect("cross-tenant get")
            .is_none());
        assert!(store
            .list(tenant_a, principal, TaskFilter::default())
            .await
            .expect("cross-tenant list")
            .is_empty());
        assert_eq!(
            store
                .stats(tenant_a, principal)
                .await
                .expect("cross-tenant stats")
                .total,
            0
        );
        assert!(store
            .history(tenant_a, principal, task_b.id, 10)
            .await
            .expect("cross-tenant history")
            .is_empty());
        assert!(store
            .activity(tenant_a, principal, task_b.id, 10)
            .await
            .expect("cross-tenant activity")
            .is_empty());
        assert!(matches!(
            store
                .record_activity(
                    tenant_a,
                    principal,
                    task_b.id,
                    "action.failed",
                    serde_json::json!({})
                )
                .await,
            Err(ChiasmError::NotFound(id)) if id == task_b.id
        ));
        assert!(matches!(
            store
                .update(
                    tenant_a,
                    principal,
                    task_b.id,
                    TaskPatch {
                        title: Some("cross-tenant mutation".to_string()),
                        ..Default::default()
                    }
                )
                .await,
            Err(ChiasmError::NotFound(id)) if id == task_b.id
        ));
        assert!(matches!(
            store.record_heartbeat(tenant_a, principal, task_b.id).await,
            Err(ChiasmError::NotFound(id)) if id == task_b.id
        ));
        assert!(!store
            .delete(tenant_a, principal, task_b.id)
            .await
            .expect("cross-tenant delete"));
        assert!(matches!(
            store
                .create_claims(tenant_a, principal, task_b.id, &["other.rs"], 1800)
                .await,
            Err(ChiasmError::NotFound(id)) if id == task_b.id
        ));
        assert!(store
            .get_claims_for_task(tenant_a, principal, task_b.id)
            .await
            .expect("cross-tenant task claims")
            .is_empty());
        assert!(store
            .get_claims_for_project(tenant_a, principal, "henosis")
            .await
            .expect("cross-tenant project claims")
            .is_empty());
        assert!(store
            .check_conflicts(tenant_a, principal, "henosis", &["shared.rs"], None)
            .await
            .expect("cross-tenant conflicts")
            .is_empty());
        assert_eq!(
            store
                .release_claims(tenant_a, principal, task_b.id)
                .await
                .expect("cross-tenant release"),
            0
        );
        assert!(matches!(
            store
                .add_dependencies(tenant_a, principal, task_b.id, &[dependency_b.id])
                .await,
            Err(ChiasmError::NotFound(id)) if id == task_b.id
        ));
        assert!(store
            .get_dependencies(tenant_a, principal, task_b.id)
            .await
            .expect("cross-tenant dependencies")
            .is_empty());
        assert!(!store
            .remove_dependency(tenant_a, principal, task_b.id, dependency_b.id)
            .await
            .expect("cross-tenant dependency removal"));
        assert!(matches!(
            store
                .check_and_unblock(tenant_a, principal, dependency_b.id)
                .await,
            Err(ChiasmError::NotFound(id)) if id == dependency_b.id
        ));
        assert!(store
            .claim_next(tenant_a, principal, PrincipalId::new(), None)
            .await
            .expect("cross-tenant queue claim")
            .is_none());
        assert_eq!(
            store
                .claim_next(tenant_b, principal, PrincipalId::new(), None)
                .await
                .expect("tenant b queue claim")
                .expect("queued task")
                .id,
            queued_b.id
        );

        let task_a = store
            .create(new_task(tenant_a, principal, "tenant a task"))
            .await
            .expect("create tenant a task");
        store
            .create_claims(tenant_a, principal, task_a.id, &["shared.rs"], 1800)
            .await
            .expect("same path may be claimed in another tenant");
        assert_eq!(
            store
                .get_claims_for_project(tenant_a, principal, "henosis")
                .await
                .expect("tenant a project claims")
                .len(),
            1
        );
        assert_eq!(
            store
                .get_claims_for_project(tenant_b, principal, "henosis")
                .await
                .expect("tenant b project claims")
                .len(),
            1
        );
    }

    #[tokio::test]
    /// Task activity is append-only, tenant-and-owner-scoped, and leaves task state unchanged.
    async fn task_activity_is_scoped_and_non_mutating() {
        let (store, _bus) = store();
        let tenant = TenantId::new();
        let principal = PrincipalId::new();
        let task = store
            .create(new_task(tenant, principal, "activity"))
            .await
            .expect("create");

        store
            .record_activity(
                tenant,
                principal,
                task.id,
                "action.invoked",
                serde_json::json!({"tool": "test", "action": "echo"}),
            )
            .await
            .expect("record activity");
        store
            .record_activity(
                tenant,
                principal,
                task.id,
                "action.completed",
                serde_json::json!({"tool": "test", "action": "echo"}),
            )
            .await
            .expect("record activity");

        let activity = store
            .activity(tenant, principal, task.id, 10)
            .await
            .expect("activity");
        assert_eq!(activity.len(), 2);
        assert_eq!(activity[0].kind, "action.completed");
        assert_eq!(activity[1].kind, "action.invoked");
        let unchanged = store
            .get(tenant, principal, task.id)
            .await
            .expect("get")
            .expect("task");
        assert_eq!(unchanged.status, TaskStatus::Active);
        assert_eq!(unchanged.summary, None);

        let error = store
            .record_activity(
                tenant,
                PrincipalId::new(),
                task.id,
                "action.failed",
                serde_json::json!({}),
            )
            .await
            .expect_err("foreign principal must fail closed");
        assert!(matches!(error, ChiasmError::NotFound(id) if id == task.id));
    }

    #[tokio::test]
    /// Update appends history and emits.
    async fn update_appends_history_and_emits() {
        let (store, bus) = store();
        let mut rx = bus.subscribe("task");
        let (tenant, principal) = (TenantId::new(), PrincipalId::new());
        let task = store
            .create(new_task(tenant, principal, "t"))
            .await
            .expect("create");
        let _ = drain_kinds(&mut rx); // discard task.created

        store
            .update(
                tenant,
                principal,
                task.id,
                TaskPatch {
                    status: Some(TaskStatus::Blocked),
                    summary: Some("waiting on dep".to_string()),
                    ..Default::default()
                },
            )
            .await
            .expect("update");
        assert_eq!(drain_kinds(&mut rx), ["task.updated"]);

        let completed = store
            .update(
                tenant,
                principal,
                task.id,
                TaskPatch {
                    status: Some(TaskStatus::Completed),
                    ..Default::default()
                },
            )
            .await
            .expect("complete");
        assert_eq!(completed.status, TaskStatus::Completed);
        assert_eq!(drain_kinds(&mut rx), ["task.completed"]);

        let history = store
            .history(tenant, principal, task.id, 10)
            .await
            .expect("history");
        // Two updates recorded, newest first.
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].status, TaskStatus::Completed);
        assert_eq!(history[1].status, TaskStatus::Blocked);
    }

    #[tokio::test]
    /// Update unknown task is not found.
    async fn update_unknown_task_is_not_found() {
        let (store, _bus) = store();
        let err = store
            .update(
                TenantId::new(),
                PrincipalId::new(),
                TaskId::new(),
                TaskPatch::default(),
            )
            .await
            .expect_err("must be NotFound");
        assert!(matches!(err, ChiasmError::NotFound(_)));
    }

    #[tokio::test]
    /// An omitted or oversized caller limit is capped by the store itself.
    ///
    /// Without this cap a caller could omit `limit` and force the store to
    /// materialize every row it holds while owning the process-wide connection
    /// mutex, stalling every other tenant's writes.
    async fn list_caps_an_omitted_or_oversized_limit() {
        let (store, _bus) = store();
        let (tenant, principal) = (TenantId::new(), PrincipalId::new());
        for index in 0..(MAX_LIST_LIMIT + 5) {
            store
                .create(new_task(tenant, principal, &format!("task-{index}")))
                .await
                .expect("create");
        }

        let omitted = store
            .list(tenant, principal, TaskFilter::default())
            .await
            .expect("list without a limit");
        assert_eq!(omitted.len(), MAX_LIST_LIMIT);

        let oversized = store
            .list(
                tenant,
                principal,
                TaskFilter {
                    limit: Some(usize::MAX),
                    ..Default::default()
                },
            )
            .await
            .expect("list with an oversized limit");
        assert_eq!(oversized.len(), MAX_LIST_LIMIT);

        let under_cap = store
            .list(
                tenant,
                principal,
                TaskFilter {
                    limit: Some(3),
                    ..Default::default()
                },
            )
            .await
            .expect("list under the cap");
        assert_eq!(under_cap.len(), 3);
    }

    #[tokio::test]
    /// List filters by status and project.
    async fn list_filters_by_status_and_project() {
        let (store, _bus) = store();
        let (tenant, principal) = (TenantId::new(), PrincipalId::new());
        let a = store
            .create(new_task(tenant, principal, "a"))
            .await
            .expect("create");
        store
            .create(new_task(tenant, principal, "b"))
            .await
            .expect("create");
        store
            .update(
                tenant,
                principal,
                a.id,
                TaskPatch {
                    status: Some(TaskStatus::Completed),
                    ..Default::default()
                },
            )
            .await
            .expect("update");

        let active = store
            .list(
                tenant,
                principal,
                TaskFilter {
                    status: Some(TaskStatus::Active),
                    ..Default::default()
                },
            )
            .await
            .expect("list");
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].title, "b");

        let all = store
            .list(tenant, principal, TaskFilter::default())
            .await
            .expect("list");
        assert_eq!(all.len(), 2);
    }

    #[tokio::test]
    /// Delete removes and emits.
    async fn delete_removes_and_emits() {
        let (store, bus) = store();
        let mut rx = bus.subscribe("task");
        let (tenant, principal) = (TenantId::new(), PrincipalId::new());
        let task = store
            .create(new_task(tenant, principal, "t"))
            .await
            .expect("create");
        let _ = drain_kinds(&mut rx);

        assert!(store
            .delete(tenant, principal, task.id)
            .await
            .expect("delete"));
        assert_eq!(drain_kinds(&mut rx), ["task.deleted"]);
        assert!(store
            .get(tenant, principal, task.id)
            .await
            .expect("get")
            .is_none());
        // Deleting again (or a non-existent task) is a no-op, no event.
        assert!(!store
            .delete(tenant, principal, task.id)
            .await
            .expect("delete"));
        assert!(drain_kinds(&mut rx).is_empty());
    }

    #[tokio::test]
    /// Stats counts by status.
    async fn stats_counts_by_status() {
        let (store, _bus) = store();
        let (tenant, principal) = (TenantId::new(), PrincipalId::new());
        let a = store
            .create(new_task(tenant, principal, "a"))
            .await
            .expect("create");
        store
            .create(new_task(tenant, principal, "b"))
            .await
            .expect("create");
        store
            .update(
                tenant,
                principal,
                a.id,
                TaskPatch {
                    status: Some(TaskStatus::Completed),
                    ..Default::default()
                },
            )
            .await
            .expect("update");
        let stats = store.stats(tenant, principal).await.expect("stats");
        assert_eq!(stats.total, 2);
        assert_eq!(stats.by_status.get("active"), Some(&1));
        assert_eq!(stats.by_status.get("completed"), Some(&1));
    }

    #[tokio::test]
    /// Tasks persist across reopen.
    async fn tasks_persist_across_reopen() {
        let root = std::env::temp_dir().join(format!("henosis-chiasm-{}", TaskId::new()));
        let tmp = root.join("state").join("chiasm.sqlite");
        let (tenant, principal) = (TenantId::new(), PrincipalId::new());
        let id;
        {
            let bus = Arc::new(AxonBus::new());
            let store = ChiasmStore::open(&tmp, bus).expect("open");
            id = store
                .create(new_task(tenant, principal, "durable"))
                .await
                .expect("create")
                .id;
        }
        {
            let bus = Arc::new(AxonBus::new());
            let store = ChiasmStore::open(&tmp, bus).expect("reopen");
            let got = store
                .get(tenant, principal, id)
                .await
                .expect("get")
                .expect("present after reopen");
            assert_eq!(got.title, "durable");
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    /// Opening a V5 database applies the tenant-boundary indexes and advances to V6.
    fn v5_database_upgrades_to_v6_tenant_indexes() {
        let root =
            std::env::temp_dir().join(format!("henosis-chiasm-v5-upgrade-{}", TaskId::new()));
        let path = root.join("state").join("chiasm.sqlite");
        {
            let database = henosis_sqlite::open_database(&path).expect("open protected V5 fixture");
            let connection = database.connection();
            for (version, sql) in MIGRATIONS.iter().filter(|(version, _)| *version <= 5) {
                connection
                    .execute_batch(sql)
                    .unwrap_or_else(|error| panic!("apply V{version} fixture: {error}"));
                connection
                    .pragma_update(None, "user_version", version)
                    .expect("set fixture version");
            }
        }

        let store =
            ChiasmStore::open(&path, Arc::new(AxonBus::new())).expect("upgrade V5 database");
        drop(store);
        let database =
            henosis_sqlite::open_database(&path).expect("open protected upgraded database");
        let connection = database.connection();
        let version: i64 = connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .expect("read schema version");
        assert_eq!(version, 6);
        let mut statement = connection
            .prepare(
                "SELECT name FROM sqlite_master WHERE type = 'index' AND name IN (\
                 'idx_chiasm_tasks_tenant_principal_status', \
                 'idx_chiasm_tasks_tenant_principal_project', \
                 'idx_chiasm_task_activity_tenant_owner') ORDER BY name",
            )
            .expect("prepare index census");
        let indexes = statement
            .query_map([], |row| row.get::<_, String>(0))
            .expect("query indexes")
            .collect::<Result<Vec<_>, _>>()
            .expect("read index names");
        assert_eq!(
            indexes,
            [
                "idx_chiasm_task_activity_tenant_owner",
                "idx_chiasm_tasks_tenant_principal_project",
                "idx_chiasm_tasks_tenant_principal_status",
            ]
        );
        drop(statement);
        drop(database);
        std::fs::remove_dir_all(&root).expect("remove V5 upgrade fixture");
    }

    /// A minimal EnqueueTask for `principal` in `tenant` under `project`.
    fn enqueue_task(
        tenant: TenantId,
        principal: PrincipalId,
        title: &str,
        project: &str,
    ) -> EnqueueTask {
        EnqueueTask {
            tenant,
            principal_id: principal,
            project: project.to_string(),
            title: title.to_string(),
            summary: None,
        }
    }

    #[tokio::test]
    /// Enqueue then claim.
    async fn enqueue_then_claim() {
        let (store, bus) = store();
        let mut rx = bus.subscribe("task");
        let (tenant, owner) = (TenantId::new(), PrincipalId::new());
        let claimer = PrincipalId::new();

        let queued = store
            .enqueue(enqueue_task(tenant, owner, "work", "henosis"))
            .await
            .expect("enqueue");
        assert_eq!(queued.status, TaskStatus::Queued);
        assert!(queued.assignee.is_none());
        assert_eq!(drain_kinds(&mut rx), ["task.queued"]);

        let claimed = store
            .claim_next(tenant, owner, claimer, None)
            .await
            .expect("claim")
            .expect("a task to claim");
        assert_eq!(claimed.id, queued.id);
        assert_eq!(claimed.status, TaskStatus::Active);
        assert_eq!(claimed.assignee, Some(claimer));
        assert!(
            claimed.last_heartbeat.is_some(),
            "claim stamps a first heartbeat"
        );
        assert_eq!(drain_kinds(&mut rx), ["task.claimed"]);

        // Queue is now empty.
        assert!(store
            .claim_next(tenant, owner, claimer, None)
            .await
            .expect("claim")
            .is_none());
    }

    #[tokio::test]
    /// Claim is fifo.
    async fn claim_is_fifo() {
        let (store, _bus) = store();
        let (tenant, owner) = (TenantId::new(), PrincipalId::new());
        let first = store
            .enqueue(enqueue_task(tenant, owner, "first", "p"))
            .await
            .expect("enqueue");
        let _second = store
            .enqueue(enqueue_task(tenant, owner, "second", "p"))
            .await
            .expect("enqueue");
        let claimed = store
            .claim_next(tenant, owner, PrincipalId::new(), None)
            .await
            .expect("claim")
            .expect("task");
        assert_eq!(
            claimed.id, first.id,
            "oldest-enqueued task is claimed first"
        );
    }

    #[tokio::test]
    /// Claim respects project filter.
    async fn claim_respects_project_filter() {
        let (store, _bus) = store();
        let (tenant, owner) = (TenantId::new(), PrincipalId::new());
        store
            .enqueue(enqueue_task(tenant, owner, "alpha-task", "alpha"))
            .await
            .expect("enqueue");
        let beta = store
            .enqueue(enqueue_task(tenant, owner, "beta-task", "beta"))
            .await
            .expect("enqueue");
        let claimed = store
            .claim_next(tenant, owner, PrincipalId::new(), Some("beta"))
            .await
            .expect("claim")
            .expect("task");
        assert_eq!(
            claimed.id, beta.id,
            "only the beta-project task is claimable"
        );
    }

    #[tokio::test]
    /// Claim is owner scoped.
    async fn claim_is_owner_scoped() {
        let (store, _bus) = store();
        let tenant = TenantId::new();
        let owner = PrincipalId::new();
        let other_owner = PrincipalId::new();
        store
            .enqueue(enqueue_task(tenant, owner, "t", "p"))
            .await
            .expect("enqueue");
        // A different owner's queue is empty.
        assert!(store
            .claim_next(tenant, other_owner, PrincipalId::new(), None)
            .await
            .expect("claim")
            .is_none());
    }

    #[tokio::test]
    /// Record heartbeat updates and notfound.
    async fn record_heartbeat_updates_and_notfound() {
        let (store, _bus) = store();
        let (tenant, principal) = (TenantId::new(), PrincipalId::new());
        let task = store
            .create(new_task(tenant, principal, "t"))
            .await
            .expect("create");
        assert!(task.last_heartbeat.is_none());

        store
            .record_heartbeat(tenant, principal, task.id)
            .await
            .expect("heartbeat");
        let got = store
            .get(tenant, principal, task.id)
            .await
            .expect("get")
            .expect("present");
        assert!(
            got.last_heartbeat.is_some(),
            "heartbeat sets last_heartbeat"
        );

        // Unknown task, or another principal's task, is NotFound.
        let err = store
            .record_heartbeat(tenant, principal, TaskId::new())
            .await
            .expect_err("unknown task");
        assert!(matches!(err, ChiasmError::NotFound(_)));
        let err = store
            .record_heartbeat(tenant, PrincipalId::new(), task.id)
            .await
            .expect_err("wrong owner");
        assert!(matches!(err, ChiasmError::NotFound(_)));
    }

    #[tokio::test]
    /// Fresh heartbeat is not stale.
    async fn fresh_heartbeat_is_not_stale() {
        let (store, _bus) = store();
        let (tenant, principal) = (TenantId::new(), PrincipalId::new());
        let task = store
            .create(new_task(tenant, principal, "t"))
            .await
            .expect("create");
        store
            .record_heartbeat(tenant, principal, task.id)
            .await
            .expect("heartbeat");
        // Just beaten, default 300s interval, grace 1.0 -> not overdue.
        assert!(store.mark_stale(1.0).await.expect("sweep").is_empty());
        let got = store
            .get(tenant, principal, task.id)
            .await
            .expect("get")
            .expect("present");
        assert_eq!(got.status, TaskStatus::Active);
    }

    #[tokio::test]
    /// Overdue heartbeat marks stale.
    async fn overdue_heartbeat_marks_stale() {
        let (store, bus) = store();
        let mut rx = bus.subscribe("task");
        let (tenant, principal) = (TenantId::new(), PrincipalId::new());
        let task = store
            .create(new_task(tenant, principal, "t"))
            .await
            .expect("create");
        store
            .record_heartbeat(tenant, principal, task.id)
            .await
            .expect("heartbeat");
        let _ = drain_kinds(&mut rx);

        // grace 0.0 -> threshold 0 -> any elapsed time is overdue.
        let staled = store.mark_stale(0.0).await.expect("sweep");
        assert_eq!(staled.len(), 1);
        assert_eq!(staled[0].id, task.id);
        assert_eq!(staled[0].status, TaskStatus::Stale);
        assert_eq!(drain_kinds(&mut rx), ["task.stale"]);

        let got = store
            .get(tenant, principal, task.id)
            .await
            .expect("get")
            .expect("present");
        assert_eq!(got.status, TaskStatus::Stale);
        let history = store
            .history(tenant, principal, task.id, 10)
            .await
            .expect("history");
        assert_eq!(
            history[0].status,
            TaskStatus::Stale,
            "stale recorded in history"
        );

        // A task with no heartbeat is never swept, even at grace 0.0.
        let unbeaten = store
            .create(new_task(tenant, principal, "unbeaten"))
            .await
            .expect("create");
        assert!(store
            .mark_stale(0.0)
            .await
            .expect("sweep")
            .iter()
            .all(|t| t.id != unbeaten.id));
    }

    #[tokio::test]
    /// A heartbeat that wins after candidate selection prevents every stale side effect.
    async fn stale_compare_and_set_preserves_refreshed_task() {
        let (store, bus) = store();
        let mut receiver = bus.subscribe("task");
        let tenant = TenantId::new();
        let principal = PrincipalId::new();
        let task = store
            .create(new_task(tenant, principal, "race"))
            .await
            .expect("create");
        let old_heartbeat = ts_to_db(&ts_plus(&Timestamp::now(), -60)).expect("old heartbeat");
        {
            let connection = store.lock();
            connection
                .execute(
                    "UPDATE chiasm_tasks SET last_heartbeat = ?1, updated_at = ?1 WHERE id = ?2",
                    rusqlite::params![old_heartbeat, task.id.to_string()],
                )
                .expect("seed old heartbeat");
        }
        store
            .create_claims(tenant, principal, task.id, &["race.rs"], 1800)
            .await
            .expect("claim");
        let candidates = store.stale_candidates().expect("candidates");
        assert_eq!(candidates.len(), 1);

        store
            .record_heartbeat(tenant, principal, task.id)
            .await
            .expect("refresh heartbeat");
        let _ = drain_kinds(&mut receiver);
        let staled = store
            .mark_stale_candidates(candidates, 0.0, Timestamp::now().as_offset_date_time())
            .expect("compare-and-set sweep");

        assert!(staled.is_empty());
        assert_eq!(
            store
                .get(tenant, principal, task.id)
                .await
                .expect("get")
                .expect("task")
                .status,
            TaskStatus::Active
        );
        assert!(store
            .history(tenant, principal, task.id, 10)
            .await
            .expect("history")
            .is_empty());
        assert_eq!(
            store
                .get_claims_for_task(tenant, principal, task.id)
                .await
                .expect("claims")
                .len(),
            1
        );
        assert!(drain_kinds(&mut receiver).is_empty());
    }

    #[tokio::test]
    /// Create and list claims.
    async fn create_and_list_claims() {
        let (store, bus) = store();
        let mut rx = bus.subscribe("task");
        let (tenant, principal) = (TenantId::new(), PrincipalId::new());
        let task = store
            .create(new_task(tenant, principal, "t"))
            .await
            .expect("create");
        let _ = drain_kinds(&mut rx);

        let claims = store
            .create_claims(tenant, principal, task.id, &["a.rs", "b.rs"], 1800)
            .await
            .expect("claims");
        assert_eq!(claims.len(), 2);
        // The project comes from the task itself, and a fresh lease is live.
        assert!(claims.iter().all(|c| c.project == "henosis" && !c.released));
        assert_eq!(drain_kinds(&mut rx), ["claim.created"]);

        let listed = store
            .get_claims_for_task(tenant, principal, task.id)
            .await
            .expect("list");
        assert_eq!(listed, claims, "stored claims round-trip exactly");
        let by_project = store
            .get_claims_for_project(tenant, principal, "henosis")
            .await
            .expect("list");
        assert_eq!(by_project, claims);

        // Empty paths: nothing created, nothing emitted.
        assert!(store
            .create_claims(tenant, principal, task.id, &[], 1800)
            .await
            .expect("claims")
            .is_empty());
        assert!(drain_kinds(&mut rx).is_empty());
    }

    #[tokio::test]
    /// Create claims requires owned task.
    async fn create_claims_requires_owned_task() {
        let (store, _bus) = store();
        let (tenant, principal) = (TenantId::new(), PrincipalId::new());
        let task = store
            .create(new_task(tenant, principal, "t"))
            .await
            .expect("create");
        // Unknown task.
        let err = store
            .create_claims(tenant, principal, TaskId::new(), &["x.rs"], 60)
            .await
            .expect_err("unknown task");
        assert!(matches!(err, ChiasmError::NotFound(_)));
        // Another principal's task.
        let err = store
            .create_claims(tenant, PrincipalId::new(), task.id, &["x.rs"], 60)
            .await
            .expect_err("foreign task");
        assert!(matches!(err, ChiasmError::NotFound(_)));
    }

    #[tokio::test]
    /// Conflict detection and self exclusion.
    async fn conflict_detection_and_self_exclusion() {
        let (store, _bus) = store();
        let (tenant, principal) = (TenantId::new(), PrincipalId::new());
        let holder = store
            .create(new_task(tenant, principal, "holder"))
            .await
            .expect("create");
        let requester = store
            .create(new_task(tenant, principal, "requester"))
            .await
            .expect("create");
        store
            .create_claims(tenant, principal, holder.id, &["src/lib.rs"], 1800)
            .await
            .expect("claims");

        let conflicts = store
            .check_conflicts(
                tenant,
                principal,
                "henosis",
                &["src/lib.rs", "other.rs"],
                Some(requester.id),
            )
            .await
            .expect("check");
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].path, "src/lib.rs");
        assert_eq!(conflicts[0].claimed_by_task, holder.id);
        assert_eq!(conflicts[0].claimed_by_principal, principal);

        // The holder re-checking its own paths does not self-block.
        let own = store
            .check_conflicts(
                tenant,
                principal,
                "henosis",
                &["src/lib.rs"],
                Some(holder.id),
            )
            .await
            .expect("check");
        assert!(own.is_empty());
    }

    #[tokio::test]
    /// A conflict rolls back every earlier lease in the same path batch.
    async fn create_claims_rolls_back_entire_conflicting_batch() {
        let (store, _bus) = store();
        let tenant = TenantId::new();
        let principal = PrincipalId::new();
        let holder = store
            .create(new_task(tenant, principal, "holder"))
            .await
            .expect("create holder");
        let requester = store
            .create(new_task(tenant, principal, "requester"))
            .await
            .expect("create requester");
        store
            .create_claims(tenant, principal, holder.id, &["shared.rs"], 1800)
            .await
            .expect("holder claim");

        let error = store
            .create_claims(
                tenant,
                principal,
                requester.id,
                &["free.rs", "shared.rs"],
                1800,
            )
            .await
            .expect_err("conflicting batch");
        assert!(
            matches!(
                error,
                ChiasmError::ClaimConflict {
                    path,
                    claimed_by_task
                } if path == "shared.rs" && claimed_by_task == holder.id
            ),
            "the active holder is identified"
        );
        assert!(
            store
                .get_claims_for_task(tenant, principal, requester.id)
                .await
                .expect("requester claims")
                .is_empty(),
            "the earlier free path insert was rolled back"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    /// Two stores racing for one path produce one owner and one typed conflict.
    async fn create_claims_is_atomic_across_store_instances() {
        let root =
            std::env::temp_dir().join(format!("henosis-chiasm-claim-race-{}", TaskId::new()));
        let path = root.join("state").join("chiasm.sqlite");
        let bus = Arc::new(AxonBus::new());
        let first_store = Arc::new(ChiasmStore::open(&path, bus.clone()).expect("first store"));
        let second_store = Arc::new(ChiasmStore::open(&path, bus).expect("second store"));
        let tenant = TenantId::new();
        let principal = PrincipalId::new();
        let first_task = first_store
            .create(new_task(tenant, principal, "first"))
            .await
            .expect("first task");
        let second_task = second_store
            .create(new_task(tenant, principal, "second"))
            .await
            .expect("second task");
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let runtime = tokio::runtime::Handle::current();

        let first_attempt = {
            let store = first_store.clone();
            let barrier = barrier.clone();
            let runtime = runtime.clone();
            tokio::task::spawn_blocking(move || {
                barrier.wait();
                runtime.block_on(store.create_claims(
                    tenant,
                    principal,
                    first_task.id,
                    &["shared.rs"],
                    1800,
                ))
            })
        };
        let second_attempt = {
            let store = second_store.clone();
            let barrier = barrier.clone();
            tokio::task::spawn_blocking(move || {
                barrier.wait();
                runtime.block_on(store.create_claims(
                    tenant,
                    principal,
                    second_task.id,
                    &["shared.rs"],
                    1800,
                ))
            })
        };
        let outcomes = [
            first_attempt.await.expect("first join"),
            second_attempt.await.expect("second join"),
        ];
        assert_eq!(outcomes.iter().filter(|result| result.is_ok()).count(), 1);
        assert_eq!(
            outcomes
                .iter()
                .filter(|result| matches!(result, Err(ChiasmError::ClaimConflict { .. })))
                .count(),
            1
        );
        assert_eq!(
            first_store
                .get_claims_for_project(tenant, principal, "henosis")
                .await
                .expect("project claims")
                .len(),
            1
        );

        drop(first_store);
        drop(second_store);
        std::fs::remove_dir_all(&root).expect("remove test database");
    }

    #[tokio::test]
    /// Conflicts are owner scoped.
    async fn conflicts_are_owner_scoped() {
        let (store, _bus) = store();
        let tenant = TenantId::new();
        let owner = PrincipalId::new();
        let other = PrincipalId::new();
        let task = store
            .create(new_task(tenant, owner, "t"))
            .await
            .expect("create");
        store
            .create_claims(tenant, owner, task.id, &["shared.rs"], 1800)
            .await
            .expect("claims");
        // Another principal's coordination space sees no conflict on the same project+path.
        let conflicts = store
            .check_conflicts(tenant, other, "henosis", &["shared.rs"], None)
            .await
            .expect("check");
        assert!(conflicts.is_empty());
    }

    #[tokio::test]
    /// Expired claims are inactive.
    async fn expired_claims_are_inactive() {
        let (store, _bus) = store();
        let (tenant, principal) = (TenantId::new(), PrincipalId::new());
        let task = store
            .create(new_task(tenant, principal, "t"))
            .await
            .expect("create");
        // TTL 0: expires_at == claimed_at, already in the past by check time.
        store
            .create_claims(tenant, principal, task.id, &["old.rs"], 0)
            .await
            .expect("claims");
        assert!(store
            .check_conflicts(tenant, principal, "henosis", &["old.rs"], None)
            .await
            .expect("check")
            .is_empty());
        assert!(store
            .get_claims_for_task(tenant, principal, task.id)
            .await
            .expect("list")
            .is_empty());
        assert!(store
            .get_claims_for_project(tenant, principal, "henosis")
            .await
            .expect("list")
            .is_empty());
    }

    #[tokio::test]
    /// Release claims releases and emits.
    async fn release_claims_releases_and_emits() {
        let (store, bus) = store();
        let mut rx = bus.subscribe("task");
        let (tenant, principal) = (TenantId::new(), PrincipalId::new());
        let task = store
            .create(new_task(tenant, principal, "t"))
            .await
            .expect("create");
        store
            .create_claims(tenant, principal, task.id, &["main.rs"], 1800)
            .await
            .expect("claims");
        let _ = drain_kinds(&mut rx);

        assert_eq!(
            store
                .release_claims(tenant, principal, task.id)
                .await
                .expect("release"),
            1
        );
        assert_eq!(drain_kinds(&mut rx), ["claim.released"]);
        assert!(store
            .get_claims_for_task(tenant, principal, task.id)
            .await
            .expect("list")
            .is_empty());
        assert!(store
            .check_conflicts(tenant, principal, "henosis", &["main.rs"], None)
            .await
            .expect("check")
            .is_empty());

        // Idempotent: a second release (or a foreign principal) releases zero, no event.
        assert_eq!(
            store
                .release_claims(tenant, principal, task.id)
                .await
                .expect("release"),
            0
        );
        assert_eq!(
            store
                .release_claims(tenant, PrincipalId::new(), task.id)
                .await
                .expect("release"),
            0
        );
        assert!(drain_kinds(&mut rx).is_empty());
    }

    #[tokio::test]
    /// Heartbeat refreshes claim leases.
    async fn heartbeat_refreshes_claim_leases() {
        let (store, _bus) = store();
        let (tenant, principal) = (TenantId::new(), PrincipalId::new());
        let task = store
            .create(new_task(tenant, principal, "t"))
            .await
            .expect("create");
        // An immediately-lapsed lease (TTL 0) is inactive...
        let claimed = store
            .create_claims(tenant, principal, task.id, &["a.rs"], 0)
            .await
            .expect("claims");
        assert!(store
            .get_claims_for_task(tenant, principal, task.id)
            .await
            .expect("list")
            .is_empty());
        // ...until a heartbeat proves the holder alive and extends it to now + 600s.
        store
            .record_heartbeat(tenant, principal, task.id)
            .await
            .expect("heartbeat");
        let refreshed = store
            .get_claims_for_task(tenant, principal, task.id)
            .await
            .expect("list");
        assert_eq!(refreshed.len(), 1);
        assert!(
            refreshed[0].expires_at.as_offset_date_time()
                > claimed[0].expires_at.as_offset_date_time(),
            "heartbeat pushed the lease expiry forward"
        );
    }

    #[tokio::test]
    /// Stale sweep forfeits claims.
    async fn stale_sweep_forfeits_claims() {
        let (store, bus) = store();
        let mut rx = bus.subscribe("task");
        let (tenant, principal) = (TenantId::new(), PrincipalId::new());
        let task = store
            .create(new_task(tenant, principal, "t"))
            .await
            .expect("create");
        store
            .record_heartbeat(tenant, principal, task.id)
            .await
            .expect("heartbeat");
        store
            .create_claims(tenant, principal, task.id, &["w.rs"], 1800)
            .await
            .expect("claims");
        let _ = drain_kinds(&mut rx);

        // grace 0.0 -> any elapsed time is overdue.
        let staled = store.mark_stale(0.0).await.expect("sweep");
        assert_eq!(staled.len(), 1);
        assert_eq!(staled[0].id, task.id);
        assert_eq!(drain_kinds(&mut rx), ["task.stale", "claim.released"]);
        assert!(
            store
                .get_claims_for_task(tenant, principal, task.id)
                .await
                .expect("list")
                .is_empty(),
            "a staled task forfeits its leases"
        );
    }

    #[tokio::test]
    /// Add and list dependencies.
    async fn add_and_list_dependencies() {
        let (store, _bus) = store();
        let (tenant, principal) = (TenantId::new(), PrincipalId::new());
        let t1 = store
            .create(new_task(tenant, principal, "task-1"))
            .await
            .expect("create");
        let t2 = store
            .create(new_task(tenant, principal, "task-2"))
            .await
            .expect("create");
        let t3 = store
            .create(new_task(tenant, principal, "task-3"))
            .await
            .expect("create");

        store
            .add_dependencies(tenant, principal, t3.id, &[t1.id, t2.id])
            .await
            .expect("add");
        // Duplicate edges are ignored.
        store
            .add_dependencies(tenant, principal, t3.id, &[t1.id])
            .await
            .expect("re-add");

        let deps = store
            .get_dependencies(tenant, principal, t3.id)
            .await
            .expect("list");
        assert_eq!(deps.len(), 2);
        assert_eq!(deps[0].depends_on, t1.id);
        assert_eq!(deps[0].depends_on_title.as_deref(), Some("task-1"));
        assert_eq!(deps[0].depends_on_status, Some(TaskStatus::Active));
        assert_eq!(deps[1].depends_on, t2.id);

        // Owner-scoped read: another principal sees no edges.
        assert!(store
            .get_dependencies(tenant, PrincipalId::new(), t3.id)
            .await
            .expect("list")
            .is_empty());
    }

    #[tokio::test]
    /// Self dependency rejected.
    async fn self_dependency_rejected() {
        let (store, _bus) = store();
        let (tenant, principal) = (TenantId::new(), PrincipalId::new());
        let t = store
            .create(new_task(tenant, principal, "t"))
            .await
            .expect("create");
        let err = store
            .add_dependencies(tenant, principal, t.id, &[t.id])
            .await
            .expect_err("self-dependency");
        assert!(matches!(err, ChiasmError::SelfDependency(id) if id == t.id));
    }

    #[tokio::test]
    /// Circular dependency rejected.
    async fn circular_dependency_rejected() {
        let (store, _bus) = store();
        let (tenant, principal) = (TenantId::new(), PrincipalId::new());
        let t1 = store
            .create(new_task(tenant, principal, "t1"))
            .await
            .expect("create");
        let t2 = store
            .create(new_task(tenant, principal, "t2"))
            .await
            .expect("create");
        let t3 = store
            .create(new_task(tenant, principal, "t3"))
            .await
            .expect("create");

        store
            .add_dependencies(tenant, principal, t2.id, &[t1.id])
            .await
            .expect("t2 -> t1");
        // Direct cycle: t1 -> t2 while t2 -> t1 exists.
        let err = store
            .add_dependencies(tenant, principal, t1.id, &[t2.id])
            .await
            .expect_err("direct cycle");
        assert!(matches!(err, ChiasmError::DependencyCycle { .. }));
        // Transitive cycle: with t3 -> t2 -> t1, adding t1 -> t3 closes the loop.
        store
            .add_dependencies(tenant, principal, t3.id, &[t2.id])
            .await
            .expect("t3 -> t2");
        let err = store
            .add_dependencies(tenant, principal, t1.id, &[t3.id])
            .await
            .expect_err("transitive cycle");
        assert!(matches!(err, ChiasmError::DependencyCycle { .. }));
        // A rejected batch inserts nothing.
        assert!(store
            .get_dependencies(tenant, principal, t1.id)
            .await
            .expect("list")
            .is_empty());
    }

    #[tokio::test]
    /// Cross principal dependency rejected.
    async fn cross_principal_dependency_rejected() {
        let (store, _bus) = store();
        let tenant = TenantId::new();
        let owner = PrincipalId::new();
        let other = PrincipalId::new();
        let mine = store
            .create(new_task(tenant, owner, "mine"))
            .await
            .expect("create");
        let theirs = store
            .create(new_task(tenant, other, "theirs"))
            .await
            .expect("create");
        let err = store
            .add_dependencies(tenant, owner, mine.id, &[theirs.id])
            .await
            .expect_err("cross-principal edge");
        assert!(matches!(err, ChiasmError::NotFound(id) if id == theirs.id));
    }

    #[tokio::test]
    /// Remove dependency works.
    async fn remove_dependency_works() {
        let (store, _bus) = store();
        let (tenant, principal) = (TenantId::new(), PrincipalId::new());
        let t1 = store
            .create(new_task(tenant, principal, "t1"))
            .await
            .expect("create");
        let t2 = store
            .create(new_task(tenant, principal, "t2"))
            .await
            .expect("create");
        store
            .add_dependencies(tenant, principal, t2.id, &[t1.id])
            .await
            .expect("add");

        // Another principal cannot remove it.
        assert!(!store
            .remove_dependency(tenant, PrincipalId::new(), t2.id, t1.id)
            .await
            .expect("remove"));
        assert!(store
            .remove_dependency(tenant, principal, t2.id, t1.id)
            .await
            .expect("remove"));
        assert!(store
            .get_dependencies(tenant, principal, t2.id)
            .await
            .expect("list")
            .is_empty());
        // Removing a missing edge reports false.
        assert!(!store
            .remove_dependency(tenant, principal, t2.id, t1.id)
            .await
            .expect("remove"));
    }

    #[tokio::test]
    /// Auto unblock on completion.
    async fn auto_unblock_on_completion() {
        let (store, bus) = store();
        let mut rx = bus.subscribe("task");
        let (tenant, principal) = (TenantId::new(), PrincipalId::new());
        let blocker = store
            .create(new_task(tenant, principal, "blocker"))
            .await
            .expect("create");
        let blocked = store
            .create(new_task(tenant, principal, "blocked"))
            .await
            .expect("create");
        store
            .add_dependencies(tenant, principal, blocked.id, &[blocker.id])
            .await
            .expect("add");
        store
            .update(
                tenant,
                principal,
                blocked.id,
                TaskPatch {
                    status: Some(TaskStatus::Blocked),
                    ..Default::default()
                },
            )
            .await
            .expect("block");
        let _ = drain_kinds(&mut rx);

        assert!(
            store
                .check_and_unblock(tenant, principal, blocker.id)
                .await
                .expect("incomplete trigger")
                .is_empty(),
            "an incomplete trigger cannot unblock its dependent"
        );
        store
            .update(
                tenant,
                principal,
                blocker.id,
                TaskPatch {
                    status: Some(TaskStatus::Completed),
                    ..Default::default()
                },
            )
            .await
            .expect("complete blocker");
        let _ = drain_kinds(&mut rx);

        let unblocked = store
            .check_and_unblock(tenant, principal, blocker.id)
            .await
            .expect("unblock");
        assert_eq!(unblocked.len(), 1);
        assert_eq!(unblocked[0].id, blocked.id);
        assert_eq!(unblocked[0].status, TaskStatus::Active);
        assert_eq!(drain_kinds(&mut rx), ["task.updated", "task.unblocked"]);

        let history = store
            .history(tenant, principal, blocked.id, 10)
            .await
            .expect("history");
        assert_eq!(
            history[0].summary.as_deref(),
            Some("auto-unblocked: all dependencies completed")
        );
    }

    #[tokio::test]
    /// Unblock requires all dependencies completed.
    async fn unblock_requires_all_dependencies_completed() {
        let (store, _bus) = store();
        let (tenant, principal) = (TenantId::new(), PrincipalId::new());
        let t1 = store
            .create(new_task(tenant, principal, "t1"))
            .await
            .expect("create");
        let t2 = store
            .create(new_task(tenant, principal, "t2"))
            .await
            .expect("create");
        let dependent = store
            .create(new_task(tenant, principal, "dependent"))
            .await
            .expect("create");
        store
            .add_dependencies(tenant, principal, dependent.id, &[t1.id, t2.id])
            .await
            .expect("add");
        store
            .update(
                tenant,
                principal,
                dependent.id,
                TaskPatch {
                    status: Some(TaskStatus::Blocked),
                    ..Default::default()
                },
            )
            .await
            .expect("block");

        // t2 is still incomplete -> nothing unblocks.
        store
            .update(
                tenant,
                principal,
                t1.id,
                TaskPatch {
                    status: Some(TaskStatus::Completed),
                    ..Default::default()
                },
            )
            .await
            .expect("complete t1");
        assert!(store
            .check_and_unblock(tenant, principal, t1.id)
            .await
            .expect("check")
            .is_empty());

        // Completing t2 unblocks the dependent (t1 is already completed in the DB).
        store
            .update(
                tenant,
                principal,
                t2.id,
                TaskPatch {
                    status: Some(TaskStatus::Completed),
                    ..Default::default()
                },
            )
            .await
            .expect("complete t2");
        let unblocked = store
            .check_and_unblock(tenant, principal, t2.id)
            .await
            .expect("check");
        assert_eq!(unblocked.len(), 1);
        assert_eq!(unblocked[0].id, dependent.id);
    }

    #[tokio::test]
    /// Unblock only activates blocked dependents.
    async fn unblock_only_activates_blocked_dependents() {
        let (store, _bus) = store();
        let (tenant, principal) = (TenantId::new(), PrincipalId::new());
        let blocker = store
            .create(new_task(tenant, principal, "blocker"))
            .await
            .expect("create");
        let done = store
            .create(new_task(tenant, principal, "done"))
            .await
            .expect("create");
        store
            .add_dependencies(tenant, principal, done.id, &[blocker.id])
            .await
            .expect("add");
        // The dependent already completed -- it must NOT be resurrected to active.
        store
            .update(
                tenant,
                principal,
                done.id,
                TaskPatch {
                    status: Some(TaskStatus::Completed),
                    ..Default::default()
                },
            )
            .await
            .expect("complete dependent");
        store
            .update(
                tenant,
                principal,
                blocker.id,
                TaskPatch {
                    status: Some(TaskStatus::Completed),
                    ..Default::default()
                },
            )
            .await
            .expect("complete blocker");
        assert!(store
            .check_and_unblock(tenant, principal, blocker.id)
            .await
            .expect("check")
            .is_empty());
        let got = store
            .get(tenant, principal, done.id)
            .await
            .expect("get")
            .expect("present");
        assert_eq!(got.status, TaskStatus::Completed);

        // And the completed task itself must be owned: a foreign principal is NotFound.
        let err = store
            .check_and_unblock(tenant, PrincipalId::new(), blocker.id)
            .await
            .expect_err("foreign completed task");
        assert!(matches!(err, ChiasmError::NotFound(_)));
    }

    /// Regression: a caller-controlled `ttl_seconds` at the extremes must not
    /// panic the timestamp arithmetic; it saturates to a far-future/past instant.
    #[test]
    fn ts_plus_saturates_instead_of_panicking() {
        let now = Timestamp::now();
        let far_future = ts_plus(&now, i64::MAX);
        assert!(far_future.as_offset_date_time() > now.as_offset_date_time());
        let far_past = ts_plus(&now, i64::MIN);
        assert!(far_past.as_offset_date_time() < now.as_offset_date_time());
        // A normal ttl still works exactly.
        let in_an_hour = ts_plus(&now, 3600);
        assert!(in_an_hour.as_offset_date_time() > now.as_offset_date_time());
    }
}
