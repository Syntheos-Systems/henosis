#![deny(missing_docs)]
#![warn(clippy::all)]
//! # henosis-thymus
//!
//! Thymus, the quality-evaluation kernel service for the Henosis substrate.
//!
//! The Kleos thymus service keyed everything on stringly `agent` fields inside a
//! `user_id: i64` shard. This extraction puts quality on the principal model: the evaluated
//! agent and the evaluator are [`syntheos_contracts::PrincipalId`]s, rubric criteria are typed
//! [`Criterion`]s with the Kleos weighted-normalized scoring formula, drift vocabulary is
//! typed enums, and quality changes publish typed events onto the in-process
//! [`syntheos_axon`] bus. No `user_id: i64` survives in any public type.
//!
//! The Soma linkage goes through the [`QualitySink`] seam: evaluations propagate the agent's
//! rolling average, drift events propagate the agent's distinct drift-type tokens, and the
//! server adapts `SomaStore::update_quality` to the trait at wiring time -- no kernel crate
//! depends on another kernel crate.
//!
//! ## Scope
//!
//! Thymus provides rubric CRUD with criteria validation, evaluations, quality metrics,
//! behavioral-drift events, and stats. Session-quality rows and LLM judging remain outside this
//! service.

pub mod error;
pub mod events;
pub mod model;
pub mod store;

pub use error::ThymusError;
pub use events::{DriftDetected, EvaluationCompleted, MetricRecorded, QUALITY_CHANNEL};
pub use model::{
    AgentScores, Criterion, DriftEvent, DriftSeverity, DriftType, Evaluation, EvaluationFilter,
    MetricSummary, NewDriftEvent, NewEvaluation, NewMetric, NewRubric, QualityMetric, Rubric,
    RubricPatch, ThymusStats,
};
pub use store::{QualitySink, ThymusStore};
