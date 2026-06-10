//! The SQLite-backed Chiasm task store.
//!
//! Reimplements the Kleos chiasm task surface (`kleos-lib/src/services/chiasm/tasks.rs`) against
//! the Henosis substrate: ownership is a [`PrincipalId`] (every read/write scopes on it, replacing
//! the Kleos `WHERE user_id = ?` predicate), lifecycle events are typed and published to the
//! in-process [`AxonBus`], and schema is managed by the kernel-crate migration convention
//! (`PRAGMA user_version` + `migrations/Vn__*.sql`). Concurrency: one `Connection` behind a
//! `Mutex` (a pool can replace it later without changing this surface).
//!
//! Slice 1 covers task CRUD, history, and stats. Queue/claim, heartbeat/stale, path claims,
//! dependencies, and the legacy `user_id -> PrincipalId` backfill land in later slices.

use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};

use rusqlite::{Connection, OptionalExtension};
use syntheos_axon::AxonBus;
use syntheos_contracts::{PrincipalId, TaskId, TenantId, Timestamp, TypedEvent};

use crate::error::ChiasmError;
use crate::events::{
    TaskClaimed, TaskCompleted, TaskCreated, TaskDeleted, TaskQueued, TaskStale, TaskUpdated,
};
use crate::model::{
    ChiasmStats, EnqueueTask, NewTask, Task, TaskFilter, TaskPatch, TaskStatus, TaskUpdate,
};

/// Ordered schema migrations, applied by `PRAGMA user_version`. Append-only (see the DB convention).
const MIGRATIONS: &[(i64, &str)] = &[(1, include_str!("../migrations/V1__chiasm_tasks.sql"))];

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

    /// Record a liveness heartbeat for an owned task, refreshing `last_heartbeat`. Returns
    /// [`ChiasmError::NotFound`] if the task does not exist or is owned by another principal.
    ///
    /// (Path-claim expiry refresh, which Kleos folds in here, lands with the claims slice.)
    pub async fn record_heartbeat(
        &self,
        principal: PrincipalId,
        id: TaskId,
    ) -> Result<(), ChiasmError> {
        let now = ts_to_db(&Timestamp::now())?;
        let updated = {
            let conn = self.lock();
            conn.execute(
                "UPDATE chiasm_tasks SET last_heartbeat = ?1, updated_at = ?1 \
                 WHERE id = ?2 AND principal_id = ?3",
                rusqlite::params![now, id.to_string(), principal.to_string()],
            )
            .map_err(berr)?
        };
        if updated == 0 {
            return Err(ChiasmError::NotFound(id));
        }
        Ok(())
    }

    /// System-wide sweep (NOT owner-scoped -- a maintenance task) that marks every active/paused
    /// task whose heartbeat is overdue as [`TaskStatus::Stale`], appends a history row, and emits
    /// `task.stale`. A task is overdue when `now - last_heartbeat > heartbeat_interval_secs *
    /// grace_multiplier`. Returns the tasks it staled.
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
            {
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
                tx.commit().map_err(berr)?;
            }
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
            staled.push(task);
        }
        Ok(staled)
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
}
