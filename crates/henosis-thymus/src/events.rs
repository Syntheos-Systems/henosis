//! The quality events Thymus publishes onto the Axon bus.
//!
//! They live here (a service crate) rather than in `syntheos-contracts` because they are
//! Thymus's domain events, but they implement the contracts' [`TypedEvent`] trait so any
//! in-process reactor (narration, supervision, the future EidolonGate policy state) can
//! subscribe without depending on Thymus. Payloads carry identifying strings and coarse
//! signal only -- never evaluation inputs/outputs.

use serde::{Deserialize, Serialize};
use syntheos_contracts::TypedEvent;

/// The coarse channel every Thymus quality event travels on.
pub const QUALITY_CHANNEL: &str = "quality";

/// An evaluation was recorded.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvaluationCompleted {
    /// The evaluation's id.
    pub evaluation_id: i64,
    /// The evaluated agent's principal id.
    pub agent: String,
    /// What was evaluated.
    pub subject: String,
    /// The rubric scored against.
    pub rubric_id: i64,
    /// The weighted overall score in [0, 1].
    pub overall_score: f64,
}

/// Emit `EvaluationCompleted` on the quality channel.
impl TypedEvent for EvaluationCompleted {
    const CHANNEL: &'static str = QUALITY_CHANNEL;
    const KIND: &'static str = "evaluation.completed";
}

/// A metric data point was recorded.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MetricRecorded {
    /// The agent the metric describes.
    pub agent: String,
    /// Metric name.
    pub metric: String,
    /// The data point.
    pub value: f64,
}

/// Emit `MetricRecorded` on the quality channel.
impl TypedEvent for MetricRecorded {
    const CHANNEL: &'static str = QUALITY_CHANNEL;
    const KIND: &'static str = "metric.recorded";
}

/// A behavioral-drift observation was recorded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DriftDetected {
    /// The drifting agent's principal id.
    pub agent: String,
    /// The drift category token.
    pub drift_type: String,
    /// The severity token.
    pub severity: String,
}

/// Emit `DriftDetected` on the quality channel.
impl TypedEvent for DriftDetected {
    const CHANNEL: &'static str = QUALITY_CHANNEL;
    const KIND: &'static str = "drift.detected";
}
