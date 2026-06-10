//! The SQLite-backed Chiasm task store.
//!
//! Reimplements the Kleos chiasm task surface (`kleos-lib/src/services/chiasm/tasks.rs`) against
//! the Henosis substrate: ownership is a [`PrincipalId`] (every read/write scopes on it, replacing
//! the Kleos `WHERE user_id = ?` predicate), lifecycle events are typed and published to the
//! in-process [`AxonBus`], and schema is managed by the kernel-crate migration convention
//! (`PRAGMA user_version` + `migrations/Vn__*.sql`). Concurrency: one `Connection` behind a
//! `Mutex` (a pool can replace it later without changing this surface).
//!
//! Slices 1-3 cover task CRUD/history/stats, the work queue (enqueue/claim), heartbeat + stale
//! sweep, path claims (TTL leases), and the dependency DAG (BFS cycle check + auto-unblock).
//! The legacy `user_id -> PrincipalId` backfill lands in a later slice.

use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};

use rusqlite::{Connection, OptionalExtension};
use syntheos_axon::AxonBus;
use syntheos_contracts::{PrincipalId, TaskId, TenantId, Timestamp, TypedEvent};

use crate::error::ChiasmError;
use crate::events::{
    ClaimCreated, ClaimReleased, TaskClaimed, TaskCompleted, TaskCreated, TaskDeleted, TaskQueued,
    TaskStale, TaskUnblocked, TaskUpdated,
};
use crate::model::{
    ChiasmStats, Dependency, EnqueueTask, NewTask, PathClaim, PathConflict, Task, TaskFilter,
    TaskPatch, TaskStatus, TaskUpdate,
};

/// Ordered schema migrations, applied by `PRAGMA user_version`. Append-only (see the DB convention).
const MIGRATIONS: &[(i64, &str)] = &[
    (1, include_str!("../migrations/V1__chiasm_tasks.sql")),
    (2, include_str!("../migrations/V2__chiasm_claims_deps.sql")),
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
    /// The one connection, serialized by a `Mutex` (rusqlite `Connection` is `Send`, not `Sync`).
    conn: Mutex<Connection>,
    /// The bus task-lifecycle events are published onto.
    bus: Arc<AxonBus>,
}

/// Map a generic rusqlite error to an opaque backend error.
fn berr(e: rusqlite::Error) -> ChiasmError {
    ChiasmError::Backend(e.to_string())
}

/// Serialize a [`Timestamp`] to its stored RFC3339-UTC string (via the contracts wire form).
fn ts_to_db(ts: &Timestamp) -> Result<String, ChiasmError> {
    serde_json::to_value(ts)
        .ok()
        .and_then(|v| v.as_str().map(String::from))
        .ok_or_else(|| ChiasmError::Backend("timestamp serialize".to_string()))
}

/// Parse a stored RFC3339 string back into a UTC-normalized [`Timestamp`].
fn ts_from_db(s: &str) -> Result<Timestamp, ChiasmError> {
    serde_json::from_value(serde_json::Value::String(s.to_string()))
        .map_err(|e| ChiasmError::Backend(format!("timestamp parse {s:?}: {e}")))
}

/// The instant `secs` seconds after `ts`. Used for path-claim lease expiry.
fn ts_plus(ts: &Timestamp, secs: i64) -> Timestamp {
    Timestamp::from_utc(ts.as_offset_date_time() + time::Duration::seconds(secs))
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

impl RawClaim {
    /// Parse raw columns into a typed [`PathClaim`], surfacing any corrupt value as a backend error.
    fn into_claim(self) -> Result<PathClaim, ChiasmError> {
        Ok(PathClaim {
            id: self.id,
            task_id: self
                .task_id
                .parse::<TaskId>()
                .map_err(|e| ChiasmError::Backend(format!("corrupt task_id {:?}: {e}", self.task_id)))?,
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
            tenant: self
                .tenant
                .parse::<TenantId>()
                .map_err(|e| ChiasmError::Backend(format!("corrupt tenant {:?}: {e}", self.tenant)))?,
            principal_id: parse_id(&self.principal_id, "principal_id")?,
            assignee: self.assignee.as_deref().map(|s| parse_id(s, "assignee")).transpose()?,
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

impl ChiasmStore {
    /// Open (creating the file if absent) a store at `path`, applying any pending migrations.
    pub fn open(path: impl AsRef<Path>, bus: Arc<AxonBus>) -> Result<Self, ChiasmError> {
        let conn = Connection::open(path).map_err(berr)?;
        Self::from_conn(conn, bus)
    }

    /// Open an ephemeral in-memory store. For tests and throwaway use.
    pub fn open_in_memory(bus: Arc<AxonBus>) -> Result<Self, ChiasmError> {
        let conn = Connection::open_in_memory().map_err(berr)?;
        Self::from_conn(conn, bus)
    }

    /// Enable foreign keys, apply migrations, and wrap the connection.
    fn from_conn(mut conn: Connection, bus: Arc<AxonBus>) -> Result<Self, ChiasmError> {
        conn.pragma_update(None, "foreign_keys", true).map_err(berr)?;
        apply_migrations(&mut conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
            bus,
        })
    }

    /// Lock the connection, recovering from a poisoned mutex.
    fn lock(&self) -> MutexGuard<'_, Connection> {
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

    /// Look up a task by id, scoped to its owner. `Ok(None)` if absent or owned by another principal.
    pub async fn get(
        &self,
        principal: PrincipalId,
        id: TaskId,
    ) -> Result<Option<Task>, ChiasmError> {
        let conn = self.lock();
        Self::get_in(&conn, principal, id)
    }

    /// Owner-scoped lookup against an arbitrary connection (also used inside an update transaction).
    fn get_in(
        conn: &Connection,
        principal: PrincipalId,
        id: TaskId,
    ) -> Result<Option<Task>, ChiasmError> {
        let raw = conn
            .query_row(
                &format!("SELECT {TASK_COLUMNS} FROM chiasm_tasks WHERE id = ?1 AND principal_id = ?2"),
                rusqlite::params![id.to_string(), principal.to_string()],
                read_raw,
            )
            .optional()
            .map_err(berr)?;
        raw.map(RawTask::into_task).transpose()
    }

    /// List a principal's tasks, newest-updated first, AND-filtered by [`TaskFilter`].
    pub async fn list(
        &self,
        principal: PrincipalId,
        filter: TaskFilter,
    ) -> Result<Vec<Task>, ChiasmError> {
        let mut sql = format!("SELECT {TASK_COLUMNS} FROM chiasm_tasks WHERE principal_id = ?1");
        let mut args: Vec<rusqlite::types::Value> = vec![principal.to_string().into()];
        let mut n = 1;
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
        // limit/offset are `usize`, safe to inline. OFFSET needs a LIMIT in SQLite (-1 = unbounded).
        match (filter.limit, filter.offset) {
            (Some(l), Some(o)) => sql.push_str(&format!(" LIMIT {l} OFFSET {o}")),
            (Some(l), None) => sql.push_str(&format!(" LIMIT {l}")),
            (None, Some(o)) => sql.push_str(&format!(" LIMIT -1 OFFSET {o}")),
            (None, None) => {}
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

    /// Apply a partial update to an owned task, append a history row, and emit `task.updated` (or
    /// `task.completed` when the new status is terminal). Update + history are one transaction.
    pub async fn update(
        &self,
        principal: PrincipalId,
        id: TaskId,
        patch: TaskPatch,
    ) -> Result<Task, ChiasmError> {
        let task = {
            let mut conn = self.lock();
            let tx = conn.transaction().map_err(berr)?;
            let mut task = Self::get_in(&tx, principal, id)?.ok_or(ChiasmError::NotFound(id))?;
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
                 updated_at = ?5 WHERE id = ?6 AND principal_id = ?7",
                rusqlite::params![
                    &task.title,
                    task.status.as_str(),
                    &task.summary,
                    task.assignee.map(|a| a.to_string()),
                    ts_to_db(&task.updated_at)?,
                    task.id.to_string(),
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

    /// Delete an owned task (its history cascades). Returns whether a row was removed; emits
    /// `task.deleted` on a real deletion.
    pub async fn delete(&self, principal: PrincipalId, id: TaskId) -> Result<bool, ChiasmError> {
        // Fetch first (scoped) so the event can carry the task's tenant/principal.
        let Some(task) = self.get(principal, id).await? else {
            return Ok(false);
        };
        let removed = {
            let conn = self.lock();
            conn.execute(
                "DELETE FROM chiasm_tasks WHERE id = ?1 AND principal_id = ?2",
                rusqlite::params![id.to_string(), principal.to_string()],
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

    /// Return an owned task's change history, newest first, capped at `limit`.
    pub async fn history(
        &self,
        principal: PrincipalId,
        id: TaskId,
        limit: usize,
    ) -> Result<Vec<TaskUpdate>, ChiasmError> {
        let conn = self.lock();
        let mut stmt = conn
            .prepare(
                "SELECT u.id, u.task_id, u.status, u.summary, u.created_at \
                 FROM chiasm_task_updates u JOIN chiasm_tasks t ON t.id = u.task_id \
                 WHERE u.task_id = ?1 AND t.principal_id = ?2 ORDER BY u.id DESC LIMIT ?3",
            )
            .map_err(berr)?;
        let rows = stmt
            .query_map(
                rusqlite::params![id.to_string(), principal.to_string(), limit as i64],
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
                task_id: task_id
                    .parse::<TaskId>()
                    .map_err(|e| ChiasmError::Backend(format!("corrupt task_id {task_id:?}: {e}")))?,
                status: TaskStatus::parse(&status)?,
                summary,
                created_at: ts_from_db(&created_at)?,
            });
        }
        Ok(out)
    }

    /// Aggregate task counts for a principal.
    pub async fn stats(&self, principal: PrincipalId) -> Result<ChiasmStats, ChiasmError> {
        let conn = self.lock();
        let mut stmt = conn
            .prepare("SELECT status, COUNT(*) FROM chiasm_tasks WHERE principal_id = ?1 GROUP BY status")
            .map_err(berr)?;
        let rows = stmt
            .query_map(rusqlite::params![principal.to_string()], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
            })
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
        owner: PrincipalId,
        claimer: PrincipalId,
        project: Option<&str>,
    ) -> Result<Option<Task>, ChiasmError> {
        let now = ts_to_db(&Timestamp::now())?;
        let project_clause = if project.is_some() {
            "AND project = ?4"
        } else {
            ""
        };
        let sql = format!(
            "UPDATE chiasm_tasks SET assignee = ?2, status = 'active', last_heartbeat = ?3, \
             updated_at = ?3 WHERE id = (SELECT id FROM chiasm_tasks WHERE principal_id = ?1 \
             AND assignee IS NULL AND status = 'queued' {project_clause} \
             ORDER BY rowid ASC LIMIT 1) RETURNING {TASK_COLUMNS}"
        );
        let raw = {
            let conn = self.lock();
            let result = match project {
                Some(p) => conn.query_row(
                    &sql,
                    rusqlite::params![owner.to_string(), claimer.to_string(), now, p],
                    read_raw,
                ),
                None => conn.query_row(
                    &sql,
                    rusqlite::params![owner.to_string(), claimer.to_string(), now],
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

    /// Record a liveness heartbeat for an owned task, refreshing `last_heartbeat` and extending
    /// every unreleased path-claim lease the task holds to now + 600s. The lease refresh is
    /// fire-and-forget (Kleos parity): a refresh failure is logged, never fatal, because task
    /// liveness must not depend on the claims table. Like Kleos, the refresh also revives an
    /// unreleased lease that had already lapsed -- the heartbeat proves the holder is still
    /// alive and working. Returns [`ChiasmError::NotFound`] if the task does not exist or is
    /// owned by another principal.
    pub async fn record_heartbeat(
        &self,
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
                     WHERE id = ?2 AND principal_id = ?3",
                    rusqlite::params![now, id.to_string(), principal.to_string()],
                )
                .map_err(berr)?;
            if updated > 0 {
                if let Err(e) = conn.execute(
                    "UPDATE chiasm_path_claims SET expires_at = ?1 \
                     WHERE task_id = ?2 AND released = 0",
                    rusqlite::params![lease, id.to_string()],
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
        const STALE_SUMMARY: &str = "marked stale: heartbeat overdue";
        let candidates: Vec<Task> = {
            let conn = self.lock();
            let mut stmt = conn
                .prepare(&format!(
                    "SELECT {TASK_COLUMNS} FROM chiasm_tasks \
                     WHERE status IN ('active', 'paused') AND last_heartbeat IS NOT NULL"
                ))
                .map_err(berr)?;
            let rows = stmt.query_map([], read_raw).map_err(berr)?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row.map_err(berr)?.into_task()?);
            }
            out
        };
        let now_odt = Timestamp::now().as_offset_date_time();
        let mut staled = Vec::new();
        for mut task in candidates {
            let Some(hb) = task.last_heartbeat.as_ref().map(|t| t.as_offset_date_time()) else {
                continue;
            };
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
                tx.execute(
                    "UPDATE chiasm_tasks SET status = 'stale', summary = ?2, updated_at = ?3 \
                     WHERE id = ?1",
                    rusqlite::params![task.id.to_string(), STALE_SUMMARY, now_db],
                )
                .map_err(berr)?;
                tx.execute(
                    "INSERT INTO chiasm_task_updates (task_id, status, summary, created_at) \
                     VALUES (?1, 'stale', ?2, ?3)",
                    rusqlite::params![task.id.to_string(), STALE_SUMMARY, now_db],
                )
                .map_err(berr)?;
                let released = tx
                    .execute(
                        "UPDATE chiasm_path_claims SET released = 1 \
                         WHERE task_id = ?1 AND released = 0",
                        rusqlite::params![task.id.to_string()],
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

    /// Create TTL path-claim leases on `paths` for an owned task, and emit `claim.created`.
    ///
    /// Each lease is created at now and expires at now + `ttl_seconds`; heartbeats on the task
    /// extend unreleased leases. The project is taken from the task itself (Kleos accepted it
    /// as a separate argument, which let a claim land in a different project than its task).
    /// Returns the new claims, in path order. An empty `paths` creates nothing and emits
    /// nothing. [`ChiasmError::NotFound`] if the task does not exist or is owned by another
    /// principal.
    pub async fn create_claims(
        &self,
        principal: PrincipalId,
        task_id: TaskId,
        paths: &[&str],
        ttl_seconds: i64,
    ) -> Result<Vec<PathClaim>, ChiasmError> {
        let now = Timestamp::now();
        let expires = ts_plus(&now, ttl_seconds);
        let (task, claims) = {
            let mut conn = self.lock();
            let tx = conn.transaction().map_err(berr)?;
            let task = Self::get_in(&tx, principal, task_id)?.ok_or(ChiasmError::NotFound(task_id))?;
            let mut claims = Vec::with_capacity(paths.len());
            for &path in paths {
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
                 ORDER BY id ASC"
            ))
            .map_err(berr)?;
        let mut conflicts = Vec::new();
        for &path in paths {
            let rows = stmt
                .query_map(
                    rusqlite::params![principal.to_string(), project, path],
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

    /// List a task's active (unreleased, unexpired) claims, oldest first. Owner-scoped: another
    /// principal's task yields an empty list (matching `history`'s read semantics).
    pub async fn get_claims_for_task(
        &self,
        principal: PrincipalId,
        task_id: TaskId,
    ) -> Result<Vec<PathClaim>, ChiasmError> {
        let now = Timestamp::now();
        let conn = self.lock();
        let mut stmt = conn
            .prepare(&format!(
                "SELECT {CLAIM_COLUMNS} FROM chiasm_path_claims \
                 WHERE task_id = ?1 AND principal_id = ?2 AND released = 0 ORDER BY id ASC"
            ))
            .map_err(berr)?;
        let rows = stmt
            .query_map(
                rusqlite::params![task_id.to_string(), principal.to_string()],
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

    /// List every active (unreleased, unexpired) claim in one of a principal's projects,
    /// oldest first.
    pub async fn get_claims_for_project(
        &self,
        principal: PrincipalId,
        project: &str,
    ) -> Result<Vec<PathClaim>, ChiasmError> {
        let now = Timestamp::now();
        let conn = self.lock();
        let mut stmt = conn
            .prepare(&format!(
                "SELECT {CLAIM_COLUMNS} FROM chiasm_path_claims \
                 WHERE principal_id = ?1 AND project = ?2 AND released = 0 ORDER BY id ASC"
            ))
            .map_err(berr)?;
        let rows = stmt
            .query_map(rusqlite::params![principal.to_string(), project], read_raw_claim)
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

    /// Release every unreleased claim an owned task holds (`released = 1`), returning how many
    /// were released, and emit `claim.released` when any were. Idempotent: a second release, a
    /// task with no live claims, or another principal's task releases zero and emits nothing.
    pub async fn release_claims(
        &self,
        principal: PrincipalId,
        task_id: TaskId,
    ) -> Result<usize, ChiasmError> {
        let Some(task) = self.get(principal, task_id).await? else {
            return Ok(0);
        };
        let count = {
            let conn = self.lock();
            conn.execute(
                "UPDATE chiasm_path_claims SET released = 1 \
                 WHERE task_id = ?1 AND principal_id = ?2 AND released = 0",
                rusqlite::params![task_id.to_string(), principal.to_string()],
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

    /// Whether `needle` is reachable from `start` by walking `depends_on` edges (BFS). Used to
    /// reject a new edge `needle -> start` that would close a cycle.
    fn reaches(conn: &Connection, start: TaskId, needle: TaskId) -> Result<bool, ChiasmError> {
        let mut visited = std::collections::HashSet::new();
        let mut queue = std::collections::VecDeque::new();
        queue.push_back(start);
        let mut stmt = conn
            .prepare("SELECT depends_on FROM chiasm_task_dependencies WHERE task_id = ?1")
            .map_err(berr)?;
        while let Some(current) = queue.pop_front() {
            if current == needle {
                return Ok(true);
            }
            if !visited.insert(current) {
                continue;
            }
            let rows = stmt
                .query_map(rusqlite::params![current.to_string()], |r| {
                    r.get::<_, String>(0)
                })
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
    /// ([`ChiasmError::DependencyCycle`], BFS over existing edges). Both endpoints must be
    /// owned by `principal` -- a missing or foreign target is [`ChiasmError::NotFound`], so
    /// cross-principal edges cannot exist by construction. Duplicate edges are ignored. The
    /// whole batch is one transaction: any rejection inserts nothing, and the cycle check sees
    /// edges added earlier in the same batch.
    pub async fn add_dependencies(
        &self,
        principal: PrincipalId,
        task_id: TaskId,
        depends_on: &[TaskId],
    ) -> Result<(), ChiasmError> {
        let now = ts_to_db(&Timestamp::now())?;
        let mut conn = self.lock();
        let tx = conn.transaction().map_err(berr)?;
        Self::get_in(&tx, principal, task_id)?.ok_or(ChiasmError::NotFound(task_id))?;
        for &dep in depends_on {
            if dep == task_id {
                return Err(ChiasmError::SelfDependency(task_id));
            }
            Self::get_in(&tx, principal, dep)?.ok_or(ChiasmError::NotFound(dep))?;
            if Self::reaches(&tx, dep, task_id)? {
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

    /// List a task's dependency edges, oldest first, each joined with the depended-on task's
    /// current title and status. Owner-scoped: another principal's task yields an empty list.
    pub async fn get_dependencies(
        &self,
        principal: PrincipalId,
        task_id: TaskId,
    ) -> Result<Vec<Dependency>, ChiasmError> {
        let conn = self.lock();
        let mut stmt = conn
            .prepare(
                "SELECT d.id, d.task_id, d.depends_on, dt.title, dt.status, d.created_at \
                 FROM chiasm_task_dependencies d \
                 JOIN chiasm_tasks t ON t.id = d.task_id \
                 LEFT JOIN chiasm_tasks dt ON dt.id = d.depends_on \
                 WHERE d.task_id = ?1 AND t.principal_id = ?2 ORDER BY d.id ASC",
            )
            .map_err(berr)?;
        let rows = stmt
            .query_map(
                rusqlite::params![task_id.to_string(), principal.to_string()],
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

    /// Remove one dependency edge from an owned task. Returns whether an edge was removed
    /// (`false` for a missing edge or another principal's task).
    pub async fn remove_dependency(
        &self,
        principal: PrincipalId,
        task_id: TaskId,
        depends_on: TaskId,
    ) -> Result<bool, ChiasmError> {
        let conn = self.lock();
        let n = conn
            .execute(
                "DELETE FROM chiasm_task_dependencies WHERE task_id = ?1 AND depends_on = ?2 \
                 AND task_id IN (SELECT id FROM chiasm_tasks WHERE principal_id = ?3)",
                rusqlite::params![
                    task_id.to_string(),
                    depends_on.to_string(),
                    principal.to_string()
                ],
            )
            .map_err(berr)?;
        Ok(n > 0)
    }

    /// After `completed_task_id` completes, activate every owned dependent task that is
    /// currently [`TaskStatus::Blocked`] and whose dependencies are ALL completed. Each unblock
    /// goes through [`Self::update`] (history row + `task.updated`) and then emits
    /// `task.unblocked`. The just-completed task counts as completed even if the caller has not
    /// yet committed its status change, so the call is order-independent. Returns the tasks it
    /// activated. [`ChiasmError::NotFound`] if the completed task is not owned by `principal`.
    ///
    /// Two deliberate deviations from the Kleos port: the unblock is scoped to the dependent's
    /// owner principal (Kleos hardcoded `user_id = 1`), and only `blocked` dependents are
    /// activated (Kleos activated dependents in ANY status, which could resurrect a completed
    /// or stale task).
    pub async fn check_and_unblock(
        &self,
        principal: PrincipalId,
        completed_task_id: TaskId,
    ) -> Result<Vec<Task>, ChiasmError> {
        let candidates: Vec<TaskId> = {
            let conn = self.lock();
            Self::get_in(&conn, principal, completed_task_id)?
                .ok_or(ChiasmError::NotFound(completed_task_id))?;
            // Blocked dependents of the completed task with no other incomplete dependency.
            let mut stmt = conn
                .prepare(
                    "SELECT DISTINCT d.task_id FROM chiasm_task_dependencies d \
                     JOIN chiasm_tasks t ON t.id = d.task_id \
                     WHERE d.depends_on = ?1 AND t.principal_id = ?2 AND t.status = 'blocked' \
                       AND NOT EXISTS (\
                           SELECT 1 FROM chiasm_task_dependencies d2 \
                           JOIN chiasm_tasks t2 ON t2.id = d2.depends_on \
                           WHERE d2.task_id = d.task_id AND d2.depends_on != ?1 \
                             AND t2.status != 'completed') \
                     ORDER BY d.task_id",
                )
                .map_err(berr)?;
            let rows = stmt
                .query_map(
                    rusqlite::params![completed_task_id.to_string(), principal.to_string()],
                    |r| r.get::<_, String>(0),
                )
                .map_err(berr)?;
            let mut ids = Vec::new();
            for row in rows {
                let id = row.map_err(berr)?;
                ids.push(id.parse::<TaskId>().map_err(|e| {
                    ChiasmError::Backend(format!("corrupt task_id {id:?}: {e}"))
                })?);
            }
            ids
        };
        let mut unblocked = Vec::new();
        for id in candidates {
            let task = self
                .update(
                    principal,
                    id,
                    TaskPatch {
                        status: Some(TaskStatus::Active),
                        summary: Some("auto-unblocked: all dependencies completed".to_string()),
                        ..Default::default()
                    },
                )
                .await?;
            self.emit(
                &TaskUnblocked {
                    task_id: task.id.to_string(),
                    completed_dependency: completed_task_id.to_string(),
                },
                task.tenant,
                task.principal_id,
            );
            unblocked.push(task);
        }
        Ok(unblocked)
    }
}

/// Insert a fully-formed [`Task`] row. Shared by `create` and `enqueue`.
fn insert_task(conn: &Connection, task: &Task) -> Result<(), ChiasmError> {
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
fn apply_migrations(conn: &mut Connection) -> Result<(), ChiasmError> {
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
    fn drain_kinds(rx: &mut tokio::sync::broadcast::Receiver<syntheos_contracts::AxonEnvelope>) -> Vec<String> {
        let mut kinds = Vec::new();
        while let Ok(env) = rx.try_recv() {
            kinds.push(env.kind);
        }
        kinds
    }

    #[tokio::test]
    async fn create_then_get_roundtrips() {
        let (store, _bus) = store();
        let (tenant, principal) = (TenantId::new(), PrincipalId::new());
        let made = store
            .create(new_task(tenant, principal, "ship chiasm"))
            .await
            .expect("create");
        assert_eq!(made.status, TaskStatus::Active);
        assert_eq!(made.output_format, "raw");
        let got = store.get(principal, made.id).await.expect("get").expect("present");
        assert_eq!(got, made);
    }

    #[tokio::test]
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
    async fn get_is_owner_scoped() {
        let (store, _bus) = store();
        let tenant = TenantId::new();
        let owner = PrincipalId::new();
        let other = PrincipalId::new();
        let task = store.create(new_task(tenant, owner, "secret")).await.expect("create");
        // The owner sees it; a different principal does not.
        assert!(store.get(owner, task.id).await.expect("get").is_some());
        assert!(store.get(other, task.id).await.expect("get").is_none());
    }

    #[tokio::test]
    async fn update_appends_history_and_emits() {
        let (store, bus) = store();
        let mut rx = bus.subscribe("task");
        let (tenant, principal) = (TenantId::new(), PrincipalId::new());
        let task = store.create(new_task(tenant, principal, "t")).await.expect("create");
        let _ = drain_kinds(&mut rx); // discard task.created

        store
            .update(
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

        let history = store.history(principal, task.id, 10).await.expect("history");
        // Two updates recorded, newest first.
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].status, TaskStatus::Completed);
        assert_eq!(history[1].status, TaskStatus::Blocked);
    }

    #[tokio::test]
    async fn update_unknown_task_is_not_found() {
        let (store, _bus) = store();
        let err = store
            .update(PrincipalId::new(), TaskId::new(), TaskPatch::default())
            .await
            .expect_err("must be NotFound");
        assert!(matches!(err, ChiasmError::NotFound(_)));
    }

    #[tokio::test]
    async fn list_filters_by_status_and_project() {
        let (store, _bus) = store();
        let (tenant, principal) = (TenantId::new(), PrincipalId::new());
        let a = store.create(new_task(tenant, principal, "a")).await.expect("create");
        store.create(new_task(tenant, principal, "b")).await.expect("create");
        store
            .update(principal, a.id, TaskPatch { status: Some(TaskStatus::Completed), ..Default::default() })
            .await
            .expect("update");

        let active = store
            .list(principal, TaskFilter { status: Some(TaskStatus::Active), ..Default::default() })
            .await
            .expect("list");
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].title, "b");

        let all = store.list(principal, TaskFilter::default()).await.expect("list");
        assert_eq!(all.len(), 2);
    }

    #[tokio::test]
    async fn delete_removes_and_emits() {
        let (store, bus) = store();
        let mut rx = bus.subscribe("task");
        let (tenant, principal) = (TenantId::new(), PrincipalId::new());
        let task = store.create(new_task(tenant, principal, "t")).await.expect("create");
        let _ = drain_kinds(&mut rx);

        assert!(store.delete(principal, task.id).await.expect("delete"));
        assert_eq!(drain_kinds(&mut rx), ["task.deleted"]);
        assert!(store.get(principal, task.id).await.expect("get").is_none());
        // Deleting again (or a non-existent task) is a no-op, no event.
        assert!(!store.delete(principal, task.id).await.expect("delete"));
        assert!(drain_kinds(&mut rx).is_empty());
    }

    #[tokio::test]
    async fn stats_counts_by_status() {
        let (store, _bus) = store();
        let (tenant, principal) = (TenantId::new(), PrincipalId::new());
        let a = store.create(new_task(tenant, principal, "a")).await.expect("create");
        store.create(new_task(tenant, principal, "b")).await.expect("create");
        store
            .update(principal, a.id, TaskPatch { status: Some(TaskStatus::Completed), ..Default::default() })
            .await
            .expect("update");
        let stats = store.stats(principal).await.expect("stats");
        assert_eq!(stats.total, 2);
        assert_eq!(stats.by_status.get("active"), Some(&1));
        assert_eq!(stats.by_status.get("completed"), Some(&1));
    }

    #[tokio::test]
    async fn tasks_persist_across_reopen() {
        let tmp = std::env::temp_dir().join(format!("henosis-chiasm-{}.sqlite", TaskId::new()));
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
            let got = store.get(principal, id).await.expect("get").expect("present after reopen");
            assert_eq!(got.title, "durable");
        }
        let _ = std::fs::remove_file(&tmp);
    }

    /// A minimal EnqueueTask for `principal` in `tenant` under `project`.
    fn enqueue_task(tenant: TenantId, principal: PrincipalId, title: &str, project: &str) -> EnqueueTask {
        EnqueueTask {
            tenant,
            principal_id: principal,
            project: project.to_string(),
            title: title.to_string(),
            summary: None,
        }
    }

    #[tokio::test]
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
            .claim_next(owner, claimer, None)
            .await
            .expect("claim")
            .expect("a task to claim");
        assert_eq!(claimed.id, queued.id);
        assert_eq!(claimed.status, TaskStatus::Active);
        assert_eq!(claimed.assignee, Some(claimer));
        assert!(claimed.last_heartbeat.is_some(), "claim stamps a first heartbeat");
        assert_eq!(drain_kinds(&mut rx), ["task.claimed"]);

        // Queue is now empty.
        assert!(store.claim_next(owner, claimer, None).await.expect("claim").is_none());
    }

    #[tokio::test]
    async fn claim_is_fifo() {
        let (store, _bus) = store();
        let (tenant, owner) = (TenantId::new(), PrincipalId::new());
        let first = store.enqueue(enqueue_task(tenant, owner, "first", "p")).await.expect("enqueue");
        let _second = store.enqueue(enqueue_task(tenant, owner, "second", "p")).await.expect("enqueue");
        let claimed = store
            .claim_next(owner, PrincipalId::new(), None)
            .await
            .expect("claim")
            .expect("task");
        assert_eq!(claimed.id, first.id, "oldest-enqueued task is claimed first");
    }

    #[tokio::test]
    async fn claim_respects_project_filter() {
        let (store, _bus) = store();
        let (tenant, owner) = (TenantId::new(), PrincipalId::new());
        store.enqueue(enqueue_task(tenant, owner, "alpha-task", "alpha")).await.expect("enqueue");
        let beta = store.enqueue(enqueue_task(tenant, owner, "beta-task", "beta")).await.expect("enqueue");
        let claimed = store
            .claim_next(owner, PrincipalId::new(), Some("beta"))
            .await
            .expect("claim")
            .expect("task");
        assert_eq!(claimed.id, beta.id, "only the beta-project task is claimable");
    }

    #[tokio::test]
    async fn claim_is_owner_scoped() {
        let (store, _bus) = store();
        let tenant = TenantId::new();
        let owner = PrincipalId::new();
        let other_owner = PrincipalId::new();
        store.enqueue(enqueue_task(tenant, owner, "t", "p")).await.expect("enqueue");
        // A different owner's queue is empty.
        assert!(store
            .claim_next(other_owner, PrincipalId::new(), None)
            .await
            .expect("claim")
            .is_none());
    }

    #[tokio::test]
    async fn record_heartbeat_updates_and_notfound() {
        let (store, _bus) = store();
        let (tenant, principal) = (TenantId::new(), PrincipalId::new());
        let task = store.create(new_task(tenant, principal, "t")).await.expect("create");
        assert!(task.last_heartbeat.is_none());

        store.record_heartbeat(principal, task.id).await.expect("heartbeat");
        let got = store.get(principal, task.id).await.expect("get").expect("present");
        assert!(got.last_heartbeat.is_some(), "heartbeat sets last_heartbeat");

        // Unknown task, or another principal's task, is NotFound.
        let err = store
            .record_heartbeat(principal, TaskId::new())
            .await
            .expect_err("unknown task");
        assert!(matches!(err, ChiasmError::NotFound(_)));
        let err = store
            .record_heartbeat(PrincipalId::new(), task.id)
            .await
            .expect_err("wrong owner");
        assert!(matches!(err, ChiasmError::NotFound(_)));
    }

    #[tokio::test]
    async fn fresh_heartbeat_is_not_stale() {
        let (store, _bus) = store();
        let (tenant, principal) = (TenantId::new(), PrincipalId::new());
        let task = store.create(new_task(tenant, principal, "t")).await.expect("create");
        store.record_heartbeat(principal, task.id).await.expect("heartbeat");
        // Just beaten, default 300s interval, grace 1.0 -> not overdue.
        assert!(store.mark_stale(1.0).await.expect("sweep").is_empty());
        let got = store.get(principal, task.id).await.expect("get").expect("present");
        assert_eq!(got.status, TaskStatus::Active);
    }

    #[tokio::test]
    async fn overdue_heartbeat_marks_stale() {
        let (store, bus) = store();
        let mut rx = bus.subscribe("task");
        let (tenant, principal) = (TenantId::new(), PrincipalId::new());
        let task = store.create(new_task(tenant, principal, "t")).await.expect("create");
        store.record_heartbeat(principal, task.id).await.expect("heartbeat");
        let _ = drain_kinds(&mut rx);

        // grace 0.0 -> threshold 0 -> any elapsed time is overdue.
        let staled = store.mark_stale(0.0).await.expect("sweep");
        assert_eq!(staled.len(), 1);
        assert_eq!(staled[0].id, task.id);
        assert_eq!(staled[0].status, TaskStatus::Stale);
        assert_eq!(drain_kinds(&mut rx), ["task.stale"]);

        let got = store.get(principal, task.id).await.expect("get").expect("present");
        assert_eq!(got.status, TaskStatus::Stale);
        let history = store.history(principal, task.id, 10).await.expect("history");
        assert_eq!(history[0].status, TaskStatus::Stale, "stale recorded in history");

        // A task with no heartbeat is never swept, even at grace 0.0.
        let unbeaten = store.create(new_task(tenant, principal, "unbeaten")).await.expect("create");
        assert!(store.mark_stale(0.0).await.expect("sweep").iter().all(|t| t.id != unbeaten.id));
    }

    #[tokio::test]
    async fn create_and_list_claims() {
        let (store, bus) = store();
        let mut rx = bus.subscribe("task");
        let (tenant, principal) = (TenantId::new(), PrincipalId::new());
        let task = store.create(new_task(tenant, principal, "t")).await.expect("create");
        let _ = drain_kinds(&mut rx);

        let claims = store
            .create_claims(principal, task.id, &["a.rs", "b.rs"], 1800)
            .await
            .expect("claims");
        assert_eq!(claims.len(), 2);
        // The project comes from the task itself, and a fresh lease is live.
        assert!(claims.iter().all(|c| c.project == "henosis" && !c.released));
        assert_eq!(drain_kinds(&mut rx), ["claim.created"]);

        let listed = store.get_claims_for_task(principal, task.id).await.expect("list");
        assert_eq!(listed, claims, "stored claims round-trip exactly");
        let by_project = store.get_claims_for_project(principal, "henosis").await.expect("list");
        assert_eq!(by_project, claims);

        // Empty paths: nothing created, nothing emitted.
        assert!(store.create_claims(principal, task.id, &[], 1800).await.expect("claims").is_empty());
        assert!(drain_kinds(&mut rx).is_empty());
    }

    #[tokio::test]
    async fn create_claims_requires_owned_task() {
        let (store, _bus) = store();
        let (tenant, principal) = (TenantId::new(), PrincipalId::new());
        let task = store.create(new_task(tenant, principal, "t")).await.expect("create");
        // Unknown task.
        let err = store
            .create_claims(principal, TaskId::new(), &["x.rs"], 60)
            .await
            .expect_err("unknown task");
        assert!(matches!(err, ChiasmError::NotFound(_)));
        // Another principal's task.
        let err = store
            .create_claims(PrincipalId::new(), task.id, &["x.rs"], 60)
            .await
            .expect_err("foreign task");
        assert!(matches!(err, ChiasmError::NotFound(_)));
    }

    #[tokio::test]
    async fn conflict_detection_and_self_exclusion() {
        let (store, _bus) = store();
        let (tenant, principal) = (TenantId::new(), PrincipalId::new());
        let holder = store.create(new_task(tenant, principal, "holder")).await.expect("create");
        let requester = store.create(new_task(tenant, principal, "requester")).await.expect("create");
        store.create_claims(principal, holder.id, &["src/lib.rs"], 1800).await.expect("claims");

        let conflicts = store
            .check_conflicts(principal, "henosis", &["src/lib.rs", "other.rs"], Some(requester.id))
            .await
            .expect("check");
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].path, "src/lib.rs");
        assert_eq!(conflicts[0].claimed_by_task, holder.id);
        assert_eq!(conflicts[0].claimed_by_principal, principal);

        // The holder re-checking its own paths does not self-block.
        let own = store
            .check_conflicts(principal, "henosis", &["src/lib.rs"], Some(holder.id))
            .await
            .expect("check");
        assert!(own.is_empty());
    }

    #[tokio::test]
    async fn conflicts_are_owner_scoped() {
        let (store, _bus) = store();
        let tenant = TenantId::new();
        let owner = PrincipalId::new();
        let other = PrincipalId::new();
        let task = store.create(new_task(tenant, owner, "t")).await.expect("create");
        store.create_claims(owner, task.id, &["shared.rs"], 1800).await.expect("claims");
        // Another principal's coordination space sees no conflict on the same project+path.
        let conflicts = store
            .check_conflicts(other, "henosis", &["shared.rs"], None)
            .await
            .expect("check");
        assert!(conflicts.is_empty());
    }

    #[tokio::test]
    async fn expired_claims_are_inactive() {
        let (store, _bus) = store();
        let (tenant, principal) = (TenantId::new(), PrincipalId::new());
        let task = store.create(new_task(tenant, principal, "t")).await.expect("create");
        // TTL 0: expires_at == claimed_at, already in the past by check time.
        store.create_claims(principal, task.id, &["old.rs"], 0).await.expect("claims");
        assert!(store
            .check_conflicts(principal, "henosis", &["old.rs"], None)
            .await
            .expect("check")
            .is_empty());
        assert!(store.get_claims_for_task(principal, task.id).await.expect("list").is_empty());
        assert!(store.get_claims_for_project(principal, "henosis").await.expect("list").is_empty());
    }

    #[tokio::test]
    async fn release_claims_releases_and_emits() {
        let (store, bus) = store();
        let mut rx = bus.subscribe("task");
        let (tenant, principal) = (TenantId::new(), PrincipalId::new());
        let task = store.create(new_task(tenant, principal, "t")).await.expect("create");
        store.create_claims(principal, task.id, &["main.rs"], 1800).await.expect("claims");
        let _ = drain_kinds(&mut rx);

        assert_eq!(store.release_claims(principal, task.id).await.expect("release"), 1);
        assert_eq!(drain_kinds(&mut rx), ["claim.released"]);
        assert!(store.get_claims_for_task(principal, task.id).await.expect("list").is_empty());
        assert!(store
            .check_conflicts(principal, "henosis", &["main.rs"], None)
            .await
            .expect("check")
            .is_empty());

        // Idempotent: a second release (or a foreign principal) releases zero, no event.
        assert_eq!(store.release_claims(principal, task.id).await.expect("release"), 0);
        assert_eq!(store.release_claims(PrincipalId::new(), task.id).await.expect("release"), 0);
        assert!(drain_kinds(&mut rx).is_empty());
    }

    #[tokio::test]
    async fn heartbeat_refreshes_claim_leases() {
        let (store, _bus) = store();
        let (tenant, principal) = (TenantId::new(), PrincipalId::new());
        let task = store.create(new_task(tenant, principal, "t")).await.expect("create");
        // An immediately-lapsed lease (TTL 0) is inactive...
        let claimed = store.create_claims(principal, task.id, &["a.rs"], 0).await.expect("claims");
        assert!(store.get_claims_for_task(principal, task.id).await.expect("list").is_empty());
        // ...until a heartbeat proves the holder alive and extends it to now + 600s.
        store.record_heartbeat(principal, task.id).await.expect("heartbeat");
        let refreshed = store.get_claims_for_task(principal, task.id).await.expect("list");
        assert_eq!(refreshed.len(), 1);
        assert!(
            refreshed[0].expires_at.as_offset_date_time()
                > claimed[0].expires_at.as_offset_date_time(),
            "heartbeat pushed the lease expiry forward"
        );
    }

    #[tokio::test]
    async fn stale_sweep_forfeits_claims() {
        let (store, bus) = store();
        let mut rx = bus.subscribe("task");
        let (tenant, principal) = (TenantId::new(), PrincipalId::new());
        let task = store.create(new_task(tenant, principal, "t")).await.expect("create");
        store.record_heartbeat(principal, task.id).await.expect("heartbeat");
        store.create_claims(principal, task.id, &["w.rs"], 1800).await.expect("claims");
        let _ = drain_kinds(&mut rx);

        // grace 0.0 -> any elapsed time is overdue.
        let staled = store.mark_stale(0.0).await.expect("sweep");
        assert_eq!(staled.len(), 1);
        assert_eq!(staled[0].id, task.id);
        assert_eq!(drain_kinds(&mut rx), ["task.stale", "claim.released"]);
        assert!(
            store.get_claims_for_task(principal, task.id).await.expect("list").is_empty(),
            "a staled task forfeits its leases"
        );
    }

    #[tokio::test]
    async fn add_and_list_dependencies() {
        let (store, _bus) = store();
        let (tenant, principal) = (TenantId::new(), PrincipalId::new());
        let t1 = store.create(new_task(tenant, principal, "task-1")).await.expect("create");
        let t2 = store.create(new_task(tenant, principal, "task-2")).await.expect("create");
        let t3 = store.create(new_task(tenant, principal, "task-3")).await.expect("create");

        store.add_dependencies(principal, t3.id, &[t1.id, t2.id]).await.expect("add");
        // Duplicate edges are ignored.
        store.add_dependencies(principal, t3.id, &[t1.id]).await.expect("re-add");

        let deps = store.get_dependencies(principal, t3.id).await.expect("list");
        assert_eq!(deps.len(), 2);
        assert_eq!(deps[0].depends_on, t1.id);
        assert_eq!(deps[0].depends_on_title.as_deref(), Some("task-1"));
        assert_eq!(deps[0].depends_on_status, Some(TaskStatus::Active));
        assert_eq!(deps[1].depends_on, t2.id);

        // Owner-scoped read: another principal sees no edges.
        assert!(store
            .get_dependencies(PrincipalId::new(), t3.id)
            .await
            .expect("list")
            .is_empty());
    }

    #[tokio::test]
    async fn self_dependency_rejected() {
        let (store, _bus) = store();
        let (tenant, principal) = (TenantId::new(), PrincipalId::new());
        let t = store.create(new_task(tenant, principal, "t")).await.expect("create");
        let err = store
            .add_dependencies(principal, t.id, &[t.id])
            .await
            .expect_err("self-dependency");
        assert!(matches!(err, ChiasmError::SelfDependency(id) if id == t.id));
    }

    #[tokio::test]
    async fn circular_dependency_rejected() {
        let (store, _bus) = store();
        let (tenant, principal) = (TenantId::new(), PrincipalId::new());
        let t1 = store.create(new_task(tenant, principal, "t1")).await.expect("create");
        let t2 = store.create(new_task(tenant, principal, "t2")).await.expect("create");
        let t3 = store.create(new_task(tenant, principal, "t3")).await.expect("create");

        store.add_dependencies(principal, t2.id, &[t1.id]).await.expect("t2 -> t1");
        // Direct cycle: t1 -> t2 while t2 -> t1 exists.
        let err = store
            .add_dependencies(principal, t1.id, &[t2.id])
            .await
            .expect_err("direct cycle");
        assert!(matches!(err, ChiasmError::DependencyCycle { .. }));
        // Transitive cycle: with t3 -> t2 -> t1, adding t1 -> t3 closes the loop.
        store.add_dependencies(principal, t3.id, &[t2.id]).await.expect("t3 -> t2");
        let err = store
            .add_dependencies(principal, t1.id, &[t3.id])
            .await
            .expect_err("transitive cycle");
        assert!(matches!(err, ChiasmError::DependencyCycle { .. }));
        // A rejected batch inserts nothing.
        assert!(store.get_dependencies(principal, t1.id).await.expect("list").is_empty());
    }

    #[tokio::test]
    async fn cross_principal_dependency_rejected() {
        let (store, _bus) = store();
        let tenant = TenantId::new();
        let owner = PrincipalId::new();
        let other = PrincipalId::new();
        let mine = store.create(new_task(tenant, owner, "mine")).await.expect("create");
        let theirs = store.create(new_task(tenant, other, "theirs")).await.expect("create");
        let err = store
            .add_dependencies(owner, mine.id, &[theirs.id])
            .await
            .expect_err("cross-principal edge");
        assert!(matches!(err, ChiasmError::NotFound(id) if id == theirs.id));
    }

    #[tokio::test]
    async fn remove_dependency_works() {
        let (store, _bus) = store();
        let (tenant, principal) = (TenantId::new(), PrincipalId::new());
        let t1 = store.create(new_task(tenant, principal, "t1")).await.expect("create");
        let t2 = store.create(new_task(tenant, principal, "t2")).await.expect("create");
        store.add_dependencies(principal, t2.id, &[t1.id]).await.expect("add");

        // Another principal cannot remove it.
        assert!(!store
            .remove_dependency(PrincipalId::new(), t2.id, t1.id)
            .await
            .expect("remove"));
        assert!(store.remove_dependency(principal, t2.id, t1.id).await.expect("remove"));
        assert!(store.get_dependencies(principal, t2.id).await.expect("list").is_empty());
        // Removing a missing edge reports false.
        assert!(!store.remove_dependency(principal, t2.id, t1.id).await.expect("remove"));
    }

    #[tokio::test]
    async fn auto_unblock_on_completion() {
        let (store, bus) = store();
        let mut rx = bus.subscribe("task");
        let (tenant, principal) = (TenantId::new(), PrincipalId::new());
        let blocker = store.create(new_task(tenant, principal, "blocker")).await.expect("create");
        let blocked = store.create(new_task(tenant, principal, "blocked")).await.expect("create");
        store.add_dependencies(principal, blocked.id, &[blocker.id]).await.expect("add");
        store
            .update(
                principal,
                blocked.id,
                TaskPatch { status: Some(TaskStatus::Blocked), ..Default::default() },
            )
            .await
            .expect("block");
        let _ = drain_kinds(&mut rx);

        // Order-independent: the blocker's completion need not be committed yet.
        let unblocked = store.check_and_unblock(principal, blocker.id).await.expect("unblock");
        assert_eq!(unblocked.len(), 1);
        assert_eq!(unblocked[0].id, blocked.id);
        assert_eq!(unblocked[0].status, TaskStatus::Active);
        assert_eq!(drain_kinds(&mut rx), ["task.updated", "task.unblocked"]);

        let history = store.history(principal, blocked.id, 10).await.expect("history");
        assert_eq!(
            history[0].summary.as_deref(),
            Some("auto-unblocked: all dependencies completed")
        );
    }

    #[tokio::test]
    async fn unblock_requires_all_dependencies_completed() {
        let (store, _bus) = store();
        let (tenant, principal) = (TenantId::new(), PrincipalId::new());
        let t1 = store.create(new_task(tenant, principal, "t1")).await.expect("create");
        let t2 = store.create(new_task(tenant, principal, "t2")).await.expect("create");
        let dependent = store.create(new_task(tenant, principal, "dependent")).await.expect("create");
        store
            .add_dependencies(principal, dependent.id, &[t1.id, t2.id])
            .await
            .expect("add");
        store
            .update(
                principal,
                dependent.id,
                TaskPatch { status: Some(TaskStatus::Blocked), ..Default::default() },
            )
            .await
            .expect("block");

        // t2 is still incomplete -> nothing unblocks.
        store
            .update(
                principal,
                t1.id,
                TaskPatch { status: Some(TaskStatus::Completed), ..Default::default() },
            )
            .await
            .expect("complete t1");
        assert!(store.check_and_unblock(principal, t1.id).await.expect("check").is_empty());

        // Completing t2 unblocks the dependent (t1 is already completed in the DB).
        store
            .update(
                principal,
                t2.id,
                TaskPatch { status: Some(TaskStatus::Completed), ..Default::default() },
            )
            .await
            .expect("complete t2");
        let unblocked = store.check_and_unblock(principal, t2.id).await.expect("check");
        assert_eq!(unblocked.len(), 1);
        assert_eq!(unblocked[0].id, dependent.id);
    }

    #[tokio::test]
    async fn unblock_only_activates_blocked_dependents() {
        let (store, _bus) = store();
        let (tenant, principal) = (TenantId::new(), PrincipalId::new());
        let blocker = store.create(new_task(tenant, principal, "blocker")).await.expect("create");
        let done = store.create(new_task(tenant, principal, "done")).await.expect("create");
        store.add_dependencies(principal, done.id, &[blocker.id]).await.expect("add");
        // The dependent already completed -- it must NOT be resurrected to active.
        store
            .update(
                principal,
                done.id,
                TaskPatch { status: Some(TaskStatus::Completed), ..Default::default() },
            )
            .await
            .expect("complete dependent");
        assert!(store.check_and_unblock(principal, blocker.id).await.expect("check").is_empty());
        let got = store.get(principal, done.id).await.expect("get").expect("present");
        assert_eq!(got.status, TaskStatus::Completed);

        // And the completed task itself must be owned: a foreign principal is NotFound.
        let err = store
            .check_and_unblock(PrincipalId::new(), blocker.id)
            .await
            .expect_err("foreign completed task");
        assert!(matches!(err, ChiasmError::NotFound(_)));
    }
}
