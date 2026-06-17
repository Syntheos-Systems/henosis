//! synapse-tools: Tool trait, built-in coding tools, and capability gating for Synapse agent.

pub mod bash;
pub mod capability;
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
pub use executor::ToolRegistryExecutor;
pub use pistis_gate::{PistisClient, PistisGate};
pub use recall::{RecallDueMemory, RecallOptions, fetch_recall_due, recall_due_as_blocks};
pub use skill_invoke::SkillInvokeTool;
pub use tool::{
    AgentTool, GateDecision, PermissiveGate, SharedGate, ToolGate, ToolRegistry, ToolResult,
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
