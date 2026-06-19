//! `SynapseExecutor`: `AgentExecutor` backed by `synapse_core::agent_loop`.
//!
//! This is the Synapse-native executor. Rift-bridge instantiates it to run
//! Synapse sessions as room participants. Configuration is injected at
//! construction time; the executor is stateless across calls (no stored
//! conversation history -- each `discuss` and `execute` call is independent).
//!
//! ## discuss mode
//! One LLM round-trip, no tool access, returns the first assistant text block.
//! An empty `ToolRegistry` is used so the model cannot call tools.
//!
//! ## execute mode
//! Full `agent_loop` with the injected `ToolRegistry`. `AgentEvent`s are
//! mapped to `ProgressUpdate`s and forwarded to the bridge's `progress_tx`.

use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use futures::StreamExt;
use tokio::sync::mpsc;

use synapse_provider::Provider;
use synapse_session::SessionStore;
use synapse_tools::{PermissiveGate, PistisGate, ToolRegistry};

use crate::agent::agent_loop;
use crate::executor::{
    AgentExecutor, AgentResponse, Capability, DiscussionContext, ExecutionResult, ExecutionSandbox,
    HealthStatus, ProgressUpdate, TaskContext,
};
use crate::hooks::HookConfig;
use crate::types::{AgentConfig, AgentEvent};

// ---------------------------------------------------------------------------
// SynapseExecutor
// ---------------------------------------------------------------------------

/// Synapse-native implementation of `AgentExecutor`.
///
/// Holds the shared infrastructure needed to run agent loops: the LLM provider,
/// full tool registry, hook configuration, session store, and a base
/// `AgentConfig` template. Each `discuss`/`execute` call clones and customises
/// the template for that invocation.
pub struct SynapseExecutor {
    /// LLM provider (Anthropic, proxy, etc.).
    provider: Arc<dyn Provider + Send + Sync>,
    /// All tools available in execution mode.
    tools: Arc<ToolRegistry>,
    /// Hook configuration applied in execution mode.
    hooks: Arc<HookConfig>,
    /// Optional session store for persisting turns (execution mode only).
    session_store: Option<Arc<SessionStore>>,
    /// Template configuration cloned for each invocation.
    base_config: AgentConfig,
}

/// Adds inherent behavior for `SynapseExecutor`.
impl SynapseExecutor {
    /// Construct a new `SynapseExecutor`.
    ///
    /// `base_config` is cloned for each discussion/execution invocation; callers
    /// should set reasonable defaults for `model`, `system_prompt`, `max_turns`,
    /// and `max_tokens`. The `tool_gate`, `hooks`, `session_store`, and
    /// `session_id` fields are overridden per-call.
    pub fn new(
        provider: Arc<dyn Provider + Send + Sync>,
        tools: Arc<ToolRegistry>,
        hooks: Arc<HookConfig>,
        session_store: Option<Arc<SessionStore>>,
        base_config: AgentConfig,
    ) -> Self {
        Self {
            provider,
            tools,
            hooks,
            session_store,
            base_config,
        }
    }

    /// Build an `AgentConfig` for a discussion turn.
    ///
    /// Discussion turns use an empty tool registry, a single-turn limit, and
    /// the persona framing from the context as the system prompt.
    fn discussion_config(&self, ctx: &DiscussionContext) -> (AgentConfig, Arc<ToolRegistry>) {
        let system = ctx
            .system_framing
            .clone()
            .unwrap_or_else(|| self.base_config.system_prompt.clone());

        let mut config = self.base_config.clone();
        config.system_prompt = system;
        // Single turn -- discuss does not loop.
        config.max_turns = 1;
        // No session persistence for lightweight discussion turns.
        config.session_store = None;
        config.session_id = None;
        // No tool gate needed -- empty registry means no tools to gate.
        config.tool_gate = None;
        config.hooks = None;
        config.cwd = self.base_config.cwd.clone();

        // Empty registry: discussion mode has no tool access.
        let empty_tools = Arc::new(ToolRegistry::new());
        (config, empty_tools)
    }

    /// Build an `AgentConfig` for an execution session.
    ///
    /// Uses the full tool registry, the task's working directory, and the
    /// hook configuration. Session persistence is enabled when a store is
    /// available.
    fn execution_config(&self, task: &TaskContext) -> AgentConfig {
        let mut config = self.base_config.clone();
        let inner_gate = config
            .tool_gate
            .clone()
            .unwrap_or_else(|| Arc::new(PermissiveGate));
        config.cwd = task.sandbox.working_dir.clone();
        config.session_store = self.session_store.clone();
        config.hooks = Some(self.hooks.clone());
        config.tool_gate = Some(Arc::new(PistisGate::from_granted_capabilities(
            task.granted_capabilities.clone(),
            inner_gate,
        )));
        config
    }

    /// Format the `DiscussionContext` as a single user message for the LLM.
    ///
    /// Combines recent conversation history and persona context into one string.
    /// External content is bracketed to separate it from instruction framing.
    fn format_discussion_message(ctx: &DiscussionContext) -> String {
        let mut parts: Vec<String> = Vec::new();

        if let Some(name) = &ctx.persona_name {
            parts.push(format!("[persona: {name}]"));
        }

        if !ctx.relevant_memories.is_empty() {
            parts.push("[relevant memories]".into());
            for mem in &ctx.relevant_memories {
                parts.push(format!("  - {mem}"));
            }
            parts.push("[/relevant memories]".into());
        }

        if let Some(tasks) = &ctx.active_tasks_summary {
            parts.push(format!("[active tasks]\n{tasks}\n[/active tasks]"));
        }

        if !ctx.recent_messages.is_empty() {
            parts.push("[conversation]".into());
            for msg in &ctx.recent_messages {
                parts.push(format!("{}: {}", msg.author, msg.text));
            }
            parts.push("[/conversation]".into());
        }

        parts.push("Respond naturally as your persona. If you have nothing to add, reply with exactly: [PASS]".into());

        parts.join("\n")
    }

    /// Format the `TaskContext` as the initial user message for `agent_loop`.
    ///
    /// Injects task description, prior context, and Pistis capability list.
    fn format_execution_message(task: &TaskContext) -> String {
        let caps: Vec<&str> = task
            .granted_capabilities
            .iter()
            .map(|c| c.as_str())
            .collect();

        let mut parts = vec![
            format!("[task_id: {}]", task.task_id),
            format!("[granted_capabilities: {}]", caps.join(", ")),
            String::new(),
            task.description.clone(),
        ];

        if let Some(prior) = &task.prior_context {
            parts.push(format!("\n[prior context]\n{prior}\n[/prior context]"));
        }

        parts.join("\n")
    }

    /// Extract the first assistant text block from a stream of `AgentEvent`s.
    ///
    /// Collects all `AgentEvent::Text` deltas and joins them. Returns `None`
    /// if no text was produced (e.g., the model passed with `[PASS]` or the
    /// stream was empty).
    async fn collect_text(stream: impl futures::Stream<Item = AgentEvent>) -> String {
        futures::pin_mut!(stream);
        let mut text = String::new();
        while let Some(event) = stream.next().await {
            if let AgentEvent::Text(delta) = event {
                text.push_str(&delta);
            }
        }
        text
    }
}

/// Implements `AgentExecutor` behavior for `SynapseExecutor`.
#[async_trait]
impl AgentExecutor for SynapseExecutor {
    /// Return the hard-coded capability set for the Synapse runtime.
    ///
    /// Real Pistis grant validation happens in `PistisGate`, not here. This
    /// list tells the bridge what capabilities to request before spawning.
    fn required_capabilities(&self) -> Vec<Capability> {
        vec![
            Capability::new(Capability::FS_READ),
            Capability::new(Capability::FS_WRITE),
            Capability::new(Capability::BASH),
            Capability::new(Capability::NETWORK),
        ]
    }

    /// The executor's self-reported sandbox.
    ///
    /// NOTE: `branch` is a hardcoded placeholder (`agent/synapse/unset`), NOT derived from
    /// `base_config`. The real per-task worktree branch is computed by the bridge
    /// (`sandbox::branch_name(agent, task_id)`), and this method is currently unread by the
    /// supervised path. `working_dir` is `base_config.cwd`. Wire a real branch (or remove this
    /// method) when the executor owns sandbox derivation. See scripts/known-incomplete.md row 16.
    fn sandbox(&self) -> ExecutionSandbox {
        ExecutionSandbox {
            branch: "agent/synapse/unset".into(),
            working_dir: self.base_config.cwd.clone(),
            max_runtime_secs: 3600,
        }
    }

    /// Run one discussion turn.
    ///
    /// Formats the conversation context as a single user message, calls
    /// `agent_loop` with no tools (max_turns=1), and returns the text. If the
    /// model responds with exactly `[PASS]`, returns `None`.
    async fn discuss(&self, context: DiscussionContext) -> Result<Option<AgentResponse>> {
        let message = Self::format_discussion_message(&context);
        let (config, tools) = self.discussion_config(&context);

        let stream = agent_loop(config, Arc::clone(&self.provider), tools, message);
        let text = Self::collect_text(stream).await;

        let trimmed = text.trim();
        if trimmed.is_empty() || trimmed == "[PASS]" {
            return Ok(None);
        }

        Ok(Some(AgentResponse {
            text: trimmed.to_string(),
            execution_proposal: None,
        }))
    }

    /// Run a full execution session for an approved task.
    ///
    /// Maps `AgentEvent`s to `ProgressUpdate`s and forwards them on
    /// `progress_tx`. Continues until the stream ends, the channel closes
    /// (bridge abort), or an error event arrives.
    ///
    /// Returns `ExecutionResult::Success` on clean completion,
    /// `ExecutionResult::Failed` on `AgentEvent::Error` or channel closure.
    async fn execute(
        &self,
        task: TaskContext,
        progress_tx: mpsc::Sender<ProgressUpdate>,
    ) -> Result<ExecutionResult> {
        let message = Self::format_execution_message(&task);
        let config = self.execution_config(&task);

        let stream = agent_loop(
            config,
            Arc::clone(&self.provider),
            Arc::clone(&self.tools),
            message,
        );
        futures::pin_mut!(stream);

        let mut text = String::new();
        let mut errored = false;
        let mut error_reason = String::new();

        while let Some(event) = stream.next().await {
            match &event {
                AgentEvent::Text(delta) => {
                    text.push_str(delta);
                }
                AgentEvent::ToolStart { name, .. }
                    if progress_tx
                        .send(ProgressUpdate::ToolStarted {
                            tool_name: name.clone(),
                        })
                        .await
                        .is_err() =>
                {
                    // If the bridge dropped the receiver, abort.
                    return Ok(ExecutionResult::Failed {
                        reason: "bridge disconnected during execution".into(),
                        partial_work: true,
                    });
                }
                AgentEvent::ToolStart { .. } => {}
                AgentEvent::ToolResult { is_error, .. } => {
                    // ToolResult carries no tool name -- emit with placeholder.
                    let _ = progress_tx
                        .send(ProgressUpdate::ToolCompleted {
                            tool_name: "tool".into(),
                            is_error: *is_error,
                        })
                        .await;
                }
                AgentEvent::Error(msg) => {
                    errored = true;
                    error_reason = msg.clone();
                    // Attempt to notify bridge; ignore send failure at this point.
                    let _ = progress_tx.send(ProgressUpdate::Failed(msg.clone())).await;
                    break;
                }
                // TurnStart, TurnEnd, Usage, Cost, ModelSwitch: not forwarded as progress.
                _ => {}
            }
        }

        if errored {
            return Ok(ExecutionResult::Failed {
                reason: error_reason,
                partial_work: !text.is_empty(),
            });
        }

        let _ = progress_tx.send(ProgressUpdate::Done).await;

        Ok(ExecutionResult::Success {
            summary: text.lines().next().unwrap_or("task complete").to_string(),
            commit_hash: None,
            evidence: if text.is_empty() { None } else { Some(text) },
        })
    }

    /// Return `HealthStatus::Ready`.
    ///
    /// Placeholder implementation. A future revision will probe the provider
    /// and verify Kleos reachability before returning `Ready`.
    async fn health_check(&self) -> Result<HealthStatus> {
        Ok(HealthStatus::Ready)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hooks::HookConfig;
    use crate::types::AgentConfig;
    use serde_json::Value;
    use synapse_tools::{GateDecision, PermissiveGate, ToolGate, ToolResult};
    use std::path::PathBuf;

    /// Stub provider that satisfies `Provider + Send + Sync` without network calls.
    struct StubProvider;

    /// Test gate that always denies execution with a fixed reason.
    struct DenyGate {
        /// Denial reason returned from `before_execute`.
        reason: &'static str,
    }

    /// Implements `ToolGate` behavior for `DenyGate`.
    #[async_trait::async_trait]
    impl ToolGate for DenyGate {
        /// Return a fixed denial reason for every tool execution.
        async fn before_execute(
            &self,
            _name: &str,
            _params: &Value,
            _cwd: &std::path::Path,
        ) -> GateDecision {
            GateDecision::Deny(self.reason.to_string())
        }

        /// Leave post-execution behavior as a no-op for tests.
        async fn after_execute(
            &self,
            _name: &str,
            _params: &Value,
            _result: &ToolResult,
            _cwd: &std::path::Path,
        ) {
        }
    }

    /// Implements `synapse_provider::Provider` behavior for `StubProvider`.
    #[async_trait::async_trait]
    impl synapse_provider::Provider for StubProvider {
        /// Returns this component's stable registry name.
        fn name(&self) -> &str {
            "stub"
        }

        /// Handles `send` behavior.
        async fn send(
            &self,
            _request: &synapse_provider::ChatRequest,
        ) -> Result<synapse_provider::ChatResponse> {
            Ok(synapse_provider::ChatResponse {
                id: "stub-id".into(),
                content: vec![synapse_provider::ContentBlock::Text {
                    text: "stub response".into(),
                }],
                stop_reason: synapse_provider::StopReason::EndTurn,
                usage: synapse_provider::Usage {
                    input_tokens: 1,
                    output_tokens: 1,
                    ..Default::default()
                },
            })
        }

        /// Handles `send_streaming` behavior.
        fn send_streaming(
            &self,
            _req: &synapse_provider::ChatRequest,
        ) -> std::pin::Pin<
            Box<dyn futures::Stream<Item = Result<synapse_provider::StreamEvent>> + Send>,
        > {
            use futures::stream;
            // Emit a minimal stream: one text delta then stop.
            Box::pin(stream::iter(vec![
                Ok(synapse_provider::StreamEvent::ContentDelta(
                    "stub response".into(),
                )),
                Ok(synapse_provider::StreamEvent::MessageStop(
                    synapse_provider::StopReason::EndTurn,
                )),
            ]))
        }
    }

    /// Build a minimal `SynapseExecutor` suitable for tests.
    fn make_executor() -> SynapseExecutor {
        let provider: Arc<dyn Provider + Send + Sync> = Arc::new(StubProvider);
        let tools = Arc::new(ToolRegistry::new());
        let hooks = Arc::new(HookConfig::default());
        let config = AgentConfig {
            model: "stub-model".into(),
            system_prompt: "you are a test agent".into(),
            cwd: PathBuf::from("/tmp"),
            max_turns: 4,
            max_tokens: 1024,
            session_store: None,
            session_id: None,
            depth: 0,
            compression: None,
            router: None,
            max_tool_result_tokens: 0,
            tool_gate: None,
            hooks: None,
        };
        SynapseExecutor::new(provider, tools, hooks, None, config)
    }

    /// Build a minimal `TaskContext` for execution-config tests.
    fn make_task(granted_capabilities: Vec<Capability>) -> TaskContext {
        TaskContext {
            task_id: "task-1".into(),
            description: "Run a task".into(),
            sandbox: ExecutionSandbox {
                branch: "agent/synapse/task-1".into(),
                working_dir: PathBuf::from("/tmp/task-1"),
                max_runtime_secs: 60,
            },
            granted_capabilities,
            prior_context: None,
        }
    }

    /// `health_check` returns `Ready` without any network calls.
    #[tokio::test]
    async fn health_check_returns_ready() {
        let executor = make_executor();
        let status = executor.health_check().await.expect("health_check failed");
        assert_eq!(status, HealthStatus::Ready);
    }

    /// `required_capabilities` returns a non-empty list.
    #[test]
    fn required_capabilities_non_empty() {
        let executor = make_executor();
        let caps = executor.required_capabilities();
        assert!(!caps.is_empty());
    }

    /// `execution_config` denies tools that the task did not grant, even with a permissive base gate.
    #[tokio::test]
    async fn execution_config_denies_tools_missing_task_grants() {
        let provider: Arc<dyn Provider + Send + Sync> = Arc::new(StubProvider);
        let tools = Arc::new(ToolRegistry::new());
        let hooks = Arc::new(HookConfig::default());
        let config = AgentConfig {
            model: "stub-model".into(),
            system_prompt: "you are a test agent".into(),
            cwd: PathBuf::from("/tmp"),
            max_turns: 4,
            max_tokens: 1024,
            session_store: None,
            session_id: None,
            depth: 0,
            compression: None,
            router: None,
            max_tool_result_tokens: 0,
            tool_gate: Some(Arc::new(PermissiveGate)),
            hooks: None,
        };
        let executor = SynapseExecutor::new(provider, tools, hooks, None, config);

        let config = executor.execution_config(&make_task(vec![Capability::new(Capability::FS_READ)]));
        let decision = config
            .tool_gate
            .expect("execution config should install a task-local gate")
            .before_execute("bash", &Value::Null, std::path::Path::new("/tmp/task-1"))
            .await;

        assert!(matches!(decision, GateDecision::Deny(_)));
    }

    /// `execution_config` still consults the existing inner gate after grants pass.
    #[tokio::test]
    async fn execution_config_preserves_existing_inner_gate() {
        let provider: Arc<dyn Provider + Send + Sync> = Arc::new(StubProvider);
        let tools = Arc::new(ToolRegistry::new());
        let hooks = Arc::new(HookConfig::default());
        let config = AgentConfig {
            model: "stub-model".into(),
            system_prompt: "you are a test agent".into(),
            cwd: PathBuf::from("/tmp"),
            max_turns: 4,
            max_tokens: 1024,
            session_store: None,
            session_id: None,
            depth: 0,
            compression: None,
            router: None,
            max_tool_result_tokens: 0,
            tool_gate: Some(Arc::new(DenyGate {
                reason: "inner gate deny",
            })),
            hooks: None,
        };
        let executor = SynapseExecutor::new(provider, tools, hooks, None, config);

        let config = executor.execution_config(&make_task(vec![Capability::new(Capability::FS_READ)]));
        let decision = config
            .tool_gate
            .expect("execution config should install a task-local gate")
            .before_execute("read", &Value::Null, std::path::Path::new("/tmp/task-1"))
            .await;

        match decision {
            GateDecision::Deny(message) => assert_eq!(message, "inner gate deny"),
            GateDecision::Allow => panic!("expected the inner gate to deny execution"),
        }
    }
}
