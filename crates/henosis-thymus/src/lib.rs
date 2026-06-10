#![deny(missing_docs)]
#![warn(clippy::all)]
//! # henosis-thymus
//!
//! Thymus, the quality-evaluation kernel service, extracted from `kleos-lib` onto the Henosis
//! substrate (Phase 1 Story 1.5, the fifth and final extracted kernel service).
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
//! Slice 1 (this commit): rubric CRUD with criteria validation, evaluations (weighted scoring,
//! agent rolling summaries), quality metrics (record + series summary), behavioral-drift
//! events (typed vocabulary, distinct-flag propagation), and stats. NOT ported here: Kleos
//! session-quality rows (coupled to Kleos sessions; they arrive with the Eidolon supervisor in
//! Phase 2) and the LLM session judge (parallel track T1, Kleos-internal until the cutover).

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
