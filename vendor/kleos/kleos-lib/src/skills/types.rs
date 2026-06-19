use serde::{Deserialize, Serialize};

// First-class kind discrimination for the Skills Cloud (v50+).
//
// Plain `Skill` is the legacy default for hand-captured / hand-written
// content. The other variants come from the plugin importer:
// - Agent: serialized agent definition (Task subagent prompt + tools).
// - Command: serialized slash command body.
// - Workflow: synthesized from MCP-converted servers or hook configs;
//   anything that is "process-flavored" rather than a single prompt.
//
// Stored as TEXT in `skill_records.kind` (DEFAULT 'skill').
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
#[derive(Default)]
pub enum SkillKind {
    #[default]
    Skill,
    Agent,
    Command,
    Workflow,
}

// Display and FromStr for SkillKind.
impl std::fmt::Display for SkillKind {
    /// Formats the kind as its lowercase string representation.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Skill => write!(f, "skill"),
            Self::Agent => write!(f, "agent"),
            Self::Command => write!(f, "command"),
            Self::Workflow => write!(f, "workflow"),
        }
    }
}

// FromStr for SkillKind.
impl std::str::FromStr for SkillKind {
    /// Parse error type.
    type Err = crate::EngError;
    /// Parses a lowercase string into a `SkillKind` variant.
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "skill" => Ok(Self::Skill),
            "agent" => Ok(Self::Agent),
            "command" => Ok(Self::Command),
            "workflow" => Ok(Self::Workflow),
            _ => Err(crate::EngError::InvalidInput(format!(
                "unknown skillkind: {}",
                s
            ))),
        }
    }
}

/// Broad categorization of a skill's intended use.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum SkillCategory {
    ToolGuide,
    #[default]
    Workflow,
    Reference,
}

// Display and FromStr for SkillCategory.
impl std::fmt::Display for SkillCategory {
    /// Formats the category as its snake_case string representation.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ToolGuide => write!(f, "tool_guide"),
            Self::Workflow => write!(f, "workflow"),
            Self::Reference => write!(f, "reference"),
        }
    }
}

// FromStr for SkillCategory.
impl std::str::FromStr for SkillCategory {
    /// Parse error type.
    type Err = crate::EngError;
    /// Parses a snake_case string into a `SkillCategory` variant.
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "tool_guide" => Ok(Self::ToolGuide),
            "workflow" => Ok(Self::Workflow),
            "reference" => Ok(Self::Reference),
            _ => Err(crate::EngError::InvalidInput(format!(
                "unknown skillcategory: {}",
                s
            ))),
        }
    }
}

/// Access visibility for a skill.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
#[derive(Default)]
pub enum SkillVisibility {
    #[default]
    Private,
    Public,
}

// Display and FromStr for SkillVisibility.
impl std::fmt::Display for SkillVisibility {
    /// Formats the visibility as its lowercase string representation.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Private => write!(f, "private"),
            Self::Public => write!(f, "public"),
        }
    }
}

// FromStr for SkillVisibility.
impl std::str::FromStr for SkillVisibility {
    /// Parse error type.
    type Err = crate::EngError;
    /// Parses a lowercase string into a `SkillVisibility` variant.
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "private" => Ok(Self::Private),
            "public" => Ok(Self::Public),
            _ => Err(crate::EngError::InvalidInput(format!(
                "unknown skillvisibility: {}",
                s
            ))),
        }
    }
}

/// Records how a skill was first produced.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
#[derive(Default)]
pub enum SkillOrigin {
    #[default]
    Imported,
    Captured,
    Derived,
    Fixed,
}

// Display and FromStr for SkillOrigin.
impl std::fmt::Display for SkillOrigin {
    /// Formats the origin as its lowercase string representation.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Imported => write!(f, "imported"),
            Self::Captured => write!(f, "captured"),
            Self::Derived => write!(f, "derived"),
            Self::Fixed => write!(f, "fixed"),
        }
    }
}

// FromStr for SkillOrigin.
impl std::str::FromStr for SkillOrigin {
    /// Parse error type.
    type Err = crate::EngError;
    /// Parses a lowercase string into a `SkillOrigin` variant.
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "imported" => Ok(Self::Imported),
            "captured" => Ok(Self::Captured),
            "derived" => Ok(Self::Derived),
            "fixed" => Ok(Self::Fixed),
            _ => Err(crate::EngError::InvalidInput(format!(
                "unknown skillorigin: {}",
                s
            ))),
        }
    }
}

/// Classification of how a skill was evolved.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum EvolutionType {
    Fix,
    Derived,
    Captured,
}

// Display and FromStr for EvolutionType.
impl std::fmt::Display for EvolutionType {
    /// Formats the evolution type as its lowercase string representation.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Fix => write!(f, "fix"),
            Self::Derived => write!(f, "derived"),
            Self::Captured => write!(f, "captured"),
        }
    }
}

// FromStr for EvolutionType.
impl std::str::FromStr for EvolutionType {
    /// Parse error type.
    type Err = crate::EngError;
    /// Parses a lowercase string into an `EvolutionType` variant.
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "fix" => Ok(Self::Fix),
            "derived" => Ok(Self::Derived),
            "captured" => Ok(Self::Captured),
            _ => Err(crate::EngError::InvalidInput(format!(
                "unknown evolutiontype: {}",
                s
            ))),
        }
    }
}

/// What event triggered a skill evolution pass.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EvolutionTrigger {
    Analysis,
    ToolDegradation,
    MetricMonitor,
}

// Display for EvolutionTrigger.
impl std::fmt::Display for EvolutionTrigger {
    /// Formats the trigger as its snake_case string representation.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Analysis => write!(f, "analysis"),
            Self::ToolDegradation => write!(f, "tool_degradation"),
            Self::MetricMonitor => write!(f, "metric_monitor"),
        }
    }
}

/// Representation format for a skill patch payload.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum PatchType {
    Full,
    Diff,
    Patch,
}

// Display for PatchType.
impl std::fmt::Display for PatchType {
    /// Formats the patch type as its lowercase string representation.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Full => write!(f, "full"),
            Self::Diff => write!(f, "diff"),
            Self::Patch => write!(f, "patch"),
        }
    }
}

/// A single result row returned by the skill search endpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillSearchResult {
    pub skill_id: i64,
    pub name: String,
    pub description: String,
    pub agent: String,
    pub category: String,
    pub origin: String,
    pub score: f64,
    pub source: String,
}

/// Rolled-up quality counters for a single skill.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillQualityMetrics {
    pub skill_id: i64,
    pub total_executions: i32,
    pub success_count: i32,
    pub failure_count: i32,
    pub success_rate: f64,
    pub avg_duration_ms: Option<f64>,
    pub trust_score: f64,
}

/// Input payload for submitting a manual skill judgment.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillJudgmentInput {
    pub skill_id: i64,
    pub skill_applied: bool,
    pub note: String,
}

/// An evolver's recommendation for mutating one or more skills.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvolutionSuggestion {
    pub evolution_type: String,
    pub target_skill_ids: Vec<i64>,
    pub category: Option<String>,
    pub direction: String,
}

/// Result returned after applying an in-place skill edit.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillEditResult {
    pub success: bool,
    pub skill_dir: String,
    pub content: String,
    pub snapshot: std::collections::HashMap<String, String>,
    pub diff: String,
    pub error: Option<String>,
}

/// A skill candidate fetched from or being pushed to the Cloud library.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CloudSkillCandidate {
    pub skill_id: String,
    pub name: String,
    pub description: String,
    pub content: String,
    pub category: String,
    pub origin: String,
    pub tags: Vec<String>,
}

/// Metadata attached when uploading a skill to the Cloud library.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UploadMeta {
    pub origin: String,
    pub parent_skill_ids: Vec<i64>,
    pub tags: Vec<String>,
    pub created_by: String,
    pub change_summary: String,
}

/// Persisted record of a tool dependency declared by a skill.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDependencyRecord {
    pub skill_id: i64,
    pub tool_name: String,
    pub is_optional: bool,
}

/// A named, ordered stage within the skill execution pipeline.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PipelineStage {
    pub id: String,
    pub name: String,
    pub description: String,
    pub order: i32,
}

/// Returns the canonical ordered list of pipeline stages.
pub fn pipeline_stages() -> Vec<PipelineStage> {
    vec![
        PipelineStage {
            id: "initialize".into(),
            name: "Initialize".into(),
            description: "Load grounding client and skill registry".into(),
            order: 0,
        },
        PipelineStage {
            id: "select-skills".into(),
            name: "Skill Selection".into(),
            description: "Hybrid search for matching skills, rank".into(),
            order: 1,
        },
        PipelineStage {
            id: "skill-phase".into(),
            name: "Skill Phase".into(),
            description: "Execute task with skill context via LLM".into(),
            order: 2,
        },
        PipelineStage {
            id: "tool-fallback".into(),
            name: "Tool Fallback".into(),
            description: "Retry with tools only if skill phase fails".into(),
            order: 3,
        },
        PipelineStage {
            id: "analysis".into(),
            name: "Analysis".into(),
            description: "Run execution analyzer, persist results".into(),
            order: 4,
        },
        PipelineStage {
            id: "evolution".into(),
            name: "Evolution".into(),
            description: "Trigger FIX/DERIVED/CAPTURED based on analysis".into(),
            order: 5,
        },
    ]
}

// -- Core DTOs (moved from mod.rs; re-exported by skills/mod.rs) --

/// Full skill record as stored in `skill_records`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Skill {
    pub id: i64,
    pub name: String,
    pub agent: String,
    pub description: Option<String>,
    pub code: String,
    pub language: String,
    pub version: i32,
    pub parent_skill_id: Option<i64>,
    pub root_skill_id: Option<i64>,
    pub trust_score: f64,
    pub success_count: i32,
    pub failure_count: i32,
    pub execution_count: i32,
    pub avg_duration_ms: Option<f64>,
    pub is_active: bool,
    pub is_deprecated: bool,
    pub metadata: Option<String>,
    pub user_id: i64,
    pub created_at: String,
    pub updated_at: String,
    // Skills Cloud (v50+) fields. Default to "skill" / NULL when absent so
    // legacy rows deserialize cleanly.
    #[serde(default = "default_kind")]
    pub kind: String,
    #[serde(default)]
    pub source_plugin: Option<String>,
    #[serde(default)]
    pub source_path: Option<String>,
    #[serde(default)]
    pub content_hash: Option<String>,
}

/// Returns the default kind string for serde deserialization.
fn default_kind() -> String {
    "skill".to_string()
}

/// Request body for creating a new skill.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateSkillRequest {
    pub name: String,
    pub agent: String,
    pub description: Option<String>,
    pub code: String,
    pub language: Option<String>,
    pub parent_skill_id: Option<i64>,
    pub metadata: Option<String>,
    pub user_id: Option<i64>,
    pub tags: Option<Vec<String>>,
    pub tool_deps: Option<Vec<String>>,
    // Skills Cloud (v50+): all optional; importer fills these, hand-captured
    // skills omit them and default to NULL / "skill".
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub source_plugin: Option<String>,
    #[serde(default)]
    pub source_path: Option<String>,
    #[serde(default)]
    pub content_hash: Option<String>,
}

/// Request body for updating an existing skill; all fields are optional.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateSkillRequest {
    pub code: Option<String>,
    pub description: Option<String>,
    pub is_active: Option<bool>,
    pub is_deprecated: Option<bool>,
    pub metadata: Option<String>,
    // Skills Cloud: importer re-runs may rewrite kind / hash without touching
    // the rest of the row. None means "leave as-is".
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub source_path: Option<String>,
    #[serde(default)]
    pub content_hash: Option<String>,
}

/// A single execution attempt recorded in `execution_analyses`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionRecord {
    pub id: i64,
    pub skill_id: i64,
    pub success: bool,
    pub duration_ms: Option<f64>,
    pub error_type: Option<String>,
    pub error_message: Option<String>,
    pub input_hash: Option<String>,
    pub output_hash: Option<String>,
    pub metadata: Option<String>,
    pub created_at: String,
}

/// A stored judgment score for a skill from a judge agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillJudgment {
    pub id: i64,
    pub skill_id: i64,
    pub judge_agent: String,
    pub score: f64,
    pub rationale: Option<String>,
    pub created_at: String,
}

/// A single tool quality observation stored in `tool_quality_records`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolQuality {
    pub id: i64,
    pub tool_name: String,
    pub agent: String,
    pub success: bool,
    pub latency_ms: Option<f64>,
    pub error_type: Option<String>,
    pub created_at: String,
}

// -- Submodule DTOs --

/// Patch format detected from incoming patch payload content.
#[derive(Debug, Clone, PartialEq)]
pub enum DetectedPatchType {
    Full,
    SearchReplace,
    MultiFile,
}

/// A result row returned by a cloud skill search query.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CloudSearchResult {
    pub skill_id: String,
    pub name: String,
    pub description: String,
    pub category: String,
    pub origin: String,
    pub tags: Vec<String>,
    pub score: f64,
}

/// Aggregate counts and averages across the skill registry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillOverview {
    pub total_skills: i64,
    pub active_skills: i64,
    pub deprecated_skills: i64,
    pub total_executions: i64,
    pub avg_trust_score: f64,
}

/// Per-skill execution and trust statistics for dashboard display.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillStats {
    pub id: i64,
    pub name: String,
    pub execution_count: i32,
    pub success_count: i32,
    pub failure_count: i32,
    pub trust_score: f64,
    pub computed_score: f64,
}

/// A single message in a conversation thread sent to an LLM backend.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationMessage {
    pub role: String,
    pub content: String,
    #[serde(default)]
    pub priority: Option<u8>,
}

/// Structured analysis of a single skill execution outcome.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionAnalysis {
    pub skill_applied: bool,
    pub skill_helpful: bool,
    pub tool_calls: Vec<String>,
    pub error_category: Option<String>,
    pub improvement_notes: Option<String>,
}

/// Request body for triggering a skill evolution operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvolutionRequest {
    pub evolution_type: String,
    pub target_skill_ids: Vec<i64>,
    pub category: Option<String>,
    pub direction: String,
}

/// Outcome of a skill evolution operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvolutionResult {
    pub success: bool,
    pub skill_id: Option<i64>,
    pub evolution_type: String,
    pub message: String,
}

/// One row in the recent-evolution feed returned by
/// `GET /skills/evolution/recent`. `origin` is one of
/// `fixed` | `derived` | `captured`, sourced from `skill_tags`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvolutionFeedRow {
    pub skill_id: i64,
    pub name: String,
    pub version: i32,
    pub origin: String,
    pub parent_ids: Vec<i64>,
    pub agent: String,
    pub created_at: String,
}

/// Unit tests for skill type serialization round-trips.
#[cfg(test)]
mod tests {
    use super::*;
    /// Verifies SkillCategory Display output.
    #[test]
    fn test_cat() {
        assert_eq!(SkillCategory::ToolGuide.to_string(), "tool_guide");
    }
    /// Verifies SkillOrigin round-trips through Display and FromStr.
    #[test]
    fn test_origin() {
        for o in &[SkillOrigin::Imported, SkillOrigin::Fixed] {
            assert_eq!(&o.to_string().parse::<SkillOrigin>().unwrap(), o);
        }
    }
    /// Verifies EvolutionType round-trips through Display and FromStr.
    #[test]
    fn test_evo() {
        for e in &[EvolutionType::Fix, EvolutionType::Captured] {
            assert_eq!(&e.to_string().parse::<EvolutionType>().unwrap(), e);
        }
    }
    /// Verifies the pipeline has the expected number of stages.
    #[test]
    fn test_stages() {
        assert_eq!(pipeline_stages().len(), 6);
    }
    /// Verifies SkillVisibility parses from string.
    #[test]
    fn test_vis() {
        assert_eq!(
            "private".parse::<SkillVisibility>().unwrap(),
            SkillVisibility::Private
        );
    }
}
