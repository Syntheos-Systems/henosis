//! Validation and materialization of immutable managed room revisions.

use std::fmt;
use std::path::Path;
#[cfg(test)]
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use henosis_rift_server::models::agent_control::CredentialMode;
use henosis_rift_server::models::agent_control::ExecutionCapabilityCatalog;
use serde_json::Value;
use uuid::Uuid;

use crate::config::{AgentConfig, BridgeConfig, ExecutorConfig};
use crate::executor::HealthStatus;
use crate::executors::build_executor;

/// Output returned by a credential-broker-mediated command after broker scrubbing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediatedCommandOutput {
    /// Whether the broker terminated the command at its deadline.
    pub timed_out: bool,
    /// Child exit code when the platform supplied one.
    pub exit_code: Option<i32>,
    /// Broker-scrubbed standard output bytes.
    pub stdout: Vec<u8>,
    /// Broker-scrubbed standard error bytes.
    pub stderr: Vec<u8>,
}

/// Narrow command boundary implemented by the Henosis-owned credential broker client.
#[async_trait]
pub trait PhylaxCommandRunner: Send + Sync {
    /// Run an allowlisted command while the broker injects one credential variable.
    async fn run(
        &self,
        category: &str,
        slot: &str,
        env_var: &str,
        argv: &[String],
    ) -> Result<MediatedCommandOutput, MaterializeError>;
}

/// Secret-free metadata resolved for one opaque credential binding.
#[derive(Clone)]
pub struct ResolvedCredentialBinding {
    /// Opaque binding identifier stored in Rift desired state.
    pub binding_id: Uuid,
    /// Rift human who owns the binding.
    pub owner_user_id: Uuid,
    /// Phylax credential category.
    pub category: String,
    /// Phylax credential slot name.
    pub slot: String,
    /// Environment variable the broker injects only into the child process.
    pub env_var: String,
    /// Harness IDs this binding may authenticate.
    pub allowed_harness_ids: Vec<String>,
    /// Broker command runner that never returns credential material.
    pub runner: Arc<dyn PhylaxCommandRunner>,
}

/// Redacts broker addressing details from routine configuration diagnostics.
impl fmt::Debug for ResolvedCredentialBinding {
    /// Show only stable ownership identifiers and the allowed harness list.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResolvedCredentialBinding")
            .field("binding_id", &self.binding_id)
            .field("owner_user_id", &self.owner_user_id)
            .field("allowed_harness_ids", &self.allowed_harness_ids)
            .field("broker_target", &"[REDACTED]")
            .finish()
    }
}

/// Resolves deployment-owned credential metadata without exposing credential values.
#[async_trait]
pub trait CredentialBindingResolver: Send + Sync {
    /// Resolve one opaque binding, returning `None` when no current record exists.
    async fn resolve_binding(
        &self,
        binding_id: Uuid,
    ) -> Result<Option<ResolvedCredentialBinding>, MaterializeError>;
}

/// Runtime command path selected after capability and ownership validation.
#[derive(Clone, Default)]
pub enum ResolvedExecutionMode {
    /// Invoke a CLI using an authenticated session already present on the host.
    #[default]
    HostSession,
    /// Invoke a CLI through Phylax without exposing its credential to the bridge.
    Phylax(ResolvedCredentialBinding),
}

/// Keeps runtime configuration diagnostics free of broker routing data.
impl fmt::Debug for ResolvedExecutionMode {
    /// Render only the selected authentication mode.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::HostSession => formatter.write_str("HostSession"),
            Self::Phylax(_) => formatter.write_str("Phylax([REDACTED])"),
        }
    }
}

/// Managed connection coordinates that replace deployment-local TOML values.
#[derive(Clone)]
pub struct ManagedRoomConnection {
    /// Rift HTTP API base URL.
    pub api_url: String,
    /// Rift WebSocket endpoint.
    pub ws_url: String,
    /// Rift JWT signing secret used for agent identities.
    pub jwt_secret: String,
    /// Dedicated secret accepted only by bridge-internal routes.
    pub bridge_secret: String,
    /// Durable Rift server identifier.
    pub server_id: Uuid,
    /// Durable Rift channel identifier.
    pub channel_id: Uuid,
}

/// Redacts the two managed Rift secrets from diagnostic output.
impl fmt::Debug for ManagedRoomConnection {
    /// Show connection coordinates while replacing secret values with a fixed marker.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ManagedRoomConnection")
            .field("api_url", &self.api_url)
            .field("ws_url", &self.ws_url)
            .field("jwt_secret", &"[REDACTED]")
            .field("bridge_secret", &"[REDACTED]")
            .field("server_id", &self.server_id)
            .field("channel_id", &self.channel_id)
            .finish()
    }
}

/// One desired seat enriched with the safe identity and behavior fields needed at runtime.
#[derive(Debug, Clone)]
pub struct ManagedSeat {
    /// Stable desired-state seat identifier.
    pub seat_id: Uuid,
    /// Persistent Rift agent user identifier.
    pub agent_user_id: Uuid,
    /// Rift human who owns the agent identity.
    pub owner_user_id: Uuid,
    /// Agent display name.
    pub name: String,
    /// Unique Rift username.
    pub username: String,
    /// Stable catalog harness identifier.
    pub harness_id: String,
    /// Stable catalog model identifier.
    pub model_id: String,
    /// Typed, non-secret harness settings.
    pub settings: Value,
    /// Opaque deployment-owned credential binding identifier.
    pub credential_binding_id: Option<Uuid>,
    /// Whether this seat participates in the running room.
    pub enabled: bool,
    /// Non-negative roster order.
    pub position: i32,
    /// Base engagement probability retained from deployment behavior policy.
    pub base_chance: f64,
    /// System prompt retained from deployment behavior policy.
    pub system_prompt: String,
}

/// Stable failure produced while validating or materializing one desired revision.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct MaterializeError {
    /// Stable machine-readable failure code safe to persist in Rift.
    pub code: &'static str,
    /// Bounded secret-free failure detail safe for the dashboard.
    pub message: String,
}

/// Constructs one stable, secret-free materialization failure.
pub fn materialize_error(code: &'static str, message: impl Into<String>) -> MaterializeError {
    MaterializeError {
        code,
        message: message.into(),
    }
}

/// Validate seats against the current catalog and resolve their credential execution modes.
pub async fn validate_seats(
    base: &BridgeConfig,
    catalog: &ExecutionCapabilityCatalog,
    bindings: &dyn CredentialBindingResolver,
    seats: &[ManagedSeat],
) -> Result<Vec<ResolvedExecutionMode>, MaterializeError> {
    let mut modes = Vec::with_capacity(seats.len());
    for seat in seats {
        let harness = catalog
            .harnesses
            .iter()
            .find(|harness| harness.id == seat.harness_id)
            .ok_or_else(|| {
                materialize_error(
                    "unknown_harness",
                    format!("harness {:?} is not in the host catalog", seat.harness_id),
                )
            })?;
        if !harness.available {
            return Err(materialize_error(
                "harness_unavailable",
                format!("harness {:?} is unavailable on this host", harness.id),
            ));
        }
        let model = harness
            .models
            .iter()
            .find(|model| model.id == seat.model_id)
            .ok_or_else(|| {
                materialize_error(
                    "unknown_model",
                    format!(
                        "model {:?} is not allowed for harness {:?}",
                        seat.model_id, harness.id
                    ),
                )
            })?;
        if !model.available {
            return Err(materialize_error(
                "model_unavailable",
                format!("model {:?} is unavailable on this host", model.id),
            ));
        }
        harness.validate_settings(&seat.settings).map_err(|error| {
            let code = if error.code == "unsupported_setting_value"
                && seat.settings.get("reasoning_effort").is_some()
            {
                "invalid_reasoning_effort"
            } else {
                error.code
            };
            materialize_error(code, error.message)
        })?;

        let mode = match seat.credential_binding_id {
            None if harness.credential_mode == CredentialMode::RequiredBinding => {
                return Err(materialize_error(
                    "credential_not_ready",
                    format!("harness {:?} requires a credential binding", harness.id),
                ));
            }
            None => ResolvedExecutionMode::HostSession,
            Some(binding_id) => {
                if harness.credential_mode == CredentialMode::HostSession {
                    return Err(materialize_error(
                        "credential_harness_mismatch",
                        format!(
                            "harness {:?} does not accept credential bindings",
                            harness.id
                        ),
                    ));
                }
                if !has_mediated_template(base, &seat.harness_id) {
                    return Err(materialize_error(
                        "harness_unavailable",
                        format!(
                            "harness {:?} has no broker-compatible executable",
                            seat.harness_id
                        ),
                    ));
                }
                let binding = bindings.resolve_binding(binding_id).await?.ok_or_else(|| {
                    materialize_error(
                        "credential_not_ready",
                        format!("credential binding {binding_id} is not ready"),
                    )
                })?;
                if binding.owner_user_id != seat.owner_user_id {
                    return Err(materialize_error(
                        "credential_owner_mismatch",
                        "credential binding belongs to a different human owner",
                    ));
                }
                if !binding
                    .allowed_harness_ids
                    .iter()
                    .any(|allowed| allowed == &seat.harness_id)
                {
                    return Err(materialize_error(
                        "credential_harness_mismatch",
                        format!(
                            "credential binding cannot authenticate harness {:?}",
                            seat.harness_id
                        ),
                    ));
                }
                ResolvedExecutionMode::Phylax(binding)
            }
        };
        modes.push(mode);
    }
    Ok(modes)
}

/// Load one base deployment configuration and replace only managed connection and roster state.
pub fn materialize_revision(
    base_path: &Path,
    managed: ManagedRoomConnection,
    seats: Vec<ManagedSeat>,
    execution_modes: Vec<ResolvedExecutionMode>,
) -> Result<BridgeConfig, MaterializeError> {
    if seats.len() != execution_modes.len() {
        return Err(materialize_error(
            "materialize_failed",
            "validated execution modes did not match the desired seat count",
        ));
    }
    let base = BridgeConfig::load_for_managed_room(
        base_path,
        managed.api_url,
        managed.ws_url,
        managed.jwt_secret,
        managed.bridge_secret,
        managed.server_id,
        managed.channel_id,
    )
    .map_err(|_| {
        materialize_error(
            "materialize_failed",
            "base bridge configuration could not be loaded",
        )
    })?;
    materialize_loaded_revision(base, seats, execution_modes)
}

/// Replace the roster of an already loaded base config while retaining every other subsystem.
pub fn materialize_loaded_revision(
    mut base: BridgeConfig,
    seats: Vec<ManagedSeat>,
    execution_modes: Vec<ResolvedExecutionMode>,
) -> Result<BridgeConfig, MaterializeError> {
    let templates = base.agents.clone();
    let mut paired = seats
        .into_iter()
        .zip(execution_modes)
        .filter(|(seat, _)| seat.enabled)
        .collect::<Vec<_>>();
    paired.sort_by_key(|(seat, _)| seat.position);

    let mut agents = Vec::with_capacity(paired.len());
    for (seat, execution_mode) in paired {
        let executor = executor_for_seat(&templates, &seat, &execution_mode)?;
        agents.push(AgentConfig {
            name: seat.name,
            username: seat.username,
            executor,
            base_chance: seat.base_chance,
            system_prompt: seat.system_prompt,
            execution_mode,
        });
    }
    if agents.is_empty() {
        return Err(materialize_error(
            "materialize_failed",
            "a managed room must contain at least one enabled agent seat",
        ));
    }
    base.agents = agents;
    Ok(base)
}

/// Build one executor config by overriding a deployment-owned harness template.
fn executor_for_seat(
    templates: &[AgentConfig],
    seat: &ManagedSeat,
    execution_mode: &ResolvedExecutionMode,
) -> Result<ExecutorConfig, MaterializeError> {
    let requires_absolute_binary = matches!(execution_mode, ResolvedExecutionMode::Phylax(_));
    let template = templates
        .iter()
        .find(|agent| {
            harness_id(&agent.executor) == seat.harness_id
                && executor_template_available(&agent.executor)
                && (!requires_absolute_binary || executor_template_is_absolute(&agent.executor))
        })
        .ok_or_else(|| {
            materialize_error(
                "harness_unavailable",
                format!("harness {:?} has no deployment template", seat.harness_id),
            )
        })?;
    match &template.executor {
        ExecutorConfig::ClaudeCode {
            binary, max_tokens, ..
        } => Ok(ExecutorConfig::ClaudeCode {
            binary: binary.clone(),
            model: Some(seat.model_id.clone()),
            max_tokens: *max_tokens,
        }),
        ExecutorConfig::Codex { binary, .. } => Ok(ExecutorConfig::Codex {
            binary: binary.clone(),
            model: seat.model_id.clone(),
            reasoning_effort: setting_string(&seat.settings, "reasoning_effort")?,
        }),
        ExecutorConfig::Synapse {
            provider,
            host,
            token,
            api_key,
            max_tokens,
            max_turns,
            cwd,
            ..
        } => Ok(ExecutorConfig::Synapse {
            provider: provider.clone(),
            model: (seat.model_id != "default").then(|| seat.model_id.clone()),
            host: host.clone(),
            token: token.clone(),
            api_key: api_key.clone(),
            max_tokens: setting_u32(&seat.settings, "max_tokens")?.or(*max_tokens),
            max_turns: setting_usize(&seat.settings, "max_turns")?.or(*max_turns),
            cwd: cwd.clone(),
        }),
    }
}

/// Report whether a harness has an available absolute executable suitable for broker execution.
fn has_mediated_template(base: &BridgeConfig, selected_harness_id: &str) -> bool {
    base.agents.iter().any(|agent| {
        harness_id(&agent.executor) == selected_harness_id
            && executor_template_available(&agent.executor)
            && executor_template_is_absolute(&agent.executor)
    })
}

/// Report whether one template names an absolute executable accepted by Phylax.
fn executor_template_is_absolute(executor: &ExecutorConfig) -> bool {
    match executor {
        ExecutorConfig::ClaudeCode { binary, .. } | ExecutorConfig::Codex { binary, .. } => {
            binary.is_absolute()
        }
        ExecutorConfig::Synapse { .. } => false,
    }
}

/// Keep materialization aligned with the availability reported by host discovery.
fn executor_template_available(executor: &ExecutorConfig) -> bool {
    match executor {
        ExecutorConfig::ClaudeCode { binary, .. } | ExecutorConfig::Codex { binary, .. } => {
            crate::catalog::command_available(binary)
        }
        ExecutorConfig::Synapse { .. } => true,
    }
}

/// Return the stable catalog ID for one executor template.
pub(crate) fn harness_id(executor: &ExecutorConfig) -> &'static str {
    match executor {
        ExecutorConfig::ClaudeCode { .. } => "claude-code",
        ExecutorConfig::Codex { .. } => "codex",
        ExecutorConfig::Synapse { .. } => "synapse",
    }
}

/// Read an optional string setting after catalog validation.
fn setting_string(settings: &Value, key: &str) -> Result<Option<String>, MaterializeError> {
    settings
        .get(key)
        .map(|value| {
            value.as_str().map(str::to_string).ok_or_else(|| {
                materialize_error(
                    "materialize_failed",
                    format!("setting {key:?} is not a string"),
                )
            })
        })
        .transpose()
}

/// Read an optional `u32` setting after catalog validation.
fn setting_u32(settings: &Value, key: &str) -> Result<Option<u32>, MaterializeError> {
    settings
        .get(key)
        .map(|value| {
            value
                .as_u64()
                .and_then(|number| u32::try_from(number).ok())
                .ok_or_else(|| {
                    materialize_error(
                        "materialize_failed",
                        format!("setting {key:?} is not a valid unsigned integer"),
                    )
                })
        })
        .transpose()
}

/// Read an optional `usize` setting after catalog validation.
fn setting_usize(settings: &Value, key: &str) -> Result<Option<usize>, MaterializeError> {
    settings
        .get(key)
        .map(|value| {
            value
                .as_u64()
                .and_then(|number| usize::try_from(number).ok())
                .ok_or_else(|| {
                    materialize_error(
                        "materialize_failed",
                        format!("setting {key:?} is not a valid unsigned integer"),
                    )
                })
        })
        .transpose()
}

/// Construct and health-check every candidate executor without provisioning or starting a room.
pub async fn preflight_revision(config: &BridgeConfig) -> Result<(), MaterializeError> {
    for agent in &config.agents {
        let executor = build_executor(agent).map_err(|_| {
            materialize_error(
                "executor_unavailable",
                format!("executor for agent {:?} could not be built", agent.username),
            )
        })?;
        match executor.health_check().await.map_err(|error| {
            materialize_error(
                "executor_unavailable",
                format!(
                    "executor health check failed for {:?}: {error}",
                    agent.username
                ),
            )
        })? {
            HealthStatus::Ready => {}
            HealthStatus::Degraded(reason) | HealthStatus::Unavailable(reason) => {
                return Err(materialize_error("executor_unavailable", reason));
            }
        }
    }
    Ok(())
}

/// Test helpers for managed revision validation and materialization.
#[cfg(test)]
pub(crate) mod test_support {
    use super::*;

    /// Materialize directly from an in-memory base without parsing a private config file.
    pub(crate) fn materialize_loaded(
        base: BridgeConfig,
        seats: Vec<ManagedSeat>,
        modes: Vec<ResolvedExecutionMode>,
    ) -> Result<BridgeConfig, MaterializeError> {
        materialize_loaded_revision(base, seats, modes)
    }
}

#[cfg(test)]
/// Contract tests for capability validation, credential ownership, and roster replacement.
mod tests {
    use std::collections::HashMap;
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Mutex;

    use serde_json::json;

    use crate::catalog::discover_catalog;
    use crate::executor::{AgentExecutor, DiscussionContext};
    use crate::executors::CodexExecutor;

    use super::test_support::materialize_loaded;
    use super::*;

    /// UUID-scoped executable fixture used by host catalog discovery.
    struct ExecutableFixture {
        /// Isolated fixture directory.
        root: PathBuf,
        /// Executable file inside the fixture directory.
        path: PathBuf,
    }

    /// Creates one harmless executable file and cleans it up after each test.
    impl ExecutableFixture {
        /// Build a fresh executable shell fixture.
        fn new(name: &str) -> Self {
            let root =
                std::env::temp_dir().join(format!("henosis-materialize-{name}-{}", Uuid::new_v4()));
            fs::create_dir(&root).expect("create executable fixture directory");
            let path = root.join(name);
            fs::write(&path, "#!/bin/sh\nexit 0\n").expect("write executable fixture");
            #[cfg(unix)]
            {
                let mut permissions = fs::metadata(&path).expect("stat fixture").permissions();
                permissions.set_mode(0o700);
                fs::set_permissions(&path, permissions).expect("make fixture executable");
            }
            Self { root, path }
        }
    }

    /// Removes only the UUID-scoped fixture directory.
    impl Drop for ExecutableFixture {
        /// Clean up the isolated executable fixture.
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    /// In-memory binding resolver used to exercise ownership and readiness failures.
    struct FakeResolver {
        /// Resolved bindings indexed by opaque ID.
        bindings: HashMap<Uuid, ResolvedCredentialBinding>,
    }

    /// Returns only records explicitly installed by the test.
    #[async_trait]
    impl CredentialBindingResolver for FakeResolver {
        /// Resolve one fake binding without any external I/O.
        async fn resolve_binding(
            &self,
            binding_id: Uuid,
        ) -> Result<Option<ResolvedCredentialBinding>, MaterializeError> {
            Ok(self.bindings.get(&binding_id).cloned())
        }
    }

    /// Captured broker call proving the command boundary excludes credential contents.
    #[derive(Debug, Clone, PartialEq, Eq)]
    struct BrokerCall {
        /// Credential category received by the runner.
        category: String,
        /// Credential slot received by the runner.
        slot: String,
        /// Credential environment variable received by the runner.
        env_var: String,
        /// Complete argv received by the runner.
        argv: Vec<String>,
    }

    /// Spy runner that captures safe broker inputs and returns one Codex JSONL message.
    #[derive(Default)]
    struct SpyRunner {
        /// Calls made through the credential boundary.
        calls: Mutex<Vec<BrokerCall>>,
    }

    /// Records safe routing metadata without ever receiving a resolved secret.
    #[async_trait]
    impl PhylaxCommandRunner for SpyRunner {
        /// Capture the four allowed inputs and return scrubbed fixture output.
        async fn run(
            &self,
            category: &str,
            slot: &str,
            env_var: &str,
            argv: &[String],
        ) -> Result<MediatedCommandOutput, MaterializeError> {
            self.calls
                .lock()
                .expect("spy runner lock")
                .push(BrokerCall {
                    category: category.to_string(),
                    slot: slot.to_string(),
                    env_var: env_var.to_string(),
                    argv: argv.to_vec(),
                });
            Ok(MediatedCommandOutput {
                timed_out: false,
                exit_code: Some(0),
                stdout: b"{\"type\":\"item.completed\",\"item\":{\"type\":\"agent_message\",\"text\":\"Brokered\"}}\n"
                    .to_vec(),
                stderr: Vec::new(),
            })
        }
    }

    /// Parse a feature-complete base config around explicit harness binaries.
    fn base_config(claude_binary: &Path, codex_binary: &Path) -> BridgeConfig {
        toml::from_str(&format!(
            r#"
            [rift]
            api_url = "http://127.0.0.1:3200"
            ws_url = "ws://127.0.0.1:3200/ws"
            jwt_secret = "jwt-secret-that-is-at-least-32-bytes"
            bridge_secret = "bridge-secret-that-is-at-least-32-bytes"
            server_id = "00000000-0000-0000-0000-000000000001"
            channel_id = "00000000-0000-0000-0000-000000000002"
            pause_poll_secs = 7

            [bridge]
            cooldown_secs = 31
            turn_budget = 6
            thread_ceiling = 33
            context_window = 51

            [capabilities]
            original = ["fs_read"]

            [[workspaces]]
            name = "henosis"
            path = "/tmp/henosis"
            cargo_target_dir = "/tmp/cargo-target"

            [execution]
            worktrees_root = "/tmp/worktrees"
            max_concurrent_executions = 2
            approval_ttl_secs = 1801
            max_runtime_secs = 1802

            [pistis]
            enabled = true
            orchestrator_url = "http://127.0.0.1:4400"
            auth_token_cred = "henosis/pistis"
            room = "!room:example"

            [control]
            bind_addr = "127.0.0.1:3210"
            auth_token = "control-token-that-is-at-least-32-bytes"

            [personas]
            library_path = "/tmp/personas"
            growth_root = "/tmp/growth"
            max_same_persona = 2
            challenger_slot = true

            [kleos]
            in_process = true
            db_dir = "/tmp/kleos"

            [embedding]
            model = "bge-m3"
            semantic_threshold = 0.86
            reignition_threshold = 0.87
            reignition_damp = 0.2
            reignition_ttl_secs = 900

            [stimulus]
            enabled = true
            poll_secs = 61

            [[agents]]
            name = "Claude Template"
            username = "claude-template"
            base_chance = 0.4
            system_prompt = "Claude template prompt"
            executor = {{ type = "ClaudeCode", binary = {:?}, model = "sonnet", max_tokens = 4096 }}

            [[agents]]
            name = "Codex Template"
            username = "codex-template"
            base_chance = 0.5
            system_prompt = "Codex template prompt"
            executor = {{ type = "Codex", binary = {:?}, model = "gpt-5.6-sol", reasoning_effort = "medium" }}
            "#,
            claude_binary.display().to_string(),
            codex_binary.display().to_string(),
        ))
        .expect("parse base bridge config")
    }

    /// Construct one managed seat with deterministic safe behavior fields.
    fn seat(owner_user_id: Uuid, harness_id: &str, model_id: &str, position: i32) -> ManagedSeat {
        ManagedSeat {
            seat_id: Uuid::new_v4(),
            agent_user_id: Uuid::new_v4(),
            owner_user_id,
            name: format!("Agent {position}"),
            username: format!("agent-{position}"),
            harness_id: harness_id.to_string(),
            model_id: model_id.to_string(),
            settings: json!({}),
            credential_binding_id: None,
            enabled: true,
            position,
            base_chance: 0.25 + f64::from(position) / 10.0,
            system_prompt: format!("Prompt {position}"),
        }
    }

    /// Construct the minimal discussion context needed by the mediated executor test.
    fn discussion_context() -> DiscussionContext {
        DiscussionContext {
            recent_messages: Vec::new(),
            persona_name: None,
            relevant_memories: Vec::new(),
            active_tasks_summary: None,
            channel_id: "general".to_string(),
            system_framing: Some("Answer precisely.".to_string()),
        }
    }

    /// A missing binary marks only its own configured harness unavailable.
    #[test]
    fn discovery_isolates_missing_harness_binaries() {
        let claude = ExecutableFixture::new("claude");
        let missing_codex = claude.root.join("missing-codex");
        let catalog = discover_catalog(&base_config(&claude.path, &missing_codex), Uuid::new_v4());

        let claude_harness = catalog
            .harnesses
            .iter()
            .find(|harness| harness.id == "claude-code")
            .expect("Claude harness");
        let codex_harness = catalog
            .harnesses
            .iter()
            .find(|harness| harness.id == "codex")
            .expect("Codex harness");
        assert!(claude_harness.available);
        assert!(!codex_harness.available);
        assert!(claude_harness
            .models
            .iter()
            .any(|model| model.id == "sonnet" && model.available));
        assert!(claude_harness.settings.is_empty());
        assert!(codex_harness
            .models
            .iter()
            .any(|model| model.id == "gpt-5.6-sol" && !model.available));
    }

    /// A configured Synapse provider without a model exposes a reversible default choice.
    #[test]
    fn synapse_without_model_round_trips_default_selection() {
        let claude = ExecutableFixture::new("claude");
        let codex = ExecutableFixture::new("codex");
        let mut base = base_config(&claude.path, &codex.path);
        base.agents.push(AgentConfig {
            name: "Synapse Template".to_string(),
            username: "synapse-template".to_string(),
            executor: ExecutorConfig::Synapse {
                provider: "claude-max".to_string(),
                model: None,
                host: None,
                token: None,
                api_key: None,
                max_tokens: Some(4096),
                max_turns: Some(4),
                cwd: Some(PathBuf::from("/tmp/henosis")),
            },
            base_chance: 0.3,
            system_prompt: "Synapse template prompt".to_string(),
            execution_mode: ResolvedExecutionMode::HostSession,
        });
        let catalog = discover_catalog(&base, Uuid::new_v4());
        let harness = catalog
            .harnesses
            .iter()
            .find(|harness| harness.id == "synapse")
            .expect("Synapse harness");
        assert!(harness
            .models
            .iter()
            .any(|model| model.id == "default" && model.available));

        let owner = Uuid::new_v4();
        let materialized = materialize_loaded(
            base,
            vec![seat(owner, "synapse", "default", 0)],
            vec![ResolvedExecutionMode::HostSession],
        )
        .expect("materialize default Synapse model");
        assert!(matches!(
            &materialized.agents[0].executor,
            ExecutorConfig::Synapse { model: None, .. }
        ));
    }

    /// Model selections route to their harness templates and preserve non-roster base settings.
    #[tokio::test]
    async fn materialization_routes_models_orders_seats_and_preserves_base() {
        let claude = ExecutableFixture::new("claude");
        let codex = ExecutableFixture::new("codex");
        let base = base_config(&claude.path, &codex.path);
        let owner = Uuid::new_v4();
        let materialized = materialize_loaded(
            base,
            vec![
                seat(owner, "codex", "gpt-5.6-sol", 9),
                seat(owner, "claude-code", "sonnet", 2),
            ],
            vec![
                ResolvedExecutionMode::HostSession,
                ResolvedExecutionMode::HostSession,
            ],
        )
        .expect("materialize roster");

        assert_eq!(materialized.agents[0].username, "agent-2");
        assert_eq!(materialized.agents[1].username, "agent-9");
        assert!(matches!(
            &materialized.agents[0].executor,
            ExecutorConfig::ClaudeCode { model: Some(model), .. } if model == "sonnet"
        ));
        assert!(matches!(
            &materialized.agents[1].executor,
            ExecutorConfig::Codex { model, .. } if model == "gpt-5.6-sol"
        ));
        assert_eq!(materialized.bridge.cooldown_secs, 31);
        assert_eq!(materialized.rift.pause_poll_secs, Some(7));
        assert_eq!(
            materialized
                .execution
                .as_ref()
                .unwrap()
                .max_concurrent_executions,
            2
        );
        assert_eq!(
            materialized.workspaces[0].cargo_target_dir.as_deref(),
            Some(Path::new("/tmp/cargo-target"))
        );
        assert!(materialized.pistis.as_ref().unwrap().enabled);
        assert!(materialized.personas.as_ref().unwrap().challenger_slot);
        assert!(materialized.embedding.is_some());
        assert!(materialized.stimulus.as_ref().unwrap().enabled);
        preflight_revision(&materialized)
            .await
            .expect("materialized executors pass construction and health checks");
    }

    /// Unknown capabilities, bad reasoning effort, and missing bindings use stable codes.
    #[tokio::test]
    async fn validation_returns_stable_capability_and_readiness_errors() {
        let claude = ExecutableFixture::new("claude");
        let codex = ExecutableFixture::new("codex");
        let base = base_config(&claude.path, &codex.path);
        let catalog = discover_catalog(&base, Uuid::new_v4());
        let resolver = FakeResolver {
            bindings: HashMap::new(),
        };
        let owner = Uuid::new_v4();

        let unknown_harness = seat(owner, "missing", "model", 0);
        assert_eq!(
            validate_seats(&base, &catalog, &resolver, &[unknown_harness])
                .await
                .unwrap_err()
                .code,
            "unknown_harness"
        );
        let unknown_model = seat(owner, "codex", "missing-model", 0);
        assert_eq!(
            validate_seats(&base, &catalog, &resolver, &[unknown_model])
                .await
                .unwrap_err()
                .code,
            "unknown_model"
        );
        let mut invalid_effort = seat(owner, "codex", "gpt-5.6-sol", 0);
        invalid_effort.settings = json!({"reasoning_effort": "impossible"});
        assert_eq!(
            validate_seats(&base, &catalog, &resolver, &[invalid_effort])
                .await
                .unwrap_err()
                .code,
            "invalid_reasoning_effort"
        );
        let mut missing_binding = seat(owner, "codex", "gpt-5.6-sol", 0);
        missing_binding.credential_binding_id = Some(Uuid::new_v4());
        assert_eq!(
            validate_seats(&base, &catalog, &resolver, &[missing_binding])
                .await
                .unwrap_err()
                .code,
            "credential_not_ready"
        );
    }

    /// Broker execution rejects a PATH-only template before resolving its binding.
    #[tokio::test]
    async fn mediated_validation_requires_absolute_executable() {
        let claude = ExecutableFixture::new("claude");
        let codex = ExecutableFixture::new("codex");
        let mut base = base_config(&claude.path, &codex.path);
        let catalog = discover_catalog(&base, Uuid::new_v4());
        let codex_template = base
            .agents
            .iter_mut()
            .find(|agent| matches!(agent.executor, ExecutorConfig::Codex { .. }))
            .expect("Codex template");
        let ExecutorConfig::Codex { binary, .. } = &mut codex_template.executor else {
            panic!("selected template changed harness");
        };
        *binary = PathBuf::from("codex");
        let resolver = FakeResolver {
            bindings: HashMap::new(),
        };
        let mut desired = seat(Uuid::new_v4(), "codex", "gpt-5.6-sol", 0);
        desired.credential_binding_id = Some(Uuid::new_v4());

        assert_eq!(
            validate_seats(&base, &catalog, &resolver, &[desired])
                .await
                .unwrap_err()
                .code,
            "harness_unavailable"
        );
    }

    /// Materialization selects the absolute template validated for a brokered seat.
    #[test]
    fn mediated_materialization_uses_absolute_template() {
        let claude = ExecutableFixture::new("claude");
        let codex = ExecutableFixture::new("codex");
        let mut base = base_config(&claude.path, &codex.path);
        let absolute_template = base
            .agents
            .iter()
            .find(|agent| matches!(agent.executor, ExecutorConfig::Codex { .. }))
            .expect("Codex template")
            .clone();
        let ExecutorConfig::Codex { binary, .. } = &mut base.agents[1].executor else {
            panic!("base template ordering changed");
        };
        *binary = PathBuf::from("codex");
        base.agents.push(absolute_template);
        let binding = ResolvedCredentialBinding {
            binding_id: Uuid::new_v4(),
            owner_user_id: Uuid::new_v4(),
            category: "openai".to_string(),
            slot: "codex-seat".to_string(),
            env_var: "OPENAI_API_KEY".to_string(),
            allowed_harness_ids: vec!["codex".to_string()],
            runner: Arc::new(SpyRunner::default()),
        };
        let owner = Uuid::new_v4();

        let materialized = materialize_loaded(
            base,
            vec![seat(owner, "codex", "gpt-5.6-sol", 0)],
            vec![ResolvedExecutionMode::Phylax(binding)],
        )
        .expect("materialize brokered seat");
        assert!(matches!(
            &materialized.agents[0].executor,
            ExecutorConfig::Codex { binary, .. } if binary == &codex.path
        ));
    }

    /// A binding owned by another human fails before a replacement config can be built.
    #[tokio::test]
    async fn credential_owner_mismatch_fails_closed() {
        let claude = ExecutableFixture::new("claude");
        let codex = ExecutableFixture::new("codex");
        let base = base_config(&claude.path, &codex.path);
        let catalog = discover_catalog(&base, Uuid::new_v4());
        let runner = Arc::new(SpyRunner::default());
        let binding_id = Uuid::new_v4();
        let binding = ResolvedCredentialBinding {
            binding_id,
            owner_user_id: Uuid::new_v4(),
            category: "anthropic".to_string(),
            slot: "agent-one".to_string(),
            env_var: "ANTHROPIC_API_KEY".to_string(),
            allowed_harness_ids: vec!["claude-code".to_string()],
            runner,
        };
        let resolver = FakeResolver {
            bindings: HashMap::from([(binding_id, binding)]),
        };
        let mut desired = seat(Uuid::new_v4(), "claude-code", "sonnet", 0);
        desired.credential_binding_id = Some(binding_id);

        assert_eq!(
            validate_seats(&base, &catalog, &resolver, &[desired])
                .await
                .unwrap_err()
                .code,
            "credential_owner_mismatch"
        );
    }

    /// Mediated Codex receives only category, slot, env name, and argv with no secret value.
    #[tokio::test]
    async fn phylax_runner_boundary_never_receives_resolved_secret() {
        let runner = Arc::new(SpyRunner::default());
        let binding = ResolvedCredentialBinding {
            binding_id: Uuid::new_v4(),
            owner_user_id: Uuid::new_v4(),
            category: "openai".to_string(),
            slot: "codex-seat".to_string(),
            env_var: "OPENAI_API_KEY".to_string(),
            allowed_harness_ids: vec!["codex".to_string()],
            runner: runner.clone(),
        };
        let executor = CodexExecutor::new(
            PathBuf::from("/opt/codex/bin/codex"),
            "gpt-5.6-sol".to_string(),
            Some("medium".to_string()),
        )
        .with_execution_mode(ResolvedExecutionMode::Phylax(binding));

        let response = executor
            .discuss(discussion_context())
            .await
            .expect("mediated discussion succeeds")
            .expect("agent response");

        assert_eq!(response.text, "Brokered");
        let calls = runner.calls.lock().expect("spy calls");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].category, "openai");
        assert_eq!(calls[0].slot, "codex-seat");
        assert_eq!(calls[0].env_var, "OPENAI_API_KEY");
        assert_eq!(calls[0].argv[0], "/opt/codex/bin/codex");
        assert!(calls[0]
            .argv
            .iter()
            .all(|argument| argument != "SUPERSECRET"));
    }
}
