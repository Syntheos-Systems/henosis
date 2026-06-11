#![deny(missing_docs)]
#![warn(clippy::all)]
//! # agent-forge-lib
//!
//! Agent-Forge as a library (Phase 2 Story 2.1): the structured-reasoning gate tools --
//! spec/hypothesis/approaches/verify/challenge, Tree-sitter AST search and repo maps, session
//! learning, and the skills surface -- extracted from the Kleos-repo CLI binary so
//! `syntheos-server` (and the Phase 2 EidolonGate work) can call them in-process. The thin
//! `agent-forge` binary in this crate keeps the exact CLI contract (subcommand + `--input` /
//! `--output` JSON files + `--db`), so existing hooks keep working against this build.
//!
//! This is a copy-and-own absorption (the chiasm/soma precedent): the Kleos repo's
//! `agent-forge` keeps shipping untouched until the cutover retires it; this copy is kept
//! intentionally close to upstream (chrono timestamps and all) so fixes can be ported across
//! by diff until then.
//!
//! The one structural change from upstream: Kleos HTTP coupling goes through the
//! [`SkillsBridge`] seam instead of a hardwired client (the OutputFilter/Narrator/StepExecutor
//! pattern). The CLI wires the feature-gated [`bridge::HttpSkillsBridge`] (`KLEOS_URL` +
//! `KLEOS_API_KEY` env; the phylax-broker keyless fallback deliberately stays in the
//! Kleos-repo binary, which keeps that cross-repo dependency out of this workspace).
//! NOTE: the roadmap story text mentions a `FORGE_URL` fallback; no such variable exists in
//! the upstream source or hooks -- the real variable is `KLEOS_URL`, and nothing was invented
//! for the phantom one.

pub mod bridge;
pub mod db;
pub mod json_io;
pub mod tools;
pub mod treesitter;

pub use bridge::SkillsBridge;
pub use db::Database;
pub use json_io::Output;
pub use tools::{ToolError, ToolResult};

use serde_json::Value;

/// Every agent-forge tool, one variant per CLI subcommand (the names map 1:1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(missing_docs)] // The variants ARE the documentation: they mirror the CLI verbs 1:1.
pub enum Tool {
    SpecTask,
    ConsiderApproaches,
    LogHypothesis,
    LogOutcome,
    RecallErrors,
    Verify,
    ChallengeCode,
    CommentCheck,
    Checkpoint,
    Rollback,
    SessionLearn,
    SessionRecall,
    SessionDiff,
    Think,
    DeclareUnknowns,
    UpdateSpec,
    ListSpecs,
    GetSpec,
    Stats,
    RepoMap,
    SearchCode,
    SkillSearch,
    SkillCapture,
    SkillRecordExec,
    SkillFix,
    SkillDerive,
    SkillLineage,
}

/// Deserialize `input` into the tool's typed input and run it, used by both the CLI and
/// in-process callers. Any failure (bad input shape or tool error) lands in the standard
/// [`Output`] error envelope rather than an `Err`, mirroring the CLI contract.
///
/// `bridge` supplies the skills backend; with `None`, the six `Skill*` tools (and the
/// opportunistic skill lookups inside `SpecTask`/`Verify`/`SessionLearn`) degrade exactly as
/// the upstream CLI does without a reachable Kleos.
pub fn run_tool(
    db: &Database,
    tool: Tool,
    input: Value,
    bridge: Option<&dyn SkillsBridge>,
) -> Output {
    /// Deserialize the tool input, run the tool, and flatten both error layers into `Output`.
    fn run<T: serde::de::DeserializeOwned>(
        input: Value,
        f: impl FnOnce(T) -> ToolResult,
    ) -> Output {
        match serde_json::from_value::<T>(input) {
            Ok(typed) => match f(typed) {
                Ok(output) => output,
                Err(e) => Output::error(e.to_string()),
            },
            Err(e) => Output::error(format!("Failed to parse JSON: {e}")),
        }
    }
    match tool {
        Tool::SpecTask => run(input, |i| tools::spec::spec_task(db, bridge, i)),
        Tool::ConsiderApproaches => run(input, |i| tools::approaches::consider_approaches(db, i)),
        Tool::LogHypothesis => run(input, |i| tools::hypothesis::log_hypothesis(db, i)),
        Tool::LogOutcome => run(input, |i| tools::hypothesis::log_outcome(db, i)),
        Tool::RecallErrors => run(input, |i| tools::hypothesis::recall_errors(db, i)),
        Tool::Verify => run(input, |i| tools::verify::verify(db, bridge, i)),
        Tool::ChallengeCode => run(input, |i| tools::verify::challenge_code(db, i)),
        Tool::CommentCheck => run(input, |i| tools::comments::comment_check(db, i)),
        Tool::Checkpoint => run(input, |i| tools::session::checkpoint(db, i)),
        Tool::Rollback => run(input, |i| tools::session::rollback(db, i)),
        Tool::SessionLearn => run(input, |i| tools::session::session_learn(db, bridge, i)),
        Tool::SessionRecall => run(input, |i| tools::session::session_recall(db, i)),
        Tool::SessionDiff => run(input, |i| tools::verify::session_diff(db, i)),
        Tool::Think => run(input, |i| tools::think::think(db, i)),
        Tool::DeclareUnknowns => run(input, |i| tools::think::declare_unknowns(db, i)),
        Tool::UpdateSpec => run(input, |i| tools::spec::update_spec(db, i)),
        Tool::ListSpecs => run(input, |i| tools::spec::list_specs(db, i)),
        Tool::GetSpec => run(input, |i| tools::spec::get_spec(db, i)),
        Tool::Stats => run(input, |i| tools::stats::stats(db, i)),
        Tool::RepoMap => run(input, |i| tools::ast::repo_map::repo_map(db, i)),
        Tool::SearchCode => run(input, |i| tools::ast::search::search_code(db, i)),
        Tool::SkillSearch => run(input, |i| tools::skills::skill_search(bridge, i)),
        Tool::SkillCapture => run(input, |i| tools::skills::skill_capture(bridge, i)),
        Tool::SkillRecordExec => run(input, |i| tools::skills::skill_record_exec(bridge, i)),
        Tool::SkillFix => run(input, |i| tools::skills::skill_fix(bridge, i)),
        Tool::SkillDerive => run(input, |i| tools::skills::skill_derive(bridge, i)),
        Tool::SkillLineage => run(input, |i| tools::skills::skill_lineage(bridge, i)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh forge database in a temp dir.
    fn db() -> (Database, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = Database::open(&dir.path().join("forge.db")).expect("open");
        (db, dir)
    }

    /// The lib API runs a full spec lifecycle in-process: create, get, list.
    #[test]
    fn run_tool_spec_roundtrip() {
        let (db, _dir) = db();
        let created = run_tool(
            &db,
            Tool::SpecTask,
            serde_json::json!({
                "task_description": "wire the forge lib",
                "task_type": "feature",
                "acceptance_criteria": ["lib API callable", "CLI contract intact"],
                "interface_contract": "run_tool(db, tool, input, bridge) -> Output",
                "edge_cases": ["no bridge", "bad input", "missing db"],
            }),
            None,
        );
        assert!(created.success, "{}", created.message);
        let id = created.id.clone().expect("spec id");

        let got = run_tool(
            &db,
            Tool::GetSpec,
            serde_json::json!({ "spec_id": id }),
            None,
        );
        assert!(got.success, "{}", got.message);
        let listed = run_tool(&db, Tool::ListSpecs, serde_json::json!({}), None);
        assert!(listed.success, "{}", listed.message);
    }

    /// Without a bridge, skill tools degrade to the standard error envelope (no panic, no Err).
    #[test]
    fn skill_tools_without_bridge_degrade_cleanly() {
        let (db, _dir) = db();
        let out = run_tool(
            &db,
            Tool::SkillSearch,
            serde_json::json!({ "query": "anything" }),
            None,
        );
        assert!(!out.success);
        assert!(out.message.contains("bridge"), "{}", out.message);
    }

    /// Malformed input lands in the error envelope, mirroring the CLI contract.
    #[test]
    fn bad_input_becomes_error_envelope() {
        let (db, _dir) = db();
        let out = run_tool(
            &db,
            Tool::SpecTask,
            serde_json::json!({ "task_description": 42 }),
            None,
        );
        assert!(!out.success);
        assert!(out.message.contains("parse"), "{}", out.message);
    }
}
