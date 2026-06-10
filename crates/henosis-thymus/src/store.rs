//! The SQLite-backed Thymus quality store.
//!
//! Reimplements the Kleos thymus service (`kleos-lib/src/services/thymus.rs`) against the
//! Henosis substrate: the evaluated agent and the evaluator are [`PrincipalId`]s (replacing
//! stringly `agent` fields inside a `user_id` shard), rubric criteria are typed, quality
//! events are typed and published to the in-process [`AxonBus`], and schema is managed by the
//! kernel-crate migration convention. Concurrency: one `Connection` behind a `Mutex`, the
//! established pattern.
//!
//! The Soma linkage goes through the [`QualitySink`] seam: after an evaluation, the agent's
//! rolling average propagates to the sink; after a drift event, the agent's current distinct
//! drift-type tokens do. The server adapts `SomaStore::update_quality` to the trait at wiring
//! time, so neither kernel crate depends on the other. Propagation is fire-and-forget: the
//! evaluation row is the record, the presence projection is a cache of it.
//!
//! NOT ported in this slice: Kleos session-quality rows (coupled to Kleos sessions; they
//! arrive with the Eidolon supervisor in Phase 2) and the LLM judge (parallel track T1, which
//! stays in Kleos until the cutover).

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};

use async_trait::async_trait;
use rusqlite::{Connection, OptionalExtension};
use syntheos_axon::AxonBus;
use syntheos_contracts::{PrincipalId, TenantId, Timestamp, TypedEvent};

use crate::error::ThymusError;
use crate::events::{DriftDetected, EvaluationCompleted, MetricRecorded};
use crate::model::{
    AgentScores, Criterion, DriftEvent, DriftSeverity, DriftType, Evaluation, EvaluationFilter,
    MetricSummary, NewDriftEvent, NewEvaluation, NewMetric, NewRubric, QualityMetric, Rubric,
    RubricPatch, ThymusStats,
};

/// Ordered schema migrations, applied by `PRAGMA user_version`. Append-only (see the DB convention).
const MIGRATIONS: &[(i64, &str)] = &[(1, include_str!("../migrations/V1__thymus_quality.sql"))];

/// Where evaluation/drift outcomes propagate to (Soma's presence projection, at wiring time).
///
/// Implementations must be cheap and tolerant: propagation is fire-and-forget, and an error
/// only produces a warning log -- the Thymus row is the record.
#[async_trait]
pub trait QualitySink: Send + Sync {
    /// Apply a quality update for `agent`: a new rolling score and/or replacement drift flags.
    async fn apply(
        &self,
        agent: PrincipalId,
        quality_score: Option<f64>,
        drift_flags: Option<Vec<String>>,
    ) -> Result<(), String>;
}

/// The quality store.
///
/// Share it as `Arc<ThymusStore>`; all methods take `&self`.
pub struct ThymusStore {
    /// The one connection, serialized by a `Mutex` (rusqlite `Connection` is `Send`, not `Sync`).
    conn: Mutex<Connection>,
    /// The bus quality events are published onto.
    bus: Arc<AxonBus>,
    /// The optional propagation seam. `None` = evaluations stay Thymus-local.
    sink: Option<Box<dyn QualitySink>>,
}

/// Map a generic rusqlite error to an opaque backend error.
fn berr(e: rusqlite::Error) -> ThymusError {
    ThymusError::Backend(e.to_string())
}

/// Serialize a [`Timestamp`] to its stored RFC3339-UTC string (via the contracts wire form).
fn ts_to_db(ts: &Timestamp) -> Result<String, ThymusError> {
    serde_json::to_value(ts)
        .ok()
        .and_then(|v| v.as_str().map(String::from))
        .ok_or_else(|| ThymusError::Backend("timestamp serialize".to_string()))
}

/// Parse a stored RFC3339 string back into a UTC-normalized [`Timestamp`].
fn ts_from_db(s: &str) -> Result<Timestamp, ThymusError> {
    serde_json::from_value(serde_json::Value::String(s.to_string()))
        .map_err(|e| ThymusError::Backend(format!("timestamp parse {s:?}: {e}")))
}

/// Parse a stored PrincipalId string.
fn pid_from_db(s: &str) -> Result<PrincipalId, ThymusError> {
    s.parse::<PrincipalId>()
        .map_err(|e| ThymusError::Backend(format!("corrupt principal {s:?}: {e}")))
}

/// Parse a stored TenantId string.
fn tid_from_db(s: &str) -> Result<TenantId, ThymusError> {
    s.parse::<TenantId>()
        .map_err(|e| ThymusError::Backend(format!("corrupt tenant {s:?}: {e}")))
}

/// Validate rubric criteria: non-empty, unique non-empty names, positive weights, sane scales.
fn validate_criteria(criteria: &[Criterion]) -> Result<(), ThymusError> {
    if criteria.is_empty() {
        return Err(ThymusError::InvalidInput("rubric has no criteria".to_string()));
    }
    let mut names = std::collections::HashSet::new();
    for c in criteria {
        if c.name.trim().is_empty() {
            return Err(ThymusError::InvalidInput("empty criterion name".to_string()));
        }
        if !names.insert(c.name.as_str()) {
            return Err(ThymusError::InvalidInput(format!(
                "duplicate criterion name {:?}",
                c.name
            )));
        }
        if c.weight <= 0.0 {
            return Err(ThymusError::InvalidInput(format!(
                "criterion {:?} weight must be positive",
                c.name
            )));
        }
        if c.scale_max <= c.scale_min {
            return Err(ThymusError::InvalidInput(format!(
                "criterion {:?} scale_max must exceed scale_min",
                c.name
            )));
        }
    }
    Ok(())
}

/// Compute the weighted overall score (the Kleos formula): per criterion, normalize the raw
/// score into its scale, multiply by weight, and divide the sum by the total weight. Every
/// criterion must have a score.
fn compute_weighted_score(
    criteria: &[Criterion],
    scores: &BTreeMap<String, f64>,
) -> Result<f64, ThymusError> {
    let mut weighted_sum = 0.0;
    let mut total_weight = 0.0;
    for c in criteria {
        let raw = scores.get(&c.name).copied().ok_or_else(|| {
            ThymusError::InvalidInput(format!("missing score for criterion {:?}", c.name))
        })?;
        let normalized = (raw - c.scale_min) / (c.scale_max - c.scale_min);
        weighted_sum += normalized * c.weight;
        total_weight += c.weight;
    }
    Ok(weighted_sum / total_weight)
}

/// The raw column values of one `thymus_rubrics` row.
struct RawRubric {
    /// Rubric id.
    id: i64,
    /// TenantId string.
    tenant: String,
    /// Owner PrincipalId string.
    principal_id: String,
    /// Rubric name.
    name: String,
    /// Optional description.
    description: Option<String>,
    /// Criterion JSON array text.
    criteria: String,
    /// Creation time (RFC3339).
    created_at: String,
    /// Last-modification time (RFC3339).
    updated_at: String,
}

/// Read a `thymus_rubrics` row positionally.
fn read_raw_rubric(row: &rusqlite::Row) -> rusqlite::Result<RawRubric> {
    Ok(RawRubric {
        id: row.get(0)?,
        tenant: row.get(1)?,
        principal_id: row.get(2)?,
        name: row.get(3)?,
        description: row.get(4)?,
        criteria: row.get(5)?,
        created_at: row.get(6)?,
        updated_at: row.get(7)?,
    })
}

impl RawRubric {
    /// Parse raw columns into a typed [`Rubric`].
    fn into_rubric(self) -> Result<Rubric, ThymusError> {
        Ok(Rubric {
            id: self.id,
            tenant: tid_from_db(&self.tenant)?,
            principal_id: pid_from_db(&self.principal_id)?,
            name: self.name,
            description: self.description,
            criteria: serde_json::from_str(&self.criteria)
                .map_err(|e| ThymusError::Backend(format!("corrupt criteria: {e}")))?,
            created_at: ts_from_db(&self.created_at)?,
            updated_at: ts_from_db(&self.updated_at)?,
        })
    }
}

/// The columns of `thymus_rubrics`, in [`read_raw_rubric`] order.
const RUBRIC_COLUMNS: &str =
    "id, tenant, principal_id, name, description, criteria, created_at, updated_at";

/// The columns of `thymus_evaluations`, in [`read_raw_evaluation`] order.
const EVALUATION_COLUMNS: &str = "id, rubric_id, tenant, principal_id, agent, evaluator, \
    subject, input, output, scores, overall_score, notes, created_at";

/// The raw column values of one `thymus_evaluations` row.
struct RawEvaluation {
    /// Evaluation id.
    id: i64,
    /// Rubric id.
    rubric_id: i64,
    /// TenantId string.
    tenant: String,
    /// Owner PrincipalId string.
    principal_id: String,
    /// Evaluated agent PrincipalId string.
    agent: String,
    /// Evaluator PrincipalId string.
    evaluator: String,
    /// Subject text.
    subject: String,
    /// Input JSON text.
    input: String,
    /// Output JSON text.
    output: String,
    /// Scores JSON text.
    scores: String,
    /// Weighted overall score.
    overall_score: f64,
    /// Optional notes.
    notes: Option<String>,
    /// Creation time (RFC3339).
    created_at: String,
}

/// Read a `thymus_evaluations` row positionally.
fn read_raw_evaluation(row: &rusqlite::Row) -> rusqlite::Result<RawEvaluation> {
    Ok(RawEvaluation {
        id: row.get(0)?,
        rubric_id: row.get(1)?,
        tenant: row.get(2)?,
        principal_id: row.get(3)?,
        agent: row.get(4)?,
        evaluator: row.get(5)?,
        subject: row.get(6)?,
        input: row.get(7)?,
        output: row.get(8)?,
        scores: row.get(9)?,
        overall_score: row.get(10)?,
        notes: row.get(11)?,
        created_at: row.get(12)?,
    })
}

impl RawEvaluation {
    /// Parse raw columns into a typed [`Evaluation`].
    fn into_evaluation(self) -> Result<Evaluation, ThymusError> {
        Ok(Evaluation {
            id: self.id,
            rubric_id: self.rubric_id,
            tenant: tid_from_db(&self.tenant)?,
            principal_id: pid_from_db(&self.principal_id)?,
            agent: pid_from_db(&self.agent)?,
            evaluator: pid_from_db(&self.evaluator)?,
            subject: self.subject,
            input: serde_json::from_str(&self.input)
                .map_err(|e| ThymusError::Backend(format!("corrupt input: {e}")))?,
            output: serde_json::from_str(&self.output)
                .map_err(|e| ThymusError::Backend(format!("corrupt output: {e}")))?,
            scores: serde_json::from_str(&self.scores)
                .map_err(|e| ThymusError::Backend(format!("corrupt scores: {e}")))?,
            overall_score: self.overall_score,
            notes: self.notes,
            created_at: ts_from_db(&self.created_at)?,
        })
    }
}

impl ThymusStore {
    /// Open (creating the file if absent) a store at `path`, applying any pending migrations.
    /// No sink is attached; see [`Self::with_quality_sink`].
    pub fn open(path: impl AsRef<Path>, bus: Arc<AxonBus>) -> Result<Self, ThymusError> {
        let conn = Connection::open(path).map_err(berr)?;
        Self::from_conn(conn, bus)
    }

    /// Open an ephemeral in-memory store. For tests and throwaway use.
    pub fn open_in_memory(bus: Arc<AxonBus>) -> Result<Self, ThymusError> {
        let conn = Connection::open_in_memory().map_err(berr)?;
        Self::from_conn(conn, bus)
    }

    /// Attach the [`QualitySink`] evaluation/drift outcomes propagate to (the server adapts
    /// `SomaStore::update_quality` to it). Builder-style, used at wiring time.
    pub fn with_quality_sink(mut self, sink: Box<dyn QualitySink>) -> Self {
        self.sink = Some(sink);
        self
    }

    /// Enable foreign keys, apply migrations, and wrap the connection.
    fn from_conn(mut conn: Connection, bus: Arc<AxonBus>) -> Result<Self, ThymusError> {
        conn.pragma_update(None, "foreign_keys", true).map_err(berr)?;
        apply_migrations(&mut conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
            bus,
            sink: None,
        })
    }

    /// Lock the connection, recovering from a poisoned mutex.
    fn lock(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Publish a quality event, fire-and-forget. A publish failure is logged, never fatal.
    fn emit<E: TypedEvent>(&self, event: &E, tenant: TenantId, principal: PrincipalId) {
        if let Err(e) = self.bus.publish_event(event, tenant, principal) {
            tracing::warn!(error = %e, kind = E::KIND, "failed to publish thymus quality event");
        }
    }

    /// Propagate to the sink, fire-and-forget (a failure is a warning; the row is the record).
    async fn propagate(
        &self,
        agent: PrincipalId,
        quality_score: Option<f64>,
        drift_flags: Option<Vec<String>>,
    ) {
        if let Some(sink) = &self.sink {
            if let Err(e) = sink.apply(agent, quality_score, drift_flags).await {
                tracing::warn!(error = %e, agent = %agent, "quality sink propagation failed");
            }
        }
    }

    /// Define a new rubric (criteria validated).
    pub async fn create_rubric(&self, new: NewRubric) -> Result<Rubric, ThymusError> {
        if new.name.trim().is_empty() {
            return Err(ThymusError::InvalidInput("rubric name required".to_string()));
        }
        validate_criteria(&new.criteria)?;
        let now = Timestamp::now();
        let conn = self.lock();
        conn.execute(
            "INSERT INTO thymus_rubrics \
             (tenant, principal_id, name, description, criteria, created_at, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
            rusqlite::params![
                new.tenant.to_string(),
                new.principal_id.to_string(),
                &new.name,
                &new.description,
                serde_json::to_string(&new.criteria)
                    .map_err(|e| ThymusError::Backend(format!("criteria serialize: {e}")))?,
                ts_to_db(&now)?,
            ],
        )
        .map_err(|e| match &e {
            rusqlite::Error::SqliteFailure(f, Some(msg))
                if f.code == rusqlite::ErrorCode::ConstraintViolation
                    && msg.contains("thymus_rubrics.name") =>
            {
                ThymusError::InvalidInput(format!("rubric name already exists: {:?}", new.name))
            }
            _ => berr(e),
        })?;
        Ok(Rubric {
            id: conn.last_insert_rowid(),
            tenant: new.tenant,
            principal_id: new.principal_id,
            name: new.name,
            description: new.description,
            criteria: new.criteria,
            created_at: now,
            updated_at: now,
        })
    }

    /// Look up an owned rubric by id. `Ok(None)` if absent or owned by another principal.
    pub async fn get_rubric(
        &self,
        principal: PrincipalId,
        id: i64,
    ) -> Result<Option<Rubric>, ThymusError> {
        let conn = self.lock();
        Self::get_rubric_in(&conn, principal, id)
    }

    /// Owner-scoped rubric lookup against an arbitrary connection.
    fn get_rubric_in(
        conn: &Connection,
        principal: PrincipalId,
        id: i64,
    ) -> Result<Option<Rubric>, ThymusError> {
        let raw = conn
            .query_row(
                &format!(
                    "SELECT {RUBRIC_COLUMNS} FROM thymus_rubrics \
                     WHERE id = ?1 AND principal_id = ?2"
                ),
                rusqlite::params![id, principal.to_string()],
                read_raw_rubric,
            )
            .optional()
            .map_err(berr)?;
        raw.map(RawRubric::into_rubric).transpose()
    }

    /// List a principal's rubrics, newest-updated first.
    pub async fn list_rubrics(&self, principal: PrincipalId) -> Result<Vec<Rubric>, ThymusError> {
        let conn = self.lock();
        let mut stmt = conn
            .prepare(&format!(
                "SELECT {RUBRIC_COLUMNS} FROM thymus_rubrics \
                 WHERE principal_id = ?1 ORDER BY updated_at DESC"
            ))
            .map_err(berr)?;
        let rows = stmt
            .query_map(rusqlite::params![principal.to_string()], read_raw_rubric)
            .map_err(berr)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(berr)?.into_rubric()?);
        }
        Ok(out)
    }

    /// Apply a partial update to an owned rubric (replacement criteria re-validated).
    pub async fn update_rubric(
        &self,
        principal: PrincipalId,
        id: i64,
        patch: RubricPatch,
    ) -> Result<Rubric, ThymusError> {
        if let Some(criteria) = &patch.criteria {
            validate_criteria(criteria)?;
        }
        let conn = self.lock();
        let mut rubric =
            Self::get_rubric_in(&conn, principal, id)?.ok_or(ThymusError::RubricNotFound(id))?;
        if let Some(name) = patch.name {
            rubric.name = name;
        }
        if let Some(description) = patch.description {
            rubric.description = Some(description);
        }
        if let Some(criteria) = patch.criteria {
            rubric.criteria = criteria;
        }
        rubric.updated_at = Timestamp::now();
        conn.execute(
            "UPDATE thymus_rubrics SET name = ?1, description = ?2, criteria = ?3, updated_at = ?4 \
             WHERE id = ?5 AND principal_id = ?6",
            rusqlite::params![
                &rubric.name,
                &rubric.description,
                serde_json::to_string(&rubric.criteria)
                    .map_err(|e| ThymusError::Backend(format!("criteria serialize: {e}")))?,
                ts_to_db(&rubric.updated_at)?,
                id,
                principal.to_string(),
            ],
        )
        .map_err(berr)?;
        Ok(rubric)
    }

    /// Delete an owned rubric. Returns whether a row was removed;
    /// [`ThymusError::RubricInUse`] if evaluations reference it (they are the audit record).
    pub async fn delete_rubric(&self, principal: PrincipalId, id: i64) -> Result<bool, ThymusError> {
        let conn = self.lock();
        conn.execute(
            "DELETE FROM thymus_rubrics WHERE id = ?1 AND principal_id = ?2",
            rusqlite::params![id, principal.to_string()],
        )
        .map(|n| n > 0)
        .map_err(|e| match &e {
            rusqlite::Error::SqliteFailure(f, _)
                if f.code == rusqlite::ErrorCode::ConstraintViolation =>
            {
                ThymusError::RubricInUse(id)
            }
            _ => berr(e),
        })
    }

    /// Record an evaluation: compute the weighted overall score against the owned rubric,
    /// persist, emit `evaluation.completed`, and propagate the agent's NEW rolling average to
    /// the quality sink (fire-and-forget).
    pub async fn evaluate(&self, new: NewEvaluation) -> Result<Evaluation, ThymusError> {
        let now = Timestamp::now();
        let (evaluation, rolling_avg) = {
            let conn = self.lock();
            let rubric = Self::get_rubric_in(&conn, new.principal_id, new.rubric_id)?
                .ok_or(ThymusError::RubricNotFound(new.rubric_id))?;
            let overall_score = compute_weighted_score(&rubric.criteria, &new.scores)?;
            let input = new.input.unwrap_or_else(|| serde_json::json!({}));
            let output = new.output.unwrap_or_else(|| serde_json::json!({}));
            conn.execute(
                "INSERT INTO thymus_evaluations \
                 (rubric_id, tenant, principal_id, agent, evaluator, subject, input, output, \
                  scores, overall_score, notes, created_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                rusqlite::params![
                    new.rubric_id,
                    rubric.tenant.to_string(),
                    new.principal_id.to_string(),
                    new.agent.to_string(),
                    new.evaluator.to_string(),
                    &new.subject,
                    input.to_string(),
                    output.to_string(),
                    serde_json::to_string(&new.scores)
                        .map_err(|e| ThymusError::Backend(format!("scores serialize: {e}")))?,
                    overall_score,
                    &new.notes,
                    ts_to_db(&now)?,
                ],
            )
            .map_err(berr)?;
            let evaluation = Evaluation {
                id: conn.last_insert_rowid(),
                rubric_id: new.rubric_id,
                tenant: rubric.tenant,
                principal_id: new.principal_id,
                agent: new.agent,
                evaluator: new.evaluator,
                subject: new.subject,
                input,
                output,
                scores: new.scores,
                overall_score,
                notes: new.notes,
                created_at: now,
            };
            let rolling_avg: f64 = conn
                .query_row(
                    "SELECT AVG(overall_score) FROM thymus_evaluations \
                     WHERE principal_id = ?1 AND agent = ?2",
                    rusqlite::params![
                        evaluation.principal_id.to_string(),
                        evaluation.agent.to_string()
                    ],
                    |r| r.get(0),
                )
                .map_err(berr)?;
            (evaluation, rolling_avg)
        };
        self.emit(
            &EvaluationCompleted {
                evaluation_id: evaluation.id,
                agent: evaluation.agent.to_string(),
                subject: evaluation.subject.clone(),
                rubric_id: evaluation.rubric_id,
                overall_score: evaluation.overall_score,
            },
            evaluation.tenant,
            evaluation.principal_id,
        );
        self.propagate(evaluation.agent, Some(rolling_avg), None).await;
        Ok(evaluation)
    }

    /// Look up an owned evaluation by id.
    pub async fn get_evaluation(
        &self,
        principal: PrincipalId,
        id: i64,
    ) -> Result<Option<Evaluation>, ThymusError> {
        let conn = self.lock();
        let raw = conn
            .query_row(
                &format!(
                    "SELECT {EVALUATION_COLUMNS} FROM thymus_evaluations \
                     WHERE id = ?1 AND principal_id = ?2"
                ),
                rusqlite::params![id, principal.to_string()],
                read_raw_evaluation,
            )
            .optional()
            .map_err(berr)?;
        raw.map(RawEvaluation::into_evaluation).transpose()
    }

    /// List a principal's evaluations, newest first, AND-filtered by [`EvaluationFilter`].
    pub async fn list_evaluations(
        &self,
        principal: PrincipalId,
        filter: EvaluationFilter,
    ) -> Result<Vec<Evaluation>, ThymusError> {
        let mut sql =
            format!("SELECT {EVALUATION_COLUMNS} FROM thymus_evaluations WHERE principal_id = ?1");
        let mut args: Vec<rusqlite::types::Value> = vec![principal.to_string().into()];
        let mut n = 1;
        if let Some(agent) = &filter.agent {
            n += 1;
            sql.push_str(&format!(" AND agent = ?{n}"));
            args.push(agent.to_string().into());
        }
        if let Some(rubric_id) = filter.rubric_id {
            n += 1;
            sql.push_str(&format!(" AND rubric_id = ?{n}"));
            args.push(rubric_id.into());
        }
        sql.push_str(" ORDER BY id DESC");
        match (filter.limit, filter.offset) {
            (Some(l), Some(o)) => sql.push_str(&format!(" LIMIT {l} OFFSET {o}")),
            (Some(l), None) => sql.push_str(&format!(" LIMIT {l}")),
            (None, Some(o)) => sql.push_str(&format!(" LIMIT -1 OFFSET {o}")),
            (None, None) => {}
        }
        let conn = self.lock();
        let mut stmt = conn.prepare(&sql).map_err(berr)?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(args.iter()), read_raw_evaluation)
            .map_err(berr)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(berr)?.into_evaluation()?);
        }
        Ok(out)
    }

    /// An agent's rolling evaluation summary: overall average, count, and per-criterion raw
    /// averages (computed in Rust from the stored score objects).
    pub async fn agent_scores(
        &self,
        principal: PrincipalId,
        agent: PrincipalId,
    ) -> Result<AgentScores, ThymusError> {
        let evaluations = self
            .list_evaluations(
                principal,
                EvaluationFilter {
                    agent: Some(agent),
                    ..Default::default()
                },
            )
            .await?;
        let count = evaluations.len() as i64;
        let overall_avg = if count == 0 {
            0.0
        } else {
            evaluations.iter().map(|e| e.overall_score).sum::<f64>() / count as f64
        };
        let mut sums: BTreeMap<String, (f64, i64)> = BTreeMap::new();
        for evaluation in &evaluations {
            for (name, score) in &evaluation.scores {
                let entry = sums.entry(name.clone()).or_insert((0.0, 0));
                entry.0 += score;
                entry.1 += 1;
            }
        }
        let by_criterion = sums
            .into_iter()
            .map(|(name, (sum, n))| (name, sum / n as f64))
            .collect();
        Ok(AgentScores {
            agent: agent.to_string(),
            overall_avg,
            evaluation_count: count,
            by_criterion,
        })
    }

    /// Record a metric data point and emit `metric.recorded`.
    pub async fn record_metric(&self, new: NewMetric) -> Result<QualityMetric, ThymusError> {
        if new.metric.trim().is_empty() {
            return Err(ThymusError::InvalidInput("metric name required".to_string()));
        }
        let tags = new.tags.unwrap_or_else(|| serde_json::json!({}));
        if !tags.is_object() {
            return Err(ThymusError::InvalidInput("tags must be a JSON object".to_string()));
        }
        let now = Timestamp::now();
        let metric = {
            let conn = self.lock();
            conn.execute(
                "INSERT INTO thymus_metrics \
                 (tenant, principal_id, agent, metric, value, tags, recorded_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                rusqlite::params![
                    new.tenant.to_string(),
                    new.principal_id.to_string(),
                    new.agent.to_string(),
                    &new.metric,
                    new.value,
                    tags.to_string(),
                    ts_to_db(&now)?,
                ],
            )
            .map_err(berr)?;
            QualityMetric {
                id: conn.last_insert_rowid(),
                tenant: new.tenant,
                principal_id: new.principal_id,
                agent: new.agent,
                metric: new.metric,
                value: new.value,
                tags,
                recorded_at: now,
            }
        };
        self.emit(
            &MetricRecorded {
                agent: metric.agent.to_string(),
                metric: metric.metric.clone(),
                value: metric.value,
            },
            metric.tenant,
            metric.principal_id,
        );
        Ok(metric)
    }

    /// Summarize one (agent, metric) series for a principal.
    pub async fn metric_summary(
        &self,
        principal: PrincipalId,
        agent: PrincipalId,
        metric: &str,
    ) -> Result<MetricSummary, ThymusError> {
        let conn = self.lock();
        conn.query_row(
            "SELECT COUNT(*), COALESCE(AVG(value), 0), COALESCE(MIN(value), 0), \
             COALESCE(MAX(value), 0) FROM thymus_metrics \
             WHERE principal_id = ?1 AND agent = ?2 AND metric = ?3",
            rusqlite::params![principal.to_string(), agent.to_string(), metric],
            |r| {
                Ok(MetricSummary {
                    count: r.get(0)?,
                    avg: r.get(1)?,
                    min: r.get(2)?,
                    max: r.get(3)?,
                })
            },
        )
        .map_err(berr)
    }

    /// Record a behavioral-drift observation, emit `drift.detected`, and propagate the agent's
    /// current distinct drift-type tokens to the quality sink as replacement drift flags.
    pub async fn record_drift_event(&self, new: NewDriftEvent) -> Result<DriftEvent, ThymusError> {
        let severity = new.severity.unwrap_or(DriftSeverity::Medium);
        let now = Timestamp::now();
        let (event, flags) = {
            let conn = self.lock();
            conn.execute(
                "INSERT INTO thymus_drift_events \
                 (tenant, principal_id, agent, session, drift_type, severity, signal, created_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                rusqlite::params![
                    new.tenant.to_string(),
                    new.principal_id.to_string(),
                    new.agent.to_string(),
                    &new.session,
                    new.drift_type.as_str(),
                    severity.as_str(),
                    &new.signal,
                    ts_to_db(&now)?,
                ],
            )
            .map_err(berr)?;
            let event = DriftEvent {
                id: conn.last_insert_rowid(),
                tenant: new.tenant,
                principal_id: new.principal_id,
                agent: new.agent,
                session: new.session,
                drift_type: new.drift_type,
                severity,
                signal: new.signal,
                created_at: now,
            };
            let mut stmt = conn
                .prepare(
                    "SELECT DISTINCT drift_type FROM thymus_drift_events \
                     WHERE principal_id = ?1 AND agent = ?2 ORDER BY drift_type",
                )
                .map_err(berr)?;
            let rows = stmt
                .query_map(
                    rusqlite::params![event.principal_id.to_string(), event.agent.to_string()],
                    |r| r.get::<_, String>(0),
                )
                .map_err(berr)?;
            let mut flags = Vec::new();
            for row in rows {
                flags.push(row.map_err(berr)?);
            }
            (event, flags)
        };
        self.emit(
            &DriftDetected {
                agent: event.agent.to_string(),
                drift_type: event.drift_type.as_str().to_string(),
                severity: event.severity.as_str().to_string(),
            },
            event.tenant,
            event.principal_id,
        );
        self.propagate(event.agent, None, Some(flags)).await;
        Ok(event)
    }

    /// List a principal's drift events, newest first, optionally filtered to one agent and
    /// capped at `limit`.
    pub async fn list_drift_events(
        &self,
        principal: PrincipalId,
        agent: Option<PrincipalId>,
        limit: usize,
    ) -> Result<Vec<DriftEvent>, ThymusError> {
        let mut sql = "SELECT id, tenant, principal_id, agent, session, drift_type, severity, \
                       signal, created_at FROM thymus_drift_events WHERE principal_id = ?1"
            .to_string();
        let mut args: Vec<rusqlite::types::Value> = vec![principal.to_string().into()];
        if let Some(agent) = agent {
            sql.push_str(" AND agent = ?2");
            args.push(agent.to_string().into());
        }
        sql.push_str(&format!(" ORDER BY id DESC LIMIT {limit}"));
        let conn = self.lock();
        let mut stmt = conn.prepare(&sql).map_err(berr)?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(args.iter()), |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, Option<String>>(4)?,
                    r.get::<_, String>(5)?,
                    r.get::<_, String>(6)?,
                    r.get::<_, String>(7)?,
                    r.get::<_, String>(8)?,
                ))
            })
            .map_err(berr)?;
        let mut out = Vec::new();
        for row in rows {
            let (id, tenant, owner, agent, session, drift_type, severity, signal, created_at) =
                row.map_err(berr)?;
            out.push(DriftEvent {
                id,
                tenant: tid_from_db(&tenant)?,
                principal_id: pid_from_db(&owner)?,
                agent: pid_from_db(&agent)?,
                session,
                drift_type: DriftType::parse(&drift_type)?,
                severity: DriftSeverity::parse(&severity)?,
                signal,
                created_at: ts_from_db(&created_at)?,
            });
        }
        Ok(out)
    }

    /// Aggregate quality counts for a principal.
    pub async fn stats(&self, principal: PrincipalId) -> Result<ThymusStats, ThymusError> {
        let conn = self.lock();
        let one = |sql: &str| -> Result<i64, ThymusError> {
            conn.query_row(sql, rusqlite::params![principal.to_string()], |r| r.get(0))
                .map_err(berr)
        };
        let rubrics = one("SELECT COUNT(*) FROM thymus_rubrics WHERE principal_id = ?1")?;
        let evaluations = one("SELECT COUNT(*) FROM thymus_evaluations WHERE principal_id = ?1")?;
        let metrics = one("SELECT COUNT(*) FROM thymus_metrics WHERE principal_id = ?1")?;
        let drift_events = one("SELECT COUNT(*) FROM thymus_drift_events WHERE principal_id = ?1")?;
        let mut stmt = conn
            .prepare(
                "SELECT drift_type || '/' || severity, COUNT(*) FROM thymus_drift_events \
                 WHERE principal_id = ?1 GROUP BY drift_type, severity",
            )
            .map_err(berr)?;
        let rows = stmt
            .query_map(rusqlite::params![principal.to_string()], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
            })
            .map_err(berr)?;
        let mut drift_by_type = BTreeMap::new();
        for row in rows {
            let (key, count) = row.map_err(berr)?;
            drift_by_type.insert(key, count);
        }
        Ok(ThymusStats {
            rubrics,
            evaluations,
            metrics,
            drift_events,
            drift_by_type,
        })
    }
}

/// Apply every migration whose version exceeds `PRAGMA user_version`, each in its own transaction,
/// bumping `user_version` as it goes. Idempotent: an up-to-date database applies nothing.
fn apply_migrations(conn: &mut Connection) -> Result<(), ThymusError> {
    let mut version: i64 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .map_err(berr)?;
    for (v, sql) in MIGRATIONS {
        if *v > version {
            let tx = conn.transaction().map_err(berr)?;
            tx.execute_batch(sql)
                .map_err(|e| ThymusError::Backend(format!("migration V{v} failed: {e}")))?;
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
    use std::sync::Mutex as StdMutex;

    /// A recording sink capturing every propagation.
    #[derive(Default)]
    struct RecordingSink {
        /// The (agent, score, flags) tuples received, in order.
        calls: StdMutex<Vec<(PrincipalId, Option<f64>, Option<Vec<String>>)>>,
    }

    /// Records the call and succeeds.
    #[async_trait]
    impl QualitySink for &'static RecordingSink {
        async fn apply(
            &self,
            agent: PrincipalId,
            quality_score: Option<f64>,
            drift_flags: Option<Vec<String>>,
        ) -> Result<(), String> {
            self.calls
                .lock()
                .unwrap()
                .push((agent, quality_score, drift_flags));
            Ok(())
        }
    }

    /// A store with a leaked recording sink attached, plus the sink and the bus.
    fn store_with_sink() -> (ThymusStore, &'static RecordingSink, Arc<AxonBus>) {
        let sink: &'static RecordingSink = Box::leak(Box::default());
        let bus = Arc::new(AxonBus::new());
        let store = ThymusStore::open_in_memory(bus.clone())
            .expect("open")
            .with_quality_sink(Box::new(sink));
        (store, sink, bus)
    }

    /// A two-criterion rubric: quality (weight 2, scale 0-10) + speed (weight 1, scale 0-5).
    async fn rubric(store: &ThymusStore, principal: PrincipalId) -> Rubric {
        store
            .create_rubric(NewRubric {
                tenant: TenantId::new(),
                principal_id: principal,
                name: format!("code-review-{principal}"),
                description: None,
                criteria: vec![
                    Criterion {
                        name: "quality".to_string(),
                        weight: 2.0,
                        scale_min: 0.0,
                        scale_max: 10.0,
                    },
                    Criterion {
                        name: "speed".to_string(),
                        weight: 1.0,
                        scale_min: 0.0,
                        scale_max: 5.0,
                    },
                ],
            })
            .await
            .expect("rubric")
    }

    /// Scores for the test rubric.
    fn scores(quality: f64, speed: f64) -> BTreeMap<String, f64> {
        BTreeMap::from([("quality".to_string(), quality), ("speed".to_string(), speed)])
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
    async fn rubric_crud_and_validation() {
        let (store, _sink, _bus) = store_with_sink();
        let principal = PrincipalId::new();
        let r = rubric(&store, principal).await;
        let got = store.get_rubric(principal, r.id).await.expect("get").expect("present");
        assert_eq!(got, r);
        assert!(store.get_rubric(PrincipalId::new(), r.id).await.expect("get").is_none());

        // Empty criteria, duplicate names, bad weight/scale are rejected.
        let bad = |criteria: Vec<Criterion>| NewRubric {
            tenant: TenantId::new(),
            principal_id: principal,
            name: format!("bad-{}", PrincipalId::new()),
            description: None,
            criteria,
        };
        assert!(matches!(
            store.create_rubric(bad(vec![])).await.expect_err("empty"),
            ThymusError::InvalidInput(_)
        ));
        let c = Criterion { name: "x".to_string(), weight: 1.0, scale_min: 0.0, scale_max: 1.0 };
        assert!(store
            .create_rubric(bad(vec![c.clone(), c.clone()]))
            .await
            .is_err(), "duplicate names");
        assert!(store
            .create_rubric(bad(vec![Criterion { weight: 0.0, ..c.clone() }]))
            .await
            .is_err(), "zero weight");
        assert!(store
            .create_rubric(bad(vec![Criterion { scale_max: 0.0, ..c }]))
            .await
            .is_err(), "inverted scale");

        let updated = store
            .update_rubric(
                principal,
                r.id,
                RubricPatch { description: Some("desc".to_string()), ..Default::default() },
            )
            .await
            .expect("update");
        assert_eq!(updated.description.as_deref(), Some("desc"));
        assert_eq!(store.list_rubrics(principal).await.expect("list").len(), 1);
        assert!(store.delete_rubric(principal, r.id).await.expect("delete"));
    }

    #[tokio::test]
    async fn evaluate_computes_weighted_score_and_propagates() {
        let (store, sink, bus) = store_with_sink();
        let mut rx = bus.subscribe("quality");
        let principal = PrincipalId::new();
        let agent = PrincipalId::new();
        let r = rubric(&store, principal).await;

        // quality 5/10 (norm 0.5, weight 2) + speed 5/5 (norm 1.0, weight 1) -> 2/3.
        let evaluation = store
            .evaluate(NewEvaluation {
                principal_id: principal,
                rubric_id: r.id,
                agent,
                evaluator: principal,
                subject: "slice review".to_string(),
                input: None,
                output: None,
                scores: scores(5.0, 5.0),
                notes: None,
            })
            .await
            .expect("evaluate");
        assert!((evaluation.overall_score - 2.0 / 3.0).abs() < 1e-9);
        assert_eq!(drain_kinds(&mut rx), ["evaluation.completed"]);
        // The sink received the rolling average (one evaluation -> itself).
        {
            let calls = sink.calls.lock().unwrap();
            assert_eq!(calls.len(), 1);
            assert_eq!(calls[0].0, agent);
            assert!((calls[0].1.expect("score") - 2.0 / 3.0).abs() < 1e-9);
        }

        // A second evaluation moves the rolling average.
        store
            .evaluate(NewEvaluation {
                principal_id: principal,
                rubric_id: r.id,
                agent,
                evaluator: principal,
                subject: "second".to_string(),
                input: None,
                output: None,
                scores: scores(10.0, 5.0), // norm 1.0*2 + 1.0*1 / 3 = 1.0
                notes: None,
            })
            .await
            .expect("evaluate");
        let calls = sink.calls.lock().unwrap();
        let expected_avg = (2.0 / 3.0 + 1.0) / 2.0;
        assert!((calls[1].1.expect("score") - expected_avg).abs() < 1e-9);

        // Missing criterion score is rejected.
        drop(calls);
        let err = store
            .evaluate(NewEvaluation {
                principal_id: principal,
                rubric_id: r.id,
                agent,
                evaluator: principal,
                subject: "broken".to_string(),
                input: None,
                output: None,
                scores: BTreeMap::from([("quality".to_string(), 5.0)]),
                notes: None,
            })
            .await
            .expect_err("missing score");
        assert!(matches!(err, ThymusError::InvalidInput(_)));
    }

    #[tokio::test]
    async fn rubric_with_evaluations_cannot_be_deleted() {
        let (store, _sink, _bus) = store_with_sink();
        let principal = PrincipalId::new();
        let r = rubric(&store, principal).await;
        store
            .evaluate(NewEvaluation {
                principal_id: principal,
                rubric_id: r.id,
                agent: PrincipalId::new(),
                evaluator: principal,
                subject: "s".to_string(),
                input: None,
                output: None,
                scores: scores(5.0, 5.0),
                notes: None,
            })
            .await
            .expect("evaluate");
        let err = store.delete_rubric(principal, r.id).await.expect_err("in use");
        assert!(matches!(err, ThymusError::RubricInUse(_)));
    }

    #[tokio::test]
    async fn list_and_agent_scores_aggregate() {
        let (store, _sink, _bus) = store_with_sink();
        let principal = PrincipalId::new();
        let agent = PrincipalId::new();
        let r = rubric(&store, principal).await;
        for (q, s) in [(5.0, 5.0), (10.0, 0.0)] {
            store
                .evaluate(NewEvaluation {
                    principal_id: principal,
                    rubric_id: r.id,
                    agent,
                    evaluator: principal,
                    subject: "s".to_string(),
                    input: None,
                    output: None,
                    scores: scores(q, s),
                    notes: None,
                })
                .await
                .expect("evaluate");
        }
        let mine = store
            .list_evaluations(
                principal,
                EvaluationFilter { agent: Some(agent), ..Default::default() },
            )
            .await
            .expect("list");
        assert_eq!(mine.len(), 2);
        assert!(store
            .list_evaluations(PrincipalId::new(), EvaluationFilter::default())
            .await
            .expect("list")
            .is_empty());

        let summary = store.agent_scores(principal, agent).await.expect("scores");
        assert_eq!(summary.evaluation_count, 2);
        // overall: (2/3 + 2/3) / 2 -- second eval: quality 10/10 (1.0*2) + speed 0/5 (0) / 3 = 2/3.
        assert!((summary.overall_avg - 2.0 / 3.0).abs() < 1e-9);
        assert!((summary.by_criterion["quality"] - 7.5).abs() < 1e-9);
        assert!((summary.by_criterion["speed"] - 2.5).abs() < 1e-9);
    }

    #[tokio::test]
    async fn metrics_record_and_summarize() {
        let (store, _sink, bus) = store_with_sink();
        let mut rx = bus.subscribe("quality");
        let tenant = TenantId::new();
        let principal = PrincipalId::new();
        let agent = PrincipalId::new();
        for value in [100.0, 200.0, 300.0] {
            store
                .record_metric(NewMetric {
                    tenant,
                    principal_id: principal,
                    agent,
                    metric: "latency_ms".to_string(),
                    value,
                    tags: None,
                })
                .await
                .expect("metric");
        }
        assert_eq!(drain_kinds(&mut rx), vec!["metric.recorded"; 3]);
        let summary = store
            .metric_summary(principal, agent, "latency_ms")
            .await
            .expect("summary");
        assert_eq!(summary.count, 3);
        assert!((summary.avg - 200.0).abs() < 1e-9);
        assert!((summary.min - 100.0).abs() < 1e-9);
        assert!((summary.max - 300.0).abs() < 1e-9);
        // Non-object tags are rejected.
        let err = store
            .record_metric(NewMetric {
                tenant,
                principal_id: principal,
                agent,
                metric: "x".to_string(),
                value: 1.0,
                tags: Some(serde_json::json!([1])),
            })
            .await
            .expect_err("bad tags");
        assert!(matches!(err, ThymusError::InvalidInput(_)));
    }

    #[tokio::test]
    async fn drift_events_propagate_distinct_flags() {
        let (store, sink, bus) = store_with_sink();
        let mut rx = bus.subscribe("quality");
        let tenant = TenantId::new();
        let principal = PrincipalId::new();
        let agent = PrincipalId::new();
        let drift = |drift_type: DriftType| NewDriftEvent {
            tenant,
            principal_id: principal,
            agent,
            session: None,
            drift_type,
            severity: None,
            signal: "observed".to_string(),
        };
        let event = store.record_drift_event(drift(DriftType::Priority)).await.expect("drift");
        assert_eq!(event.severity, DriftSeverity::Medium, "default severity");
        store.record_drift_event(drift(DriftType::Safety)).await.expect("drift");
        store.record_drift_event(drift(DriftType::Priority)).await.expect("drift");
        assert_eq!(drain_kinds(&mut rx), vec!["drift.detected"; 3]);

        // The sink saw the distinct, sorted token set after the third event.
        let calls = sink.calls.lock().unwrap();
        assert_eq!(calls.len(), 3);
        assert_eq!(
            calls[2].2.as_deref(),
            Some(["priority".to_string(), "safety".to_string()].as_slice())
        );
        drop(calls);

        let events = store
            .list_drift_events(principal, Some(agent), 10)
            .await
            .expect("list");
        assert_eq!(events.len(), 3);

        let stats = store.stats(principal).await.expect("stats");
        assert_eq!(stats.drift_events, 3);
        assert_eq!(stats.drift_by_type.get("priority/medium"), Some(&2));
        assert_eq!(stats.drift_by_type.get("safety/medium"), Some(&1));
    }

    #[tokio::test]
    async fn quality_persists_across_reopen() {
        let tmp =
            std::env::temp_dir().join(format!("henosis-thymus-{}.sqlite", PrincipalId::new()));
        let principal = PrincipalId::new();
        let rubric_id;
        {
            let store = ThymusStore::open(&tmp, Arc::new(AxonBus::new())).expect("open");
            rubric_id = store
                .create_rubric(NewRubric {
                    tenant: TenantId::new(),
                    principal_id: principal,
                    name: "durable".to_string(),
                    description: None,
                    criteria: vec![Criterion {
                        name: "q".to_string(),
                        weight: 1.0,
                        scale_min: 0.0,
                        scale_max: 1.0,
                    }],
                })
                .await
                .expect("rubric")
                .id;
        }
        {
            let store = ThymusStore::open(&tmp, Arc::new(AxonBus::new())).expect("reopen");
            let got = store
                .get_rubric(principal, rubric_id)
                .await
                .expect("get")
                .expect("present after reopen");
            assert_eq!(got.name, "durable");
        }
        let _ = std::fs::remove_file(&tmp);
    }
}
