//! Spec lifecycle tools: `spec_task` creates a new task spec in the forge DB;
//! `update_spec` transitions its status; `list_specs` paginates all specs;
//! `get_spec` fetches a single spec together with its linked hypotheses,
//! approaches, learnings, and verification records.

use crate::bridge::SkillsBridge;
use crate::db::Database;
use crate::json_io::Output;
use crate::tools::{set_session_active, ToolError, ToolResult};
use chrono::Utc;
use serde::Deserialize;
use uuid::Uuid;

/// Input for `spec_task`: all fields that define a new task specification.
/// `acceptance_criteria` requires at least 2 items; `edge_cases` requires at least 3.
#[derive(Deserialize)]
pub struct SpecTaskInput {
    /// What the task is.
    pub task_description: Option<String>,
    /// Task category (e.g. `feature`, `bugfix`).
    pub task_type: Option<String>,
    /// Done-when criteria (at least two).
    pub acceptance_criteria: Option<Vec<String>>,
    /// The interface the work must satisfy.
    pub interface_contract: Option<String>,
    /// Edge cases the work must handle (at least three).
    pub edge_cases: Option<Vec<String>>,
    /// Files the work is expected to touch.
    pub files_to_touch: Option<Vec<String>>,
    /// Upstream dependencies/preconditions.
    pub dependencies: Option<String>,
}

/// The set of recognised task types enforced at spec creation time.
const VALID_TASK_TYPES: &[&str] = &[
    "feature",
    "bugfix",
    "refactor",
    "enhancement",
    "test",
    "docs",
];

/// Validate the input, persist a new spec row to the DB, set the session-active
/// marker for the enforce hook, and return the new spec ID along with any
/// skills from Kleos that are relevant to the task description.
pub fn spec_task(
    db: &Database,
    bridge: Option<&dyn SkillsBridge>,
    input: SpecTaskInput,
) -> ToolResult {
    let task_description = input
        .task_description
        .ok_or_else(|| ToolError::MissingField("task_description".into()))?;

    let task_type = input
        .task_type
        .ok_or_else(|| ToolError::MissingField("task_type".into()))?;

    if !VALID_TASK_TYPES.contains(&task_type.as_str()) {
        return Err(ToolError::InvalidValue(format!(
            "task_type must be one of: {}",
            VALID_TASK_TYPES.join(", ")
        )));
    }

    let acceptance_criteria = input
        .acceptance_criteria
        .ok_or_else(|| ToolError::MissingField("acceptance_criteria".into()))?;

    if acceptance_criteria.len() < 2 {
        return Err(ToolError::InvalidValue(
            "Minimum 2 acceptance criteria required".into(),
        ));
    }

    let interface_contract = input
        .interface_contract
        .ok_or_else(|| ToolError::MissingField("interface_contract".into()))?;

    let edge_cases = input.edge_cases.unwrap_or_default();
    if edge_cases.len() < 3 {
        return Err(ToolError::InvalidValue(
            "Minimum 3 edge cases required".into(),
        ));
    }

    let id = format!("spec_{}", &Uuid::new_v4().to_string()[..8]);
    let now = Utc::now().timestamp();

    db.conn()
        .execute(
            r#"
            INSERT INTO specs (id, created_at, task_description, task_type, acceptance_criteria, interface_contract, edge_cases, files_to_touch, dependencies, status)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 'active')
            "#,
            rusqlite::params![
                id,
                now,
                task_description,
                task_type,
                serde_json::to_string(&acceptance_criteria).unwrap(),
                interface_contract,
                serde_json::to_string(&edge_cases).unwrap(),
                input.files_to_touch.map(|v| serde_json::to_string(&v).unwrap()),
                input.dependencies,
            ],
        )
        .map_err(|e| ToolError::DatabaseError(e.to_string()))?;

    set_session_active(&id, &task_type);

    // Opportunistic: search for relevant skills (best-effort; no bridge = no lookup).
    let related_skills = bridge
        .and_then(|b| b.search_skills(&task_description, Some(5)).ok())
        .and_then(|v| v.get("skills").cloned());

    let mut output = Output::ok_with_id(id, "Spec created");
    if let Some(skills) = related_skills {
        output.data = Some(serde_json::json!({ "related_skills": skills }));
    }
    Ok(output)
}

/// Input for `update_spec`: the spec to update, its new status, and an optional note.
#[derive(Deserialize)]
pub struct UpdateSpecInput {
    /// The spec to update.
    pub spec_id: Option<String>,
    /// New status token.
    pub status: Option<String>,
    /// Free-text note recorded with the update.
    pub note: Option<String>,
}

/// The set of valid status values a spec can transition to.
const VALID_STATUSES: &[&str] = &["active", "completed", "failed", "blocked"];

/// Transition `spec_id` to a new status, recording an optional note and
/// setting `completed_at` automatically when the status is terminal.
pub fn update_spec(db: &Database, input: UpdateSpecInput) -> ToolResult {
    let spec_id = input
        .spec_id
        .ok_or_else(|| ToolError::MissingField("spec_id".into()))?;
    let status = input
        .status
        .ok_or_else(|| ToolError::MissingField("status".into()))?;

    if !VALID_STATUSES.contains(&status.as_str()) {
        return Err(ToolError::InvalidValue(format!(
            "status must be one of: {}",
            VALID_STATUSES.join(", ")
        )));
    }

    let now = Utc::now().timestamp();
    let completed_at = if status == "completed" || status == "failed" {
        Some(now)
    } else {
        None
    };

    let rows = db
        .conn()
        .execute(
            "UPDATE specs SET status = ?1, status_note = ?2, completed_at = ?3 WHERE id = ?4",
            rusqlite::params![status, input.note, completed_at, spec_id],
        )
        .map_err(|e| ToolError::DatabaseError(e.to_string()))?;

    if rows == 0 {
        return Err(ToolError::InvalidValue(format!(
            "Spec not found: {}",
            spec_id
        )));
    }

    Ok(Output::ok(format!("Spec {} marked as {}", spec_id, status)))
}

/// Input for `list_specs`: optional status filter and result cap (default 20).
#[derive(Deserialize)]
pub struct ListSpecsInput {
    /// Only specs with this status.
    pub status: Option<String>,
    /// Maximum specs to return.
    pub limit: Option<usize>,
}

/// Return specs ordered by creation time descending, optionally filtered to a
/// single status value. Each row includes its description, type, status, and timestamps.
pub fn list_specs(db: &Database, input: ListSpecsInput) -> ToolResult {
    let limit = input.limit.unwrap_or(20);

    let (query, params): (&str, Vec<Box<dyn rusqlite::types::ToSql>>) = if let Some(ref status) =
        input.status
    {
        (
            "SELECT id, task_description, task_type, status, created_at, completed_at, status_note FROM specs WHERE status = ?1 ORDER BY created_at DESC LIMIT ?2",
            vec![Box::new(status.clone()), Box::new(limit as i64)],
        )
    } else {
        (
            "SELECT id, task_description, task_type, status, created_at, completed_at, status_note FROM specs ORDER BY created_at DESC LIMIT ?1",
            vec![Box::new(limit as i64)],
        )
    };

    let mut stmt = db
        .conn()
        .prepare(query)
        .map_err(|e| ToolError::DatabaseError(e.to_string()))?;

    let rows = stmt
        .query_map(rusqlite::params_from_iter(params.iter()), |row| {
            Ok(serde_json::json!({
                "id": row.get::<_, String>(0)?,
                "task_description": row.get::<_, String>(1)?,
                "task_type": row.get::<_, String>(2)?,
                "status": row.get::<_, String>(3)?,
                "created_at": row.get::<_, i64>(4)?,
                "completed_at": row.get::<_, Option<i64>>(5)?,
                "status_note": row.get::<_, Option<String>>(6)?,
            }))
        })
        .map_err(|e| ToolError::DatabaseError(e.to_string()))?;

    let results: Vec<_> = rows.filter_map(|r| r.ok()).collect();

    let mut output = Output::ok(format!("Found {} specs", results.len()));
    output.data = Some(serde_json::json!({ "specs": results }));
    Ok(output)
}

/// Input for `get_spec`: the ID of the spec to retrieve.
#[derive(Deserialize)]
pub struct GetSpecInput {
    /// The spec to fetch.
    pub spec_id: Option<String>,
}

/// Fetch a full spec by ID, joining in all related hypotheses, approaches,
/// session learnings, and verification records so the agent sees the complete
/// history for that task in one call.
pub fn get_spec(db: &Database, input: GetSpecInput) -> ToolResult {
    let spec_id = input
        .spec_id
        .ok_or_else(|| ToolError::MissingField("spec_id".into()))?;

    let spec: serde_json::Value = db
        .conn()
        .query_row(
            "SELECT id, task_description, task_type, acceptance_criteria, interface_contract, edge_cases, files_to_touch, dependencies, status, created_at, completed_at, status_note FROM specs WHERE id = ?1",
            rusqlite::params![spec_id],
            |row| {
                Ok(serde_json::json!({
                    "id": row.get::<_, String>(0)?,
                    "task_description": row.get::<_, String>(1)?,
                    "task_type": row.get::<_, String>(2)?,
                    "acceptance_criteria": row.get::<_, String>(3)?,
                    "interface_contract": row.get::<_, Option<String>>(4)?,
                    "edge_cases": row.get::<_, Option<String>>(5)?,
                    "files_to_touch": row.get::<_, Option<String>>(6)?,
                    "dependencies": row.get::<_, Option<String>>(7)?,
                    "status": row.get::<_, String>(8)?,
                    "created_at": row.get::<_, i64>(9)?,
                    "completed_at": row.get::<_, Option<i64>>(10)?,
                    "status_note": row.get::<_, Option<String>>(11)?,
                }))
            },
        )
        .map_err(|e| ToolError::DatabaseError(format!("Spec not found: {}", e)))?;

    // Get related hypotheses
    let mut hyp_stmt = db
        .conn()
        .prepare(
            "SELECT id, hypothesis, outcome, confidence FROM hypotheses WHERE spec_id = ?1 ORDER BY created_at DESC",
        )
        .map_err(|e| ToolError::DatabaseError(e.to_string()))?;

    let hypotheses: Vec<serde_json::Value> = hyp_stmt
        .query_map(rusqlite::params![spec_id], |row| {
            Ok(serde_json::json!({
                "id": row.get::<_, String>(0)?,
                "hypothesis": row.get::<_, String>(1)?,
                "outcome": row.get::<_, Option<String>>(2)?,
                "confidence": row.get::<_, f64>(3)?,
            }))
        })
        .map_err(|e| ToolError::DatabaseError(e.to_string()))?
        .filter_map(|r| r.ok())
        .collect();

    // Get related approaches
    let mut app_stmt = db
        .conn()
        .prepare(
            "SELECT id, name, score, chosen FROM approaches WHERE spec_id = ?1 ORDER BY created_at DESC",
        )
        .map_err(|e| ToolError::DatabaseError(e.to_string()))?;

    let approaches: Vec<serde_json::Value> = app_stmt
        .query_map(rusqlite::params![spec_id], |row| {
            Ok(serde_json::json!({
                "id": row.get::<_, String>(0)?,
                "name": row.get::<_, String>(1)?,
                "score": row.get::<_, Option<f64>>(2)?,
                "chosen": row.get::<_, i64>(3)?,
            }))
        })
        .map_err(|e| ToolError::DatabaseError(e.to_string()))?
        .filter_map(|r| r.ok())
        .collect();

    // Get related learnings
    let mut learn_stmt = db
        .conn()
        .prepare(
            "SELECT id, discovery FROM session_learns WHERE spec_id = ?1 ORDER BY created_at DESC",
        )
        .map_err(|e| ToolError::DatabaseError(e.to_string()))?;

    let learnings: Vec<serde_json::Value> = learn_stmt
        .query_map(rusqlite::params![spec_id], |row| {
            Ok(serde_json::json!({
                "id": row.get::<_, String>(0)?,
                "discovery": row.get::<_, String>(1)?,
            }))
        })
        .map_err(|e| ToolError::DatabaseError(e.to_string()))?
        .filter_map(|r| r.ok())
        .collect();

    // Get related verifications
    let mut ver_stmt = db
        .conn()
        .prepare(
            "SELECT id, command, success, duration_ms, criteria_index FROM verifications WHERE spec_id = ?1 ORDER BY created_at DESC",
        )
        .map_err(|e| ToolError::DatabaseError(e.to_string()))?;

    let verifications: Vec<serde_json::Value> = ver_stmt
        .query_map(rusqlite::params![spec_id], |row| {
            Ok(serde_json::json!({
                "id": row.get::<_, String>(0)?,
                "command": row.get::<_, String>(1)?,
                "success": row.get::<_, bool>(2)?,
                "duration_ms": row.get::<_, Option<i64>>(3)?,
                "criteria_index": row.get::<_, Option<i64>>(4)?,
            }))
        })
        .map_err(|e| ToolError::DatabaseError(e.to_string()))?
        .filter_map(|r| r.ok())
        .collect();

    let mut output = Output::ok(format!("Spec {}", spec_id));
    output.data = Some(serde_json::json!({
        "spec": spec,
        "hypotheses": hypotheses,
        "approaches": approaches,
        "learnings": learnings,
        "verifications": verifications,
    }));
    Ok(output)
}
