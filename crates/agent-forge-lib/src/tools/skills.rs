//! Skills tools -- thin wrappers that translate agent-forge CLI calls into
//! Kleos skill API requests. Each public function corresponds to one CLI
//! subcommand (SkillSearch, SkillCapture, SkillRecordExec, SkillFix,
//! SkillDerive, SkillLineage).

use crate::bridge::SkillsBridge;
use crate::json_io::Output;
use crate::tools::{ToolError, ToolResult};
use serde::Deserialize;

/// Resolve the optional bridge, mapping its absence to the uniform `ToolError`.
fn require(bridge: Option<&dyn SkillsBridge>) -> Result<&dyn SkillsBridge, ToolError> {
    bridge.ok_or_else(|| ToolError::IoError("no skills bridge configured".into()))
}

// --- SkillSearch ---

#[derive(Deserialize)]
/// Input for `skill_search`.
pub struct SkillSearchInput {
    /// Search query.
    pub query: Option<String>,
    /// Maximum results.
    pub limit: Option<usize>,
}

/// Search Kleos for skills matching `query`, returning up to `limit` results.
pub fn skill_search(bridge: Option<&dyn SkillsBridge>, input: SkillSearchInput) -> ToolResult {
    let query = input
        .query
        .ok_or_else(|| ToolError::MissingField("query".into()))?;
    let bridge = require(bridge)?;
    let result = bridge
        .search_skills(&query, input.limit)
        .map_err(ToolError::IoError)?;

    let skills = result
        .get("skills")
        .cloned()
        .unwrap_or(serde_json::json!([]));
    let count = skills.as_array().map(|a| a.len()).unwrap_or(0);

    let mut output = Output::ok(format!("Found {} matching skills", count));
    output.data = Some(serde_json::json!({ "skills": skills }));
    Ok(output)
}

// --- SkillCapture ---

#[derive(Deserialize)]
/// Input for `skill_capture`.
pub struct SkillCaptureInput {
    /// Natural-language skill description (max 2000 chars).
    pub description: Option<String>,
    /// Originating agent label.
    pub agent: Option<String>,
}

/// Submit a new skill description to Kleos and return the assigned skill ID.
/// Rejects descriptions longer than 2000 characters before sending.
pub fn skill_capture(bridge: Option<&dyn SkillsBridge>, input: SkillCaptureInput) -> ToolResult {
    let description = input
        .description
        .ok_or_else(|| ToolError::MissingField("description".into()))?;
    if description.len() > 2000 {
        return Err(ToolError::InvalidValue(
            "description exceeds 2000 char limit".into(),
        ));
    }
    let bridge = require(bridge)?;
    let result = bridge
        .capture_skill(&description, input.agent.as_deref())
        .map_err(ToolError::IoError)?;

    let skill_id = result
        .get("skill_id")
        .and_then(|v| v.as_i64())
        .unwrap_or(-1);
    let message = result
        .get("message")
        .and_then(|v| v.as_str())
        .unwrap_or("captured");

    let mut output =
        Output::ok_with_id(skill_id.to_string(), format!("Skill captured: {}", message));
    output.data = Some(result);
    Ok(output)
}

// --- SkillRecordExec ---

#[derive(Deserialize)]
/// Input for `skill_record_exec`.
pub struct SkillRecordExecInput {
    /// The executed skill.
    pub skill_id: Option<i64>,
    /// Whether the execution succeeded.
    pub success: Option<bool>,
    /// Wall-clock duration of the execution.
    pub duration_ms: Option<f64>,
    /// Coarse error category on failure.
    pub error_type: Option<String>,
    /// Error detail on failure.
    pub error_message: Option<String>,
}

/// Record one execution attempt for a skill, including success/failure and
/// optional timing and error details.
pub fn skill_record_exec(
    bridge: Option<&dyn SkillsBridge>,
    input: SkillRecordExecInput,
) -> ToolResult {
    let skill_id = input
        .skill_id
        .ok_or_else(|| ToolError::MissingField("skill_id".into()))?;
    let success = input
        .success
        .ok_or_else(|| ToolError::MissingField("success".into()))?;
    let bridge = require(bridge)?;
    bridge
        .record_execution(
            skill_id,
            success,
            input.duration_ms,
            input.error_type.as_deref(),
            input.error_message.as_deref(),
        )
        .map_err(ToolError::IoError)?;

    Ok(Output::ok(format!(
        "Recorded {} execution for skill #{}",
        if success { "successful" } else { "failed" },
        skill_id
    )))
}

// --- SkillFix ---

#[derive(Deserialize)]
/// Input for `skill_fix`.
pub struct SkillFixInput {
    /// The skill to fix.
    pub skill_id: Option<i64>,
    /// Free-text guidance for the fix.
    pub hint: Option<String>,
}

/// Ask Kleos to create a corrected version of the given skill, optionally
/// guided by a free-text hint describing what to change.
pub fn skill_fix(bridge: Option<&dyn SkillsBridge>, input: SkillFixInput) -> ToolResult {
    let skill_id = input
        .skill_id
        .ok_or_else(|| ToolError::MissingField("skill_id".into()))?;
    let bridge = require(bridge)?;
    let result = bridge
        .fix_skill(skill_id, input.hint.as_deref())
        .map_err(ToolError::IoError)?;

    let new_id = result
        .get("skill_id")
        .and_then(|v| v.as_i64())
        .unwrap_or(-1);
    let message = result
        .get("message")
        .and_then(|v| v.as_str())
        .unwrap_or("fixed");

    let mut output = Output::ok_with_id(new_id.to_string(), format!("Skill fixed: {}", message));
    output.data = Some(result);
    Ok(output)
}

// --- SkillDerive ---

#[derive(Deserialize)]
/// Input for `skill_derive`.
pub struct SkillDeriveInput {
    /// Parent skills to derive from (at least one).
    pub parent_ids: Option<Vec<i64>>,
    /// Natural-language mutation/combination prompt.
    pub direction: Option<String>,
    /// Originating agent label.
    pub agent: Option<String>,
}

/// Derive a new skill from one or more parents using the given direction prompt.
/// Requires at least one parent ID and a direction no longer than 2000 characters.
pub fn skill_derive(bridge: Option<&dyn SkillsBridge>, input: SkillDeriveInput) -> ToolResult {
    let parent_ids = input
        .parent_ids
        .ok_or_else(|| ToolError::MissingField("parent_ids".into()))?;
    if parent_ids.is_empty() {
        return Err(ToolError::InvalidValue(
            "at least one parent_id required".into(),
        ));
    }
    let direction = input
        .direction
        .ok_or_else(|| ToolError::MissingField("direction".into()))?;
    if direction.len() > 2000 {
        return Err(ToolError::InvalidValue(
            "direction exceeds 2000 char limit".into(),
        ));
    }
    let bridge = require(bridge)?;
    let result = bridge
        .derive_skill(&parent_ids, &direction, input.agent.as_deref())
        .map_err(ToolError::IoError)?;

    let new_id = result
        .get("skill_id")
        .and_then(|v| v.as_i64())
        .unwrap_or(-1);
    let message = result
        .get("message")
        .and_then(|v| v.as_str())
        .unwrap_or("derived");

    let mut output = Output::ok_with_id(new_id.to_string(), format!("Skill derived: {}", message));
    output.data = Some(result);
    Ok(output)
}

// --- SkillLineage ---

#[derive(Deserialize)]
/// Input for `skill_lineage`.
pub struct SkillLineageInput {
    /// The skill whose lineage is fetched.
    pub skill_id: Option<i64>,
}

/// Fetch the full ancestor/descendant lineage graph for the given skill ID.
pub fn skill_lineage(bridge: Option<&dyn SkillsBridge>, input: SkillLineageInput) -> ToolResult {
    let skill_id = input
        .skill_id
        .ok_or_else(|| ToolError::MissingField("skill_id".into()))?;
    let bridge = require(bridge)?;
    let result = bridge.get_lineage(skill_id).map_err(ToolError::IoError)?;

    let mut output = Output::ok(format!("Lineage for skill #{}", skill_id));
    output.data = Some(result);
    Ok(output)
}
