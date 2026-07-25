//! The SQLite-backed Loom workflow store and its dependency-driven step engine.
//!
//! Workflows and runs are scoped on both [`TenantId`] and [`PrincipalId`], use
//! [`WorkflowId`]/[`RunId`] (UUID v8), and publish typed lifecycle events on the in-process
//! [`AxonBus`]. The versioned SQLite schema uses one `Connection` behind a `Mutex`.
//!
//! The engine (`advance_run`) advances a run by starting every pending step whose dependencies
//! are all completed, handing it the run input overlaid with its
//! dependency outputs; a run completes when no step is pending or running, merging completed
//! outputs. Execution is delegated through the [`StepExecutor`] seam: the attached executor
//! runs the types it claims inline (the built-in [`crate::TransformExecutor`] covers pure-JSON
//! steps); every other started step waits for external completion via [`LoomStore::complete_step`].
//!
//! Definitions are validated as DAGs when written and again before a persisted definition starts
//! a run. `create_run` advances immediately, and [`LoomStore::sweep_timeouts`] enforces
//! `timeout_ms`.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};

use henosis_sqlite::OpenedDatabase;
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
    StepStatus, StepType, Workflow, WorkflowPatch, MAX_STEP_RETRIES, MAX_STEP_TIMEOUT_MS,
    MAX_WORKFLOW_DEPTH, MAX_WORKFLOW_STEPS,
};

/// Ordered schema migrations, applied by `PRAGMA user_version`. Append-only (see the DB convention).
const MIGRATIONS: &[(i64, &str)] = &[
    (1, include_str!("../migrations/V1__loom_workflows.sql")),
    (2, include_str!("../migrations/V2__loom_tenant_indexes.sql")),
];

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
    /// The database and its path guard, serialized by a `Mutex`.
    conn: Mutex<OpenedDatabase>,
    /// The bus workflow lifecycle events are published onto.
    bus: Arc<AxonBus>,
    /// The optional inline executor seam. `None` = every step waits for external completion.
    executor: Option<Box<dyn StepExecutor>>,
}

/// The result of an exact-attempt lifecycle transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StepTransition {
    /// The captured attempt or its owning run is no longer active.
    Stale,
    /// The captured attempt completed.
    Completed,
    /// The captured attempt failed and was reset for another retry.
    Retrying,
    /// The captured attempt exhausted its retry budget and failed its run.
    Failed,
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

/// Validate a workflow definition for unique names, resolvable dependencies, and acyclicity.
fn validate_steps(steps: &[StepDef]) -> Result<(), LoomError> {
    if steps.len() > MAX_WORKFLOW_STEPS {
        return Err(LoomError::InvalidInput(format!(
            "workflow step count {} exceeds limit {MAX_WORKFLOW_STEPS}",
            steps.len()
        )));
    }
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

/// Enforce workflow execution limits before a definition is persisted or instantiated.
///
/// Assumes [`validate_steps`] already established that every dependency exists and the graph is
/// acyclic.
fn validate_limits(steps: &[StepDef]) -> Result<(), LoomError> {
    for step in steps {
        if let Some(retries) = step.max_retries {
            if retries < 0 {
                return Err(LoomError::InvalidInput(format!(
                    "step {:?} max_retries must be >= 0",
                    step.name
                )));
            }
            if retries > MAX_STEP_RETRIES {
                return Err(LoomError::InvalidInput(format!(
                    "step {:?} max_retries {retries} exceeds limit {MAX_STEP_RETRIES}",
                    step.name
                )));
            }
        }
        if let Some(timeout_ms) = step.timeout_ms {
            if timeout_ms < 0 {
                return Err(LoomError::InvalidInput(format!(
                    "step {:?} timeout_ms must be >= 0",
                    step.name
                )));
            }
            if timeout_ms > MAX_STEP_TIMEOUT_MS {
                return Err(LoomError::InvalidInput(format!(
                    "step {:?} timeout_ms {timeout_ms} exceeds limit {MAX_STEP_TIMEOUT_MS}",
                    step.name
                )));
            }
        }
    }
    let depth = longest_chain_depth(steps);
    if depth > MAX_WORKFLOW_DEPTH {
        return Err(LoomError::InvalidInput(format!(
            "workflow dependency depth {depth} exceeds limit {MAX_WORKFLOW_DEPTH}"
        )));
    }
    Ok(())
}

/// Compute the longest dependency chain iteratively for a validated workflow DAG.
fn longest_chain_depth(steps: &[StepDef]) -> usize {
    let mut depth: HashMap<&str, usize> = steps.iter().map(|s| (s.name.as_str(), 1)).collect();
    let mut indegree: HashMap<&str, usize> = steps.iter().map(|s| (s.name.as_str(), 0)).collect();
    let mut dependents: HashMap<&str, Vec<&str>> = HashMap::new();
    for step in steps {
        for dep in step.depends_on.iter().flatten() {
            *indegree.get_mut(step.name.as_str()).expect("known name") += 1;
            dependents
                .entry(dep.as_str())
                .or_default()
                .push(step.name.as_str());
        }
    }
    let mut queue: Vec<&str> = indegree
        .iter()
        .filter(|(_, count)| **count == 0)
        .map(|(name, _)| *name)
        .collect();
    let mut max_depth = 0;
    while let Some(name) = queue.pop() {
        let current_depth = depth[name];
        max_depth = max_depth.max(current_depth);
        for &child in dependents.get(name).into_iter().flatten() {
            let child_depth = depth.get_mut(child).expect("known name");
            *child_depth = (*child_depth).max(current_depth + 1);
            let remaining = indegree.get_mut(child).expect("known name");
            *remaining -= 1;
            if *remaining == 0 {
                queue.push(child);
            }
        }
    }
    max_depth
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
        let database = henosis_sqlite::open_database(path)
            .map_err(|error| LoomError::Backend(error.to_string()))?;
        Self::from_database(database, bus)
    }

    /// Open an ephemeral in-memory store. For tests and throwaway use.
    pub fn open_in_memory(bus: Arc<AxonBus>) -> Result<Self, LoomError> {
        let database = OpenedDatabase::open_in_memory().map_err(berr)?;
        Self::from_database(database, bus)
    }

    /// Attach a [`StepExecutor`] that runs the step types it claims inline during advance
    /// passes. Builder-style, used at server wiring time.
    pub fn with_executor(mut self, executor: Box<dyn StepExecutor>) -> Self {
        self.executor = Some(executor);
        self
    }

    /// Enable foreign keys and apply migrations while the database retains its path guard.
    fn from_database(mut database: OpenedDatabase, bus: Arc<AxonBus>) -> Result<Self, LoomError> {
        database
            .pragma_update(None, "foreign_keys", true)
            .map_err(berr)?;
        apply_migrations(&mut database)?;
        Ok(Self {
            conn: Mutex::new(database),
            bus,
            executor: None,
        })
    }

    /// Lock the connection, recovering from a poisoned mutex.
    fn lock(&self) -> MutexGuard<'_, OpenedDatabase> {
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
        validate_limits(&new.steps)?;
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

    /// Look up an owned workflow by id within a tenant.
    ///
    /// Returns `Ok(None)` when the workflow is absent or belongs to another tenant or principal.
    pub async fn get_workflow(
        &self,
        tenant: TenantId,
        principal: PrincipalId,
        id: WorkflowId,
    ) -> Result<Option<Workflow>, LoomError> {
        let conn = self.lock();
        Self::get_workflow_in(&conn, tenant, principal, id)
    }

    /// Tenant-and-owner-scoped workflow lookup against an arbitrary connection.
    fn get_workflow_in(
        conn: &Connection,
        tenant: TenantId,
        principal: PrincipalId,
        id: WorkflowId,
    ) -> Result<Option<Workflow>, LoomError> {
        let raw = conn
            .query_row(
                &format!(
                    "SELECT {WORKFLOW_COLUMNS} FROM loom_workflows \
                     WHERE id = ?1 AND tenant = ?2 AND principal_id = ?3"
                ),
                rusqlite::params![id.to_string(), tenant.to_string(), principal.to_string()],
                read_raw_workflow,
            )
            .optional()
            .map_err(berr)?;
        raw.map(RawWorkflow::into_workflow).transpose()
    }

    /// Look up an owned workflow by name within a tenant.
    pub async fn get_workflow_by_name(
        &self,
        tenant: TenantId,
        principal: PrincipalId,
        name: &str,
    ) -> Result<Option<Workflow>, LoomError> {
        let conn = self.lock();
        let raw = conn
            .query_row(
                &format!(
                    "SELECT {WORKFLOW_COLUMNS} FROM loom_workflows \
                     WHERE tenant = ?1 AND principal_id = ?2 AND name = ?3"
                ),
                rusqlite::params![tenant.to_string(), principal.to_string(), name],
                read_raw_workflow,
            )
            .optional()
            .map_err(berr)?;
        raw.map(RawWorkflow::into_workflow).transpose()
    }

    /// List a principal's workflows within a tenant, newest-updated first.
    pub async fn list_workflows(
        &self,
        tenant: TenantId,
        principal: PrincipalId,
    ) -> Result<Vec<Workflow>, LoomError> {
        let conn = self.lock();
        let mut stmt = conn
            .prepare(&format!(
                "SELECT {WORKFLOW_COLUMNS} FROM loom_workflows \
                 WHERE tenant = ?1 AND principal_id = ?2 ORDER BY updated_at DESC"
            ))
            .map_err(berr)?;
        let rows = stmt
            .query_map(
                rusqlite::params![tenant.to_string(), principal.to_string()],
                read_raw_workflow,
            )
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
        tenant: TenantId,
        principal: PrincipalId,
        id: WorkflowId,
        patch: WorkflowPatch,
    ) -> Result<Workflow, LoomError> {
        if let Some(steps) = &patch.steps {
            validate_steps(steps)?;
            validate_limits(steps)?;
        }
        let conn = self.lock();
        let mut workflow = Self::get_workflow_in(&conn, tenant, principal, id)?
            .ok_or(LoomError::WorkflowNotFound(id))?;
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
             WHERE id = ?5 AND tenant = ?6 AND principal_id = ?7",
            rusqlite::params![
                &workflow.name,
                &workflow.description,
                serde_json::to_string(&workflow.steps)
                    .map_err(|e| LoomError::Backend(format!("steps serialize: {e}")))?,
                ts_to_db(&workflow.updated_at)?,
                id.to_string(),
                tenant.to_string(),
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
        tenant: TenantId,
        principal: PrincipalId,
        id: WorkflowId,
    ) -> Result<bool, LoomError> {
        let conn = self.lock();
        let n = conn
            .execute(
                "DELETE FROM loom_workflows \
                 WHERE id = ?1 AND tenant = ?2 AND principal_id = ?3",
                rusqlite::params![id.to_string(), tenant.to_string(), principal.to_string()],
            )
            .map_err(berr)?;
        Ok(n > 0)
    }

    /// Start a run of an owned workflow: instantiate its steps, emit `workflow.run.created`,
    /// and advance immediately (a deviation from Kleos, which left runs pending until an
    /// external nudge -- a self-starting engine is what "the step graph runs" means here).
    /// Persisted definitions are revalidated against current structure and safety limits before
    /// any run rows are written. A workflow with no steps cannot run.
    pub async fn create_run(
        &self,
        tenant: TenantId,
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
            let workflow = Self::get_workflow_in(&tx, tenant, principal, workflow_id)?
                .ok_or(LoomError::WorkflowNotFound(workflow_id))?;
            validate_steps(&workflow.steps)?;
            validate_limits(&workflow.steps)?;
            if workflow.steps.is_empty() {
                return Err(LoomError::InvalidInput("workflow has no steps".to_string()));
            }
            let run = Run {
                id: RunId::new(),
                workflow_id,
                tenant,
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
        self.advance_inner(run.id).await?;
        // Re-read: the advance pass may have started (or even completed) the run.
        self.get_run(tenant, principal, run.id)
            .await?
            .ok_or(LoomError::RunNotFound(run.id))
    }

    /// Look up an owned run by id within a tenant.
    ///
    /// Returns `Ok(None)` when the run is absent or belongs to another tenant or principal.
    pub async fn get_run(
        &self,
        tenant: TenantId,
        principal: PrincipalId,
        id: RunId,
    ) -> Result<Option<Run>, LoomError> {
        let conn = self.lock();
        let raw = conn
            .query_row(
                &format!(
                    "SELECT {RUN_COLUMNS} FROM loom_runs \
                     WHERE id = ?1 AND tenant = ?2 AND principal_id = ?3"
                ),
                rusqlite::params![id.to_string(), tenant.to_string(), principal.to_string()],
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

    /// List a principal's runs within a tenant, newest first, AND-filtered by [`RunFilter`].
    pub async fn list_runs(
        &self,
        tenant: TenantId,
        principal: PrincipalId,
        filter: RunFilter,
    ) -> Result<Vec<Run>, LoomError> {
        let mut sql =
            format!("SELECT {RUN_COLUMNS} FROM loom_runs WHERE tenant = ?1 AND principal_id = ?2");
        let mut args: Vec<rusqlite::types::Value> =
            vec![tenant.to_string().into(), principal.to_string().into()];
        let mut n = 2;
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
    pub async fn cancel_run(
        &self,
        tenant: TenantId,
        principal: PrincipalId,
        id: RunId,
    ) -> Result<bool, LoomError> {
        let run = {
            let mut conn = self.lock();
            let tx = conn.transaction().map_err(berr)?;
            let run = Self::get_run_any(&tx, id)?
                .filter(|r| r.tenant == tenant && r.principal_id == principal)
                .ok_or(LoomError::RunNotFound(id))?;
            if run.status.is_terminal() {
                return Ok(false);
            }
            let now = ts_to_db(&Timestamp::now())?;
            tx.execute(
                "UPDATE loom_runs SET status = 'cancelled', completed_at = ?2, updated_at = ?2 \
                 WHERE id = ?1 AND tenant = ?3 AND principal_id = ?4",
                rusqlite::params![
                    id.to_string(),
                    now,
                    tenant.to_string(),
                    principal.to_string()
                ],
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

    /// List a run's steps in definition order within a tenant.
    pub async fn get_steps(
        &self,
        tenant: TenantId,
        principal: PrincipalId,
        run_id: RunId,
    ) -> Result<Vec<Step>, LoomError> {
        let conn = self.lock();
        let mut stmt = conn
            .prepare(&format!(
                "SELECT {STEP_COLUMNS} FROM loom_steps s WHERE s.run_id = ?1 \
                 AND EXISTS (SELECT 1 FROM loom_runs r \
                             WHERE r.id = s.run_id AND r.tenant = ?2 \
                               AND r.principal_id = ?3) \
                 ORDER BY s.id ASC"
            ))
            .map_err(berr)?;
        let rows = stmt
            .query_map(
                rusqlite::params![
                    run_id.to_string(),
                    tenant.to_string(),
                    principal.to_string()
                ],
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
        tenant: TenantId,
        principal: PrincipalId,
        step_id: i64,
    ) -> Result<Option<Step>, LoomError> {
        let conn = self.lock();
        Ok(Self::get_step_with_run(&conn, step_id)?
            .filter(|(_, run)| run.tenant == tenant && run.principal_id == principal)
            .map(|(step, _)| step))
    }

    /// Complete an exact running attempt with `output` and advance the run.
    ///
    /// The external-completion path for action/decision/parallel/wait steps (and anything the
    /// executor does not claim). Owner-scoped; `expected_retry_count` and
    /// `expected_started_at` must match the attempt the caller actually executed.
    pub async fn complete_step(
        &self,
        tenant: TenantId,
        principal: PrincipalId,
        step_id: i64,
        expected_retry_count: i32,
        expected_started_at: Timestamp,
        output: serde_json::Value,
    ) -> Result<Step, LoomError> {
        // The guard must leave scope via the block (an explicit drop is not enough for the
        // future's Send analysis) before any await.
        let (step, run) = {
            let conn = self.lock();
            Self::get_step_with_run(&conn, step_id)?
                .filter(|(_, run)| run.tenant == tenant && run.principal_id == principal)
                .ok_or(LoomError::StepNotFound(step_id))?
        };
        match self.complete_step_inner(
            &step,
            &run,
            expected_retry_count,
            expected_started_at,
            output,
        )? {
            StepTransition::Completed => {}
            StepTransition::Stale => {
                return Err(LoomError::InvalidInput(format!(
                    "cannot complete step {step_id}: attempt is no longer active"
                )));
            }
            unexpected => {
                return Err(LoomError::Backend(format!(
                    "completion returned unexpected transition {unexpected:?}"
                )));
            }
        }
        self.advance_inner(step.run_id).await?;
        self.read_step(step_id)
    }

    /// Fail an exact running attempt and advance the run (retry semantics apply).
    ///
    /// Owner-scoped; `expected_retry_count` and `expected_started_at` must identify the attempt
    /// whose work produced the failure.
    pub async fn fail_step(
        &self,
        tenant: TenantId,
        principal: PrincipalId,
        step_id: i64,
        expected_retry_count: i32,
        expected_started_at: Timestamp,
        error: &str,
    ) -> Result<Step, LoomError> {
        let (step, run) = {
            let conn = self.lock();
            Self::get_step_with_run(&conn, step_id)?
                .filter(|(_, run)| run.tenant == tenant && run.principal_id == principal)
                .ok_or(LoomError::StepNotFound(step_id))?
        };
        match self.fail_step_inner(
            &step,
            &run,
            expected_retry_count,
            expected_started_at,
            error,
        )? {
            StepTransition::Retrying | StepTransition::Failed => {}
            StepTransition::Stale => {
                return Err(LoomError::InvalidInput(format!(
                    "cannot fail step {step_id}: attempt is no longer active"
                )));
            }
            unexpected => {
                return Err(LoomError::Backend(format!(
                    "failure returned unexpected transition {unexpected:?}"
                )));
            }
        }
        self.advance_inner(step.run_id).await?;
        self.read_step(step_id)
    }

    /// Re-read one step by id (engine-internal, post-mutation).
    fn read_step(&self, step_id: i64) -> Result<Step, LoomError> {
        let conn = self.lock();
        Self::get_step_with_run(&conn, step_id)?
            .map(|(step, _)| step)
            .ok_or(LoomError::StepNotFound(step_id))
    }

    /// Check whether a captured running step is still the current attempt of an active run.
    fn attempt_is_current(&self, step: &Step) -> Result<bool, LoomError> {
        let Some(started_at) = step.started_at else {
            return Ok(false);
        };
        let conn = self.lock();
        let current: i64 = conn
            .query_row(
                "SELECT EXISTS( \
                     SELECT 1 FROM loom_steps s \
                     JOIN loom_runs r ON r.id = s.run_id \
                     WHERE s.id = ?1 AND s.status = 'running' \
                       AND s.retry_count = ?2 AND s.started_at = ?3 \
                       AND r.status IN ('pending', 'running') \
                 )",
                rusqlite::params![step.id, step.retry_count, ts_to_db(&started_at)?],
                |row| row.get(0),
            )
            .map_err(berr)?;
        Ok(current != 0)
    }

    /// Mark an exact running attempt completed and log it without advancing the scheduler.
    fn complete_step_inner(
        &self,
        step: &Step,
        run: &Run,
        expected_retry_count: i32,
        expected_started_at: Timestamp,
        output: serde_json::Value,
    ) -> Result<StepTransition, LoomError> {
        {
            let mut conn = self.lock();
            let tx = conn.transaction().map_err(berr)?;
            let affected = tx
                .execute(
                    "UPDATE loom_steps SET status = 'completed', output = ?1, completed_at = ?2 \
                     WHERE id = ?3 AND status = 'running' \
                       AND retry_count = ?4 AND started_at = ?5 \
                       AND EXISTS (SELECT 1 FROM loom_runs r \
                                   WHERE r.id = loom_steps.run_id \
                                     AND r.status IN ('pending', 'running'))",
                    rusqlite::params![
                        output.to_string(),
                        ts_to_db(&Timestamp::now())?,
                        step.id,
                        expected_retry_count,
                        ts_to_db(&expected_started_at)?,
                    ],
                )
                .map_err(berr)?;
            if affected == 0 {
                tx.commit().map_err(berr)?;
                return Ok(StepTransition::Stale);
            }
            Self::add_log(
                &tx,
                step.run_id,
                Some(step.id),
                LogLevel::Info,
                &format!("step {:?} completed", step.name),
                None,
            )?;
            tx.commit().map_err(berr)?;
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
        Ok(StepTransition::Completed)
    }

    /// Fail an exact attempt: reset it within budget or atomically fail its step and run.
    fn fail_step_inner(
        &self,
        step: &Step,
        run: &Run,
        expected_retry_count: i32,
        expected_started_at: Timestamp,
        error: &str,
    ) -> Result<StepTransition, LoomError> {
        let will_retry = expected_retry_count < step.max_retries;
        {
            let mut conn = self.lock();
            let tx = conn.transaction().map_err(berr)?;
            if will_retry {
                let affected = tx
                    .execute(
                        "UPDATE loom_steps SET status = 'pending', retry_count = retry_count + 1, \
                         error = ?1, started_at = NULL \
                         WHERE id = ?2 AND status = 'running' \
                           AND retry_count = ?3 AND started_at = ?4 \
                           AND EXISTS (SELECT 1 FROM loom_runs r \
                                       WHERE r.id = loom_steps.run_id \
                                         AND r.status IN ('pending', 'running'))",
                        rusqlite::params![
                            error,
                            step.id,
                            expected_retry_count,
                            ts_to_db(&expected_started_at)?,
                        ],
                    )
                    .map_err(berr)?;
                if affected == 0 {
                    tx.commit().map_err(berr)?;
                    return Ok(StepTransition::Stale);
                }
                Self::add_log(
                    &tx,
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
                tx.commit().map_err(berr)?;
            } else {
                let now = ts_to_db(&Timestamp::now())?;
                let affected = tx
                    .execute(
                        "UPDATE loom_steps SET status = 'failed', error = ?1, completed_at = ?2 \
                         WHERE id = ?3 AND status = 'running' \
                           AND retry_count = ?4 AND started_at = ?5 \
                           AND EXISTS (SELECT 1 FROM loom_runs r \
                                       WHERE r.id = loom_steps.run_id \
                                         AND r.status IN ('pending', 'running'))",
                        rusqlite::params![
                            error,
                            now,
                            step.id,
                            expected_retry_count,
                            ts_to_db(&expected_started_at)?,
                        ],
                    )
                    .map_err(berr)?;
                if affected == 0 {
                    tx.commit().map_err(berr)?;
                    return Ok(StepTransition::Stale);
                }
                tx.execute(
                    "UPDATE loom_steps SET status = 'skipped', completed_at = ?1 \
                     WHERE run_id = ?2 AND id != ?3 AND status IN ('pending', 'running')",
                    rusqlite::params![now, step.run_id.to_string(), step.id],
                )
                .map_err(berr)?;
                let run_affected = tx
                    .execute(
                        "UPDATE loom_runs SET status = 'failed', error = ?1, completed_at = ?2, \
                     updated_at = ?2 WHERE id = ?3 AND status IN ('pending', 'running')",
                        rusqlite::params![error, now, step.run_id.to_string()],
                    )
                    .map_err(berr)?;
                if run_affected != 1 {
                    return Err(LoomError::Backend(format!(
                        "active step {} had no active owning run",
                        step.id
                    )));
                }
                Self::add_log(
                    &tx,
                    step.run_id,
                    Some(step.id),
                    LogLevel::Error,
                    &format!("step {:?} failed (max retries exhausted)", step.name),
                    Some(serde_json::json!({ "error": error })),
                )?;
                tx.commit().map_err(berr)?;
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
        if !will_retry {
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
        Ok(if will_retry {
            StepTransition::Retrying
        } else {
            StepTransition::Failed
        })
    }

    /// Advance an owned run: the public nudge for externally driven graphs.
    pub async fn advance_run(
        &self,
        tenant: TenantId,
        principal: PrincipalId,
        run_id: RunId,
    ) -> Result<(), LoomError> {
        {
            let conn = self.lock();
            Self::get_run_any(&conn, run_id)?
                .filter(|r| r.tenant == tenant && r.principal_id == principal)
                .ok_or(LoomError::RunNotFound(run_id))?;
        }
        self.advance_inner(run_id).await
    }

    /// Iteratively advance a run until it becomes terminal or waits on external work.
    ///
    /// Every pass claims ready steps under the database lock and executes claimed inline work
    /// after releasing it. The loop repeats only after inline execution persisted progress, so
    /// dependency chains and retries reuse one async frame.
    async fn advance_inner(&self, run_id: RunId) -> Result<(), LoomError> {
        loop {
            let (run, ready, completed) = {
                let mut conn = self.lock();
                let tx = conn.transaction().map_err(berr)?;
                let Some(run) = Self::get_run_any(&tx, run_id)? else {
                    return Err(LoomError::RunNotFound(run_id));
                };
                if run.status.is_terminal() {
                    return Ok(());
                }
                if run.status == RunStatus::Pending {
                    let affected = tx
                        .execute(
                            "UPDATE loom_runs SET status = 'running', started_at = ?2, \
                             updated_at = ?2 WHERE id = ?1 AND status = 'pending'",
                            rusqlite::params![run_id.to_string(), ts_to_db(&Timestamp::now())?],
                        )
                        .map_err(berr)?;
                    if affected == 0 {
                        tx.commit().map_err(berr)?;
                        continue;
                    }
                }
                let mut stmt = tx
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

                let by_name: HashMap<&str, &Step> = steps
                    .iter()
                    .map(|step| (step.name.as_str(), step))
                    .collect();
                let all_done = steps
                    .iter()
                    .all(|step| !matches!(step.status, StepStatus::Pending | StepStatus::Running));
                if all_done {
                    let mut merged = serde_json::Map::new();
                    for step in steps
                        .iter()
                        .filter(|step| step.status == StepStatus::Completed)
                    {
                        if let serde_json::Value::Object(map) = &step.output {
                            for (key, value) in map {
                                merged.insert(key.clone(), value.clone());
                            }
                        }
                    }
                    let now = ts_to_db(&Timestamp::now())?;
                    let affected = tx
                        .execute(
                            "UPDATE loom_runs SET status = 'completed', output = ?1, \
                             completed_at = ?2, updated_at = ?2 \
                             WHERE id = ?3 AND status IN ('pending', 'running')",
                            rusqlite::params![
                                serde_json::Value::Object(merged).to_string(),
                                now,
                                run_id.to_string()
                            ],
                        )
                        .map_err(berr)?;
                    if affected == 0 {
                        tx.commit().map_err(berr)?;
                        continue;
                    }
                    Self::add_log(&tx, run_id, None, LogLevel::Info, "run completed", None)?;
                    tx.commit().map_err(berr)?;
                    (run, Vec::new(), true)
                } else {
                    let mut ready = Vec::new();
                    for step in &steps {
                        if step.status != StepStatus::Pending {
                            continue;
                        }
                        let deps_met = step.depends_on.iter().all(|dependency| {
                            by_name
                                .get(dependency.as_str())
                                .is_some_and(|candidate| candidate.status == StepStatus::Completed)
                        });
                        if !deps_met {
                            continue;
                        }
                        let mut merged = serde_json::Map::new();
                        if let serde_json::Value::Object(map) = &run.input {
                            for (key, value) in map {
                                merged.insert(key.clone(), value.clone());
                            }
                        }
                        for dependency in &step.depends_on {
                            if let Some(dependency_step) = by_name.get(dependency.as_str()) {
                                if let serde_json::Value::Object(map) = &dependency_step.output {
                                    for (key, value) in map {
                                        merged.insert(key.clone(), value.clone());
                                    }
                                }
                            }
                        }
                        let started_at = Timestamp::now();
                        let mut started = step.clone();
                        started.input = serde_json::Value::Object(merged);
                        started.status = StepStatus::Running;
                        started.started_at = Some(started_at);
                        let affected = tx
                            .execute(
                                "UPDATE loom_steps SET status = 'running', input = ?1, \
                                 started_at = ?2 WHERE id = ?3 AND status = 'pending' \
                                   AND EXISTS (SELECT 1 FROM loom_runs r \
                                               WHERE r.id = loom_steps.run_id \
                                                 AND r.status IN ('pending', 'running'))",
                                rusqlite::params![
                                    started.input.to_string(),
                                    ts_to_db(&started_at)?,
                                    step.id
                                ],
                            )
                            .map_err(berr)?;
                        if affected == 0 {
                            continue;
                        }
                        Self::add_log(
                            &tx,
                            run_id,
                            Some(step.id),
                            LogLevel::Info,
                            &format!("step {:?} started", step.name),
                            None,
                        )?;
                        ready.push(started);
                    }
                    tx.commit().map_err(berr)?;
                    (run, ready, false)
                }
            };

            if completed {
                self.emit(
                    &RunCompleted {
                        run_id: run_id.to_string(),
                    },
                    run.tenant,
                    run.principal_id,
                );
                return Ok(());
            }

            let mut made_inline_progress = false;
            for step in ready {
                if !self.attempt_is_current(&step)? {
                    continue;
                }
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
                let started_at = step.started_at.ok_or_else(|| {
                    LoomError::Backend(format!("running step {} has no start time", step.id))
                })?;
                made_inline_progress = true;
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
                        match self.complete_step_inner(
                            &step,
                            &run,
                            step.retry_count,
                            started_at,
                            output,
                        )? {
                            StepTransition::Completed => {}
                            StepTransition::Stale => continue,
                            unexpected => {
                                return Err(LoomError::Backend(format!(
                                    "completion returned unexpected transition {unexpected:?}"
                                )));
                            }
                        }
                    }
                    Err(message) => {
                        match self.fail_step_inner(
                            &step,
                            &run,
                            step.retry_count,
                            started_at,
                            &message,
                        )? {
                            StepTransition::Retrying => {}
                            StepTransition::Failed => return Ok(()),
                            StepTransition::Stale => continue,
                            unexpected => {
                                return Err(LoomError::Backend(format!(
                                    "failure returned unexpected transition {unexpected:?}"
                                )));
                            }
                        }
                    }
                }
            }
            if !made_inline_progress {
                return Ok(());
            }
        }
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
        self.sweep_timeout_candidates(candidates, Timestamp::now())
            .await
    }

    /// Apply timeout handling to captured candidates, skipping attempts that became stale.
    async fn sweep_timeout_candidates(
        &self,
        candidates: Vec<(Step, Run)>,
        now: Timestamp,
    ) -> Result<Vec<Step>, LoomError> {
        let now = now.as_offset_date_time();
        let mut timed_out = Vec::new();
        for (step, run) in candidates {
            let Some(started_at) = step.started_at else {
                continue;
            };
            // Nanoseconds, not whole milliseconds: a sub-millisecond elapse truncates to 0ms
            // and would never trip a 0ms timeout.
            let elapsed_ns = (now - started_at.as_offset_date_time()).whole_nanoseconds();
            if elapsed_ns <= step.timeout_ms as i128 * 1_000_000 {
                continue;
            }
            let transition = self.fail_step_inner(
                &step,
                &run,
                step.retry_count,
                started_at,
                &format!("step timed out after {}ms", step.timeout_ms),
            )?;
            if transition == StepTransition::Stale {
                continue;
            }
            if transition == StepTransition::Completed {
                return Err(LoomError::Backend(format!(
                    "timeout failure for step {} completed the attempt",
                    step.id
                )));
            }
            self.advance_inner(step.run_id).await?;
            timed_out.push(self.read_step(step.id)?);
        }
        Ok(timed_out)
    }

    /// Read a run's execution log within a tenant, oldest first, capped at `limit`.
    pub async fn logs(
        &self,
        tenant: TenantId,
        principal: PrincipalId,
        run_id: RunId,
        limit: usize,
    ) -> Result<Vec<LogEntry>, LoomError> {
        let conn = self.lock();
        let mut stmt = conn
            .prepare(
                "SELECT l.id, l.run_id, l.step_id, l.level, l.message, l.data, l.created_at \
                 FROM loom_logs l JOIN loom_runs r ON r.id = l.run_id \
                 WHERE l.run_id = ?1 AND r.tenant = ?2 AND r.principal_id = ?3 \
                 ORDER BY l.id ASC LIMIT ?4",
            )
            .map_err(berr)?;
        let rows = stmt
            .query_map(
                rusqlite::params![
                    run_id.to_string(),
                    tenant.to_string(),
                    principal.to_string(),
                    limit as i64
                ],
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

    /// Aggregate workflow/run counts for a principal within a tenant.
    pub async fn stats(
        &self,
        tenant: TenantId,
        principal: PrincipalId,
    ) -> Result<LoomStats, LoomError> {
        let conn = self.lock();
        let workflows: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM loom_workflows \
                 WHERE tenant = ?1 AND principal_id = ?2",
                rusqlite::params![tenant.to_string(), principal.to_string()],
                |r| r.get(0),
            )
            .map_err(berr)?;
        let mut stmt = conn
            .prepare(
                "SELECT status, COUNT(*) FROM loom_runs \
                 WHERE tenant = ?1 AND principal_id = ?2 GROUP BY status",
            )
            .map_err(berr)?;
        let rows = stmt
            .query_map(
                rusqlite::params![tenant.to_string(), principal.to_string()],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
            )
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
fn apply_migrations(conn: &mut OpenedDatabase) -> Result<(), LoomError> {
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

    #[test]
    /// The forward migration upgrades V1 databases with tenant-aware query indexes.
    fn tenant_indexes_upgrade_existing_v1_database() {
        let mut database = OpenedDatabase::open_in_memory().expect("open V1 database");
        database
            .execute_batch(include_str!("../migrations/V1__loom_workflows.sql"))
            .expect("apply V1 schema");
        database
            .pragma_update(None, "user_version", 1)
            .expect("mark V1 schema");

        apply_migrations(&mut database).expect("upgrade to V2");

        let version: i64 = database
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .expect("read schema version");
        assert_eq!(version, 2);
        for index in [
            "idx_loom_workflows_tenant_principal_updated",
            "idx_loom_runs_tenant_principal_status",
        ] {
            let count: i64 = database
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type = 'index' AND name = ?1",
                    rusqlite::params![index],
                    |row| row.get(0),
                )
                .expect("read tenant index");
            assert_eq!(count, 1, "missing {index}");
        }
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
            .get_workflow(wf.tenant, principal, wf.id)
            .await
            .expect("get")
            .expect("present");
        assert_eq!(got, wf);
        assert!(
            store
                .get_workflow(wf.tenant, PrincipalId::new(), wf.id)
                .await
                .expect("get")
                .is_none(),
            "owner-scoped"
        );
        let by_name = store
            .get_workflow_by_name(wf.tenant, principal, &wf.name)
            .await
            .expect("get")
            .expect("present");
        assert_eq!(by_name.id, wf.id);

        let updated = store
            .update_workflow(
                wf.tenant,
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
            store
                .list_workflows(wf.tenant, principal)
                .await
                .expect("list")
                .len(),
            1
        );
        assert!(
            store
                .delete_workflow(wf.tenant, principal, wf.id)
                .await
                .expect("delete")
        );
        assert!(
            !store
                .delete_workflow(wf.tenant, principal, wf.id)
                .await
                .expect("delete")
        );
    }

    #[tokio::test]
    /// Tenant boundaries isolate every owner-scoped workflow and run operation.
    async fn same_principal_cannot_cross_tenant_boundaries() {
        let (store, _bus) = store();
        let tenant_a = TenantId::new();
        let tenant_b = TenantId::new();
        let principal = PrincipalId::new();
        let workflow_a = store
            .create_workflow(NewWorkflow {
                tenant: tenant_a,
                principal_id: principal,
                name: "tenant-a-workflow".to_string(),
                description: None,
                steps: vec![action("approve-a", &[])],
            })
            .await
            .expect("tenant A workflow");
        let workflow_b = store
            .create_workflow(NewWorkflow {
                tenant: tenant_b,
                principal_id: principal,
                name: "tenant-b-workflow".to_string(),
                description: None,
                steps: vec![action("approve-b", &[])],
            })
            .await
            .expect("tenant B workflow");
        let run_a = store
            .create_run(tenant_a, principal, workflow_a.id, None)
            .await
            .expect("tenant A run");
        let run_b = store
            .create_run(tenant_b, principal, workflow_b.id, None)
            .await
            .expect("tenant B run");
        let step_a = store
            .get_steps(tenant_a, principal, run_a.id)
            .await
            .expect("tenant A steps")
            .into_iter()
            .next()
            .expect("tenant A step");
        let step_a_started = step_a.started_at.expect("tenant A attempt start");

        assert!(
            store
                .get_workflow(tenant_b, principal, workflow_a.id)
                .await
                .expect("cross-tenant workflow lookup")
                .is_none()
        );
        assert!(
            store
                .get_workflow_by_name(tenant_b, principal, &workflow_a.name)
                .await
                .expect("cross-tenant workflow name lookup")
                .is_none()
        );
        assert_eq!(
            store
                .list_workflows(tenant_b, principal)
                .await
                .expect("tenant B workflows")
                .into_iter()
                .map(|workflow| workflow.id)
                .collect::<Vec<_>>(),
            vec![workflow_b.id]
        );
        assert!(matches!(
            store
                .update_workflow(
                    tenant_b,
                    principal,
                    workflow_a.id,
                    WorkflowPatch {
                        description: Some("forbidden".to_string()),
                        ..Default::default()
                    },
                )
                .await,
            Err(LoomError::WorkflowNotFound(id)) if id == workflow_a.id
        ));
        assert!(
            !store
                .delete_workflow(tenant_b, principal, workflow_a.id)
                .await
                .expect("cross-tenant workflow delete")
        );
        assert!(matches!(
            store
                .create_run(tenant_b, principal, workflow_a.id, None)
                .await,
            Err(LoomError::WorkflowNotFound(id)) if id == workflow_a.id
        ));

        assert!(
            store
                .get_run(tenant_b, principal, run_a.id)
                .await
                .expect("cross-tenant run lookup")
                .is_none()
        );
        assert_eq!(
            store
                .list_runs(tenant_b, principal, RunFilter::default())
                .await
                .expect("tenant B runs")
                .into_iter()
                .map(|run| run.id)
                .collect::<Vec<_>>(),
            vec![run_b.id]
        );
        assert!(matches!(
            store.cancel_run(tenant_b, principal, run_a.id).await,
            Err(LoomError::RunNotFound(id)) if id == run_a.id
        ));
        assert!(
            store
                .get_steps(tenant_b, principal, run_a.id)
                .await
                .expect("cross-tenant steps")
                .is_empty()
        );
        assert!(
            store
                .get_step(tenant_b, principal, step_a.id)
                .await
                .expect("cross-tenant step lookup")
                .is_none()
        );
        assert!(matches!(
            store
                .complete_step(
                    tenant_b,
                    principal,
                    step_a.id,
                    step_a.retry_count,
                    step_a_started,
                    serde_json::json!({"forbidden": true}),
                )
                .await,
            Err(LoomError::StepNotFound(id)) if id == step_a.id
        ));
        assert!(matches!(
            store
                .fail_step(
                    tenant_b,
                    principal,
                    step_a.id,
                    step_a.retry_count,
                    step_a_started,
                    "forbidden",
                )
                .await,
            Err(LoomError::StepNotFound(id)) if id == step_a.id
        ));
        assert!(matches!(
            store.advance_run(tenant_b, principal, run_a.id).await,
            Err(LoomError::RunNotFound(id)) if id == run_a.id
        ));
        assert!(
            store
                .logs(tenant_b, principal, run_a.id, 100)
                .await
                .expect("cross-tenant logs")
                .is_empty()
        );

        let stats_b = store
            .stats(tenant_b, principal)
            .await
            .expect("tenant B stats");
        assert_eq!(stats_b.workflows, 1);
        assert_eq!(stats_b.runs, 1);
        assert_eq!(stats_b.active_runs, 1);

        store
            .complete_step(
                tenant_a,
                principal,
                step_a.id,
                step_a.retry_count,
                step_a_started,
                serde_json::json!({"allowed": true}),
            )
            .await
            .expect("tenant A completion");
        assert_eq!(
            store
                .get_run(tenant_a, principal, run_a.id)
                .await
                .expect("tenant A run lookup")
                .expect("tenant A run")
                .status,
            RunStatus::Completed
        );
        assert_eq!(
            store
                .get_run(tenant_b, principal, run_b.id)
                .await
                .expect("tenant B run lookup")
                .expect("tenant B run")
                .status,
            RunStatus::Running
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
    /// Retry and timeout budgets accept their boundaries and reject invalid values.
    async fn step_retry_and_timeout_limits_are_enforced() {
        let (store, _bus) = store();
        let principal = PrincipalId::new();
        let workflow = |steps: Vec<StepDef>| NewWorkflow {
            tenant: TenantId::new(),
            principal_id: principal,
            name: format!("wf-{}", WorkflowId::new()),
            description: None,
            steps,
        };
        let retries = |max_retries| StepDef {
            name: "step".to_string(),
            step_type: StepType::Action,
            config: None,
            depends_on: None,
            max_retries: Some(max_retries),
            timeout_ms: None,
        };
        let timeout = |timeout_ms| StepDef {
            name: "step".to_string(),
            step_type: StepType::Action,
            config: None,
            depends_on: None,
            max_retries: None,
            timeout_ms: Some(timeout_ms),
        };

        store
            .create_workflow(workflow(vec![retries(0)]))
            .await
            .expect("zero retries");
        store
            .create_workflow(workflow(vec![retries(MAX_STEP_RETRIES)]))
            .await
            .expect("maximum retries");
        assert!(matches!(
            store
                .create_workflow(workflow(vec![retries(-1)]))
                .await
                .expect_err("negative retries"),
            LoomError::InvalidInput(_)
        ));
        assert!(matches!(
            store
                .create_workflow(workflow(vec![retries(MAX_STEP_RETRIES + 1)]))
                .await
                .expect_err("excessive retries"),
            LoomError::InvalidInput(_)
        ));

        store
            .create_workflow(workflow(vec![timeout(0)]))
            .await
            .expect("zero timeout");
        store
            .create_workflow(workflow(vec![timeout(MAX_STEP_TIMEOUT_MS)]))
            .await
            .expect("maximum timeout");
        assert!(matches!(
            store
                .create_workflow(workflow(vec![timeout(-1)]))
                .await
                .expect_err("negative timeout"),
            LoomError::InvalidInput(_)
        ));
        assert!(matches!(
            store
                .create_workflow(workflow(vec![timeout(MAX_STEP_TIMEOUT_MS + 1)]))
                .await
                .expect_err("excessive timeout"),
            LoomError::InvalidInput(_)
        ));
    }

    #[tokio::test]
    /// A chain at the depth limit is accepted and a deeper chain is rejected.
    async fn dependency_depth_limit_is_enforced() {
        let (store, _bus) = store();
        let principal = PrincipalId::new();
        let chain = |length: usize| {
            (0..length)
                .map(|index| StepDef {
                    name: format!("s{index}"),
                    step_type: StepType::Action,
                    config: None,
                    depends_on: (index > 0).then(|| vec![format!("s{}", index - 1)]),
                    max_retries: Some(0),
                    timeout_ms: None,
                })
                .collect()
        };
        let workflow = |steps| NewWorkflow {
            tenant: TenantId::new(),
            principal_id: principal,
            name: format!("wf-{}", WorkflowId::new()),
            description: None,
            steps,
        };

        store
            .create_workflow(workflow(chain(MAX_WORKFLOW_DEPTH)))
            .await
            .expect("depth boundary");
        assert!(matches!(
            store
                .create_workflow(workflow(chain(MAX_WORKFLOW_DEPTH + 1)))
                .await
                .expect_err("excessive depth"),
            LoomError::InvalidInput(_)
        ));
    }

    #[tokio::test]
    /// Workflow creation, replacement, and instantiation enforce the width limit.
    async fn workflow_width_limit_is_enforced_at_every_entry_point() {
        let (store, _bus) = store();
        let principal = PrincipalId::new();
        let wide = |count: usize| {
            (0..count)
                .map(|index| StepDef {
                    name: format!("s{index}"),
                    step_type: StepType::Action,
                    config: None,
                    depends_on: None,
                    max_retries: Some(0),
                    timeout_ms: None,
                })
                .collect::<Vec<_>>()
        };
        let new = |steps| NewWorkflow {
            tenant: TenantId::new(),
            principal_id: principal,
            name: format!("wf-{}", WorkflowId::new()),
            description: None,
            steps,
        };

        let boundary = store
            .create_workflow(new(wide(MAX_WORKFLOW_STEPS)))
            .await
            .expect("width boundary");
        assert_eq!(boundary.steps.len(), MAX_WORKFLOW_STEPS);

        let oversized = wide(MAX_WORKFLOW_STEPS + 1);
        assert!(matches!(
            store
                .create_workflow(new(oversized.clone()))
                .await
                .expect_err("oversized create"),
            LoomError::InvalidInput(_)
        ));

        let update_target = store
            .create_workflow(new(vec![action("valid", &[])]))
            .await
            .expect("update target");
        assert!(matches!(
            store
                .update_workflow(
                    update_target.tenant,
                    principal,
                    update_target.id,
                    WorkflowPatch {
                        steps: Some(oversized.clone()),
                        ..Default::default()
                    },
                )
                .await
                .expect_err("oversized update"),
            LoomError::InvalidInput(_)
        ));

        {
            let conn = store.lock();
            conn.execute(
                "UPDATE loom_workflows SET steps = ?1 WHERE id = ?2",
                rusqlite::params![
                    serde_json::to_string(&oversized).expect("serialize oversized definition"),
                    update_target.id.to_string()
                ],
            )
            .expect("persist legacy oversized definition");
        }
        assert!(matches!(
            store
                .create_run(update_target.tenant, principal, update_target.id, None)
                .await
                .expect_err("oversized legacy definition"),
            LoomError::InvalidInput(_)
        ));
    }

    #[tokio::test]
    /// Run creation rejects legacy definitions before persisting any run state.
    async fn create_run_revalidates_legacy_definitions() {
        let (store, _bus) = store();
        let (principal, workflow) = workflow_with(&store, vec![action("valid", &[])]).await;
        let oversized = vec![StepDef {
            name: "legacy".to_string(),
            step_type: StepType::Action,
            config: None,
            depends_on: None,
            max_retries: Some(MAX_STEP_RETRIES + 1),
            timeout_ms: None,
        }];
        {
            let conn = store.lock();
            conn.execute(
                "UPDATE loom_workflows SET steps = ?1 WHERE id = ?2",
                rusqlite::params![
                    serde_json::to_string(&oversized).expect("serialize oversized definition"),
                    workflow.id.to_string()
                ],
            )
            .expect("persist legacy definition");
        }

        assert!(matches!(
            store
                .create_run(workflow.tenant, principal, workflow.id, None)
                .await
                .expect_err("legacy definition"),
            LoomError::InvalidInput(_)
        ));
        let run_count: i64 = {
            let conn = store.lock();
            conn.query_row(
                "SELECT COUNT(*) FROM loom_runs WHERE workflow_id = ?1",
                rusqlite::params![workflow.id.to_string()],
                |row| row.get(0),
            )
            .expect("count runs")
        };
        assert_eq!(run_count, 0);
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
                wf.tenant,
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
        let logs = store
            .logs(run.tenant, principal, run.id, 50)
            .await
            .expect("logs");
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
        let run = store
            .create_run(wf.tenant, principal, wf.id, None)
            .await
            .expect("run");
        assert_eq!(
            run.status,
            RunStatus::Running,
            "waiting on the external step"
        );
        let _ = drain_kinds(&mut rx);

        let steps = store
            .get_steps(run.tenant, principal, run.id)
            .await
            .expect("steps");
        let approve = steps.iter().find(|s| s.name == "approve").expect("step");
        assert_eq!(approve.status, StepStatus::Running);
        let approve_started_at = approve.started_at.expect("attempt start");

        // A stranger cannot complete it.
        let err = store
            .complete_step(
                run.tenant,
                PrincipalId::new(),
                approve.id,
                approve.retry_count,
                approve_started_at,
                serde_json::json!({}),
            )
            .await
            .expect_err("foreign");
        assert!(matches!(err, LoomError::StepNotFound(_)));

        // The owner completes it; the dependent transform runs and the run finishes.
        store
            .complete_step(
                run.tenant,
                principal,
                approve.id,
                approve.retry_count,
                approve_started_at,
                serde_json::json!({"approved": true}),
            )
            .await
            .expect("complete");
        let run = store
            .get_run(run.tenant, principal, run.id)
            .await
            .expect("get")
            .expect("present");
        assert_eq!(run.status, RunStatus::Completed);
        assert_eq!(run.output["approved"], true);
        let kinds = drain_kinds(&mut rx);
        assert!(kinds.contains(&"workflow.run.completed".to_string()));
    }

    #[tokio::test]
    /// Results from an older retry cannot mutate or narrate the current attempt.
    async fn stale_attempt_results_are_rejected() {
        let (store, bus) = store();
        let mut rx = bus.subscribe("workflow");
        let (principal, workflow) = workflow_with(&store, vec![action("external", &[])]).await;
        let run = store
            .create_run(workflow.tenant, principal, workflow.id, None)
            .await
            .expect("run");
        let attempt_zero = store
            .get_steps(run.tenant, principal, run.id)
            .await
            .expect("steps")
            .into_iter()
            .next()
            .expect("attempt zero");
        let attempt_zero_started = attempt_zero.started_at.expect("attempt zero start");

        let attempt_one = store
            .fail_step(
                run.tenant,
                principal,
                attempt_zero.id,
                attempt_zero.retry_count,
                attempt_zero_started,
                "retry once",
            )
            .await
            .expect("retry");
        assert_eq!(attempt_one.status, StepStatus::Running);
        assert_eq!(attempt_one.retry_count, 1);
        let attempt_one_started = attempt_one.started_at.expect("attempt one start");
        let _ = drain_kinds(&mut rx);
        let log_count = store
            .logs(run.tenant, principal, run.id, 100)
            .await
            .expect("logs")
            .len();

        assert!(matches!(
            store
                .complete_step(
                    run.tenant,
                    principal,
                    attempt_zero.id,
                    attempt_zero.retry_count,
                    attempt_zero_started,
                    serde_json::json!({"stale": true}),
                )
                .await
                .expect_err("stale completion"),
            LoomError::InvalidInput(_)
        ));
        assert!(matches!(
            store
                .fail_step(
                    run.tenant,
                    principal,
                    attempt_zero.id,
                    attempt_zero.retry_count,
                    attempt_zero_started,
                    "stale failure",
                )
                .await
                .expect_err("stale failure"),
            LoomError::InvalidInput(_)
        ));

        let current = store
            .get_step(run.tenant, principal, attempt_zero.id)
            .await
            .expect("read current")
            .expect("current attempt");
        assert_eq!(current.status, StepStatus::Running);
        assert_eq!(current.retry_count, attempt_one.retry_count);
        assert_eq!(current.started_at, Some(attempt_one_started));
        assert_eq!(current.output, serde_json::json!({}));
        assert_eq!(
            store
                .logs(run.tenant, principal, run.id, 100)
                .await
                .expect("logs")
                .len(),
            log_count
        );
        assert!(drain_kinds(&mut rx).is_empty());

        let completed = store
            .complete_step(
                run.tenant,
                principal,
                current.id,
                current.retry_count,
                attempt_one_started,
                serde_json::json!({"current": true}),
            )
            .await
            .expect("current completion");
        assert_eq!(completed.status, StepStatus::Completed);
    }

    #[tokio::test]
    /// Completing a non running step is rejected.
    async fn completing_a_non_running_step_is_rejected() {
        let (store, _bus) = store();
        let (principal, wf) =
            workflow_with(&store, vec![action("a", &[]), action("b", &["a"])]).await;
        let run = store
            .create_run(wf.tenant, principal, wf.id, None)
            .await
            .expect("run");
        let steps = store
            .get_steps(run.tenant, principal, run.id)
            .await
            .expect("steps");
        let b = steps.iter().find(|s| s.name == "b").expect("step");
        assert_eq!(b.status, StepStatus::Pending, "deps unmet");
        let err = store
            .complete_step(
                run.tenant,
                principal,
                b.id,
                b.retry_count,
                Timestamp::now(),
                serde_json::json!({}),
            )
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

    /// An executor that consumes the full retry budget before succeeding.
    struct BoundaryRetryExecutor {
        /// Attempt count keyed by step name.
        attempts: Arc<Mutex<HashMap<String, usize>>>,
    }

    #[async_trait]
    /// Implements deterministic retry-boundary execution for transform steps.
    impl StepExecutor for BoundaryRetryExecutor {
        /// Claim transform steps used by the boundary test.
        fn handles(&self, step_type: StepType) -> bool {
            step_type == StepType::Transform
        }

        /// Fail the first maximum-budget attempts for each step, then succeed.
        async fn execute(&self, ctx: StepContext<'_>) -> Result<serde_json::Value, String> {
            let attempt = {
                let mut attempts = self
                    .attempts
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                let count = attempts.entry(ctx.name.to_string()).or_default();
                *count += 1;
                *count
            };
            let retry_limit =
                usize::try_from(MAX_STEP_RETRIES).expect("retry limit must be non-negative");
            if attempt <= retry_limit {
                Err(format!("boundary retry {attempt}"))
            } else {
                Ok(serde_json::json!({ "completed_step": ctx.name }))
            }
        }
    }

    /// An executor that records names and fails the first ready step.
    struct FailFirstExecutor {
        /// Names passed to execution, in order.
        executed: Arc<Mutex<Vec<String>>>,
    }

    #[async_trait]
    /// Implements deterministic terminal-batch behavior for transform steps.
    impl StepExecutor for FailFirstExecutor {
        /// Claim transform steps used by the terminal-batch test.
        fn handles(&self, step_type: StepType) -> bool {
            step_type == StepType::Transform
        }

        /// Record the step and fail only the step named `fail`.
        async fn execute(&self, ctx: StepContext<'_>) -> Result<serde_json::Value, String> {
            self.executed
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .push(ctx.name.to_string());
            if ctx.name == "fail" {
                Err("terminal failure".to_string())
            } else {
                Ok(serde_json::json!({ "unexpected": ctx.name }))
            }
        }
    }

    #[tokio::test]
    /// Iterative advancement completes the exact maximum depth and retry boundaries.
    async fn iterative_advance_handles_maximum_depth_and_retries() {
        let bus = Arc::new(AxonBus::new());
        let attempts = Arc::new(Mutex::new(HashMap::new()));
        let store = LoomStore::open_in_memory(bus)
            .expect("open")
            .with_executor(Box::new(BoundaryRetryExecutor {
                attempts: attempts.clone(),
            }));
        let principal = PrincipalId::new();
        let steps = (0..MAX_WORKFLOW_DEPTH)
            .map(|index| StepDef {
                name: format!("s{index}"),
                step_type: StepType::Transform,
                config: None,
                depends_on: (index > 0).then(|| vec![format!("s{}", index - 1)]),
                max_retries: Some(MAX_STEP_RETRIES),
                timeout_ms: None,
            })
            .collect();
        let workflow = store
            .create_workflow(NewWorkflow {
                tenant: TenantId::new(),
                principal_id: principal,
                name: "boundary-workflow".to_string(),
                description: None,
                steps,
            })
            .await
            .expect("create workflow");

        let run = store
            .create_run(workflow.tenant, principal, workflow.id, None)
            .await
            .expect("execute workflow");
        assert_eq!(run.status, RunStatus::Completed);
        let persisted = store
            .get_steps(run.tenant, principal, run.id)
            .await
            .expect("read steps");
        assert!(persisted.iter().all(|step| {
            step.status == StepStatus::Completed && step.retry_count == MAX_STEP_RETRIES
        }));
        let attempts = attempts.lock().unwrap_or_else(|error| error.into_inner());
        let expected =
            usize::try_from(MAX_STEP_RETRIES).expect("retry limit must be non-negative") + 1;
        assert_eq!(
            attempts.values().sum::<usize>(),
            MAX_WORKFLOW_DEPTH * expected
        );
    }

    #[tokio::test]
    /// A terminal inline failure prevents later ready work from executing.
    async fn terminal_failure_stops_ready_batch() {
        let bus = Arc::new(AxonBus::new());
        let executed = Arc::new(Mutex::new(Vec::new()));
        let store = LoomStore::open_in_memory(bus)
            .expect("open")
            .with_executor(Box::new(FailFirstExecutor {
                executed: executed.clone(),
            }));
        let principal = PrincipalId::new();
        let workflow = store
            .create_workflow(NewWorkflow {
                tenant: TenantId::new(),
                principal_id: principal,
                name: "terminal-batch".to_string(),
                description: None,
                steps: vec![
                    StepDef {
                        name: "fail".to_string(),
                        step_type: StepType::Transform,
                        config: None,
                        depends_on: None,
                        max_retries: Some(0),
                        timeout_ms: None,
                    },
                    StepDef {
                        name: "must-not-run".to_string(),
                        step_type: StepType::Transform,
                        config: None,
                        depends_on: None,
                        max_retries: Some(0),
                        timeout_ms: None,
                    },
                ],
            })
            .await
            .expect("create workflow");

        let run = store
            .create_run(workflow.tenant, principal, workflow.id, None)
            .await
            .expect("execute workflow");
        assert_eq!(run.status, RunStatus::Failed);
        assert_eq!(
            *executed.lock().unwrap_or_else(|error| error.into_inner()),
            vec!["fail".to_string()]
        );
        let steps = store
            .get_steps(run.tenant, principal, run.id)
            .await
            .expect("read steps");
        assert_eq!(steps[0].status, StepStatus::Failed);
        assert_eq!(steps[1].status, StepStatus::Skipped);
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
        let run = store
            .create_run(wf.tenant, principal, wf.id, None)
            .await
            .expect("run");
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
        let steps = store
            .get_steps(run.tenant, principal, run.id)
            .await
            .expect("steps");
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
        let run = store
            .create_run(wf.tenant, principal, wf.id, None)
            .await
            .expect("run");
        let _ = drain_kinds(&mut rx);

        assert!(
            store
                .cancel_run(run.tenant, principal, run.id)
                .await
                .expect("cancel")
        );
        assert_eq!(drain_kinds(&mut rx), ["workflow.run.cancelled"]);
        let run = store
            .get_run(run.tenant, principal, run.id)
            .await
            .expect("get")
            .expect("present");
        assert_eq!(run.status, RunStatus::Cancelled);
        let steps = store
            .get_steps(run.tenant, principal, run.id)
            .await
            .expect("steps");
        assert!(steps.iter().all(|s| s.status == StepStatus::Skipped));
        // Cancelling again is a no-op.
        assert!(
            !store
                .cancel_run(run.tenant, principal, run.id)
                .await
                .expect("cancel")
        );
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
        let run = store
            .create_run(wf.tenant, principal, wf.id, None)
            .await
            .expect("run");
        assert_eq!(run.status, RunStatus::Running);
        let steps = store
            .get_steps(run.tenant, principal, run.id)
            .await
            .expect("steps");
        let running = steps
            .into_iter()
            .find(|s| s.status == StepStatus::Running)
            .expect("a running step");

        // Concurrent cancel: the running step becomes 'skipped'.
        assert!(
            store
                .cancel_run(run.tenant, principal, run.id)
                .await
                .expect("cancel")
        );

        // Completing with the now-stale 'running' snapshot must be refused.
        let result = store
            .complete_step_inner(
                &running,
                &run,
                running.retry_count,
                running.started_at.expect("attempt start"),
                serde_json::json!({"ok": true}),
            )
            .expect("stale result");
        assert_eq!(result, StepTransition::Stale);

        // The step must remain skipped, not flipped to completed.
        let steps = store
            .get_steps(run.tenant, principal, run.id)
            .await
            .expect("steps");
        let s = steps.iter().find(|s| s.id == running.id).expect("step");
        assert_eq!(s.status, StepStatus::Skipped);
    }

    #[tokio::test]
    /// Lifecycle mutations roll back when their required log insertion fails.
    async fn lifecycle_state_and_logs_commit_atomically() {
        let (store, bus) = store();
        let mut rx = bus.subscribe("workflow");
        let principal = PrincipalId::new();
        let workflow = store
            .create_workflow(NewWorkflow {
                tenant: TenantId::new(),
                principal_id: principal,
                name: "atomic-lifecycle".to_string(),
                description: None,
                steps: ["first", "second"]
                    .into_iter()
                    .map(|name| StepDef {
                        name: name.to_string(),
                        step_type: StepType::Action,
                        config: None,
                        depends_on: None,
                        max_retries: Some(0),
                        timeout_ms: None,
                    })
                    .collect(),
            })
            .await
            .expect("workflow");
        let run = store
            .create_run(workflow.tenant, principal, workflow.id, None)
            .await
            .expect("run");
        let first = store
            .get_steps(run.tenant, principal, run.id)
            .await
            .expect("steps")
            .into_iter()
            .next()
            .expect("first step");
        let first_started = first.started_at.expect("attempt start");
        let initial_log_count = store
            .logs(run.tenant, principal, run.id, 100)
            .await
            .expect("logs")
            .len();
        let _ = drain_kinds(&mut rx);

        {
            let conn = store.lock();
            conn.execute_batch(
                "CREATE TEMP TRIGGER reject_loom_logs \
                 BEFORE INSERT ON loom_logs \
                 BEGIN SELECT RAISE(FAIL, 'forced log failure'); END;",
            )
            .expect("install log failure trigger");
        }
        assert!(matches!(
            store
                .complete_step(
                    run.tenant,
                    principal,
                    first.id,
                    first.retry_count,
                    first_started,
                    serde_json::json!({"must": "rollback"}),
                )
                .await
                .expect_err("completion log failure"),
            LoomError::Backend(_)
        ));
        {
            let conn = store.lock();
            conn.execute_batch("DROP TRIGGER reject_loom_logs;")
                .expect("remove log failure trigger");
        }

        let after_completion = store
            .get_step(run.tenant, principal, first.id)
            .await
            .expect("read step")
            .expect("step");
        assert_eq!(after_completion.status, StepStatus::Running);
        assert_eq!(after_completion.output, serde_json::json!({}));

        {
            let conn = store.lock();
            conn.execute_batch(
                "CREATE TEMP TRIGGER reject_loom_logs \
                 BEFORE INSERT ON loom_logs \
                 BEGIN SELECT RAISE(FAIL, 'forced log failure'); END;",
            )
            .expect("install log failure trigger");
        }
        assert!(matches!(
            store
                .fail_step(
                    run.tenant,
                    principal,
                    first.id,
                    first.retry_count,
                    first_started,
                    "must rollback",
                )
                .await
                .expect_err("failure log failure"),
            LoomError::Backend(_)
        ));
        {
            let conn = store.lock();
            conn.execute_batch("DROP TRIGGER reject_loom_logs;")
                .expect("remove log failure trigger");
        }

        let persisted_run = store
            .get_run(run.tenant, principal, run.id)
            .await
            .expect("read run")
            .expect("run");
        assert_eq!(persisted_run.status, RunStatus::Running);
        let persisted_steps = store
            .get_steps(run.tenant, principal, run.id)
            .await
            .expect("steps");
        assert!(
            persisted_steps
                .iter()
                .all(|step| step.status == StepStatus::Running)
        );
        assert_eq!(
            store
                .logs(run.tenant, principal, run.id, 100)
                .await
                .expect("logs")
                .len(),
            initial_log_count
        );
        assert!(drain_kinds(&mut rx).is_empty());
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
        let run = store
            .create_run(wf.tenant, principal, wf.id, None)
            .await
            .expect("run");
        assert_eq!(run.status, RunStatus::Running);

        let timed_out = store.sweep_timeouts().await.expect("sweep");
        assert_eq!(timed_out.len(), 1);
        assert_eq!(timed_out[0].status, StepStatus::Failed);
        let run = store
            .get_run(run.tenant, principal, run.id)
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
    /// A stale timeout candidate does not prevent a later overdue attempt from failing.
    async fn timeout_sweep_continues_after_stale_candidate() {
        let (store, _bus) = store();
        let principal = PrincipalId::new();
        let workflow = store
            .create_workflow(NewWorkflow {
                tenant: TenantId::new(),
                principal_id: principal,
                name: "timeout-candidates".to_string(),
                description: None,
                steps: vec![StepDef {
                    name: "wait".to_string(),
                    step_type: StepType::Wait,
                    config: None,
                    depends_on: None,
                    max_retries: Some(0),
                    timeout_ms: Some(0),
                }],
            })
            .await
            .expect("workflow");
        let stale_run = store
            .create_run(workflow.tenant, principal, workflow.id, None)
            .await
            .expect("stale run");
        let live_run = store
            .create_run(workflow.tenant, principal, workflow.id, None)
            .await
            .expect("live run");
        let stale_step = store
            .get_steps(stale_run.tenant, principal, stale_run.id)
            .await
            .expect("stale steps")
            .into_iter()
            .next()
            .expect("stale step");
        let live_step = store
            .get_steps(live_run.tenant, principal, live_run.id)
            .await
            .expect("live steps")
            .into_iter()
            .next()
            .expect("live step");
        let sweep_time = Timestamp::from_utc(
            live_step
                .started_at
                .expect("live attempt start")
                .as_offset_date_time()
                + time::Duration::nanoseconds(1),
        );

        assert!(
            store
                .cancel_run(stale_run.tenant, principal, stale_run.id)
                .await
                .expect("cancel stale run")
        );
        let timed_out = store
            .sweep_timeout_candidates(
                vec![(stale_step, stale_run), (live_step.clone(), live_run)],
                sweep_time,
            )
            .await
            .expect("apply timeout candidates");

        assert_eq!(timed_out.len(), 1);
        assert_eq!(timed_out[0].id, live_step.id);
        assert_eq!(timed_out[0].status, StepStatus::Failed);
    }

    #[tokio::test]
    /// List runs filters and stats count.
    async fn list_runs_filters_and_stats_count() {
        let (store, _bus) = store();
        let (principal, wf) = workflow_with(&store, vec![action("a", &[])]).await;
        let r1 = store
            .create_run(wf.tenant, principal, wf.id, None)
            .await
            .expect("run");
        store
            .create_run(wf.tenant, principal, wf.id, None)
            .await
            .expect("run");
        store
            .cancel_run(r1.tenant, principal, r1.id)
            .await
            .expect("cancel");

        let all = store
            .list_runs(wf.tenant, principal, RunFilter::default())
            .await
            .expect("list");
        assert_eq!(all.len(), 2);
        let cancelled = store
            .list_runs(
                wf.tenant,
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
                .list_runs(wf.tenant, PrincipalId::new(), RunFilter::default())
                .await
                .expect("list")
                .is_empty()
        );

        let stats = store.stats(wf.tenant, principal).await.expect("stats");
        assert_eq!(stats.workflows, 1);
        assert_eq!(stats.runs, 2);
        assert_eq!(stats.active_runs, 1);
        assert_eq!(stats.runs_by_status.get("cancelled"), Some(&1));
    }

    #[tokio::test]
    /// Runs persist across reopen.
    async fn runs_persist_across_reopen() {
        let root = std::env::temp_dir().join(format!("henosis-loom-{}", RunId::new()));
        let tmp = root.join("state").join("loom.sqlite");
        let tenant = TenantId::new();
        let principal = PrincipalId::new();
        let run_id;
        {
            let store = LoomStore::open(&tmp, Arc::new(AxonBus::new()))
                .expect("open")
                .with_executor(Box::new(TransformExecutor));
            let wf = store
                .create_workflow(NewWorkflow {
                    tenant,
                    principal_id: principal,
                    name: "durable".to_string(),
                    description: None,
                    steps: vec![transform("t", &[], serde_json::json!({}))],
                })
                .await
                .expect("workflow");
            run_id = store
                .create_run(tenant, principal, wf.id, None)
                .await
                .expect("run")
                .id;
        }
        {
            let store = LoomStore::open(&tmp, Arc::new(AxonBus::new())).expect("reopen");
            let got = store
                .get_run(tenant, principal, run_id)
                .await
                .expect("get")
                .expect("present after reopen");
            assert_eq!(got.status, RunStatus::Completed);
        }
        let _ = std::fs::remove_dir_all(&root);
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
            .get_workflow(wf.tenant, principal, wf.id)
            .await
            .expect("get")
            .expect("present");
        assert_eq!(
            reloaded.steps[0].step_type,
            StepType::Hephaestus,
            "step_type round-trips through serde + SQLite"
        );

        // Create a run: the engine starts the step (no executor claims it, so it stays Running).
        let run = store
            .create_run(wf.tenant, principal, wf.id, None)
            .await
            .expect("run");
        assert_eq!(run.status, RunStatus::Running, "waiting on unclaimed step");

        // Verify the step instance row carries the hephaestus token.
        let steps = store
            .get_steps(run.tenant, principal, run.id)
            .await
            .expect("steps");
        assert_eq!(steps.len(), 1);
        assert_eq!(
            steps[0].step_type,
            StepType::Hephaestus,
            "step instance persists hephaestus type"
        );
        assert_eq!(steps[0].status, StepStatus::Running);
    }
}
