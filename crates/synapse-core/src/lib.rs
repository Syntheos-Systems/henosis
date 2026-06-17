//! synapse-core: Async agent loop engine and AgentExecutor contract for Rift.
//!
//! Pure async function returning Stream<AgentEvent>.
//! No UI, no IO -- just the reasoning loop.
//!
//! The `executor` module defines the `AgentExecutor` trait used by rift-bridge.
//! The `executors` module provides the Synapse-native implementation.

pub mod agent;
pub mod automate;
pub mod commands;
pub mod compression;
pub mod context;
pub mod cost;
pub mod executor;
pub mod executors;
pub mod hooks;
pub mod persona;
pub mod router;
pub mod session_runtime;
pub mod system_prompt;
pub mod types;

pub use agent::{agent_loop, agent_loop_with_pricing, agent_turn, agent_turn_with_pricing};
pub use automate::{
    AutomateOptions, AutomateStatus, Pick, select_for_task, status as automate_status,
};
pub use commands::{
    Command, CommandOutcome, CommandRegistry, ModelCommand, PersonaCommand, QuitCommand,
    SearchCommand, SharedCommand, register_builtins,
};
pub use compression::CompressionConfig;
pub use context::ConversationContext;
pub use cost::{ModelCost, PricingTable, SessionCost};
pub use executor::{
    AgentExecutor, AgentResponse, Capability, ConversationMessage, DiscussionContext,
    ExecutionProposal, ExecutionResult, ExecutionSandbox, HealthStatus, ProgressUpdate,
    TaskContext,
};
pub use executors::SynapseExecutor;
pub use hooks::{HookConfig, HookGate, HookPhase, HookSpec, run_phase_hooks};
pub use persona::{
    Persona, ResolutionSource, ResolverOptions, list_available, load_by_name, resolve,
};
pub use router::ModelRouter;
pub use session_runtime::{
    SessionEvent, SessionId, SessionManager, SessionSnapshot, SessionStatus,
};
pub use system_prompt::{
    DEFAULT_BASE_SPINE, DEFAULT_UNTRUSTED_RULES, PromptSection, SkillIndexEntry,
    SystemPromptBuilder,
};
pub use types::{AgentConfig, AgentEvent};
