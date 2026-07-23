//! The SQLite-backed Loom workflow store and its dependency-driven step engine.
//!
//! Workflows and runs are owner-scoped on [`PrincipalId`], use [`WorkflowId`]/[`RunId`] (UUID
//! v8), and publish typed lifecycle events on the in-process [`AxonBus`]. The versioned SQLite
//! schema uses one `Connection` behind a `Mutex`.
//!
//! The engine (`advance_run`) advances a run by starting every pending step whose dependencies
//! are all completed, handing it the run input overlaid with its
//! dependency outputs; a run completes when no step is pending or running, merging completed
//! outputs. Execution is delegated through the [`StepExecutor`] seam: the attached executor
//! runs the types it claims inline (the built-in [`crate::TransformExecutor`] covers pure-JSON
//! steps); every other started step waits for external completion via [`LoomStore::complete_step`].
//!
//! Definitions are validated as DAGs at write time, `create_run` advances immediately, and
//! [`LoomStore::sweep_timeouts`] enforces `timeout_ms`.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard};

use rusqlite::{Connection, OptionalExtension};
use syntheos_axon::AxonBus;
use syntheos_contracts::{PrincipalId, RunId, TenantId, Timestamp, TypedEvent, WorkflowId};

use crate::error::LoomError;
use crate::events::{
    RunCancelled, RunCompleted, RunCreated, RunFailed, StepCompleted, StepFailed, StepStarted,
};
use crate::executor::{StepContext, StepExecutor};
use crate::model::{
    LogEntry, LogLevel, LoomStats, NewWorkflow, Run, RunFilter, RunStatus, Step, StepDef,
    StepStatus, StepType, Workflow, WorkflowPatch,
};

/// Ordered schema migrations, applied by `PRAGMA user_version`. Append-only (see the DB convention).
const MIGRATIONS: &[(i64, &str)] = &[(1, include_str!("../migrations/V1__loom_workflows.sql"))];

/// The columns of `loom_workflows`, in the order [`read_raw_workflow`] reads them.
const WORKFLOW_COLUMNS: &str =
    "id, tenant, principal_id, name, description, steps, created_at, updated_at";

/// The columns of `loom_runs`, in the order [`read_raw_run`] reads them.
const RUN_COLUMNS: &str = "id, workflow_id, tenant, principal_id, status, input, output, error, \
    started_at, completed_at, created_at, updated_at";

/// The columns of `loom_steps`, in the order [`read_raw_step`] reads them.
const STEP_COLUMNS: &str = "id, run_id, name, step_type, config, status, input, output, error, \
    depends_on, retry_count, max_retries, timeout_ms, started_at, completed_at, created_at";

/// The workflow store + step engine.
///
/// Share it as `Arc<LoomStore>`; all methods take `&self`.
pub struct LoomStore {
    /// The one connection, serialized by a `Mutex` (rusqlite `Connection` is `Send`, not `Sync`).
    conn: Mutex<Connection>,
    /// The bus workflow lifecycle events are published onto.
    bus: Arc<AxonBus>,
    /// The optional inline executor seam. `None` = every step waits for external completion.
    executor: Option<Box<dyn StepExecutor>>,
}

/// Map a generic rusqlite error to an opaque backend error.
fn berr(e: rusqlite::Error) -> LoomError {
    LoomError::Backend(e.to_string())
}

/// Serialize a [`Timestamp`] to its stored RFC3339-UTC string (via the contracts wire form).
fn ts_to_db(ts: &Timestamp) -> Result<String, LoomError> {
    serde_json::to_value(ts)
        .ok()
        .and_then(|v| v.as_str().map(String::from))
        .ok_or_else(|| LoomError::Backend("timestamp serialize".to_string()))
}

/// Parse a stored RFC3339 string back into a UTC-normalized [`Timestamp`].
fn ts_from_db(s: &str) -> Result<Timestamp, LoomError> {
    serde_json::from_value(serde_json::Value::String(s.to_string()))
        .map_err(|e| LoomError::Backend(format!("timestamp parse {s:?}: {e}")))
}

/// Validate a workflow definition: unique step names, dependencies that exist, and no cycles.
/// Caught at write time so a run can never deadlock on an unsatisfiable graph.
fn validate_steps(steps: &[StepDef]) -> Result<(), LoomError> {
    let mut names = HashSet::new();
    for step in steps {
        if step.name.trim().is_empty() {
            return Err(LoomError::InvalidDefinition("empty step name".to_string()));
        }
        if !names.insert(step.name.as_str()) {
            return Err(LoomError::InvalidDefinition(format!(
                "duplicate step name {:?}",
                step.name
            )));
        }
    }
    for step in steps {
        for dep in step.depends_on.iter().flatten() {
            if !names.contains(dep.as_str()) {
                return Err(LoomError::InvalidDefinition(format!(
                    "step {:?} depends on unknown step {:?}",
                    step.name, dep
                )));
            }
        }
    }
    // Kahn's algorithm: if a topological order cannot consume every step, there is a cycle.
    let mut indegree: HashMap<&str, usize> = steps.iter().map(|s| (s.name.as_str(), 0)).collect();
    for step in steps {
        for _ in step.depends_on.iter().flatten() {
            *indegree.get_mut(step.name.as_str()).expect("known name") += 1;
        }
    }
    let mut queue: Vec<&str> = indegree
        .iter()
        .filter(|(_, d)| **d == 0)
        .map(|(n, _)| *n)
        .collect();
    let mut consumed = 0;
    while let Some(done) = queue.pop() {
        consumed += 1;
        for step in steps {
            if step.depends_on.iter().flatten().any(|d| d == done) {
                let d = indegree.get_mut(step.name.as_str()).expect("known name");
                *d -= 1;
                if *d == 0 {
                    queue.push(step.name.as_str());
                }
            }
        }
    }
    if consumed != steps.len() {
        return Err(LoomError::InvalidDefinition(
            "dependency cycle in step graph".to_string(),
        ));
    }
    Ok(())
}

/// The raw column values of one `loom_workflows` row.
struct RawWorkflow {
    /// WorkflowId string.
    id: String,
    /// TenantId string.
    tenant: String,
    /// Owner PrincipalId string.
    principal_id: String,
    /// Workflow name.
    name: String,
    /// Optional description.
    description: Option<String>,
    /// StepDef JSON array text.
    steps: String,
    /// Creation time (RFC3339).
    created_at: String,
    /// Last-modification time (RFC3339).
    updated_at: String,
}

/// Read a `loom_workflows` row positionally (column order = [`WORKFLOW_COLUMNS`]).
fn read_raw_workflow(row: &rusqlite::Row) -> rusqlite::Result<RawWorkflow> {
    Ok(RawWorkflow {
        id: row.get(0)?,
        tenant: row.get(1)?,
        principal_id: row.get(2)?,
        name: row.get(3)?,
        description: row.get(4)?,
        steps: row.get(5)?,
        created_at: row.get(6)?,
        updated_at: row.get(7)?,
    })
}

/// Methods for `RawWorkflow`.
impl RawWorkflow {
    /// Parse raw columns into a typed [`Workflow`].
    fn into_workflow(self) -> Result<Workflow, LoomError> {
        Ok(Workflow {
            id: self.id.parse::<WorkflowId>().map_err(|e| {
                LoomError::Backend(format!("corrupt workflow id {:?}: {e}", self.id))
            })?,
            tenant: self.tenant.parse::<TenantId>().map_err(|e| {
                LoomError::Backend(format!("corrupt tenant {:?}: {e}", self.tenant))
            })?,
            principal_id: self.principal_id.parse::<PrincipalId>().map_err(|e| {
                LoomError::Backend(format!("corrupt principal_id {:?}: {e}", self.principal_id))
            })?,
            name: self.name,
            description: self.description,
            steps: serde_json::from_str(&self.steps)
                .map_err(|e| LoomError::Backend(format!("corrupt steps {:?}: {e}", self.steps)))?,
            created_at: ts_from_db(&self.created_at)?,
            updated_at: ts_from_db(&self.updated_at)?,
        })
    }
}

/// The raw column values of one `loom_runs` row.
struct RawRun {
    /// RunId string.
    id: String,
    /// WorkflowId string.
    workflow_id: String,
    /// TenantId string.
    tenant: String,
    /// Owner PrincipalId string.
    principal_id: String,
    /// RunStatus token.
    status: String,
    /// Input JSON text.
    input: String,
    /// Output JSON text.
    output: String,
    /// Failure reason.
    error: Option<String>,
    /// Start time (RFC3339).
    started_at: Option<String>,
    /// Terminal time (RFC3339).
    completed_at: Option<String>,
    /// Creation time (RFC3339).
    created_at: String,
    /// Last-modification time (RFC3339).
    updated_at: String,
}

/// Read a `loom_runs` row positionally (column order = [`RUN_COLUMNS`]).
fn read_raw_run(row: &rusqlite::Row) -> rusqlite::Result<RawRun> {
    Ok(RawRun {
        id: row.get(0)?,
        workflow_id: row.get(1)?,
        tenant: row.get(2)?,
        principal_id: row.get(3)?,
        status: row.get(4)?,
        input: row.get(5)?,
        output: row.get(6)?,
        error: row.get(7)?,
        started_at: row.get(8)?,
        completed_at: row.get(9)?,
        created_at: row.get(10)?,
        updated_at: row.get(11)?,
    })
}

/// Methods for `RawRun`.
impl RawRun {
    /// Parse raw columns into a typed [`Run`].
    fn into_run(self) -> Result<Run, LoomError> {
        Ok(Run {
            id: self
                .id
                .parse::<RunId>()
                .map_err(|e| LoomError::Backend(format!("corrupt run id {:?}: {e}", self.id)))?,
            workflow_id: self.workflow_id.parse::<WorkflowId>().map_err(|e| {
                LoomError::Backend(format!("corrupt workflow_id {:?}: {e}", self.workflow_id))
            })?,
            tenant: self.tenant.parse::<TenantId>().map_err(|e| {
                LoomError::Backend(format!("corrupt tenant {:?}: {e}", self.tenant))
            })?,
            principal_id: self.principal_id.parse::<PrincipalId>().map_err(|e| {
                LoomError::Backend(format!("corrupt principal_id {:?}: {e}", self.principal_id))
            })?,
            status: RunStatus::parse(&self.status)?,
            input: serde_json::from_str(&self.input)
                .map_err(|e| LoomError::Backend(format!("corrupt input: {e}")))?,
            output: serde_json::from_str(&self.output)
                .map_err(|e| LoomError::Backend(format!("corrupt output: {e}")))?,
            error: self.error,
            started_at: self.started_at.as_deref().map(ts_from_db).transpose()?,
            completed_at: self.completed_at.as_deref().map(ts_from_db).transpose()?,
            created_at: ts_from_db(&self.created_at)?,
            updated_at: ts_from_db(&self.updated_at)?,
        })
    }
}

/// The raw column values of one `loom_steps` row.
struct RawStep {
    /// Step id.
    id: i64,
    /// RunId string.
    run_id: String,
    /// Step name.
    name: String,
    /// StepType token.
    step_type: String,
    /// Config JSON text.
    config: String,
    /// StepStatus token.
    status: String,
    /// Input JSON text.
    input: String,
    /// Output JSON text.
    output: String,
    /// Last failure message.
    error: Option<String>,
    /// Dependency-name JSON array text.
    depends_on: String,
    /// Retry count so far.
    retry_count: i32,
    /// Retry budget.
    max_retries: i32,
    /// Per-attempt timeout (ms).
    timeout_ms: i64,
    /// Attempt start time (RFC3339).
    started_at: Option<String>,
    /// Terminal time (RFC3339).
    completed_at: Option<String>,
    /// Creation time (RFC3339).
    created_at: String,
}

/// Read a `loom_steps` row positionally (column order = [`STEP_COLUMNS`]).
fn read_raw_step(row: &rusqlite::Row) -> rusqlite::Result<RawStep> {
    Ok(RawStep {
        id: row.get(0)?,
        run_id: row.get(1)?,
        name: row.get(2)?,
        step_type: row.get(3)?,
        config: row.get(4)?,
        status: row.get(5)?,
        input: row.get(6)?,
        output: row.get(7)?,
        error: row.get(8)?,
        depends_on: row.get(9)?,
        retry_count: row.get(10)?,
        max_retries: row.get(11)?,
        timeout_ms: row.get(12)?,
        started_at: row.get(13)?,
        completed_at: row.get(14)?,
        created_at: row.get(15)?,
    })
}

/// Methods for `RawStep`.
impl RawStep {
    /// Parse raw columns into a typed [`Step`].
    fn into_step(self) -> Result<Step, LoomError> {
        Ok(Step {
            id: self.id,
            run_id: self.run_id.parse::<RunId>().map_err(|e| {
                LoomError::Backend(format!("corrupt run_id {:?}: {e}", self.run_id))
            })?,
            name: self.name,
            step_type: StepType::parse(&self.step_type)?,
            config: serde_json::from_str(&self.config)
                .map_err(|e| LoomError::Backend(format!("corrupt config: {e}")))?,
            status: StepStatus::parse(&self.status)?,
            input: serde_json::from_str(&self.input)
                .map_err(|e| LoomError::Backend(format!("corrupt input: {e}")))?,
            output: serde_json::from_str(&self.output)
                .map_err(|e| LoomError::Backend(format!("corrupt output: {e}")))?,
            error: self.error,
            depends_on: serde_json::from_str(&self.depends_on)
                .map_err(|e| LoomError::Backend(format!("corrupt depends_on: {e}")))?,
            retry_count: self.retry_count,
            max_retries: self.max_retries,
            timeout_ms: self.timeout_ms,
            started_at: self.started_at.as_deref().map(ts_from_db).transpose()?,
            completed_at: self.completed_at.as_deref().map(ts_from_db).transpose()?,
            created_at: ts_from_db(&self.created_at)?,
        })
    }
}

/// Methods for `LoomStore`.
impl LoomStore {
    /// Open (creating the file if absent) a store at `path`, applying any pending migrations.
    /// No executor is attached; see [`Self::with_executor`].
    pub fn open(path: impl AsRef<Path>, bus: Arc<AxonBus>) -> Result<Self, LoomError> {
        let conn = Connection::open(path).map_err(berr)?;
        Self::from_conn(conn, bus)
    }

    /// Open an ephemeral in-memory store. For tests and throwaway use.
    pub fn open_in_memory(bus: Arc<AxonBus>) -> Result<Self, LoomError> {
        let conn = Connection::open_in_memory().map_err(berr)?;
        Self::from_conn(conn, bus)
    }

    /// Attach a [`StepExecutor`] that runs the step types it claims inline during advance
    /// passes. Builder-style, used at server wiring time.
    pub fn with_executor(mut self, executor: Box<dyn StepExecutor>) -> Self {
        self.executor = Some(executor);
        self
    }

    /// Enable foreign keys, apply migrations, and wrap the connection.
    fn from_conn(mut conn: Connection, bus: Arc<AxonBus>) -> Result<Self, LoomError> {
        conn.pragma_update(None, "foreign_keys", true)
            .map_err(berr)?;
        apply_migrations(&mut conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
            bus,
            executor: None,
        })
    }

    /// Lock the connection, recovering from a poisoned mutex.
    fn lock(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Publish a workflow event, fire-and-forget. A publish failure is logged, never fatal.
    fn emit<E: TypedEvent>(&self, event: &E, tenant: TenantId, principal: PrincipalId) {
        if let Err(e) = self.bus.publish_event(event, tenant, principal) {
            tracing::warn!(error = %e, kind = E::KIND, "failed to publish loom workflow event");
        }
    }

    /// Append a run log line (internal; failures are real errors -- the log is the record).
    fn add_log(
        conn: &Connection,
        run_id: RunId,
        step_id: Option<i64>,
        level: LogLevel,
        message: &str,
        data: Option<serde_json::Value>,
    ) -> Result<(), LoomError> {
        conn.execute(
            "INSERT INTO loom_logs (run_id, step_id, level, message, data, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![
                run_id.to_string(),
                step_id,
                level.as_str(),
                message,
                data.unwrap_or_else(|| serde_json::json!({})).to_string(),
                ts_to_db(&Timestamp::now())?,
            ],
        )
        .map_err(berr)?;
        Ok(())
    }

    /// Define a new workflow (validated DAG) owned by `new.principal_id`.
    pub async fn create_workflow(&self, new: NewWorkflow) -> Result<Workflow, LoomError> {
        if new.name.trim().is_empty() {
            return Err(LoomError::InvalidInput(
                "workflow name required".to_string(),
            ));
        }
        validate_steps(&new.steps)?;
        let now = Timestamp::now();
        let workflow = Workflow {
            id: WorkflowId::new(),
            tenant: new.tenant,
            principal_id: new.principal_id,
            name: new.name,
            description: new.description,
            steps: new.steps,
            created_at: now,
            updated_at: now,
        };
        let conn = self.lock();
        conn.execute(
            "INSERT INTO loom_workflows \
             (id, tenant, principal_id, name, description, steps, created_at, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7)",
            rusqlite::params![
                workflow.id.to_string(),
                workflow.tenant.to_string(),
                workflow.principal_id.to_string(),
                &workflow.name,
                &workflow.description,
                serde_json::to_string(&workflow.steps)
                    .map_err(|e| LoomError::Backend(format!("steps serialize: {e}")))?,
                ts_to_db(&now)?,
            ],
        )
        .map_err(|e| match &e {
            rusqlite::Error::SqliteFailure(f, Some(msg))
                if f.code == rusqlite::ErrorCode::ConstraintViolation
                    && msg.contains("loom_workflows.name") =>
            {
                LoomError::InvalidInput(format!(
                    "workflow name already exists: {:?}",
                    workflow.name
                ))
            }
            _ => berr(e),
        })?;
        Ok(workflow)
    }

    /// Look up an owned workflow by id. `Ok(None)` if absent or owned by another principal.
    pub async fn get_workflow(
        &self,
        principal: PrincipalId,
        id: WorkflowId,
    ) -> Result<Option<Workflow>, LoomError> {
        let conn = self.lock();
        Self::get_workflow_in(&conn, principal, id)
    }

    /// Owner-scoped workflow lookup against an arbitrary connection.
    fn get_workflow_in(
        conn: &Connection,
        principal: PrincipalId,
        id: WorkflowId,
    ) -> Result<Option<Workflow>, LoomError> {
        let raw = conn
            .query_row(
                &format!(
                    "SELECT {WORKFLOW_COLUMNS} FROM loom_workflows \
                     WHERE id = ?1 AND principal_id = ?2"
                ),
                rusqlite::params![id.to_string(), principal.to_string()],
                read_raw_workflow,
            )
            .optional()
            .map_err(berr)?;
        raw.map(RawWorkflow::into_workflow).transpose()
    }

    /// Look up an owned workflow by its per-owner-unique name.
    pub async fn get_workflow_by_name(
        &self,
        principal: PrincipalId,
        name: &str,
    ) -> Result<Option<Workflow>, LoomError> {
        let conn = self.lock();
        let raw = conn
            .query_row(
                &format!(
                    "SELECT {WORKFLOW_COLUMNS} FROM loom_workflows \
                     WHERE principal_id = ?1 AND name = ?2"
                ),
                rusqlite::params![principal.to_string(), name],
                read_raw_workflow,
            )
            .optional()
            .map_err(berr)?;
        raw.map(RawWorkflow::into_workflow).transpose()
    }

    /// List a principal's workflows, newest-updated first.
    pub async fn list_workflows(&self, principal: PrincipalId) -> Result<Vec<Workflow>, LoomError> {
        let conn = self.lock();
        let mut stmt = conn
            .prepare(&format!(
                "SELECT {WORKFLOW_COLUMNS} FROM loom_workflows \
                 WHERE principal_id = ?1 ORDER BY updated_at DESC"
            ))
            .map_err(berr)?;
        let rows = stmt
            .query_map(rusqlite::params![principal.to_string()], read_raw_workflow)
            .map_err(berr)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(berr)?.into_workflow()?);
        }
        Ok(out)
    }

    /// Apply a partial update to an owned workflow (replacement steps are re-validated).
    /// Existing runs keep the step instances they were created with.
    pub async fn update_workflow(
        &self,
        principal: PrincipalId,
        id: WorkflowId,
        patch: WorkflowPatch,
    ) -> Result<Workflow, LoomError> {
        if let Some(steps) = &patch.steps {
            validate_steps(steps)?;
        }
        let conn = self.lock();
        let mut workflow =
            Self::get_workflow_in(&conn, principal, id)?.ok_or(LoomError::WorkflowNotFound(id))?;
        if let Some(name) = patch.name {
            workflow.name = name;
        }
        if let Some(description) = patch.description {
            workflow.description = Some(description);
        }
        if let Some(steps) = patch.steps {
            workflow.steps = steps;
        }
        workflow.updated_at = Timestamp::now();
        conn.execute(
            "UPDATE loom_workflows SET name = ?1, description = ?2, steps = ?3, updated_at = ?4 \
             WHERE id = ?5 AND principal_id = ?6",
            rusqlite::params![
                &workflow.name,
                &workflow.description,
                serde_json::to_string(&workflow.steps)
                    .map_err(|e| LoomError::Backend(format!("steps serialize: {e}")))?,
                ts_to_db(&workflow.updated_at)?,
                id.to_string(),
                principal.to_string(),
            ],
        )
        .map_err(berr)?;
        Ok(workflow)
    }

    /// Delete an owned workflow (its runs, steps, and logs cascade). Returns whether a row was
    /// removed.
    pub async fn delete_workflow(
        &self,
        principal: PrincipalId,
        id: WorkflowId,
    ) -> Result<bool, LoomError> {
        let conn = self.lock();
        let n = conn
            .execute(
                "DELETE FROM loom_workflows WHERE id = ?1 AND principal_id = ?2",
                rusqlite::params![id.to_string(), principal.to_string()],
            )
            .map_err(berr)?;
        Ok(n > 0)
    }

    /// Start a run of an owned workflow: instantiate its steps, emit `workflow.run.created`,
    /// and advance immediately (a deviation from Kleos, which left runs pending until an
    /// external nudge -- a self-starting engine is what "the step graph runs" means here).
    /// A workflow with no steps cannot run.
    pub async fn create_run(
        &self,
        principal: PrincipalId,
        workflow_id: WorkflowId,
        input: Option<serde_json::Value>,
    ) -> Result<Run, LoomError> {
        let input = input.unwrap_or_else(|| serde_json::json!({}));
        if !input.is_object() {
            return Err(LoomError::InvalidInput(
                "input must be a JSON object".to_string(),
            ));
        }
        let now = Timestamp::now();
        let (run, workflow_name) = {
            let mut conn = self.lock();
            let tx = conn.transaction().map_err(berr)?;
            let workflow = Self::get_workflow_in(&tx, principal, workflow_id)?
                .ok_or(LoomError::WorkflowNotFound(workflow_id))?;
            if workflow.steps.is_empty() {
                return Err(LoomError::InvalidInput("workflow has no steps".to_string()));
            }
            let run = Run {
                id: RunId::new(),
                workflow_id,
                tenant: workflow.tenant,
                principal_id: principal,
                status: RunStatus::Pending,
                input: input.clone(),
                output: serde_json::json!({}),
                error: None,
                started_at: None,
                completed_at: None,
                created_at: now,
                updated_at: now,
            };
            tx.execute(
                "INSERT INTO loom_runs \
                 (id, workflow_id, tenant, principal_id, status, input, output, created_at, updated_at) \
                 VALUES (?1, ?2, ?3, ?4, 'pending', ?5, '{}', ?6, ?6)",
                rusqlite::params![
                    run.id.to_string(),
                    workflow_id.to_string(),
                    run.tenant.to_string(),
                    principal.to_string(),
                    input.to_string(),
                    ts_to_db(&now)?,
                ],
            )
            .map_err(berr)?;
            for def in &workflow.steps {
                tx.execute(
                    "INSERT INTO loom_steps \
                     (run_id, name, step_type, config, status, input, output, depends_on, \
                      retry_count, max_retries, timeout_ms, created_at) \
                     VALUES (?1, ?2, ?3, ?4, 'pending', '{}', '{}', ?5, 0, ?6, ?7, ?8)",
                    rusqlite::params![
                        run.id.to_string(),
                        &def.name,
                        def.step_type.as_str(),
                        def.config
                            .clone()
                            .unwrap_or_else(|| serde_json::json!({}))
                            .to_string(),
                        serde_json::to_string(&def.depends_on.clone().unwrap_or_default())
                            .map_err(|e| LoomError::Backend(format!("deps serialize: {e}")))?,
                        def.max_retries.unwrap_or(3),
                        def.timeout_ms.unwrap_or(30_000),
                        ts_to_db(&now)?,
                    ],
                )
                .map_err(berr)?;
            }
            Self::add_log(
                &tx,
                run.id,
                None,
                LogLevel::Info,
                &format!("run created for workflow {:?}", workflow.name),
                None,
            )?;
            tx.commit().map_err(berr)?;
            (run, workflow.name)
        };
        self.emit(
            &RunCreated {
                run_id: run.id.to_string(),
                workflow_id: workflow_id.to_string(),
                workflow: workflow_name,
            },
            run.tenant,
            run.principal_id,
        );
        self.advance_boxed(run.id).await?;
        // Re-read: the advance pass may have started (or even completed) the run.
        self.get_run(principal, run.id)
            .await?
            .ok_or(LoomError::RunNotFound(run.id))
    }

    /// Look up an owned run by id. `Ok(None)` if absent or owned by another principal.
    pub async fn get_run(
        &self,
        principal: PrincipalId,
        id: RunId,
    ) -> Result<Option<Run>, LoomError> {
        let conn = self.lock();
        let raw = conn
            .query_row(
                &format!("SELECT {RUN_COLUMNS} FROM loom_runs WHERE id = ?1 AND principal_id = ?2"),
                rusqlite::params![id.to_string(), principal.to_string()],
                read_raw_run,
            )
            .optional()
            .map_err(berr)?;
        raw.map(RawRun::into_run).transpose()
    }

    /// Unscoped run lookup for engine internals (callers have already authorized).
    fn get_run_any(conn: &Connection, id: RunId) -> Result<Option<Run>, LoomError> {
        let raw = conn
            .query_row(
                &format!("SELECT {RUN_COLUMNS} FROM loom_runs WHERE id = ?1"),
                rusqlite::params![id.to_string()],
                read_raw_run,
            )
            .optional()
            .map_err(berr)?;
        raw.map(RawRun::into_run).transpose()
    }

    /// List a principal's runs, newest first, AND-filtered by [`RunFilter`].
    pub async fn list_runs(
        &self,
        principal: PrincipalId,
        filter: RunFilter,
    ) -> Result<Vec<Run>, LoomError> {
        let mut sql = format!("SELECT {RUN_COLUMNS} FROM loom_runs WHERE principal_id = ?1");
        let mut args: Vec<rusqlite::types::Value> = vec![principal.to_string().into()];
        let mut n = 1;
        if let Some(workflow_id) = &filter.workflow_id {
            n += 1;
            sql.push_str(&format!(" AND workflow_id = ?{n}"));
            args.push(workflow_id.to_string().into());
        }
        if let Some(status) = &filter.status {
            n += 1;
            sql.push_str(&format!(" AND status = ?{n}"));
            args.push(status.as_str().to_string().into());
        }
        sql.push_str(" ORDER BY created_at DESC");
        match (filter.limit, filter.offset) {
            (Some(l), Some(o)) => sql.push_str(&format!(" LIMIT {l} OFFSET {o}")),
            (Some(l), None) => sql.push_str(&format!(" LIMIT {l}")),
            (None, Some(o)) => sql.push_str(&format!(" LIMIT -1 OFFSET {o}")),
            (None, None) => {}
        }
        let conn = self.lock();
        let mut stmt = conn.prepare(&sql).map_err(berr)?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(args.iter()), read_raw_run)
            .map_err(berr)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(berr)?.into_run()?);
        }
        Ok(out)
    }

    /// Cancel an owned, non-terminal run: pending/running steps become `skipped`, the run
    /// becomes `cancelled`, and `workflow.run.cancelled` is emitted. Returns whether anything
    /// was cancelled (a terminal run returns `false`).
    pub async fn cancel_run(&self, principal: PrincipalId, id: RunId) -> Result<bool, LoomError> {
        let run = {
            let mut conn = self.lock();
            let tx = conn.transaction().map_err(berr)?;
            let run = Self::get_run_any(&tx, id)?
                .filter(|r| r.principal_id == principal)
                .ok_or(LoomError::RunNotFound(id))?;
            if run.status.is_terminal() {
                return Ok(false);
            }
            let now = ts_to_db(&Timestamp::now())?;
            tx.execute(
                "UPDATE loom_runs SET status = 'cancelled', completed_at = ?2, updated_at = ?2 \
                 WHERE id = ?1",
                rusqlite::params![id.to_string(), now],
            )
            .map_err(berr)?;
            tx.execute(
                "UPDATE loom_steps SET status = 'skipped' \
                 WHERE run_id = ?1 AND status IN ('pending', 'running')",
                rusqlite::params![id.to_string()],
            )
            .map_err(berr)?;
            Self::add_log(&tx, id, None, LogLevel::Info, "run cancelled", None)?;
            tx.commit().map_err(berr)?;
            run
        };
        self.emit(
            &RunCancelled {
                run_id: id.to_string(),
            },
            run.tenant,
            run.principal_id,
        );
        Ok(true)
    }

    /// List a run's steps in definition order, owner-scoped (another principal's run yields an
    /// empty list).
    pub async fn get_steps(
        &self,
        principal: PrincipalId,
        run_id: RunId,
    ) -> Result<Vec<Step>, LoomError> {
        let conn = self.lock();
        let mut stmt = conn
            .prepare(&format!(
                "SELECT {STEP_COLUMNS} FROM loom_steps s WHERE s.run_id = ?1 \
                 AND EXISTS (SELECT 1 FROM loom_runs r \
                             WHERE r.id = s.run_id AND r.principal_id = ?2) \
                 ORDER BY s.id ASC"
            ))
            .map_err(berr)?;
        let rows = stmt
            .query_map(
                rusqlite::params![run_id.to_string(), principal.to_string()],
                read_raw_step,
            )
            .map_err(berr)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(berr)?.into_step()?);
        }
        Ok(out)
    }

    /// Unscoped step + owning run lookup for the mutate-by-id paths.
    fn get_step_with_run(
        conn: &Connection,
        step_id: i64,
    ) -> Result<Option<(Step, Run)>, LoomError> {
        let raw = conn
            .query_row(
                &format!("SELECT {STEP_COLUMNS} FROM loom_steps WHERE id = ?1"),
                rusqlite::params![step_id],
                read_raw_step,
            )
            .optional()
            .map_err(berr)?;
        let Some(step) = raw.map(RawStep::into_step).transpose()? else {
            return Ok(None);
        };
        let run = Self::get_run_any(conn, step.run_id)?
            .ok_or_else(|| LoomError::Backend(format!("step {step_id} has no run")))?;
        Ok(Some((step, run)))
    }

    /// Fetch one step by id, owner-scoped through its run.
    pub async fn get_step(
        &self,
        principal: PrincipalId,
        step_id: i64,
    ) -> Result<Option<Step>, LoomError> {
        let conn = self.lock();
        Ok(Self::get_step_with_run(&conn, step_id)?
            .filter(|(_, run)| run.principal_id == principal)
            .map(|(step, _)| step))
    }

    /// Complete a running step with `output` and advance the run. The external-completion path
    /// for action/decision/parallel/wait steps (and anything the executor does not claim).
    /// Owner-scoped; the step must currently be `running`.
    pub async fn complete_step(
        &self,
        principal: PrincipalId,
        step_id: i64,
        output: serde_json::Value,
    ) -> Result<Step, LoomError> {
        // The guard must leave scope via the block (an explicit drop is not enough for the
        // future's Send analysis) before any await.
        let (step, run) = {
            let conn = self.lock();
            Self::get_step_with_run(&conn, step_id)?
                .filter(|(_, run)| run.principal_id == principal)
                .ok_or(LoomError::StepNotFound(step_id))?
        };
        self.complete_step_inner(&step, &run, output).await?;
        self.advance_boxed(step.run_id).await?;
        self.read_step(step_id)
    }

    /// Fail a running step's attempt and advance the run (retry semantics apply). Owner-scoped.
    pub async fn fail_step(
        &self,
        principal: PrincipalId,
        step_id: i64,
        error: &str,
    ) -> Result<Step, LoomError> {
        let (step, run) = {
            let conn = self.lock();
            Self::get_step_with_run(&conn, step_id)?
                .filter(|(_, run)| run.principal_id == principal)
                .ok_or(LoomError::StepNotFound(step_id))?
        };
        self.fail_step_inner(&step, &run, error).await?;
        self.read_step(step_id)
    }

    /// Re-read one step by id (engine-internal, post-mutation).
    fn read_step(&self, step_id: i64) -> Result<Step, LoomError> {
        let conn = self.lock();
        Self::get_step_with_run(&conn, step_id)?
            .map(|(step, _)| step)
            .ok_or(LoomError::StepNotFound(step_id))
    }

    /// Mark a running step completed and log it. Does NOT advance -- callers do, so the inline
    /// execution loop controls recursion.
    async fn complete_step_inner(
        &self,
        step: &Step,
        run: &Run,
        output: serde_json::Value,
    ) -> Result<(), LoomError> {
        if step.status != StepStatus::Running {
            return Err(LoomError::InvalidInput(format!(
                "cannot complete step {}: status is {:?}",
                step.id,
                step.status.as_str()
            )));
        }
        {
            let conn = self.lock();
            // TOCTOU guard: `step` is a snapshot read before this call, so a
            // concurrent cancel_run (which sets running steps to 'skipped') can
            // land between the snapshot and this write. Scope the UPDATE to
            // `status = 'running'` so the DB enforces the precondition
            // atomically; if no row changed, the step is no longer running and
            // we must NOT force it to 'completed' or emit a completion event.
            let affected = conn
                .execute(
                    "UPDATE loom_steps SET status = 'completed', output = ?1, completed_at = ?2 \
                     WHERE id = ?3 AND status = 'running'",
                    rusqlite::params![output.to_string(), ts_to_db(&Timestamp::now())?, step.id],
                )
                .map_err(berr)?;
            if affected == 0 {
                return Err(LoomError::InvalidInput(format!(
                    "cannot complete step {}: it is no longer running \
                     (concurrently cancelled or changed)",
                    step.id
                )));
            }
            Self::add_log(
                &conn,
                step.run_id,
                Some(step.id),
                LogLevel::Info,
                &format!("step {:?} completed", step.name),
                None,
            )?;
        }
        self.emit(
            &StepCompleted {
                run_id: step.run_id.to_string(),
                step_id: step.id,
                step: step.name.clone(),
            },
            run.tenant,
            run.principal_id,
        );
        Ok(())
    }

    /// Fail a step attempt: within budget it resets to pending (and the run re-advances so it
    /// restarts); past budget the step and the run fail, emitting `workflow.run.failed`.
    async fn fail_step_inner(&self, step: &Step, run: &Run, error: &str) -> Result<(), LoomError> {
        let will_retry = step.retry_count < step.max_retries;
        {
            let conn = self.lock();
            if will_retry {
                // Same TOCTOU guard as complete_step_inner: only reset a step
                // that is still 'running', so a concurrent cancel is not undone.
                let affected = conn
                    .execute(
                        "UPDATE loom_steps SET status = 'pending', retry_count = retry_count + 1, \
                         error = ?1, started_at = NULL WHERE id = ?2 AND status = 'running'",
                        rusqlite::params![error, step.id],
                    )
                    .map_err(berr)?;
                if affected == 0 {
                    return Err(LoomError::InvalidInput(format!(
                        "cannot fail/retry step {}: it is no longer running \
                         (concurrently cancelled or changed)",
                        step.id
                    )));
                }
                Self::add_log(
                    &conn,
                    step.run_id,
                    Some(step.id),
                    LogLevel::Warn,
                    &format!(
                        "step {:?} failed, retrying ({}/{})",
                        step.name,
                        step.retry_count + 1,
                        step.max_retries
                    ),
                    Some(serde_json::json!({ "error": error })),
                )?;
            } else {
                let now = ts_to_db(&Timestamp::now())?;
                // Guard the step transition on 'running' first; only fail the
                // run if this step was actually the one still running (so a
                // concurrent cancel is not overwritten with 'failed').
                let affected = conn
                    .execute(
                        "UPDATE loom_steps SET status = 'failed', error = ?1, completed_at = ?2 \
                         WHERE id = ?3 AND status = 'running'",
                        rusqlite::params![error, now, step.id],
                    )
                    .map_err(berr)?;
                if affected == 0 {
                    return Err(LoomError::InvalidInput(format!(
                        "cannot fail step {}: it is no longer running \
                         (concurrently cancelled or changed)",
                        step.id
                    )));
                }
                conn.execute(
                    "UPDATE loom_runs SET status = 'failed', error = ?1, completed_at = ?2, \
                     updated_at = ?2 WHERE id = ?3",
                    rusqlite::params![error, now, step.run_id.to_string()],
                )
                .map_err(berr)?;
                Self::add_log(
                    &conn,
                    step.run_id,
                    Some(step.id),
                    LogLevel::Error,
                    &format!("step {:?} failed (max retries exhausted)", step.name),
                    Some(serde_json::json!({ "error": error })),
                )?;
            }
        }
        self.emit(
            &StepFailed {
                run_id: step.run_id.to_string(),
                step_id: step.id,
                step: step.name.clone(),
                error: error.to_string(),
                will_retry,
            },
            run.tenant,
            run.principal_id,
        );
        if will_retry {
            self.advance_boxed(step.run_id).await?;
        } else {
            self.emit(
                &RunFailed {
                    run_id: step.run_id.to_string(),
                    failed_step: step.name.clone(),
                    error: error.to_string(),
                },
                run.tenant,
                run.principal_id,
            );
        }
        Ok(())
    }

    /// Advance an owned run: the public nudge for externally driven graphs.
    pub async fn advance_run(
        &self,
        principal: PrincipalId,
        run_id: RunId,
    ) -> Result<(), LoomError> {
        {
            let conn = self.lock();
            Self::get_run_any(&conn, run_id)?
                .filter(|r| r.principal_id == principal)
                .ok_or(LoomError::RunNotFound(run_id))?;
        }
        self.advance_boxed(run_id).await
    }

    /// Boxed recursion point: advance -> inline execute -> complete/fail -> advance.
    fn advance_boxed(
        &self,
        run_id: RunId,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<(), LoomError>> + Send + '_>> {
        Box::pin(self.advance_inner(run_id))
    }

    /// One advance pass (the Kleos algorithm): start every pending step whose dependencies are
    /// all completed (input = run input overlaid with dependency outputs), run inline whatever
    /// the executor claims, and complete the run when no step is pending or running.
    async fn advance_inner(&self, run_id: RunId) -> Result<(), LoomError> {
        // Phase A (under the lock): read state, transition run/steps, collect ready steps.
        let (run, ready) = {
            let conn = self.lock();
            let Some(run) = Self::get_run_any(&conn, run_id)? else {
                return Err(LoomError::RunNotFound(run_id));
            };
            if run.status.is_terminal() {
                return Ok(());
            }
            if run.status == RunStatus::Pending {
                conn.execute(
                    "UPDATE loom_runs SET status = 'running', started_at = ?2, updated_at = ?2 \
                     WHERE id = ?1",
                    rusqlite::params![run_id.to_string(), ts_to_db(&Timestamp::now())?],
                )
                .map_err(berr)?;
            }
            let mut stmt = conn
                .prepare(&format!(
                    "SELECT {STEP_COLUMNS} FROM loom_steps WHERE run_id = ?1 ORDER BY id ASC"
                ))
                .map_err(berr)?;
            let rows = stmt
                .query_map(rusqlite::params![run_id.to_string()], read_raw_step)
                .map_err(berr)?;
            let mut steps = Vec::new();
            for row in rows {
                steps.push(row.map_err(berr)?.into_step()?);
            }
            drop(stmt);

            let by_name: HashMap<&str, &Step> =
                steps.iter().map(|s| (s.name.as_str(), s)).collect();
            let all_done = steps
                .iter()
                .all(|s| !matches!(s.status, StepStatus::Pending | StepStatus::Running));
            if all_done {
                // Merge completed outputs, later steps overwriting earlier keys (Kleos parity).
                let mut merged = serde_json::Map::new();
                for step in steps.iter().filter(|s| s.status == StepStatus::Completed) {
                    if let serde_json::Value::Object(map) = &step.output {
                        for (k, v) in map {
                            merged.insert(k.clone(), v.clone());
                        }
                    }
                }
                let now = ts_to_db(&Timestamp::now())?;
                conn.execute(
                    "UPDATE loom_runs SET status = 'completed', output = ?1, completed_at = ?2, \
                     updated_at = ?2 WHERE id = ?3",
                    rusqlite::params![
                        serde_json::Value::Object(merged).to_string(),
                        now,
                        run_id.to_string()
                    ],
                )
                .map_err(berr)?;
                Self::add_log(&conn, run_id, None, LogLevel::Info, "run completed", None)?;
                drop(conn);
                self.emit(
                    &RunCompleted {
                        run_id: run_id.to_string(),
                    },
                    run.tenant,
                    run.principal_id,
                );
                return Ok(());
            }

            // Ready = pending with every dependency completed.
            let mut ready: Vec<Step> = Vec::new();
            for step in &steps {
                if step.status != StepStatus::Pending {
                    continue;
                }
                let deps_met = step.depends_on.iter().all(|dep| {
                    by_name
                        .get(dep.as_str())
                        .is_some_and(|d| d.status == StepStatus::Completed)
                });
                if !deps_met {
                    continue;
                }
                // Input = run input overlaid with dependency outputs.
                let mut merged = serde_json::Map::new();
                if let serde_json::Value::Object(map) = &run.input {
                    for (k, v) in map {
                        merged.insert(k.clone(), v.clone());
                    }
                }
                for dep in &step.depends_on {
                    if let Some(dep_step) = by_name.get(dep.as_str()) {
                        if let serde_json::Value::Object(map) = &dep_step.output {
                            for (k, v) in map {
                                merged.insert(k.clone(), v.clone());
                            }
                        }
                    }
                }
                let mut started = step.clone();
                started.input = serde_json::Value::Object(merged);
                started.status = StepStatus::Running;
                conn.execute(
                    "UPDATE loom_steps SET status = 'running', input = ?1, started_at = ?2 \
                     WHERE id = ?3",
                    rusqlite::params![
                        started.input.to_string(),
                        ts_to_db(&Timestamp::now())?,
                        step.id
                    ],
                )
                .map_err(berr)?;
                Self::add_log(
                    &conn,
                    run_id,
                    Some(step.id),
                    LogLevel::Info,
                    &format!("step {:?} started", step.name),
                    None,
                )?;
                ready.push(started);
            }
            (run, ready)
        };

        // Phase B (no lock held): emit start events and run inline executions, which re-enter
        // complete/fail and recurse into the next advance pass.
        for step in ready {
            self.emit(
                &StepStarted {
                    run_id: run_id.to_string(),
                    step_id: step.id,
                    step: step.name.clone(),
                },
                run.tenant,
                run.principal_id,
            );
            let Some(executor) = &self.executor else {
                continue;
            };
            if !executor.handles(step.step_type) {
                continue;
            }
            let result = executor
                .execute(StepContext {
                    run_id,
                    step_id: step.id,
                    name: &step.name,
                    step_type: step.step_type,
                    config: &step.config,
                    input: &step.input,
                    timeout_ms: step.timeout_ms,
                })
                .await;
            match result {
                Ok(output) => {
                    self.complete_step_inner(&step, &run, output).await?;
                    self.advance_boxed(run_id).await?;
                }
                Err(message) => {
                    // fail_step_inner re-advances on retry itself.
                    self.fail_step_inner(&step, &run, &message).await?;
                }
            }
        }
        Ok(())
    }

    /// System-wide sweep (NOT owner-scoped -- a maintenance task) that fails every running
    /// step whose attempt has outlived its `timeout_ms`, with normal retry semantics. Kleos
    /// stored the column and never enforced it; this is the enforcement. The overdue
    /// comparison is computed in Rust (the established ns-RFC3339 rule). Returns the steps it
    /// timed out.
    pub async fn sweep_timeouts(&self) -> Result<Vec<Step>, LoomError> {
        let candidates: Vec<(Step, Run)> = {
            let conn = self.lock();
            let mut stmt = conn
                .prepare(&format!(
                    "SELECT {STEP_COLUMNS} FROM loom_steps \
                     WHERE status = 'running' AND started_at IS NOT NULL"
                ))
                .map_err(berr)?;
            let rows = stmt
                .query_map([], read_raw_step)
                .map_err(berr)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(berr)?;
            drop(stmt);
            let mut out = Vec::new();
            for raw in rows {
                let step = raw.into_step()?;
                let run = Self::get_run_any(&conn, step.run_id)?
                    .ok_or_else(|| LoomError::Backend(format!("step {} has no run", step.id)))?;
                out.push((step, run));
            }
            out
        };
        let now = Timestamp::now().as_offset_date_time();
        let mut timed_out = Vec::new();
        for (step, run) in candidates {
            let Some(started) = step.started_at.as_ref().map(|t| t.as_offset_date_time()) else {
                continue;
            };
            // Nanoseconds, not whole milliseconds: a sub-millisecond elapse truncates to 0ms
            // and would never trip a 0ms timeout.
            let elapsed_ns = (now - started).whole_nanoseconds();
            if elapsed_ns <= step.timeout_ms as i128 * 1_000_000 {
                continue;
            }
            self.fail_step_inner(
                &step,
                &run,
                &format!("step timed out after {}ms", step.timeout_ms),
            )
            .await?;
            timed_out.push(self.read_step(step.id)?);
        }
        Ok(timed_out)
    }

    /// Read a run's execution log, oldest first, capped at `limit`. Owner-scoped.
    pub async fn logs(
        &self,
        principal: PrincipalId,
        run_id: RunId,
        limit: usize,
    ) -> Result<Vec<LogEntry>, LoomError> {
        let conn = self.lock();
        let mut stmt = conn
            .prepare(
                "SELECT l.id, l.run_id, l.step_id, l.level, l.message, l.data, l.created_at \
                 FROM loom_logs l JOIN loom_runs r ON r.id = l.run_id \
                 WHERE l.run_id = ?1 AND r.principal_id = ?2 ORDER BY l.id ASC LIMIT ?3",
            )
            .map_err(berr)?;
        let rows = stmt
            .query_map(
                rusqlite::params![run_id.to_string(), principal.to_string(), limit as i64],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, Option<i64>>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, String>(4)?,
                        r.get::<_, String>(5)?,
                        r.get::<_, String>(6)?,
                    ))
                },
            )
            .map_err(berr)?;
        let mut out = Vec::new();
        for row in rows {
            let (id, run_id_s, step_id, level, message, data, created_at) = row.map_err(berr)?;
            out.push(LogEntry {
                id,
                run_id: run_id_s
                    .parse::<RunId>()
                    .map_err(|e| LoomError::Backend(format!("corrupt run_id {run_id_s:?}: {e}")))?,
                step_id,
                level: LogLevel::parse(&level)?,
                message,
                data: serde_json::from_str(&data)
                    .map_err(|e| LoomError::Backend(format!("corrupt log data: {e}")))?,
                created_at: ts_from_db(&created_at)?,
            });
        }
        Ok(out)
    }

    /// Aggregate workflow/run counts for a principal.
    pub async fn stats(&self, principal: PrincipalId) -> Result<LoomStats, LoomError> {
        let conn = self.lock();
        let workflows: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM loom_workflows WHERE principal_id = ?1",
                rusqlite::params![principal.to_string()],
                |r| r.get(0),
            )
            .map_err(berr)?;
        let mut stmt = conn
            .prepare(
                "SELECT status, COUNT(*) FROM loom_runs WHERE principal_id = ?1 GROUP BY status",
            )
            .map_err(berr)?;
        let rows = stmt
            .query_map(rusqlite::params![principal.to_string()], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
            })
            .map_err(berr)?;
        let mut runs = 0;
        let mut active_runs = 0;
        let mut runs_by_status = BTreeMap::new();
        for row in rows {
            let (status, count) = row.map_err(berr)?;
            runs += count;
            if matches!(status.as_str(), "pending" | "running") {
                active_runs += count;
            }
            runs_by_status.insert(status, count);
        }
        Ok(LoomStats {
            workflows,
            runs,
            active_runs,
            runs_by_status,
        })
    }
}

/// Apply every migration whose version exceeds `PRAGMA user_version`, each in its own transaction,
/// bumping `user_version` as it goes. Idempotent: an up-to-date database applies nothing.
fn apply_migrations(conn: &mut Connection) -> Result<(), LoomError> {
    let mut version: i64 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .map_err(berr)?;
    for (v, sql) in MIGRATIONS {
        if *v > version {
            let tx = conn.transaction().map_err(berr)?;
            tx.execute_batch(sql)
                .map_err(|e| LoomError::Backend(format!("migration V{v} failed: {e}")))?;
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
    use crate::executor::TransformExecutor;
    use async_trait::async_trait;

    /// A store with the built-in transform executor attached, plus its bus.
    fn store() -> (LoomStore, Arc<AxonBus>) {
        let bus = Arc::new(AxonBus::new());
        let store = LoomStore::open_in_memory(bus.clone())
            .expect("open")
            .with_executor(Box::new(TransformExecutor));
        (store, bus)
    }

    /// A transform StepDef with the given name, deps, and config.
    fn transform(name: &str, deps: &[&str], config: serde_json::Value) -> StepDef {
        StepDef {
            name: name.to_string(),
            step_type: StepType::Transform,
            config: Some(config),
            depends_on: if deps.is_empty() {
                None
            } else {
                Some(deps.iter().map(|s| s.to_string()).collect())
            },
            max_retries: None,
            timeout_ms: None,
        }
    }

    /// An externally completed action StepDef.
    fn action(name: &str, deps: &[&str]) -> StepDef {
        StepDef {
            name: name.to_string(),
            step_type: StepType::Action,
            config: None,
            depends_on: if deps.is_empty() {
                None
            } else {
                Some(deps.iter().map(|s| s.to_string()).collect())
            },
            max_retries: Some(1),
            timeout_ms: None,
        }
    }

    /// Create a workflow owned by a fresh principal; returns (principal, workflow).
    async fn workflow_with(store: &LoomStore, steps: Vec<StepDef>) -> (PrincipalId, Workflow) {
        let principal = PrincipalId::new();
        let workflow = store
            .create_workflow(NewWorkflow {
                tenant: TenantId::new(),
                principal_id: principal,
                name: format!("wf-{}", WorkflowId::new()),
                description: None,
                steps,
            })
            .await
            .expect("create workflow");
        (principal, workflow)
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
    /// Workflow crud roundtrips.
    async fn workflow_crud_roundtrips() {
        let (store, _bus) = store();
        let (principal, wf) = workflow_with(&store, vec![action("a", &[])]).await;
        let got = store
            .get_workflow(principal, wf.id)
            .await
            .expect("get")
            .expect("present");
        assert_eq!(got, wf);
        assert!(
            store
                .get_workflow(PrincipalId::new(), wf.id)
                .await
                .expect("get")
                .is_none(),
            "owner-scoped"
        );
        let by_name = store
            .get_workflow_by_name(principal, &wf.name)
            .await
            .expect("get")
            .expect("present");
        assert_eq!(by_name.id, wf.id);

        let updated = store
            .update_workflow(
                principal,
                wf.id,
                WorkflowPatch {
                    description: Some("now described".to_string()),
                    ..Default::default()
                },
            )
            .await
            .expect("update");
        assert_eq!(updated.description.as_deref(), Some("now described"));

        assert_eq!(
            store.list_workflows(principal).await.expect("list").len(),
            1
        );
        assert!(
            store
                .delete_workflow(principal, wf.id)
                .await
                .expect("delete")
        );
        assert!(
            !store
                .delete_workflow(principal, wf.id)
                .await
                .expect("delete")
        );
    }

    #[tokio::test]
    /// Definition validation rejects bad graphs.
    async fn definition_validation_rejects_bad_graphs() {
        let (store, _bus) = store();
        let principal = PrincipalId::new();
        let new = |steps: Vec<StepDef>| NewWorkflow {
            tenant: TenantId::new(),
            principal_id: principal,
            name: format!("wf-{}", WorkflowId::new()),
            description: None,
            steps,
        };
        // Duplicate names.
        let err = store
            .create_workflow(new(vec![action("a", &[]), action("a", &[])]))
            .await
            .expect_err("duplicate");
        assert!(matches!(err, LoomError::InvalidDefinition(_)));
        // Unknown dependency.
        let err = store
            .create_workflow(new(vec![action("a", &["ghost"])]))
            .await
            .expect_err("unknown dep");
        assert!(matches!(err, LoomError::InvalidDefinition(_)));
        // Cycle.
        let err = store
            .create_workflow(new(vec![action("a", &["b"]), action("b", &["a"])]))
            .await
            .expect_err("cycle");
        assert!(matches!(err, LoomError::InvalidDefinition(_)));
    }

    #[tokio::test]
    /// Transform chain runs to completion inline.
    async fn transform_chain_runs_to_completion_inline() {
        let (store, bus) = store();
        let mut rx = bus.subscribe("workflow");
        let (principal, wf) = workflow_with(
            &store,
            vec![
                transform(
                    "extract",
                    &[],
                    serde_json::json!({"mapping": {"v": "src.value"}}),
                ),
                transform(
                    "render",
                    &["extract"],
                    serde_json::json!({"template": {"sentence": "value is {{v}}"}}),
                ),
            ],
        )
        .await;

        let run = store
            .create_run(
                principal,
                wf.id,
                Some(serde_json::json!({"src": {"value": 41}})),
            )
            .await
            .expect("run");
        // The whole graph ran inline: created -> steps started/completed -> run completed.
        assert_eq!(run.status, RunStatus::Completed);
        assert_eq!(
            run.output,
            serde_json::json!({"v": 41, "sentence": "value is 41"})
        );
        let kinds = drain_kinds(&mut rx);
        assert_eq!(
            kinds,
            [
                "workflow.run.created",
                "workflow.step.started",
                "workflow.step.completed",
                "workflow.step.started",
                "workflow.step.completed",
                "workflow.run.completed",
            ]
        );
        // The log recorded the journey.
        let logs = store.logs(principal, run.id, 50).await.expect("logs");
        assert!(logs.iter().any(|l| l.message.contains("run completed")));
    }

    #[tokio::test]
    /// External steps wait and complete via api.
    async fn external_steps_wait_and_complete_via_api() {
        let (store, bus) = store();
        let mut rx = bus.subscribe("workflow");
        let (principal, wf) = workflow_with(
            &store,
            vec![
                action("approve", &[]),
                transform("after", &["approve"], serde_json::json!({})),
            ],
        )
        .await;
        let run = store.create_run(principal, wf.id, None).await.expect("run");
        assert_eq!(
            run.status,
            RunStatus::Running,
            "waiting on the external step"
        );
        let _ = drain_kinds(&mut rx);

        let steps = store.get_steps(principal, run.id).await.expect("steps");
        let approve = steps.iter().find(|s| s.name == "approve").expect("step");
        assert_eq!(approve.status, StepStatus::Running);

        // A stranger cannot complete it.
        let err = store
            .complete_step(PrincipalId::new(), approve.id, serde_json::json!({}))
            .await
            .expect_err("foreign");
        assert!(matches!(err, LoomError::StepNotFound(_)));

        // The owner completes it; the dependent transform runs and the run finishes.
        store
            .complete_step(principal, approve.id, serde_json::json!({"approved": true}))
            .await
            .expect("complete");
        let run = store
            .get_run(principal, run.id)
            .await
            .expect("get")
            .expect("present");
        assert_eq!(run.status, RunStatus::Completed);
        assert_eq!(run.output["approved"], true);
        let kinds = drain_kinds(&mut rx);
        assert!(kinds.contains(&"workflow.run.completed".to_string()));
    }

    #[tokio::test]
    /// Completing a non running step is rejected.
    async fn completing_a_non_running_step_is_rejected() {
        let (store, _bus) = store();
        let (principal, wf) =
            workflow_with(&store, vec![action("a", &[]), action("b", &["a"])]).await;
        let run = store.create_run(principal, wf.id, None).await.expect("run");
        let steps = store.get_steps(principal, run.id).await.expect("steps");
        let b = steps.iter().find(|s| s.name == "b").expect("step");
        assert_eq!(b.status, StepStatus::Pending, "deps unmet");
        let err = store
            .complete_step(principal, b.id, serde_json::json!({}))
            .await
            .expect_err("not running");
        assert!(matches!(err, LoomError::InvalidInput(_)));
    }

    /// An executor that always fails its claimed type.
    struct FailingExecutor;

    /// Claims transform and always errors.
    #[async_trait]
    impl StepExecutor for FailingExecutor {
        /// Handles.
        fn handles(&self, step_type: StepType) -> bool {
            step_type == StepType::Transform
        }
        /// Execute.
        async fn execute(&self, _ctx: StepContext<'_>) -> Result<serde_json::Value, String> {
            Err("boom".to_string())
        }
    }

    #[tokio::test]
    /// Retries then fails run when exhausted.
    async fn retries_then_fails_run_when_exhausted() {
        let bus = Arc::new(AxonBus::new());
        let store = LoomStore::open_in_memory(bus.clone())
            .expect("open")
            .with_executor(Box::new(FailingExecutor));
        let mut rx = bus.subscribe("workflow");
        let principal = PrincipalId::new();
        let wf = store
            .create_workflow(NewWorkflow {
                tenant: TenantId::new(),
                principal_id: principal,
                name: "doomed".to_string(),
                description: None,
                steps: vec![StepDef {
                    name: "t".to_string(),
                    step_type: StepType::Transform,
                    config: None,
                    depends_on: None,
                    max_retries: Some(2),
                    timeout_ms: None,
                }],
            })
            .await
            .expect("workflow");
        let run = store.create_run(principal, wf.id, None).await.expect("run");
        // Inline failure loops through the retry budget synchronously: 1 try + 2 retries.
        assert_eq!(run.status, RunStatus::Failed);
        assert_eq!(run.error.as_deref(), Some("boom"));
        let kinds = drain_kinds(&mut rx);
        assert_eq!(
            kinds
                .iter()
                .filter(|k| *k == "workflow.step.failed")
                .count(),
            3,
            "one per attempt"
        );
        assert!(kinds.contains(&"workflow.run.failed".to_string()));
        let steps = store.get_steps(principal, run.id).await.expect("steps");
        assert_eq!(steps[0].status, StepStatus::Failed);
        assert_eq!(steps[0].retry_count, 2);
    }

    #[tokio::test]
    /// Cancel skips unfinished steps.
    async fn cancel_skips_unfinished_steps() {
        let (store, bus) = store();
        let mut rx = bus.subscribe("workflow");
        let (principal, wf) =
            workflow_with(&store, vec![action("a", &[]), action("b", &["a"])]).await;
        let run = store.create_run(principal, wf.id, None).await.expect("run");
        let _ = drain_kinds(&mut rx);

        assert!(store.cancel_run(principal, run.id).await.expect("cancel"));
        assert_eq!(drain_kinds(&mut rx), ["workflow.run.cancelled"]);
        let run = store
            .get_run(principal, run.id)
            .await
            .expect("get")
            .expect("present");
        assert_eq!(run.status, RunStatus::Cancelled);
        let steps = store.get_steps(principal, run.id).await.expect("steps");
        assert!(steps.iter().all(|s| s.status == StepStatus::Skipped));
        // Cancelling again is a no-op.
        assert!(!store.cancel_run(principal, run.id).await.expect("cancel"));
    }

    #[tokio::test]
    /// Complete step refuses after concurrent cancel.
    async fn complete_step_refuses_after_concurrent_cancel() {
        // Regression for the TOCTOU race: completing a step from a stale
        // 'running' snapshot must not overwrite a concurrent cancel (which marks
        // the step 'skipped') with 'completed'.
        let (store, _bus) = store();
        let principal = PrincipalId::new();
        let wf = store
            .create_workflow(NewWorkflow {
                tenant: TenantId::new(),
                principal_id: principal,
                name: "wait-wf".to_string(),
                description: None,
                steps: vec![StepDef {
                    name: "wait".to_string(),
                    step_type: StepType::Wait,
                    config: None,
                    depends_on: None,
                    max_retries: Some(0),
                    // Large timeout so the step stays running, not swept.
                    timeout_ms: Some(60_000),
                }],
            })
            .await
            .expect("workflow");
        let run = store.create_run(principal, wf.id, None).await.expect("run");
        assert_eq!(run.status, RunStatus::Running);
        let steps = store.get_steps(principal, run.id).await.expect("steps");
        let running = steps
            .into_iter()
            .find(|s| s.status == StepStatus::Running)
            .expect("a running step");

        // Concurrent cancel: the running step becomes 'skipped'.
        assert!(store.cancel_run(principal, run.id).await.expect("cancel"));

        // Completing with the now-stale 'running' snapshot must be refused.
        let result = store
            .complete_step_inner(&running, &run, serde_json::json!({"ok": true}))
            .await;
        assert!(result.is_err(), "stale complete must be refused");

        // The step must remain skipped, not flipped to completed.
        let steps = store.get_steps(principal, run.id).await.expect("steps");
        let s = steps.iter().find(|s| s.id == running.id).expect("step");
        assert_eq!(s.status, StepStatus::Skipped);
    }

    #[tokio::test]
    /// Sweep times out overdue running steps.
    async fn sweep_times_out_overdue_running_steps() {
        let (store, _bus) = store();
        let principal = PrincipalId::new();
        let wf = store
            .create_workflow(NewWorkflow {
                tenant: TenantId::new(),
                principal_id: principal,
                name: "slow".to_string(),
                description: None,
                steps: vec![StepDef {
                    name: "wait".to_string(),
                    step_type: StepType::Wait,
                    config: None,
                    depends_on: None,
                    max_retries: Some(0),
                    // 0ms: any elapsed time is overdue.
                    timeout_ms: Some(0),
                }],
            })
            .await
            .expect("workflow");
        let run = store.create_run(principal, wf.id, None).await.expect("run");
        assert_eq!(run.status, RunStatus::Running);

        let timed_out = store.sweep_timeouts().await.expect("sweep");
        assert_eq!(timed_out.len(), 1);
        assert_eq!(timed_out[0].status, StepStatus::Failed);
        let run = store
            .get_run(principal, run.id)
            .await
            .expect("get")
            .expect("present");
        assert_eq!(run.status, RunStatus::Failed);
        assert!(
            run.error
                .as_deref()
                .unwrap_or_default()
                .contains("timed out")
        );
        // Nothing left to sweep.
        assert!(store.sweep_timeouts().await.expect("sweep").is_empty());
    }

    #[tokio::test]
    /// List runs filters and stats count.
    async fn list_runs_filters_and_stats_count() {
        let (store, _bus) = store();
        let (principal, wf) = workflow_with(&store, vec![action("a", &[])]).await;
        let r1 = store.create_run(principal, wf.id, None).await.expect("run");
        store.create_run(principal, wf.id, None).await.expect("run");
        store.cancel_run(principal, r1.id).await.expect("cancel");

        let all = store
            .list_runs(principal, RunFilter::default())
            .await
            .expect("list");
        assert_eq!(all.len(), 2);
        let cancelled = store
            .list_runs(
                principal,
                RunFilter {
                    status: Some(RunStatus::Cancelled),
                    ..Default::default()
                },
            )
            .await
            .expect("list");
        assert_eq!(cancelled.len(), 1);
        assert!(
            store
                .list_runs(PrincipalId::new(), RunFilter::default())
                .await
                .expect("list")
                .is_empty()
        );

        let stats = store.stats(principal).await.expect("stats");
        assert_eq!(stats.workflows, 1);
        assert_eq!(stats.runs, 2);
        assert_eq!(stats.active_runs, 1);
        assert_eq!(stats.runs_by_status.get("cancelled"), Some(&1));
    }

    #[tokio::test]
    /// Runs persist across reopen.
    async fn runs_persist_across_reopen() {
        let tmp = std::env::temp_dir().join(format!("henosis-loom-{}.sqlite", RunId::new()));
        let principal = PrincipalId::new();
        let run_id;
        {
            let store = LoomStore::open(&tmp, Arc::new(AxonBus::new()))
                .expect("open")
                .with_executor(Box::new(TransformExecutor));
            let wf = store
                .create_workflow(NewWorkflow {
                    tenant: TenantId::new(),
                    principal_id: principal,
                    name: "durable".to_string(),
                    description: None,
                    steps: vec![transform("t", &[], serde_json::json!({}))],
                })
                .await
                .expect("workflow");
            run_id = store
                .create_run(principal, wf.id, None)
                .await
                .expect("run")
                .id;
        }
        {
            let store = LoomStore::open(&tmp, Arc::new(AxonBus::new())).expect("reopen");
            let got = store
                .get_run(principal, run_id)
                .await
                .expect("get")
                .expect("present after reopen");
            assert_eq!(got.status, RunStatus::Completed);
        }
        let _ = std::fs::remove_file(&tmp);
    }

    #[tokio::test]
    /// A workflow with a Hephaestus step persists and reloads with the correct step type.
    ///
    /// Verifies the full round-trip: workflow definition -> SQLite -> reload, and that
    /// creating a run instantiates the step row with the `hephaestus` token. The store
    /// under test has only the TransformExecutor attached, so the Hephaestus step stays
    /// Running (no executor claims it); the test only validates persistence, not execution.
    async fn hephaestus_step_persists_and_reloads() {
        let (store, _bus) = store();
        let principal = PrincipalId::new();
        let wf = store
            .create_workflow(NewWorkflow {
                tenant: TenantId::new(),
                principal_id: principal,
                name: "heph-roundtrip".to_string(),
                description: None,
                steps: vec![StepDef {
                    name: "call-agent".to_string(),
                    step_type: StepType::Hephaestus,
                    config: Some(serde_json::json!({"input": "summarise X"})),
                    depends_on: None,
                    max_retries: Some(0),
                    timeout_ms: Some(60_000),
                }],
            })
            .await
            .expect("create workflow");

        // Reload the workflow definition and verify the step type survived the round-trip.
        let reloaded = store
            .get_workflow(principal, wf.id)
            .await
            .expect("get")
            .expect("present");
        assert_eq!(
            reloaded.steps[0].step_type,
            StepType::Hephaestus,
            "step_type round-trips through serde + SQLite"
        );

        // Create a run: the engine starts the step (no executor claims it, so it stays Running).
        let run = store.create_run(principal, wf.id, None).await.expect("run");
        assert_eq!(run.status, RunStatus::Running, "waiting on unclaimed step");

        // Verify the step instance row carries the hephaestus token.
        let steps = store.get_steps(principal, run.id).await.expect("steps");
        assert_eq!(steps.len(), 1);
        assert_eq!(
            steps[0].step_type,
            StepType::Hephaestus,
            "step instance persists hephaestus type"
        );
        assert_eq!(steps[0].status, StepStatus::Running);
    }
}
