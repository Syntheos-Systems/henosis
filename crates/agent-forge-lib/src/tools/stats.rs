//! Protocol usage statistics. Aggregates counts and rates for specs, hypotheses,
//! verifications, learnings, approaches, and checkpoints over a configurable
//! time window so the agent can self-assess adherence to the forge workflow.

use crate::db::Database;
use crate::json_io::Output;
use crate::tools::{ToolError, ToolResult};
use serde::Deserialize;

/// Input for `stats`: the number of past days to include in the window
/// (default 30). All counts are filtered to rows created within this window.
#[derive(Deserialize)]
pub struct StatsInput {
    /// Look-back window in days (defaults to all time).
    pub days: Option<i64>,
}

/// Query all forge tables for counts and derived rates over the last `days`
/// days, then return a structured summary including spec completion rate,
/// hypothesis accuracy, verification pass rate, and top error patterns.
pub fn stats(db: &Database, input: StatsInput) -> ToolResult {
    let days = input.days.unwrap_or(30);
    let cutoff = chrono::Utc::now().timestamp() - (days * 86400);

    let conn = db.conn();

    // Spec stats
    let total_specs: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM specs WHERE created_at >= ?1",
            rusqlite::params![cutoff],
            |row| row.get(0),
        )
        .unwrap_or(0);

    let completed_specs: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM specs WHERE status = 'completed' AND created_at >= ?1",
            rusqlite::params![cutoff],
            |row| row.get(0),
        )
        .unwrap_or(0);

    let failed_specs: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM specs WHERE status = 'failed' AND created_at >= ?1",
            rusqlite::params![cutoff],
            |row| row.get(0),
        )
        .unwrap_or(0);

    let active_specs: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM specs WHERE status = 'active' AND created_at >= ?1",
            rusqlite::params![cutoff],
            |row| row.get(0),
        )
        .unwrap_or(0);

    let blocked_specs: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM specs WHERE status = 'blocked' AND created_at >= ?1",
            rusqlite::params![cutoff],
            |row| row.get(0),
        )
        .unwrap_or(0);

    // Hypothesis stats
    let total_hypotheses: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM hypotheses WHERE created_at >= ?1",
            rusqlite::params![cutoff],
            |row| row.get(0),
        )
        .unwrap_or(0);

    let correct_hypotheses: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM hypotheses WHERE outcome = 'correct' AND created_at >= ?1",
            rusqlite::params![cutoff],
            |row| row.get(0),
        )
        .unwrap_or(0);

    let incorrect_hypotheses: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM hypotheses WHERE outcome = 'incorrect' AND created_at >= ?1",
            rusqlite::params![cutoff],
            |row| row.get(0),
        )
        .unwrap_or(0);

    let partial_hypotheses: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM hypotheses WHERE outcome = 'partial' AND created_at >= ?1",
            rusqlite::params![cutoff],
            |row| row.get(0),
        )
        .unwrap_or(0);

    let unresolved_hypotheses: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM hypotheses WHERE outcome IS NULL AND created_at >= ?1",
            rusqlite::params![cutoff],
            |row| row.get(0),
        )
        .unwrap_or(0);

    let avg_confidence: f64 = conn
        .query_row(
            "SELECT COALESCE(AVG(confidence), 0) FROM hypotheses WHERE created_at >= ?1",
            rusqlite::params![cutoff],
            |row| row.get(0),
        )
        .unwrap_or(0.0);

    // Verification stats
    let total_verifications: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM verifications WHERE created_at >= ?1",
            rusqlite::params![cutoff],
            |row| row.get(0),
        )
        .unwrap_or(0);

    let passed_verifications: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM verifications WHERE success = 1 AND created_at >= ?1",
            rusqlite::params![cutoff],
            |row| row.get(0),
        )
        .unwrap_or(0);

    let avg_verify_duration: f64 = conn
        .query_row(
            "SELECT COALESCE(AVG(duration_ms), 0) FROM verifications WHERE created_at >= ?1",
            rusqlite::params![cutoff],
            |row| row.get(0),
        )
        .unwrap_or(0.0);

    // Learning stats
    let total_learnings: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM session_learns WHERE created_at >= ?1",
            rusqlite::params![cutoff],
            |row| row.get(0),
        )
        .unwrap_or(0);

    // Approach stats
    let total_approaches: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM approaches WHERE created_at >= ?1",
            rusqlite::params![cutoff],
            |row| row.get(0),
        )
        .unwrap_or(0);

    let chosen_approaches: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM approaches WHERE chosen = 1 AND created_at >= ?1",
            rusqlite::params![cutoff],
            |row| row.get(0),
        )
        .unwrap_or(0);

    // Checkpoint stats
    let total_checkpoints: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM checkpoints WHERE created_at >= ?1",
            rusqlite::params![cutoff],
            |row| row.get(0),
        )
        .unwrap_or(0);

    // Compute rates
    let spec_completion_rate = if total_specs > 0 {
        (completed_specs as f64 / total_specs as f64) * 100.0
    } else {
        0.0
    };

    let resolved_hypotheses = correct_hypotheses + incorrect_hypotheses + partial_hypotheses;
    let hypothesis_accuracy = if resolved_hypotheses > 0 {
        (correct_hypotheses as f64 / resolved_hypotheses as f64) * 100.0
    } else {
        0.0
    };

    let verify_pass_rate = if total_verifications > 0 {
        (passed_verifications as f64 / total_verifications as f64) * 100.0
    } else {
        0.0
    };

    // Top error patterns (most common bug descriptions)
    let mut error_stmt = conn
        .prepare(
            "SELECT bug_description, COUNT(*) as cnt FROM hypotheses WHERE created_at >= ?1 GROUP BY bug_description ORDER BY cnt DESC LIMIT 5",
        )
        .map_err(|e| ToolError::DatabaseError(e.to_string()))?;

    let error_patterns: Vec<serde_json::Value> = error_stmt
        .query_map(rusqlite::params![cutoff], |row| {
            Ok(serde_json::json!({
                "description": row.get::<_, String>(0)?,
                "count": row.get::<_, i64>(1)?,
            }))
        })
        .map_err(|e| ToolError::DatabaseError(e.to_string()))?
        .filter_map(|r| r.ok())
        .collect();

    // Task type distribution
    let mut type_stmt = conn
        .prepare(
            "SELECT task_type, COUNT(*) as cnt FROM specs WHERE created_at >= ?1 GROUP BY task_type ORDER BY cnt DESC",
        )
        .map_err(|e| ToolError::DatabaseError(e.to_string()))?;

    let task_types: Vec<serde_json::Value> = type_stmt
        .query_map(rusqlite::params![cutoff], |row| {
            Ok(serde_json::json!({
                "type": row.get::<_, String>(0)?,
                "count": row.get::<_, i64>(1)?,
            }))
        })
        .map_err(|e| ToolError::DatabaseError(e.to_string()))?
        .filter_map(|r| r.ok())
        .collect();

    let mut output = Output::ok(format!(
        "Protocol stats (last {} days): {} specs ({:.0}% completed), {} hypotheses ({:.0}% accurate), {} verifications ({:.0}% pass)",
        days, total_specs, spec_completion_rate, total_hypotheses, hypothesis_accuracy, total_verifications, verify_pass_rate
    ));

    output.data = Some(serde_json::json!({
        "period_days": days,
        "specs": {
            "total": total_specs,
            "completed": completed_specs,
            "failed": failed_specs,
            "active": active_specs,
            "blocked": blocked_specs,
            "completion_rate": format!("{:.1}%", spec_completion_rate),
        },
        "hypotheses": {
            "total": total_hypotheses,
            "correct": correct_hypotheses,
            "incorrect": incorrect_hypotheses,
            "partial": partial_hypotheses,
            "unresolved": unresolved_hypotheses,
            "avg_confidence": format!("{:.2}", avg_confidence),
            "accuracy_rate": format!("{:.1}%", hypothesis_accuracy),
        },
        "verifications": {
            "total": total_verifications,
            "passed": passed_verifications,
            "failed": total_verifications - passed_verifications,
            "pass_rate": format!("{:.1}%", verify_pass_rate),
            "avg_duration_ms": format!("{:.0}", avg_verify_duration),
        },
        "learnings": {
            "total": total_learnings,
        },
        "approaches": {
            "total": total_approaches,
            "chosen": chosen_approaches,
        },
        "checkpoints": {
            "total": total_checkpoints,
        },
        "error_patterns": error_patterns,
        "task_type_distribution": task_types,
    }));

    Ok(output)
}
