//! The Thymus domain types, reshaped onto the Henosis principal model.
//!
//! The Kleos types carried stringly `agent`/`evaluator` fields inside a `user_id: i64` shard,
//! and rubric criteria were raw JSON. Here the evaluated agent and the evaluator are
//! [`PrincipalId`]s, criteria are typed [`Criterion`]s, drift vocabulary is typed enums, and
//! timestamps are [`Timestamp`] (UTC). Rubrics, evaluations, metrics, and drift events keep
//! `i64` keys: they are Thymus-internal content/audit rows that nothing outside the crate
//! references by id (unlike Loom's workflows/runs). No `user_id: i64` survives the port.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use syntheos_contracts::{PrincipalId, TenantId, Timestamp};

use crate::error::ThymusError;

/// One scoring criterion of a rubric.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Criterion {
    /// The criterion name; evaluation scores key on this.
    pub name: String,
    /// Relative weight in the overall score (defaults to 1.0).
    #[serde(default = "default_weight")]
    pub weight: f64,
    /// Bottom of the raw scoring scale (defaults to 0.0).
    #[serde(default)]
    pub scale_min: f64,
    /// Top of the raw scoring scale (defaults to 1.0).
    #[serde(default = "default_scale_max")]
    pub scale_max: f64,
}

/// Serde default: criterion weight 1.0.
fn default_weight() -> f64 {
    1.0
}

/// Serde default: scale max 1.0.
fn default_scale_max() -> f64 {
    1.0
}

/// An evaluation rubric: a named, owner-scoped set of weighted criteria.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Rubric {
    /// Thymus-internal rubric id.
    pub id: i64,
    /// Tenant the rubric belongs to.
    pub tenant: TenantId,
    /// Owner principal. All reads/writes scope on this.
    pub principal_id: PrincipalId,
    /// Rubric name, unique per owner.
    pub name: String,
    /// Optional description.
    pub description: Option<String>,
    /// The scoring criteria (never empty).
    pub criteria: Vec<Criterion>,
    /// Creation time.
    pub created_at: Timestamp,
    /// Last-modification time.
    pub updated_at: Timestamp,
}

/// The fields required to define a new rubric.
#[derive(Debug, Clone)]
pub struct NewRubric {
    /// Tenant the rubric belongs to.
    pub tenant: TenantId,
    /// Owner principal.
    pub principal_id: PrincipalId,
    /// Rubric name, unique per owner.
    pub name: String,
    /// Optional description.
    pub description: Option<String>,
    /// The scoring criteria (must be non-empty, names unique).
    pub criteria: Vec<Criterion>,
}

/// A partial update to a rubric. `None` leaves that field unchanged.
#[derive(Debug, Clone, Default)]
pub struct RubricPatch {
    /// New name.
    pub name: Option<String>,
    /// New description.
    pub description: Option<String>,
    /// Replacement criteria (re-validated).
    pub criteria: Option<Vec<Criterion>>,
}

/// One recorded evaluation of an agent's work against a rubric.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Evaluation {
    /// Thymus-internal evaluation id.
    pub id: i64,
    /// The rubric scored against.
    pub rubric_id: i64,
    /// Tenant the evaluation belongs to.
    pub tenant: TenantId,
    /// Owner principal (whose quality program this is).
    pub principal_id: PrincipalId,
    /// The evaluated agent's principal.
    pub agent: PrincipalId,
    /// The evaluating principal (human reviewer, judge agent, ...).
    pub evaluator: PrincipalId,
    /// What was evaluated (a task title, session id, artifact name, ...).
    pub subject: String,
    /// The work's input, for audit (defaults to `{}`).
    pub input: serde_json::Value,
    /// The work's output, for audit (defaults to `{}`).
    pub output: serde_json::Value,
    /// Raw per-criterion scores, keyed by criterion name.
    pub scores: BTreeMap<String, f64>,
    /// The weighted overall score in [0, 1].
    pub overall_score: f64,
    /// Optional evaluator notes.
    pub notes: Option<String>,
    /// When the evaluation was recorded.
    pub created_at: Timestamp,
}

/// The fields required to record an evaluation. The overall score is computed by the store.
#[derive(Debug, Clone)]
pub struct NewEvaluation {
    /// Tenant the evaluation belongs to.
    pub tenant: TenantId,
    /// Owner principal (must own the rubric).
    pub principal_id: PrincipalId,
    /// The rubric to score against.
    pub rubric_id: i64,
    /// The evaluated agent's principal.
    pub agent: PrincipalId,
    /// The evaluating principal.
    pub evaluator: PrincipalId,
    /// What was evaluated.
    pub subject: String,
    /// The work's input (defaults to `{}`).
    pub input: Option<serde_json::Value>,
    /// The work's output (defaults to `{}`).
    pub output: Option<serde_json::Value>,
    /// Raw per-criterion scores; every rubric criterion must be present.
    pub scores: BTreeMap<String, f64>,
    /// Optional notes.
    pub notes: Option<String>,
}

/// Filters for [`crate::ThymusStore::list_evaluations`].
#[derive(Debug, Clone, Default)]
pub struct EvaluationFilter {
    /// Only evaluations of this agent.
    pub agent: Option<PrincipalId>,
    /// Only evaluations against this rubric.
    pub rubric_id: Option<i64>,
    /// Maximum rows to return (`None` = no limit).
    pub limit: Option<usize>,
    /// Rows to skip (for pagination).
    pub offset: Option<usize>,
}

/// An agent's rolling evaluation summary.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentScores {
    /// The agent's principal id string.
    pub agent: String,
    /// Average overall score across the agent's evaluations.
    pub overall_avg: f64,
    /// How many evaluations the average covers.
    pub evaluation_count: i64,
    /// Average raw score per criterion name.
    pub by_criterion: BTreeMap<String, f64>,
}

/// One recorded quality-metric data point.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QualityMetric {
    /// Thymus-internal metric id.
    pub id: i64,
    /// Tenant the metric belongs to.
    pub tenant: TenantId,
    /// Owner principal.
    pub principal_id: PrincipalId,
    /// The agent the metric describes.
    pub agent: PrincipalId,
    /// Metric name (e.g. `latency_ms`, `review_pass_rate`).
    pub metric: String,
    /// The data point.
    pub value: f64,
    /// Free-form dimension tags (a JSON object; defaults to `{}`).
    pub tags: serde_json::Value,
    /// When the point was recorded.
    pub recorded_at: Timestamp,
}

/// The fields required to record a metric data point.
#[derive(Debug, Clone)]
pub struct NewMetric {
    /// Tenant the metric belongs to.
    pub tenant: TenantId,
    /// Owner principal.
    pub principal_id: PrincipalId,
    /// The agent the metric describes.
    pub agent: PrincipalId,
    /// Metric name.
    pub metric: String,
    /// The data point.
    pub value: f64,
    /// Free-form dimension tags (must be a JSON object when supplied).
    pub tags: Option<serde_json::Value>,
}

/// Aggregate statistics for one (agent, metric) series.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MetricSummary {
    /// How many points the series holds.
    pub count: i64,
    /// Series average.
    pub avg: f64,
    /// Series minimum.
    pub min: f64,
    /// Series maximum.
    pub max: f64,
}

/// The category of behavioral drift observed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DriftType {
    /// Priority inversion (working on the wrong thing).
    Priority,
    /// Framework/methodology drift.
    Framework,
    /// Interaction-style drift.
    Interaction,
    /// Semantic drift (meaning of instructions reinterpreted).
    Meaning,
    /// Safety-posture drift.
    Safety,
    /// Structural drift (output shape degrading).
    Structural,
}

/// Storage-token conversion methods for [`DriftType`].
impl DriftType {
    /// The canonical storage/wire token for this drift type.
    pub fn as_str(&self) -> &'static str {
        match self {
            DriftType::Priority => "priority",
            DriftType::Framework => "framework",
            DriftType::Interaction => "interaction",
            DriftType::Meaning => "meaning",
            DriftType::Safety => "safety",
            DriftType::Structural => "structural",
        }
    }

    /// Parse a drift-type token, rejecting anything unknown.
    pub fn parse(s: &str) -> Result<Self, ThymusError> {
        match s {
            "priority" => Ok(DriftType::Priority),
            "framework" => Ok(DriftType::Framework),
            "interaction" => Ok(DriftType::Interaction),
            "meaning" => Ok(DriftType::Meaning),
            "safety" => Ok(DriftType::Safety),
            "structural" => Ok(DriftType::Structural),
            other => Err(ThymusError::InvalidToken(other.to_string())),
        }
    }
}

/// How serious a drift observation is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DriftSeverity {
    /// Cosmetic.
    Low,
    /// Worth watching (the default).
    Medium,
    /// Needs intervention.
    High,
    /// Stop-the-line.
    Critical,
}

/// Storage-token conversion methods for [`DriftSeverity`].
impl DriftSeverity {
    /// The canonical storage/wire token for this severity.
    pub fn as_str(&self) -> &'static str {
        match self {
            DriftSeverity::Low => "low",
            DriftSeverity::Medium => "medium",
            DriftSeverity::High => "high",
            DriftSeverity::Critical => "critical",
        }
    }

    /// Parse a severity token, rejecting anything unknown.
    pub fn parse(s: &str) -> Result<Self, ThymusError> {
        match s {
            "low" => Ok(DriftSeverity::Low),
            "medium" => Ok(DriftSeverity::Medium),
            "high" => Ok(DriftSeverity::High),
            "critical" => Ok(DriftSeverity::Critical),
            other => Err(ThymusError::InvalidToken(other.to_string())),
        }
    }
}

/// One recorded behavioral-drift observation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DriftEvent {
    /// Thymus-internal event id.
    pub id: i64,
    /// Tenant the event belongs to.
    pub tenant: TenantId,
    /// Owner principal.
    pub principal_id: PrincipalId,
    /// The drifting agent's principal.
    pub agent: PrincipalId,
    /// The session the drift was observed in, when known.
    pub session: Option<String>,
    /// The drift category.
    pub drift_type: DriftType,
    /// How serious it is.
    pub severity: DriftSeverity,
    /// The observed signal (what tripped the detector).
    pub signal: String,
    /// When the event was recorded.
    pub created_at: Timestamp,
}

/// The fields required to record a drift event.
#[derive(Debug, Clone)]
pub struct NewDriftEvent {
    /// Tenant the event belongs to.
    pub tenant: TenantId,
    /// Owner principal.
    pub principal_id: PrincipalId,
    /// The drifting agent's principal.
    pub agent: PrincipalId,
    /// The session the drift was observed in, when known.
    pub session: Option<String>,
    /// The drift category.
    pub drift_type: DriftType,
    /// Severity (defaults to [`DriftSeverity::Medium`]).
    pub severity: Option<DriftSeverity>,
    /// The observed signal.
    pub signal: String,
}

/// Aggregate quality counts for one principal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThymusStats {
    /// Rubrics owned by the principal.
    pub rubrics: i64,
    /// Total evaluations recorded.
    pub evaluations: i64,
    /// Total metric data points.
    pub metrics: i64,
    /// Total drift events.
    pub drift_events: i64,
    /// Drift-event counts per `type/severity` token pair.
    pub drift_by_type: BTreeMap<String, i64>,
}
