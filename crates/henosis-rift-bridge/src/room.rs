//! Room state machine.
//!
//! Orchestrates the engagement engine, turn manager, loop guard, context
//! assembly, and executor to handle incoming messages and generate agent responses.

use std::collections::HashMap;
use std::sync::Arc;
use uuid::Uuid;

use crate::capability::CapabilityOracle;
use crate::config::{AgentConfig, BridgeDaemonConfig, ExecutorConfig, PersonaSettings, WorkspaceConfig};
use crate::context::build_discussion_context;
use crate::engagement::EngagementEngine;
use crate::growth::GrowthStore;
use crate::persona_alloc::{PersonaAllocator, PersonaAssignment};
use crate::relevance;
use crate::error::BridgeError;
use crate::execution::approval::ApprovalRegistry;
use crate::execution::command::{parse_control_command, ControlCommand};
use crate::execution::coordinator::ProposalCoordinator;
use crate::execution::preflight::{apply_runtime_policy, health_preflight, Preflight};
use crate::execution::sandbox::{resolve_workspace, SandboxManager};
use crate::execution::supervisor::{ExecutionSupervisor, SupervisedTask};
use crate::execution::{PendingProposal, RiftRoomNotifier, RoomNotifier};
use crate::executor::{AgentExecutor, DiscussionContext};
use crate::executors::{build_synapse_executor, ClaudeCodeExecutor};
use crate::kleos::KleosClient;
use crate::loop_prevention::LoopGuard;
use crate::rift_client::RiftRestClient;
use crate::roster::AgentRoster;
use crate::turn_manager::TurnManager;
use crate::types::{AgentId, AgentState, RoomMessage};
use serde_json::json;

/// The room state machine. Coordinates engagement, turns, and loop prevention.
pub struct Room {
    /// Agent roster with runtime state.
    roster: AgentRoster,
    /// Per-agent executor instances.
    executors: HashMap<AgentId, Arc<dyn AgentExecutor>>,
    /// Probabilistic engagement engine.
    engagement: EngagementEngine,
    /// Turn ordering and jitter.
    turn_manager: TurnManager,
    /// Turn budgets and thread ceiling.
    loop_guard: LoopGuard,
    /// Shared Rift REST client for posting messages.
    rift: Arc<RiftRestClient>,
    /// Shared Kleos client for memory, task, and activity coordination.
    kleos: Arc<dyn KleosClient>,
    /// Daemon configuration.
    config: BridgeDaemonConfig,
    /// Project name used for Kleos scoping.
    project_name: String,
    /// Target channel for agent posts.
    channel_id: Uuid,
    /// Channel name (for context assembly prompts).
    channel_name: String,
    /// Shared approval registry for execution proposals.
    approval_registry: ApprovalRegistry,
    /// Routes proposals through capability checking and approval registration.
    coordinator: Arc<ProposalCoordinator>,
    /// Supervises approved execution sessions.
    supervisor: Arc<ExecutionSupervisor>,
    /// Creates per-task git worktrees.
    sandbox_manager: Arc<SandboxManager>,
    /// Declared workspaces for execution.
    workspaces: Arc<Vec<WorkspaceConfig>>,
    /// Bounds simultaneous execution sessions.
    exec_semaphore: Arc<tokio::sync::Semaphore>,
    /// Executors keyed by agent username (for execution dispatch).
    executors_by_username: HashMap<String, Arc<dyn AgentExecutor>>,
    /// Thread-stable persona assignment per agent (empty when personas disabled).
    personas: HashMap<AgentId, PersonaAssignment>,
    /// Per-agent growth file store (None when personas disabled).
    growth: Option<GrowthStore>,
}

/// Implements room lifecycle, inbound message handling, and agent response posting.
impl Room {
    /// Create a new room from agent configs and daemon settings.
    /// Provisions all agent users in Rift during initialization.
    #[allow(clippy::too_many_arguments)]
    pub async fn new(
        agent_configs: &[AgentConfig],
        daemon_config: BridgeDaemonConfig,
        rift: Arc<RiftRestClient>,
        kleos: Arc<dyn KleosClient>,
        project_name: String,
        channel_id: Uuid,
        oracle: Arc<dyn CapabilityOracle>,
        approval_registry: ApprovalRegistry,
        workspaces: Vec<WorkspaceConfig>,
        sandbox_manager: Arc<SandboxManager>,
        max_concurrent: usize,
        personas_config: Option<PersonaSettings>,
    ) -> Result<Self, BridgeError> {
        let roster = AgentRoster::provision(agent_configs, &rift).await?;

        // Build executor map from agent configs.
        let mut executors: HashMap<AgentId, Arc<dyn AgentExecutor>> = HashMap::new();
        for (agent, config) in roster.all().zip(agent_configs.iter()) {
            let executor: Arc<dyn AgentExecutor> = match &config.executor {
                ExecutorConfig::ClaudeCode {
                    binary,
                    model,
                    max_tokens,
                } => Arc::new(ClaudeCodeExecutor::new(
                    binary.clone(),
                    model.clone(),
                    *max_tokens,
                )),
                ExecutorConfig::Synapse {
                    provider,
                    model,
                    host,
                    token,
                    api_key,
                    max_tokens,
                    max_turns,
                    cwd,
                } => {
                    let synapse = build_synapse_executor(
                        provider,
                        model.clone(),
                        host.clone(),
                        token.clone(),
                        api_key.clone(),
                        *max_tokens,
                        *max_turns,
                        cwd.clone(),
                    )
                    .map_err(|e| {
                        BridgeError::Config(format!(
                            "failed to build SynapseExecutor for {}: {e}",
                            config.name,
                        ))
                    })?;
                    Arc::new(synapse)
                }
            };
            executors.insert(agent.id, executor);
        }

        // Register all agents with the loop guard for consensus tracking.
        let mut loop_guard =
            LoopGuard::new(daemon_config.turn_budget, daemon_config.thread_ceiling);
        for agent in roster.all() {
            loop_guard.register_agent(agent.id);
        }

        // Build the room notifier from the first provisioned agent identity.
        let first = roster
            .all()
            .next()
            .ok_or_else(|| BridgeError::Config("no agents configured".into()))?;
        let notifier: Arc<dyn RoomNotifier> = Arc::new(RiftRoomNotifier::new(
            rift.clone(),
            first.rift_user_id,
            first.username.clone(),
            channel_id,
        ));

        // Map executors by username for execution dispatch.
        let mut executors_by_username: HashMap<String, Arc<dyn AgentExecutor>> = HashMap::new();
        for agent in roster.all() {
            if let Some(exec) = executors.get(&agent.id) {
                executors_by_username.insert(agent.username.clone(), exec.clone());
            }
        }

        let coordinator = Arc::new(ProposalCoordinator::new(
            oracle,
            kleos.clone(),
            notifier.clone(),
            approval_registry.clone(),
            project_name.clone(),
            workspaces.clone(),
        ));
        let supervisor = Arc::new(ExecutionSupervisor::new(notifier.clone()));
        let exec_semaphore = Arc::new(tokio::sync::Semaphore::new(max_concurrent.max(1)));

        // Allocate thread-stable personas across the roster, if configured. The
        // channel id seeds the (stable) thread identity. Allocation failure
        // degrades to no personas rather than failing room startup.
        let (personas, growth) = match personas_config {
            Some(pc) => {
                let allocator = PersonaAllocator::new(
                    pc.library_path.clone(),
                    pc.max_same_persona,
                    pc.challenger_slot,
                );
                let agent_ids: Vec<AgentId> = roster.all().map(|a| a.id).collect();
                let map = match allocator.allocate(&channel_id.to_string(), &agent_ids, None) {
                    Ok(assignments) => assignments
                        .into_iter()
                        .map(|a| (a.agent_id, a))
                        .collect::<HashMap<_, _>>(),
                    Err(e) => {
                        tracing::warn!("persona allocation failed, continuing without personas: {e}");
                        HashMap::new()
                    }
                };
                (map, Some(GrowthStore::new(pc.growth_root.clone())))
            }
            None => (HashMap::new(), None),
        };

        Ok(Self {
            roster,
            executors,
            engagement: EngagementEngine::default(),
            turn_manager: TurnManager::new(
                daemon_config.jitter_range_ms.0,
                daemon_config.jitter_range_ms.1,
            ),
            loop_guard,
            rift,
            kleos,
            config: daemon_config,
            project_name,
            channel_id,
            channel_name: "general".to_string(),
            approval_registry,
            coordinator,
            supervisor,
            sandbox_manager,
            workspaces: Arc::new(workspaces),
            exec_semaphore,
            executors_by_username,
            personas,
            growth,
        })
    }

    /// Get a reference to the roster (for startup wiring).
    pub fn roster_ref(&self) -> &AgentRoster {
        &self.roster
    }

    /// Handle an incoming message from the room.
    /// Evaluates engagement for each idle agent, then generates and posts responses.
    pub async fn handle_message(&mut self, msg: RoomMessage) -> Result<(), BridgeError> {
        // Ignore messages from our own agents (prevent self-response loops).
        if self.roster.by_rift_user_id(msg.author_id).is_some() {
            return Ok(());
        }

        // Handle approval control commands from humans.
        if let Some(cmd) = parse_control_command(&msg.content) {
            match cmd {
                ControlCommand::Approve(id) => {
                    if let Some(proposal) = self.approval_registry.approve(id) {
                        tracing::info!("approval {} accepted by human", id);
                        self.execute_approved(proposal).await;
                    } else {
                        tracing::info!("approval {} not found", id);
                    }
                }
                ControlCommand::Reject(id) => {
                    if self.approval_registry.reject(id) {
                        tracing::info!("approval {} rejected by human", id);
                    }
                }
            }
            return Ok(());
        }

        // Human posted -- reset interleaving.
        self.turn_manager.record_human_post();

        if let Err(e) = self
            .kleos
            .report_activity(
                &self.project_name,
                "rift-bridge",
                "task.progress",
                "Human message triggered room evaluation",
                json!({
                    "channel": self.channel_name,
                    "author": msg.author_username.clone(),
                }),
            )
            .await
        {
            tracing::warn!("kleos human activity report failed: {e}");
        }

        // Check thread ceiling with a nil agent ID (just checks total_turns).
        if !self.loop_guard.can_contribute(AgentId(Uuid::nil())) {
            tracing::info!("thread ceiling reached, ignoring message");
            return Ok(());
        }

        // Evaluate each idle agent for engagement.
        let idle_agents = self.roster.idle_agents();
        let mut responders = Vec::new();

        for agent_id in idle_agents {
            let agent = match self.roster.get_mut(&agent_id) {
                Some(a) => a,
                None => continue,
            };

            if !self.loop_guard.can_contribute(agent_id) {
                continue;
            }

            if !self.turn_manager.can_post_next(agent_id) {
                continue;
            }

            // Check if the message directly addresses this agent.
            let directly_addressed = msg.content.contains(&agent.display_name)
                || msg.content.contains(&format!("@{}", agent.username));

            // Persona relevance scales engagement for non-addressed agents. A
            // direct address uses neutral relevance so it is never suppressed.
            let relevance = if directly_addressed {
                1.0
            } else {
                self.personas
                    .get(&agent_id)
                    .map(|p| relevance::score(&msg.content, &p.interests))
                    .unwrap_or(1.0)
            };

            let probability = self.engagement.compute_probability(
                agent.base_chance,
                directly_addressed,
                agent.turns_in_topic,
                relevance,
            );

            if self.engagement.should_skip(probability) {
                continue;
            }

            if self.engagement.roll(probability) {
                responders.push(agent_id);
            }
        }

        // Process responders sequentially with jitter delays.
        for agent_id in responders {
            let delay = self.turn_manager.jitter_delay();
            tokio::time::sleep(delay).await;

            if let Err(e) = self.generate_and_post(agent_id).await {
                tracing::error!("agent {:?} failed to respond: {e}", agent_id);
            }
        }

        Ok(())
    }

    /// Generate a response from an agent and post it to the room.
    async fn generate_and_post(&mut self, agent_id: AgentId) -> Result<(), BridgeError> {
        // Mark agent as thinking.
        let agent = self
            .roster
            .get_mut(&agent_id)
            .ok_or_else(|| BridgeError::Executor("agent not found".into()))?;
        agent.state = AgentState::Thinking;

        // Build context for the executor.
        let context = self.build_context(agent_id).await?;

        // Get the executor and generate a response.
        let executor = self
            .executors
            .get(&agent_id)
            .ok_or_else(|| BridgeError::Executor("no executor for agent".into()))?
            .clone();

        let response = executor
            .discuss(context)
            .await
            .map_err(|e| BridgeError::Executor(format!("discuss failed: {e}")))?;

        // Process the response.
        let agent = self
            .roster
            .get_mut(&agent_id)
            .ok_or_else(|| BridgeError::Executor("agent not found".into()))?;

        match response {
            Some(agent_resp) if self.loop_guard.is_pass(&agent_resp.text) => {
                tracing::info!("{} passed", agent.display_name);
                agent.state = AgentState::Idle;
            }
            Some(agent_resp) => {
                // Check for agreement signal.
                if self.loop_guard.contains_agreement(&agent_resp.text) {
                    self.loop_guard.record_agreement(agent_id);
                }

                // Post to room.
                self.rift
                    .send_message(
                        agent.rift_user_id,
                        &agent.username,
                        self.channel_id,
                        &agent_resp.text,
                    )
                    .await?;

                // Update tracking state.
                self.loop_guard.record_contribution(agent_id);
                self.turn_manager.record_post(agent_id);
                agent.turns_in_topic += 1;
                agent.state = AgentState::Idle;

                tracing::info!(
                    "{} posted ({} chars)",
                    agent.display_name,
                    agent_resp.text.len()
                );

                if let Err(e) = self
                    .kleos
                    .report_activity(
                        &self.project_name,
                        &agent.username,
                        "task.progress",
                        "Agent posted to the room",
                        json!({
                            "channel": self.channel_name,
                            "chars": agent_resp.text.len(),
                        }),
                    )
                    .await
                {
                    tracing::warn!("kleos agent activity report failed: {e}");
                }

                // Route an execution proposal through the coordinator.
                if let Some(proposal) = &agent_resp.execution_proposal {
                    tracing::info!(
                        "{} proposed execution: {}",
                        agent.display_name,
                        proposal.scope_summary
                    );
                    let username = agent.username.clone();
                    let proposal = proposal.clone();
                    if let Err(e) = self.coordinator.handle_proposal(&username, &proposal).await {
                        tracing::warn!("proposal handling failed: {e}");
                    }
                }

                // Check for consensus.
                if self.loop_guard.has_consensus() {
                    let consensus_tags = vec![
                        format!("project:{}", self.project_name),
                        format!("channel:{}", self.channel_name),
                        "kind:consensus".to_string(),
                    ];
                    let consensus_text =
                        format!("Consensus in #{}: {}", self.channel_name, agent_resp.text);

                    if let Err(e) = self
                        .kleos
                        .store_consensus_memory(
                            &self.project_name,
                            &consensus_text,
                            &consensus_tags,
                        )
                        .await
                    {
                        tracing::warn!("kleos consensus memory store failed: {e}");
                    }

                    if should_create_draft_task(&agent_resp.text) {
                        if let Err(e) = self
                            .kleos
                            .create_draft_task(
                                &self.project_name,
                                &agent.username,
                                &draft_task_title(&self.channel_name, &agent_resp.text),
                                &agent_resp.text,
                            )
                            .await
                        {
                            tracing::warn!("kleos draft task creation failed: {e}");
                        }
                    }

                    tracing::info!("consensus reached -- notifying room");
                    if let Some(a) = self.roster.all().next() {
                        let _ = self.rift.send_message(
                            a.rift_user_id,
                            &a.username,
                            self.channel_id,
                            "[SYSTEM] All agents have reached consensus on this topic. Human review recommended.",
                        ).await;
                    }
                }
            }
            None => {
                tracing::info!("{} returned empty response", agent.display_name);
                agent.state = AgentState::Idle;
            }
        }

        Ok(())
    }

    /// Build discussion context for an agent from recent channel messages.
    async fn build_context(&self, agent_id: AgentId) -> Result<DiscussionContext, BridgeError> {
        let agent = self
            .roster
            .all()
            .find(|a| a.id == agent_id)
            .ok_or_else(|| BridgeError::Executor("agent not found".into()))?;

        // Fetch recent messages from Rift.
        let messages = self
            .rift
            .list_messages(
                agent.rift_user_id,
                &agent.username,
                self.channel_id,
                self.config.context_window as u32,
            )
            .await?;

        // Convert to (author, content) string pairs, oldest first.
        let recent_owned: Vec<(String, String)> = messages
            .into_iter()
            .rev()
            .map(|m| {
                let author = m.author_username.unwrap_or_else(|| "unknown".to_string());
                (author, m.content)
            })
            .collect();

        let recent_refs: Vec<(&str, &str)> = recent_owned
            .iter()
            .map(|(a, t)| (a.as_str(), t.as_str()))
            .collect();

        let relevant_memories = match self
            .kleos
            .search_memories(&self.project_name, &self.channel_name, &recent_owned, 5)
            .await
        {
            Ok(memories) => memories,
            Err(e) => {
                tracing::warn!("kleos memory search failed: {e}");
                Vec::new()
            }
        };

        let active_tasks_summary =
            match self.kleos.active_tasks_summary(&self.project_name, 5).await {
                Ok(summary) => summary,
                Err(e) => {
                    tracing::warn!("kleos task summary failed: {e}");
                    None
                }
            };

        let team_members: Vec<&str> = self.roster.all().map(|a| a.display_name.as_str()).collect();

        // Persona name and growth notes for this agent, when personas are enabled.
        let persona_name = self
            .personas
            .get(&agent_id)
            .map(|p| p.persona_name.clone());
        let growth_notes = self
            .growth
            .as_ref()
            .and_then(|store| store.load(&agent_id).ok())
            .filter(|notes| !notes.trim().is_empty());

        Ok(build_discussion_context(
            &agent.system_prompt,
            &agent.display_name,
            &self.channel_name,
            recent_refs,
            team_members,
            relevant_memories,
            active_tasks_summary,
            persona_name,
            growth_notes,
        ))
    }

    /// Sweep expired approvals and notify the room for each.
    pub async fn sweep_expired_approvals(&self) {
        for expired in self.approval_registry.sweep_expired() {
            tracing::info!("approval {} expired", expired.id);
            let _ = self
                .kleos
                .update_task_status(&expired.task_id, "blocked", "approval expired")
                .await;
        }
    }

    /// Dispatch an approved proposal: create the sandbox, then spawn a
    /// supervised execution session bounded by the concurrency semaphore.
    pub async fn execute_approved(&self, proposal: PendingProposal) {
        let executor = match self.executors_by_username.get(&proposal.agent) {
            Some(e) => e.clone(),
            None => {
                tracing::error!("no executor for proposing agent {}", proposal.agent);
                return;
            }
        };

        // Health preflight before creating any worktree: the executor's runtime
        // must be ready, or the task is blocked rather than spawned into a dead
        // runtime (AgentExecutor contract: health_check is called before spawn).
        let health = match executor.health_check().await {
            Ok(status) => status,
            Err(e) => {
                let _ = self
                    .kleos
                    .update_task_status(
                        &proposal.task_id,
                        "blocked",
                        &format!("health check failed: {e}"),
                    )
                    .await;
                return;
            }
        };
        match health_preflight(health) {
            Preflight::Proceed => {}
            Preflight::ProceedDegraded(reason) => {
                tracing::warn!(
                    "executor for {} is degraded but proceeding: {reason}",
                    proposal.agent
                );
            }
            Preflight::Block(reason) => {
                tracing::error!(
                    "executor for {} unavailable, blocking task {}: {reason}",
                    proposal.agent,
                    proposal.task_id
                );
                let _ = self
                    .kleos
                    .update_task_status(
                        &proposal.task_id,
                        "blocked",
                        &format!("executor unavailable: {reason}"),
                    )
                    .await;
                return;
            }
        }

        let workspace = match resolve_workspace(&self.workspaces, &proposal.workspace) {
            Some(w) => w,
            None => {
                tracing::error!(
                    "workspace {} not found for approved task",
                    proposal.workspace
                );
                return;
            }
        };

        // Create the branch-isolated sandbox before spawning.
        let sandbox = match self
            .sandbox_manager
            .create(workspace, &proposal.agent, &proposal.task_id)
            .await
        {
            Ok(s) => s,
            Err(e) => {
                tracing::error!("sandbox creation failed: {e}");
                let _ = self
                    .kleos
                    .update_task_status(
                        &proposal.task_id,
                        "blocked",
                        &format!("sandbox failed: {e}"),
                    )
                    .await;
                return;
            }
        };

        // Clamp the worktree's wall-clock limit to the executor's declared
        // runtime policy: the bridge owns branch/path, the executor owns its
        // ceiling (never above the operator-configured limit).
        let sandbox = apply_runtime_policy(sandbox, &executor.sandbox());

        let supervisor = self.supervisor.clone();
        let semaphore = self.exec_semaphore.clone();
        let kleos = self.kleos.clone();
        let project = self.project_name.clone();
        let task = SupervisedTask {
            executor,
            task_id: proposal.task_id.clone(),
            description: proposal.scope_summary.clone(),
            sandbox,
            granted_capabilities: proposal.granted_capabilities.clone(),
            // First attempt against a fresh worktree: no prior work to resume. The
            // crash-recovery path will source a partial-work summary here.
            prior_context: None,
        };

        // Spawn so the event loop is not blocked by a long execution.
        tokio::spawn(async move {
            let _permit = match semaphore.acquire_owned().await {
                Ok(p) => p,
                Err(_) => return,
            };
            let result = supervisor.run(task).await;
            let (status, note) = match &result {
                crate::executor::ExecutionResult::Success { summary, .. } => {
                    ("completed", summary.clone())
                }
                crate::executor::ExecutionResult::Failed { reason, .. } => {
                    ("blocked", reason.clone())
                }
            };

            // Durable execution-result memory (best-effort).
            let tags = vec![
                format!("project:{project}"),
                "kind:execution-result".to_string(),
            ];
            let memory = format!(
                "Execution result for task {} ({}): {}",
                proposal.task_id, status, note
            );
            if let Err(e) = kleos.store_consensus_memory(&project, &memory, &tags).await {
                tracing::warn!("kleos execution-result memory store failed: {e}");
            }

            if let Err(e) = kleos
                .update_task_status(&proposal.task_id, status, &note)
                .await
            {
                tracing::warn!("kleos task status update failed: {e}");
            }
        });
    }
}

/// Decide whether consensus text is actionable enough to draft a Chiasm task.
fn should_create_draft_task(consensus_text: &str) -> bool {
    let lower = consensus_text.to_ascii_lowercase();
    [
        "implement",
        "fix",
        "investigate",
        "refactor",
        "test",
        "verify",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

/// Build a compact channel-scoped Chiasm draft task title from consensus text.
fn draft_task_title(channel_name: &str, consensus_text: &str) -> String {
    let trimmed = consensus_text.trim().trim_start_matches("[AGREE]").trim();
    let compact = trimmed.chars().take(72).collect::<String>();
    format!("[{}] {}", channel_name, compact)
}

#[cfg(test)]
/// Tests for consensus helper behavior.
mod tests {
    use super::{draft_task_title, should_create_draft_task};

    /// Verifies concrete implementation language creates a draft task.
    #[test]
    fn test_should_create_draft_task_accepts_actionable_consensus() {
        assert!(should_create_draft_task(
            "We should implement Kleos-backed context assembly and verify it with tests."
        ));
    }

    /// Verifies vague agreement does not create junk tasks.
    #[test]
    fn test_should_create_draft_task_rejects_vague_consensus() {
        assert!(!should_create_draft_task(
            "I agree with the overall direction and think this makes sense."
        ));
    }

    /// Verifies draft task titles stay compact and channel-scoped.
    #[test]
    fn test_draft_task_title_uses_channel_prefix() {
        let title = draft_task_title("general", "Implement Kleos-backed context assembly next.");
        assert!(title.starts_with("[general] "));
        assert!(title.contains("Implement Kleos-backed context assembly"));
    }
}
