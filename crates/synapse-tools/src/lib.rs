//! synapse-tools: Tool trait, built-in coding tools, and capability gating for Synapse agent.

pub mod bash;
pub mod capability;
mod confined_fs;
pub mod delegate;
pub mod edit;
pub mod executor;
pub mod forge;
pub mod glob;
pub mod grep;
pub mod kleos;
pub mod ls;
pub mod lsp;
pub mod pistis_gate;
pub mod read;
pub mod recall;
pub mod session;
pub mod skill_invoke;
pub mod tool;
pub mod web;
pub mod write;

pub use capability::Capability;
pub use confined_fs::ToolExecutionContext;
pub use executor::ToolRegistryExecutor;
#[cfg(feature = "henosis-pistis")]
pub use pistis_gate::henosis::HenosisAuthority;
pub use pistis_gate::{
    AuthorizationOutcome, LocalAuthority, PistisAuthority, PistisClient, PistisGate,
};
pub use recall::{RecallDueMemory, RecallOptions, fetch_recall_due, recall_due_as_blocks};
pub use skill_invoke::SkillInvokeTool;
pub use tool::{
    AgentTool, DenyAllGate, GateDecision, PermissiveGate, SharedGate, ToolGate, ToolRegistry,
    ToolResult,
};

/// Create a `ToolRegistry` pre-populated with all built-in tools.
pub fn default_tools() -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    // Core coding tools (7)
    registry.register(Box::new(bash::BashTool));
    registry.register(Box::new(read::ReadTool));
    registry.register(Box::new(write::WriteTool));
    registry.register(Box::new(edit::EditTool));
    registry.register(Box::new(grep::GrepTool));
    registry.register(Box::new(glob::GlobTool));
    registry.register(Box::new(ls::LsTool));
    // Web (2)
    registry.register(Box::new(web::WebFetchTool));
    registry.register(Box::new(web::WebSearchTool));
    // Kleos (memory, brain, graph, intelligence, skills, handoffs, activity,
    //        tasks, axon, broca, soma, thymus, loom, conversations, episodes,
    //        personality, scratchpad, gate, growth, fsrs, prompts)
    registry.register(Box::new(kleos::KleosSearchTool));
    registry.register(Box::new(kleos::KleosStoreTool));
    registry.register(Box::new(kleos::KleosDeleteTool));
    registry.register(Box::new(kleos::KleosListTool));
    registry.register(Box::new(kleos::KleosContextTool));
    registry.register(Box::new(kleos::KleosRecallTool));
    registry.register(Box::new(kleos::KleosFacetedSearchTool));
    registry.register(Box::new(kleos::KleosProfileTool));
    registry.register(Box::new(kleos::BrainQueryTool));
    registry.register(Box::new(kleos::BrainAbsorbTool));
    registry.register(Box::new(kleos::GraphSearchTool));
    registry.register(Box::new(kleos::GraphNeighborhoodTool));
    registry.register(Box::new(kleos::GraphCreateEntityTool));
    registry.register(Box::new(kleos::IntelligenceConsolidateTool));
    registry.register(Box::new(kleos::IntelligenceContradictionsTool));
    registry.register(Box::new(kleos::IntelligenceReflectTool));
    registry.register(Box::new(kleos::IntelligenceDigestTool));
    registry.register(Box::new(kleos::IntelligenceSentimentTool));
    registry.register(Box::new(kleos::IntelligenceTimeTravelTool));
    registry.register(Box::new(kleos::SkillSearchTool));
    registry.register(Box::new(kleos::SkillGetTool));
    registry.register(Box::new(kleos::SkillExecuteTool));
    registry.register(Box::new(kleos::SkillCreateTool));
    registry.register(Box::new(kleos::SkillListTool));
    registry.register(Box::new(SkillInvokeTool));
    registry.register(Box::new(kleos::HandoffStoreTool));
    registry.register(Box::new(kleos::HandoffRestoreTool));
    registry.register(Box::new(kleos::HandoffSearchTool));
    registry.register(Box::new(kleos::ActivityReportTool));
    registry.register(Box::new(kleos::TaskCreateTool));
    registry.register(Box::new(kleos::TaskUpdateTool));
    registry.register(Box::new(kleos::TaskListTool));
    registry.register(Box::new(kleos::TaskFeedTool));
    registry.register(Box::new(kleos::AxonPublishTool));
    registry.register(Box::new(kleos::AxonPollTool));
    registry.register(Box::new(kleos::BrocaLogTool));
    registry.register(Box::new(kleos::SomaRegisterTool));
    registry.register(Box::new(kleos::SomaHeartbeatTool));
    registry.register(Box::new(kleos::ThymusEvalTool));
    registry.register(Box::new(kleos::LoomCreateWorkflowTool));
    registry.register(Box::new(kleos::LoomCreateRunTool));
    registry.register(Box::new(kleos::LoomCompleteStepTool));
    registry.register(Box::new(kleos::ConversationCreateTool));
    registry.register(Box::new(kleos::ConversationMessageTool));
    registry.register(Box::new(kleos::ConversationSearchTool));
    registry.register(Box::new(kleos::EpisodeCreateTool));
    registry.register(Box::new(kleos::EpisodeFinalizeTool));
    registry.register(Box::new(kleos::PersonalityProfileTool));
    registry.register(Box::new(kleos::PersonalityDetectTool));
    registry.register(Box::new(kleos::ScratchPutTool));
    registry.register(Box::new(kleos::ScratchListTool));
    registry.register(Box::new(kleos::ScratchPromoteTool));
    registry.register(Box::new(kleos::GateCheckTool));
    registry.register(Box::new(kleos::GateRespondTool));
    registry.register(Box::new(kleos::GrowthReflectTool));
    registry.register(Box::new(kleos::GrowthObservationsTool));
    registry.register(Box::new(kleos::FsrsRecallDueTool));
    registry.register(Box::new(kleos::FsrsReviewTool));
    registry.register(Box::new(kleos::PromptGenerateTool));
    registry.register(Box::new(kleos::PromptHeaderTool));
    // Agent-forge (12)
    registry.register(Box::new(forge::RepoMapTool));
    registry.register(Box::new(forge::SearchCodeTool));
    registry.register(Box::new(forge::ExecuteTool));
    registry.register(Box::new(forge::VerifyTool));
    registry.register(Box::new(forge::AstSearchTool));
    registry.register(Box::new(forge::LogHypothesisTool));
    registry.register(Box::new(forge::LogOutcomeTool));
    registry.register(Box::new(forge::RecallErrorsTool));
    registry.register(Box::new(forge::TestImpactTool));
    registry.register(Box::new(forge::SessionDiffTool));
    registry.register(Box::new(forge::ProseAnalyzeTool));
    registry.register(Box::new(forge::ProseLearnTool));
    // LSP (2)
    registry.register(Box::new(lsp::LspDiagnosticsTool));
    registry.register(Box::new(lsp::LspSymbolSearchTool));
    // Session (2)
    registry.register(Box::new(session::SessionSearchTool));
    registry.register(Box::new(session::SessionListTool));
    registry
}

/// Verifies filesystem tools enforce the task-root boundary at their public interface.
#[cfg(test)]
mod filesystem_tool_tests {
    use super::*;
    use serde_json::{Value, json};
    use std::fs;
    use std::path::Path;

    /// Builds every filesystem tool call using the supplied path.
    fn filesystem_tool_cases(path: &str) -> Vec<(Box<dyn AgentTool>, Value)> {
        vec![
            (Box::new(read::ReadTool), json!({"file_path": path})),
            (
                Box::new(write::WriteTool),
                json!({"file_path": path, "content": "changed"}),
            ),
            (
                Box::new(edit::EditTool),
                json!({
                    "file_path": path,
                    "old_string": "outside",
                    "new_string": "changed"
                }),
            ),
            (Box::new(ls::LsTool), json!({"path": path})),
            (
                Box::new(grep::GrepTool),
                json!({"pattern": "outside", "path": path}),
            ),
            (
                Box::new(glob::GlobTool),
                json!({"pattern": "**/*", "path": path}),
            ),
        ]
    }

    /// Requires every provided tool call to fail closed.
    async fn assert_tool_errors(cases: Vec<(Box<dyn AgentTool>, Value)>, root: &Path) {
        for (tool, params) in cases {
            let name = tool.name().to_string();
            let result = tool.execute(params, root).await.expect("tool result");
            assert!(result.is_error, "{name} unexpectedly escaped the task root");
        }
    }

    /// Confirms all filesystem tools reject both parent traversal and absolute paths.
    #[tokio::test]
    async fn filesystem_tools_reject_parent_and_absolute_paths() {
        let workspace = tempfile::tempdir().expect("workspace");
        let root = workspace.path().join("task");
        fs::create_dir(&root).expect("task root");
        let outside = workspace.path().join("outside.txt");
        fs::write(&outside, b"outside").expect("outside file");

        assert_tool_errors(filesystem_tool_cases("../outside.txt"), &root).await;
        assert_tool_errors(filesystem_tool_cases(&outside.display().to_string()), &root).await;

        assert_eq!(fs::read(&outside).expect("outside file"), b"outside");
    }

    /// Confirms valid nested paths preserve write, read, edit, list, grep, and glob behavior.
    #[tokio::test]
    async fn filesystem_tools_support_valid_nested_paths() {
        let root = tempfile::tempdir().expect("task root");

        let written = write::WriteTool
            .execute(
                json!({"file_path": "nested/note.txt", "content": "alpha"}),
                root.path(),
            )
            .await
            .expect("write result");
        assert!(!written.is_error, "{}", written.content);

        let read = read::ReadTool
            .execute(json!({"file_path": "nested/note.txt"}), root.path())
            .await
            .expect("read result");
        assert!(!read.is_error, "{}", read.content);
        assert!(read.content.contains("alpha"));

        let edited = edit::EditTool
            .execute(
                json!({
                    "file_path": "nested/note.txt",
                    "old_string": "alpha",
                    "new_string": "beta"
                }),
                root.path(),
            )
            .await
            .expect("edit result");
        assert!(!edited.is_error, "{}", edited.content);

        let listed = ls::LsTool
            .execute(json!({"path": "nested"}), root.path())
            .await
            .expect("ls result");
        assert!(!listed.is_error, "{}", listed.content);
        assert!(listed.content.contains("note.txt"));

        let grepped = grep::GrepTool
            .execute(json!({"pattern": "beta", "path": "nested"}), root.path())
            .await
            .expect("grep result");
        assert!(!grepped.is_error, "{}", grepped.content);
        assert!(grepped.content.contains("beta"));

        let globbed = glob::GlobTool
            .execute(json!({"pattern": "*.txt", "path": "nested"}), root.path())
            .await
            .expect("glob result");
        assert!(!globbed.is_error, "{}", globbed.content);
        assert!(globbed.content.contains("note.txt"));
    }

    /// Confirms direct and recursive symlink escapes fail closed on Unix.
    #[cfg(unix)]
    #[tokio::test]
    async fn filesystem_tools_reject_symlink_escapes() {
        let workspace = tempfile::tempdir().expect("workspace");
        let root = workspace.path().join("task");
        let outside = workspace.path().join("outside");
        fs::create_dir(&root).expect("task root");
        fs::create_dir(&outside).expect("outside root");
        fs::write(outside.join("secret.txt"), b"TOP_SECRET").expect("outside file");
        std::os::unix::fs::symlink(&outside, root.join("escape")).expect("escape symlink");

        assert_tool_errors(
            vec![
                (
                    Box::new(read::ReadTool),
                    json!({"file_path": "escape/secret.txt"}),
                ),
                (
                    Box::new(write::WriteTool),
                    json!({"file_path": "escape/secret.txt", "content": "changed"}),
                ),
                (
                    Box::new(edit::EditTool),
                    json!({
                        "file_path": "escape/secret.txt",
                        "old_string": "TOP_SECRET",
                        "new_string": "changed"
                    }),
                ),
                (Box::new(ls::LsTool), json!({"path": "escape"})),
                (
                    Box::new(grep::GrepTool),
                    json!({"pattern": "TOP_SECRET", "path": "escape"}),
                ),
                (
                    Box::new(glob::GlobTool),
                    json!({"pattern": "**/*", "path": "escape"}),
                ),
            ],
            &root,
        )
        .await;

        let grepped = grep::GrepTool
            .execute(json!({"pattern": "TOP_SECRET", "path": "."}), &root)
            .await
            .expect("recursive grep result");
        assert!(!grepped.is_error, "{}", grepped.content);
        assert!(!grepped.content.contains("=== "));

        let globbed = glob::GlobTool
            .execute(json!({"pattern": "**/*.txt", "path": "."}), &root)
            .await
            .expect("recursive glob result");
        assert!(!globbed.is_error, "{}", globbed.content);
        assert!(!globbed.content.contains("secret.txt"));
        assert_eq!(
            fs::read(outside.join("secret.txt")).expect("outside file"),
            b"TOP_SECRET"
        );
    }
}
