//! Room state machine.
//!
//! Orchestrates the engagement engine, turn manager, loop guard, echo
//! suppression, context assembly, and executor to handle incoming messages
//! and generate agent responses.
//!
//! An inbound human (or stimulus) message seeds a bounded conversation
//! cascade: engaged agents reply in their compose slots, and each agent post
//! becomes the trigger for the next evaluation round, so agents answer each
//! other (2026-07-17 design spec; parent Rift Team Room success criterion 1).
//! The cascade is bounded by per-agent turn budgets, the thread ceiling, a
//! round cap, per-agent cooldown, engagement decay, and consensus. Topic
//! energy (budgets, agreements) resets on each inbound human message.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use uuid::Uuid;

use crate::capability::CapabilityOracle;
use crate::config::{AgentConfig, BridgeDaemonConfig, ExecutorConfig, PersonaSettings, WorkspaceConfig};
use crate::context::build_discussion_context;
use crate::echo;
use crate::engagement::{EngagementEngine, EngagementInputs};
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
    /// Monotonic count of messages posted in the room (human and agent).
    /// Feeds true turns-since-last-post recency (fixes finding N1).
    room_turn: u64,
    /// Ring buffer of recent AGENT posts, compared against candidate
    /// responses for cross-agent echo suppression (spec P3). Human/stimulus
    /// messages are deliberately absent: comparing a reply against the
    /// question it answers suppresses legitimate confirming answers
    /// (adversarial review finding, 2026-07-17).
    recent_posts: VecDeque<(AgentId, String)>,
}

/// Capacity of the recent-post ring buffer used for echo comparison.
const RECENT_POSTS_CAP: usize = 12;

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
            engagement: EngagementEngine {
                peer_response_damp: daemon_config.peer_response_damp,
                ..EngagementEngine::default()
            },
            turn_manager: TurnManager::new(
                daemon_config.slot_width_ms,
                daemon_config.slot_jitter_ms,
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
            room_turn: 0,
            recent_posts: VecDeque::new(),
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

        // Human (or external stimulus) posted: reset interleaving and restore
        // topic energy. Without this reset the loop guard's counters
        // accumulated for the process lifetime and the room died permanently
        // at the thread ceiling (finding N2).
        self.turn_manager.record_human_post();
        self.loop_guard.reset();

        // The inbound message is a room turn for recency purposes. It is NOT
        // remembered for echo comparison: echo suppression is scoped to
        // cross-agent repetition (spec P3/F4).
        self.room_turn += 1;

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

        // Drive the bounded conversation cascade. The inbound message seeds
        // round 0; each subsequent round is triggered by the last agent post
        // of the previous round, so agents can answer each other (finding N3:
        // previously roster posts never triggered peer evaluation and
        // agent-to-agent conversation was structurally impossible). Bounds:
        // per-agent turn budgets, the thread ceiling, the round cap,
        // engagement decay, per-agent cooldown, and consensus.
        let mut trigger_content = msg.content.clone();
        let mut trigger_author: Option<AgentId> = None;
        let mut rounds = 0u32;

        while rounds < self.config.max_cascade_rounds {
            // Honor a bridge pause at round boundaries: handle_message blocks
            // the main event loop's pause polling, so a multi-round cascade
            // must check for itself (adversarial review finding). Approvals
            // queue in their channel and dispatch after the cascade returns.
            if rounds > 0 {
                let paused = self.rift.is_paused().await.unwrap_or_else(|e| {
                    tracing::warn!("pause check failed mid-cascade: {e}");
                    false
                });
                if paused {
                    tracing::info!("bridge paused, aborting cascade");
                    break;
                }
            }

            let plan = self.plan_round(&trigger_content, trigger_author);
            if plan.is_empty() {
                break;
            }

            let round_start = tokio::time::Instant::now();
            let mut peers_posted = 0u32;
            let mut last_post: Option<(AgentId, String)> = None;

            for (agent_id, slot_index) in plan {
                // Pace into this agent's own compose window (spec P1): the
                // window is derived from the agent's stable slot index, with
                // jitter inside the window, never shared across agents.
                let target = round_start + self.turn_manager.slot_delay(slot_index);
                tokio::time::sleep_until(target).await;

                // Peers may have answered while this agent waited for its
                // slot: re-evaluate at the damped probability (spec P2, the
                // fourth-voice rule) and re-check turn gates.
                if peers_posted > 0
                    && !self.reroll_engagement(agent_id, &trigger_content, peers_posted)
                {
                    continue;
                }
                if !self.turn_manager.can_post_next(agent_id)
                    || !self.loop_guard.can_contribute(agent_id)
                {
                    continue;
                }

                match self.generate_and_post(agent_id).await {
                    Ok(Some(text)) => {
                        peers_posted += 1;
                        last_post = Some((agent_id, text));
                    }
                    Ok(None) => {}
                    Err(e) => {
                        tracing::error!("agent {:?} failed to respond: {e}", agent_id);
                    }
                }

                // Consensus ends the topic: no further replies or rounds.
                if self.loop_guard.has_consensus() {
                    return Ok(());
                }
            }

            // The most recent post becomes the next round's trigger; a round
            // in which nobody posted ends the cascade.
            match last_post {
                Some((author, text)) => {
                    trigger_author = Some(author);
                    trigger_content = text;
                    rounds += 1;
                }
                None => break,
            }
        }

        Ok(())
    }

    /// Plan one cascade round: evaluate every idle agent except the trigger's
    /// author for engagement against the trigger message, and return the
    /// engaged agents ordered by compose slot. Peer parity invariant: the
    /// evaluation is identical whether the trigger came from a human or an
    /// agent (no author-kind input exists).
    fn plan_round(
        &self,
        trigger_content: &str,
        trigger_author: Option<AgentId>,
    ) -> Vec<(AgentId, usize)> {
        let mut plan = Vec::new();

        for agent_id in self.roster.idle_agents() {
            if Some(agent_id) == trigger_author {
                continue;
            }
            if !self.loop_guard.can_contribute(agent_id) {
                continue;
            }
            if !self.turn_manager.can_post_next(agent_id) {
                continue;
            }

            let agent = match self.roster.all().find(|a| a.id == agent_id) {
                Some(a) => a,
                None => continue,
            };

            // Hard cooldown floor between two posts by the same agent.
            if cooldown_active(agent.last_post_at, self.config.cooldown_secs) {
                continue;
            }
            let slot_index = agent.slot_index;

            let probability = self.probability_for(agent_id, trigger_content, 0);
            if self.engagement.should_skip(probability) {
                continue;
            }
            if self.engagement.roll(probability) {
                plan.push((agent_id, slot_index));
            }
        }

        // Slot order is the speaking order: deterministic, non-overlapping.
        plan.sort_by_key(|(_, slot_index)| *slot_index);
        plan
    }

    /// Compute the engagement probability for one agent against one message,
    /// assembling direct-address, true recency (room turns since this agent
    /// last posted -- fixes finding N1), peer-coverage, and relevance inputs.
    fn probability_for(&self, agent_id: AgentId, content: &str, peer_responses: u32) -> f64 {
        let agent = match self.roster.all().find(|a| a.id == agent_id) {
            Some(a) => a,
            None => return 0.0,
        };

        // Check if the message directly addresses this agent.
        let directly_addressed = content.contains(&agent.display_name)
            || content.contains(&format!("@{}", agent.username));

        // Persona relevance scales engagement for non-addressed agents. A
        // direct address uses neutral relevance so it is never suppressed.
        let relevance = if directly_addressed {
            1.0
        } else {
            self.personas
                .get(&agent_id)
                .map(|p| relevance::score(content, &p.interests))
                .unwrap_or(1.0)
        };

        let turns_since_last_post = agent
            .last_posted_turn
            .map(|turn| u32::try_from(self.room_turn.saturating_sub(turn)).unwrap_or(u32::MAX));

        self.engagement.compute_probability(EngagementInputs {
            base_chance: agent.base_chance,
            directly_addressed,
            turns_since_last_post,
            peer_responses,
            relevance,
        })
    }

    /// Re-evaluate an already-planned agent after peers posted in the same
    /// round: apply the skip threshold, then a fresh roll at the damped
    /// probability. Returns true when the agent should still respond.
    fn reroll_engagement(&self, agent_id: AgentId, content: &str, peer_responses: u32) -> bool {
        let probability = self.probability_for(agent_id, content, peer_responses);
        if self.engagement.should_skip(probability) {
            return false;
        }
        self.engagement.roll(probability)
    }

    /// Record an agent post in the ring buffer used for cross-agent echo
    /// comparison.
    fn remember_post(&mut self, author: AgentId, text: &str) {
        self.recent_posts.push_back((author, text.to_string()));
        while self.recent_posts.len() > RECENT_POSTS_CAP {
            self.recent_posts.pop_front();
        }
    }

    /// Return an agent to Idle. Called on every compose exit path so a failed
    /// context build, generation, or send cannot wedge the agent in Thinking
    /// forever (finding N4).
    fn set_agent_idle(&mut self, agent_id: AgentId) {
        if let Some(agent) = self.roster.get_mut(&agent_id) {
            agent.state = AgentState::Idle;
        }
    }

    /// Generate a response from an agent and post it to the room. Holds the
    /// compose floor for the whole sequence (context build, generation,
    /// post) so no other agent composes against the same room state (spec P1
    /// invariant). Returns the posted text, or None when the agent passed,
    /// returned nothing, or was suppressed as a cross-agent echo.
    async fn generate_and_post(
        &mut self,
        agent_id: AgentId,
    ) -> Result<Option<String>, BridgeError> {
        // Hold the compose floor until this attempt ends (permit drops on
        // every return path).
        let _floor = self.turn_manager.acquire_floor().await;

        // Mark agent as thinking; copy identity fields for use after the
        // mutable roster borrow ends.
        let agent = self
            .roster
            .get_mut(&agent_id)
            .ok_or_else(|| BridgeError::Executor("agent not found".into()))?;
        agent.state = AgentState::Thinking;
        let display_name = agent.display_name.clone();
        let username = agent.username.clone();
        let rift_user_id = agent.rift_user_id;

        // Build context for the executor. Every failure exit must return the
        // agent to Idle: the old code used `?` here and a single failed
        // context build or generation wedged the agent in Thinking forever
        // (finding N4).
        let context = match self.build_context(agent_id).await {
            Ok(context) => context,
            Err(e) => {
                self.set_agent_idle(agent_id);
                return Err(e);
            }
        };

        // Get the executor and generate a response.
        let executor = match self.executors.get(&agent_id) {
            Some(executor) => executor.clone(),
            None => {
                self.set_agent_idle(agent_id);
                return Err(BridgeError::Executor("no executor for agent".into()));
            }
        };

        let response = match executor.discuss(context).await {
            Ok(response) => response,
            Err(e) => {
                self.set_agent_idle(agent_id);
                return Err(BridgeError::Executor(format!("discuss failed: {e}")));
            }
        };

        // Process the response.
        let agent_resp = match response {
            Some(agent_resp) => agent_resp,
            None => {
                tracing::info!("{display_name} returned empty response");
                self.set_agent_idle(agent_id);
                return Ok(None);
            }
        };

        // A pass declines the turn (line-anchored marker, spec P5).
        if self.loop_guard.is_pass(&agent_resp.text) {
            tracing::info!("{display_name} passed");
            self.set_agent_idle(agent_id);
            return Ok(None);
        }

        // Check for agreement signal (line-anchored marker, spec P5).
        let is_agreement = self.loop_guard.contains_agreement(&agent_resp.text);

        // Cross-agent echo suppression (spec P3): drop candidates that
        // substantially reproduce a recent PEER AGENT post. The buffer holds
        // only agent posts, so a reply restating the human question it
        // answers is never suppressed. Consensus votes are exempt because
        // [AGREE] messages are legitimately similar to each other. This check
        // deliberately applies even to directly addressed agents: a verbatim
        // repeat of a peer is an echo no matter who was named.
        if !is_agreement {
            let peer_texts: Vec<&str> = self
                .recent_posts
                .iter()
                .filter(|(author, _)| *author != agent_id)
                .map(|(_, text)| text.as_str())
                .collect();
            if echo::is_echo(
                &agent_resp.text,
                &peer_texts,
                self.config.echo_similarity_threshold,
            ) {
                tracing::info!("{display_name} response suppressed as cross-agent echo");
                self.set_agent_idle(agent_id);
                return Ok(None);
            }
        }

        // Post to room.
        if let Err(e) = self
            .rift
            .send_message(rift_user_id, &username, self.channel_id, &agent_resp.text)
            .await
        {
            self.set_agent_idle(agent_id);
            return Err(e);
        }

        // Record the agreement only after the message actually reached the
        // room: recording before a failed send would poison consensus with a
        // vote nobody saw (adversarial review finding).
        if is_agreement {
            self.loop_guard.record_agreement(agent_id);
        }

        // Update tracking state: the post is a room turn and this agent's
        // new recency anchor (the old code instead incremented a posts-made
        // counter that fed the recency decay inverted -- finding N1).
        self.loop_guard.record_contribution(agent_id);
        self.turn_manager.record_post(agent_id);
        self.room_turn += 1;
        let room_turn = self.room_turn;
        if let Some(agent) = self.roster.get_mut(&agent_id) {
            agent.last_posted_turn = Some(room_turn);
            agent.last_post_at = Some(std::time::Instant::now());
            agent.state = AgentState::Idle;
        }
        self.remember_post(agent_id, &agent_resp.text);

        tracing::info!("{display_name} posted ({} chars)", agent_resp.text.len());

        if let Err(e) = self
            .kleos
            .report_activity(
                &self.project_name,
                &username,
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
                "{display_name} proposed execution: {}",
                proposal.scope_summary
            );
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
                        &username,
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

        Ok(Some(agent_resp.text))
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

/// True while an agent's per-agent pacing floor is active: the agent posted
/// less than `cooldown_secs` ago. Never-posted agents are exempt, and the
/// boundary (elapsed == cooldown) counts as expired so cooldown_secs = 0
/// never blocks.
fn cooldown_active(last_post_at: Option<std::time::Instant>, cooldown_secs: u64) -> bool {
    last_post_at.is_some_and(|at| at.elapsed().as_secs() < cooldown_secs)
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
/// Tests for consensus helpers, cooldown pacing, and compose failure paths.
mod tests {
    use super::*;
    use super::{cooldown_active, draft_task_title, should_create_draft_task};
    use crate::auth::AgentAuthManager;
    use crate::capability::StaticAllowlistOracle;
    use crate::execution::supervisor::ExecutionSupervisor;
    use crate::roster::RegisteredAgent;
    use async_trait::async_trait;

    /// KleosClient stub whose operations all succeed without any network.
    struct NullKleos;

    /// Implements every KleosClient operation as a successful no-op.
    #[async_trait]
    impl KleosClient for NullKleos {
        /// Returns no memories.
        async fn search_memories(
            &self,
            _project: &str,
            _channel: &str,
            _recent: &[(String, String)],
            _limit: usize,
        ) -> Result<Vec<String>, BridgeError> {
            Ok(Vec::new())
        }

        /// Returns no task summary.
        async fn active_tasks_summary(
            &self,
            _project: &str,
            _limit: usize,
        ) -> Result<Option<String>, BridgeError> {
            Ok(None)
        }

        /// Accepts any activity report.
        async fn report_activity(
            &self,
            _project: &str,
            _agent: &str,
            _action: &str,
            _summary: &str,
            _metadata: serde_json::Value,
        ) -> Result<(), BridgeError> {
            Ok(())
        }

        /// Accepts any consensus memory.
        async fn store_consensus_memory(
            &self,
            _project: &str,
            _content: &str,
            _tags: &[String],
        ) -> Result<(), BridgeError> {
            Ok(())
        }

        /// Accepts any draft task.
        async fn create_draft_task(
            &self,
            _project: &str,
            _agent: &str,
            _title: &str,
            _summary: &str,
        ) -> Result<(), BridgeError> {
            Ok(())
        }

        /// Returns a fixed task id.
        async fn create_execution_task(
            &self,
            _project: &str,
            _agent: &str,
            _title: &str,
            _description: &str,
        ) -> Result<String, BridgeError> {
            Ok("task-test".to_string())
        }

        /// Accepts any status update.
        async fn update_task_status(
            &self,
            _task_id: &str,
            _status: &str,
            _note: &str,
        ) -> Result<(), BridgeError> {
            Ok(())
        }
    }

    /// Build a Room whose Rift endpoint is unreachable, holding one idle
    /// agent, without any network calls.
    fn offline_room() -> (Room, AgentId) {
        let agent_id = AgentId(Uuid::new_v4());
        let agent = RegisteredAgent {
            id: agent_id,
            rift_user_id: Uuid::new_v4(),
            username: "agent-test".to_string(),
            display_name: "Tester".to_string(),
            base_chance: 0.5,
            system_prompt: "test".to_string(),
            state: AgentState::Idle,
            slot_index: 0,
            last_posted_turn: None,
            last_post_at: None,
        };
        let roster = AgentRoster::from_agents(vec![agent]);

        // Port 9 on localhost: nothing listens there, connections fail fast.
        let auth = AgentAuthManager::new("test-secret".to_string());
        let rift = Arc::new(RiftRestClient::new("http://127.0.0.1:9".to_string(), auth));
        let kleos: Arc<dyn KleosClient> = Arc::new(NullKleos);
        let channel_id = Uuid::new_v4();
        let notifier: Arc<dyn crate::execution::RoomNotifier> = Arc::new(RiftRoomNotifier::new(
            rift.clone(),
            Uuid::new_v4(),
            "agent-test".to_string(),
            channel_id,
        ));
        let approval_registry = ApprovalRegistry::new(60);
        let oracle: Arc<dyn CapabilityOracle> =
            Arc::new(StaticAllowlistOracle::new(HashMap::new()));
        let coordinator = Arc::new(ProposalCoordinator::new(
            oracle,
            kleos.clone(),
            notifier.clone(),
            approval_registry.clone(),
            "test".to_string(),
            Vec::new(),
        ));
        let supervisor = Arc::new(ExecutionSupervisor::new(notifier));
        let sandbox_manager = Arc::new(SandboxManager::new(
            std::path::PathBuf::from("/tmp/rift-bridge-test-worktrees"),
            60,
        ));

        let room = Room {
            roster,
            executors: HashMap::new(),
            engagement: EngagementEngine::default(),
            turn_manager: TurnManager::new(10, 5),
            loop_guard: LoopGuard::new(5, 30),
            rift,
            kleos,
            config: BridgeDaemonConfig::default(),
            project_name: "test".to_string(),
            channel_id,
            channel_name: "general".to_string(),
            approval_registry,
            coordinator,
            supervisor,
            sandbox_manager,
            workspaces: Arc::new(Vec::new()),
            exec_semaphore: Arc::new(tokio::sync::Semaphore::new(1)),
            executors_by_username: HashMap::new(),
            personas: HashMap::new(),
            growth: None,
            room_turn: 0,
            recent_posts: VecDeque::new(),
        };
        (room, agent_id)
    }

    /// Regression test for finding N4: a failed context build (unreachable
    /// Rift) must return the agent to Idle instead of wedging it in
    /// Thinking, and must release the compose floor.
    #[tokio::test]
    async fn test_failed_compose_returns_agent_to_idle() {
        let (mut room, agent_id) = offline_room();

        let result = room.generate_and_post(agent_id).await;
        assert!(result.is_err(), "unreachable Rift must surface an error");

        let agent = room
            .roster
            .all()
            .find(|a| a.id == agent_id)
            .expect("agent still registered");
        assert_eq!(
            agent.state,
            AgentState::Idle,
            "agent must not stay wedged in Thinking after a failed compose"
        );

        let floor = tokio::time::timeout(
            std::time::Duration::from_millis(100),
            room.turn_manager.acquire_floor(),
        )
        .await;
        assert!(floor.is_ok(), "compose floor must be released after failure");
    }

    /// Verifies per-agent cooldown floor semantics (review finding:
    /// previously untested): never-posted agents are exempt, a fresh post
    /// blocks, and cooldown zero never blocks.
    #[test]
    fn test_cooldown_active_semantics() {
        assert!(!cooldown_active(None, 30));
        assert!(cooldown_active(Some(std::time::Instant::now()), 30));
        assert!(!cooldown_active(Some(std::time::Instant::now()), 0));
    }

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
