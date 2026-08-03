//! Bridge and agent roster configuration.

use serde::Deserialize;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use uuid::Uuid;

use crate::error::BridgeError;
use crate::materialize::ResolvedExecutionMode;

/// Top-level bridge configuration loaded from TOML.
#[derive(Debug, Deserialize, Clone)]
pub struct BridgeConfig {
    /// Connection settings for the Rift server.
    #[serde(default)]
    pub rift: RiftConfig,
    /// Bridge daemon behavior settings.
    #[serde(default)]
    pub bridge: BridgeDaemonConfig,
    /// Agent roster -- one entry per agent user to provision.
    pub agents: Vec<AgentConfig>,
    /// Per-agent capability allowlist for execution mode (agent username -> capability names).
    #[serde(default)]
    pub capabilities: HashMap<String, Vec<String>>,
    /// Declared workspaces an approved task may execute against.
    #[serde(default)]
    pub workspaces: Vec<WorkspaceConfig>,
    /// Execution-mode runtime settings. Absent disables execution mode.
    pub execution: Option<ExecutionSettings>,
    /// Optional Pistis-backed capability oracle settings.
    pub pistis: Option<PistisConfig>,
    /// Control HTTP server settings. Absent disables the control server.
    pub control: Option<ControlConfig>,
    /// Optional Frameshift persona allocation settings. Absent disables personas.
    pub personas: Option<PersonaSettings>,
    /// Optional in-process Kleos backend. Absent (or in_process=false) uses the
    /// standalone HTTP client; present with in_process=true backs the bridge's
    /// coordination with the in-process henosis kernel stores.
    pub kleos: Option<KleosBackendConfig>,
    /// Optional embedding tuning or HTTP override for semantic echo/loop
    /// detection. In cognition-enabled in-process deployments, absence selects
    /// the shared local provider; other deployments remain token-overlap only.
    pub embedding: Option<EmbeddingConfig>,
    /// Optional stimulus injector settings. Absent (or enabled=false)
    /// disables injection entirely.
    pub stimulus: Option<StimulusSettings>,
}

/// Embedding provider settings powering the semantic tier of echo suppression
/// and topic-reignition damping. An explicit URL selects the HTTP provider;
/// without a URL, a cognition-enabled in-process deployment uses its shared
/// Kleos provider.
#[derive(Debug, Deserialize, Clone)]
pub struct EmbeddingConfig {
    /// Full endpoint URL (e.g. `http://127.0.0.1:11434/v1/embeddings`).
    /// Omit to use Henosis's in-process Kleos provider when available.
    #[serde(default)]
    pub url: Option<String>,
    /// Model identifier passed to an explicit HTTP endpoint. The in-process
    /// provider uses the model selected by standard Kleos environment config.
    #[serde(default = "default_embedding_model")]
    pub model: String,
    /// Name of the environment variable holding the bearer token, if the
    /// endpoint needs one. Local TEI/Ollama endpoints typically do not.
    #[serde(default)]
    pub api_key_env: Option<String>,
    /// Cosine similarity at or above which a candidate response is an echo
    /// of a recent peer post.
    #[serde(default = "default_semantic_threshold")]
    pub semantic_threshold: f64,
    /// Cosine similarity at or above which a fresh trigger counts as
    /// reigniting a recently exhausted topic.
    #[serde(default = "default_semantic_threshold")]
    pub reignition_threshold: f64,
    /// Engagement multiplier applied to every agent while a cascade runs on
    /// a reignited topic (1.0 disables damping).
    #[serde(default = "default_reignition_damp")]
    pub reignition_damp: f64,
    /// Seconds an exhausted topic stays live for reignition matching.
    #[serde(default = "default_reignition_ttl_secs")]
    pub reignition_ttl_secs: u64,
}

/// Supplies semantic thresholds for the in-process Kleos provider when no
/// explicit embedding block is present.
impl Default for EmbeddingConfig {
    /// Build local-provider-ready settings with the established semantic
    /// thresholds and no external URL override.
    fn default() -> Self {
        Self {
            url: None,
            model: default_embedding_model(),
            api_key_env: None,
            semantic_threshold: default_semantic_threshold(),
            reignition_threshold: default_semantic_threshold(),
            reignition_damp: default_reignition_damp(),
            reignition_ttl_secs: default_reignition_ttl_secs(),
        }
    }
}

/// Default embedding model identifier used by both local and HTTP providers.
fn default_embedding_model() -> String {
    "bge-m3".to_string()
}

/// Default cosine threshold for both echo and reignition matching.
fn default_semantic_threshold() -> f64 {
    0.85
}

/// Default reignition engagement damp.
fn default_reignition_damp() -> f64 {
    0.3
}

/// Default lifetime of an exhausted-topic record (6 hours).
fn default_reignition_ttl_secs() -> u64 {
    21600
}

/// Stimulus injector settings for scheduled reflection, project signals,
/// per-type cooldowns, and rate limiting.
#[derive(Debug, Deserialize, Clone)]
pub struct StimulusSettings {
    /// Master switch; false keeps the injector fully absent.
    #[serde(default)]
    pub enabled: bool,
    /// Seconds between source poll cycles.
    #[serde(default = "default_stimulus_poll_secs")]
    pub poll_secs: u64,
    /// Room inactivity in seconds before a reflection prompt fires; also the
    /// reflection refire cooldown.
    #[serde(default = "default_reflection_after_secs")]
    pub reflection_after_secs: u64,
    /// Minimum seconds between two Chiasm task-change stimuli.
    #[serde(default = "default_chiasm_cooldown_secs")]
    pub chiasm_cooldown_secs: u64,
    /// Minimum seconds between two git commit stimuli.
    #[serde(default = "default_git_cooldown_secs")]
    pub git_cooldown_secs: u64,
    /// Minimum seconds between two Axon activity-event stimuli.
    #[serde(default = "default_axon_cooldown_secs")]
    pub axon_cooldown_secs: u64,
    /// Minimum seconds between two Loom workflow-run stimuli.
    #[serde(default = "default_loom_cooldown_secs")]
    pub loom_cooldown_secs: u64,
    /// Global cap on stimuli per rolling hour, across all kinds.
    #[serde(default = "default_stimulus_max_per_hour")]
    pub max_per_hour: u32,
}

/// Default seconds between stimulus poll cycles.
fn default_stimulus_poll_secs() -> u64 {
    60
}

/// Default reflection inactivity window of four hours.
fn default_reflection_after_secs() -> u64 {
    14400
}

/// Default Chiasm stimulus cooldown (15 minutes).
fn default_chiasm_cooldown_secs() -> u64 {
    900
}

/// Default git stimulus cooldown (5 minutes).
fn default_git_cooldown_secs() -> u64 {
    300
}

/// Default Axon activity-event stimulus cooldown (10 minutes).
fn default_axon_cooldown_secs() -> u64 {
    600
}

/// Default Loom workflow-run stimulus cooldown (10 minutes).
fn default_loom_cooldown_secs() -> u64 {
    600
}

/// Default global stimulus rate cap per hour.
fn default_stimulus_max_per_hour() -> u32 {
    6
}

/// Room-level Frameshift persona allocation settings.
#[derive(Debug, Deserialize, Clone)]
pub struct PersonaSettings {
    /// Frameshift persona library (catalog) root directory.
    pub library_path: PathBuf,
    /// Directory holding per-agent growth files.
    pub growth_root: PathBuf,
    /// Maximum agents allowed to hold the same persona in the room.
    #[serde(default = "default_max_same_persona")]
    pub max_same_persona: usize,
    /// Whether to reserve one challenger (contrarian) slot.
    #[serde(default)]
    pub challenger_slot: bool,
}

/// Default cap on how many agents may share a persona.
fn default_max_same_persona() -> usize {
    2
}

/// In-process Kleos backend settings. When `in_process` is true the
/// bridge opens henosis kernel stores instead of talking to Kleos over HTTP.
#[derive(Debug, Deserialize, Clone)]
pub struct KleosBackendConfig {
    /// Route the bridge's Chiasm/Broca/memory ops through in-process kernel
    /// stores instead of HttpKleosClient.
    #[serde(default)]
    pub in_process: bool,
    /// Directory for the SQLite-backed Chiasm/Broca stores. Absent = ephemeral
    /// in-memory stores (lost on restart).
    #[serde(default)]
    pub db_dir: Option<std::path::PathBuf>,
}

/// Connection to the Rift server.
#[derive(Debug, Default, Deserialize, Clone)]
pub struct RiftConfig {
    /// Rift server base URL (e.g., http://localhost:3200).
    #[serde(default)]
    pub api_url: String,
    /// WebSocket URL (e.g., ws://localhost:3200/ws).
    #[serde(default)]
    pub ws_url: String,
    /// JWT secret shared with Rift server for agent token issuance.
    #[serde(default)]
    pub jwt_secret: String,
    /// Dedicated bearer secret for bridge-only Rift routes.
    #[serde(default)]
    pub bridge_secret: String,
    /// Rift server ID to join agents to.
    #[serde(default)]
    pub server_id: Uuid,
    /// Channel ID for the team room.
    #[serde(default)]
    pub channel_id: Uuid,
    /// How often to poll bridge pause status in seconds.
    pub pause_poll_secs: Option<u64>,
}

/// Bridge daemon settings.
#[derive(Debug, Deserialize, Clone)]
pub struct BridgeDaemonConfig {
    /// Minimum seconds between two posts by the SAME agent (hard pacing
    /// floor, enforced during round planning).
    pub cooldown_secs: u64,
    /// Maximum turns per agent per topic before budget exhaustion.
    pub turn_budget: u32,
    /// Hard ceiling on total turns per conversation thread.
    pub thread_ceiling: u32,
    /// Number of recent messages to include in context.
    pub context_window: usize,
    /// Deprecated shared jitter range. Parsed for config compatibility but
    /// no longer used because slot
    /// geometry below replaces it.
    #[serde(default = "default_jitter_range_ms")]
    pub jitter_range_ms: (u64, u64),
    /// Width of each agent's compose slot in milliseconds.
    #[serde(default = "default_slot_width_ms")]
    pub slot_width_ms: u64,
    /// Jitter drawn inside an agent's own slot, in milliseconds. Must stay
    /// below `slot_width_ms`; the turn manager clamps it if not.
    #[serde(default = "default_slot_jitter_ms")]
    pub slot_jitter_ms: u64,
    /// Maximum agent-to-agent cascade rounds a single inbound message may
    /// trigger (each round is bounded by budgets and the thread ceiling).
    #[serde(default = "default_max_cascade_rounds")]
    pub max_cascade_rounds: u32,
    /// Token-overlap similarity at or above which a candidate response is
    /// suppressed as a cross-agent echo.
    #[serde(default = "default_echo_similarity_threshold")]
    pub echo_similarity_threshold: f64,
    /// Per-peer-response probability multiplier for agents not directly
    /// addressed.
    #[serde(default = "default_peer_response_damp")]
    pub peer_response_damp: f64,
}

/// Default legacy jitter range (unused at runtime, kept for parse compatibility).
fn default_jitter_range_ms() -> (u64, u64) {
    (2000, 8000)
}

/// Default compose slot width for six-second cascading windows.
fn default_slot_width_ms() -> u64 {
    6000
}

/// Default in-slot jitter: 4s of natural feel inside a 6s window, leaving a
/// 2s guard gap between consecutive slots.
fn default_slot_jitter_ms() -> u64 {
    4000
}

/// Default cascade round cap per inbound message.
fn default_max_cascade_rounds() -> u32 {
    8
}

/// Default echo suppression threshold (Jaccard over content tokens).
fn default_echo_similarity_threshold() -> f64 {
    0.5
}

/// Default peer-response damping multiplier.
fn default_peer_response_damp() -> f64 {
    0.4
}

/// Provides default daemon settings used when callers construct configs programmatically.
impl Default for BridgeDaemonConfig {
    /// Sensible defaults for the bridge daemon.
    fn default() -> Self {
        Self {
            cooldown_secs: 30,
            turn_budget: 5,
            thread_ceiling: 30,
            context_window: 50,
            jitter_range_ms: default_jitter_range_ms(),
            slot_width_ms: default_slot_width_ms(),
            slot_jitter_ms: default_slot_jitter_ms(),
            max_cascade_rounds: default_max_cascade_rounds(),
            echo_similarity_threshold: default_echo_similarity_threshold(),
            peer_response_damp: default_peer_response_damp(),
        }
    }
}

/// Configuration for a single agent in the roster.
#[derive(Debug, Deserialize, Clone)]
pub struct AgentConfig {
    /// Display name for the agent in Rift.
    pub name: String,
    /// Username for Rift registration (must be unique).
    pub username: String,
    /// Which executor to use for this agent.
    pub executor: ExecutorConfig,
    /// Base response probability (0.0 to 1.0).
    pub base_chance: f64,
    /// System prompt preamble for this agent.
    pub system_prompt: String,
    /// Runtime-only credential mediation selected by managed materialization.
    #[serde(skip)]
    pub execution_mode: ResolvedExecutionMode,
}

/// Executor backend configuration (tagged union in TOML).
#[derive(Debug, Deserialize, Clone)]
#[serde(tag = "type")]
pub enum ExecutorConfig {
    /// Run any external agent CLI as the harness.
    ///
    /// The escape hatch that keeps Henosis from being tied to one agent: the
    /// operator names a binary and the argument templates for a discussion turn
    /// and an execution session, and `{prompt}` is substituted as a single
    /// argument. Anything that takes a prompt and answers on stdout works.
    Command {
        /// Path to the harness binary, or a bare name resolved against PATH.
        binary: PathBuf,
        /// Argument template for a discussion turn; `{prompt}` is substituted.
        #[serde(default)]
        discuss_args: Vec<String>,
        /// Argument template for an execution session; `{prompt}` is substituted.
        #[serde(default)]
        execute_args: Vec<String>,
        /// Working directory for discussion turns. Execution uses the sandbox.
        cwd: Option<PathBuf>,
        /// Wall-clock ceiling per session, enforced by killing the process.
        max_runtime_secs: Option<u64>,
        /// Set to "jsonl" when the harness emits newline-delimited JSON progress.
        progress_format: Option<crate::executors::ProgressFormat>,
        /// Extra environment entries handed to the harness.
        #[serde(default)]
        env: std::collections::BTreeMap<String, String>,
        /// Ambient environment names explicitly inherited by the harness.
        #[serde(default)]
        inherit_env: Vec<String>,
        /// Start the harness from an empty environment instead of inheriting.
        #[serde(default = "default_env_clear")]
        env_clear: bool,
    },
    /// Shell out to `claude -p`. Lightweight, no tool access.
    ClaudeCode {
        /// Absolute path to claude binary.
        binary: PathBuf,
        /// Model to use (e.g., "sonnet").
        model: Option<String>,
        /// Legacy value retained for TOML compatibility; current Claude CLI ignores it.
        max_tokens: Option<u32>,
    },
    /// Invoke the Codex CLI with explicit sandboxing and JSONL output.
    Codex {
        /// Absolute path to the Codex binary.
        binary: PathBuf,
        /// Model identifier passed to `codex exec`.
        model: String,
        /// Optional model reasoning-effort override.
        reasoning_effort: Option<String>,
    },
    /// Full Synapse agent loop with provider + tool access.
    Synapse {
        /// Provider backend: "foundry-anthropic", "foundry-openai", or "anthropic".
        provider: String,
        /// Model identifier (e.g., "claude-sonnet-4-6").
        model: Option<String>,
        /// Foundry host (required for foundry-* providers).
        host: Option<String>,
        /// Foundry API token (required for foundry-* providers).
        /// In production, load via cred -- this field supports env var expansion.
        token: Option<String>,
        /// Anthropic API key (required for "anthropic" provider).
        api_key: Option<String>,
        /// Max tokens per response.
        max_tokens: Option<u32>,
        /// Max agent loop turns per discussion invocation.
        max_turns: Option<usize>,
        /// Working directory for tool execution.
        cwd: Option<PathBuf>,
    },
}

/// Implements loading and parsing for bridge configuration files.
impl BridgeConfig {
    /// Load configuration from a TOML file at the given path.
    pub fn load(path: &std::path::Path) -> Result<Self, BridgeError> {
        let config = Self::parse(path)?;
        config.validate_security()?;
        config.validate_rift_target()?;
        Ok(config)
    }

    /// Load agent behavior while Henosis supplies the managed Rift connection.
    pub fn load_for_managed_room(
        path: &std::path::Path,
        api_url: String,
        ws_url: String,
        jwt_secret: String,
        bridge_secret: String,
        server_id: Uuid,
        channel_id: Uuid,
    ) -> Result<Self, BridgeError> {
        let mut config = Self::parse(path)?;
        config.rift.api_url = api_url;
        config.rift.ws_url = ws_url;
        config.rift.jwt_secret = jwt_secret;
        config.rift.bridge_secret = bridge_secret;
        config.rift.server_id = server_id;
        config.rift.channel_id = channel_id;
        config.validate_security()?;
        config.validate_rift_target()?;
        Ok(config)
    }

    /// Export the ordered agent roster as deterministic recovery TOML.
    ///
    /// The export retains operator-authored prompts, paths, and argument
    /// templates required to reconstruct the roster, so callers must protect it
    /// as configuration data. Rift connection state, runtime-only credential
    /// mediation, command environment values, and provider connection or
    /// credential fields are intentionally absent; recovery tooling injects
    /// deployment-owned connection and credential state separately.
    pub fn export_roster_toml(&self) -> Result<String, BridgeError> {
        let agents = self
            .agents
            .iter()
            .map(export_agent)
            .collect::<Result<Vec<_>, _>>()?;
        let mut root = toml::map::Map::new();
        root.insert("agents".to_string(), toml::Value::Array(agents));
        toml::to_string(&toml::Value::Table(root)).map_err(|error| {
            BridgeError::Config(format!("failed to export recovery roster: {error}"))
        })
    }

    /// Parse one bridge file before applying deployment-owned connection data.
    fn parse(path: &std::path::Path) -> Result<Self, BridgeError> {
        let content = std::fs::read_to_string(path).map_err(|e| {
            BridgeError::Config(format!("failed to read {}: {}", path.display(), e))
        })?;
        toml::from_str(&content)
            .map_err(|e| BridgeError::Config(format!("failed to parse {}: {}", path.display(), e)))
    }

    /// Reject unsafe listener and secret settings before the bridge connects.
    fn validate_security(&self) -> Result<(), BridgeError> {
        if self.rift.jwt_secret.len() < 32 || self.rift.bridge_secret.len() < 32 {
            return Err(BridgeError::Config(
                "rift.jwt_secret and rift.bridge_secret must each contain at least 32 bytes"
                    .to_string(),
            ));
        }
        if self.rift.jwt_secret == self.rift.bridge_secret {
            return Err(BridgeError::Config(
                "rift.jwt_secret and rift.bridge_secret must differ".to_string(),
            ));
        }
        if let Some(control) = &self.control {
            control.validate(&[&self.rift.jwt_secret, &self.rift.bridge_secret])?;
        }
        for agent in &self.agents {
            if let ExecutorConfig::Command {
                binary,
                discuss_args,
                execute_args,
                max_runtime_secs,
                env,
                inherit_env,
                ..
            } = &agent.executor
            {
                if binary.as_os_str().is_empty() {
                    return Err(BridgeError::Config(format!(
                        "command executor for {} requires a binary",
                        agent.username
                    )));
                }
                validate_command_template(&agent.username, "discuss_args", discuss_args)?;
                validate_command_template(&agent.username, "execute_args", execute_args)?;
                if discuss_args == execute_args {
                    return Err(BridgeError::Config(format!(
                        "command executor for {} must use distinct discussion and execution modes",
                        agent.username
                    )));
                }
                if *max_runtime_secs == Some(0) {
                    return Err(BridgeError::Config(format!(
                        "command executor for {} requires max_runtime_secs greater than zero",
                        agent.username
                    )));
                }
                for name in inherit_env.iter().chain(env.keys()) {
                    if !valid_command_env_name(name) {
                        return Err(BridgeError::Config(format!(
                            "command executor for {} has invalid environment name {name:?}",
                            agent.username
                        )));
                    }
                }
            }
        }
        Ok(())
    }

    /// Reject missing connection coordinates on every runnable configuration.
    fn validate_rift_target(&self) -> Result<(), BridgeError> {
        if self.rift.api_url.trim().is_empty() || self.rift.ws_url.trim().is_empty() {
            return Err(BridgeError::Config(
                "rift.api_url and rift.ws_url must be configured".to_string(),
            ));
        }
        if self.rift.server_id.is_nil() || self.rift.channel_id.is_nil() {
            return Err(BridgeError::Config(
                "rift.server_id and rift.channel_id must be configured".to_string(),
            ));
        }
        Ok(())
    }
}

/// Convert one agent while excluding runtime-only credential mediation.
fn export_agent(agent: &AgentConfig) -> Result<toml::Value, BridgeError> {
    let mut table = toml::map::Map::new();
    table.insert("name".to_string(), toml::Value::String(agent.name.clone()));
    table.insert(
        "username".to_string(),
        toml::Value::String(agent.username.clone()),
    );
    table.insert(
        "base_chance".to_string(),
        toml::Value::Float(agent.base_chance),
    );
    table.insert(
        "system_prompt".to_string(),
        toml::Value::String(agent.system_prompt.clone()),
    );
    table.insert("executor".to_string(), export_executor(&agent.executor)?);
    Ok(toml::Value::Table(table))
}

/// Convert one executor while omitting explicit environment and provider secret fields.
fn export_executor(executor: &ExecutorConfig) -> Result<toml::Value, BridgeError> {
    let mut table = toml::map::Map::new();
    match executor {
        ExecutorConfig::Command {
            binary,
            discuss_args,
            execute_args,
            cwd,
            max_runtime_secs,
            progress_format,
            inherit_env,
            env_clear,
            ..
        } => {
            table.insert(
                "type".to_string(),
                toml::Value::String("Command".to_string()),
            );
            insert_path(&mut table, "binary", binary)?;
            table.insert("discuss_args".to_string(), string_array(discuss_args));
            table.insert("execute_args".to_string(), string_array(execute_args));
            if let Some(cwd) = cwd {
                insert_path(&mut table, "cwd", cwd)?;
            }
            if let Some(seconds) = max_runtime_secs {
                table.insert(
                    "max_runtime_secs".to_string(),
                    checked_integer("max_runtime_secs", *seconds)?,
                );
            }
            if let Some(format) = progress_format {
                let value = match format {
                    crate::executors::ProgressFormat::Text => "text",
                    crate::executors::ProgressFormat::Jsonl => "jsonl",
                };
                table.insert(
                    "progress_format".to_string(),
                    toml::Value::String(value.to_string()),
                );
            }
            table.insert("inherit_env".to_string(), string_array(inherit_env));
            table.insert("env_clear".to_string(), toml::Value::Boolean(*env_clear));
        }
        ExecutorConfig::ClaudeCode {
            binary,
            model,
            max_tokens,
        } => {
            table.insert(
                "type".to_string(),
                toml::Value::String("ClaudeCode".to_string()),
            );
            insert_path(&mut table, "binary", binary)?;
            insert_optional_string(&mut table, "model", model);
            if let Some(max_tokens) = max_tokens {
                table.insert(
                    "max_tokens".to_string(),
                    toml::Value::Integer(i64::from(*max_tokens)),
                );
            }
        }
        ExecutorConfig::Codex {
            binary,
            model,
            reasoning_effort,
        } => {
            table.insert("type".to_string(), toml::Value::String("Codex".to_string()));
            insert_path(&mut table, "binary", binary)?;
            table.insert("model".to_string(), toml::Value::String(model.clone()));
            insert_optional_string(&mut table, "reasoning_effort", reasoning_effort);
        }
        ExecutorConfig::Synapse {
            provider,
            model,
            max_tokens,
            max_turns,
            cwd,
            ..
        } => {
            table.insert(
                "type".to_string(),
                toml::Value::String("Synapse".to_string()),
            );
            table.insert(
                "provider".to_string(),
                toml::Value::String(provider.clone()),
            );
            insert_optional_string(&mut table, "model", model);
            if let Some(max_tokens) = max_tokens {
                table.insert(
                    "max_tokens".to_string(),
                    toml::Value::Integer(i64::from(*max_tokens)),
                );
            }
            if let Some(max_turns) = max_turns {
                table.insert(
                    "max_turns".to_string(),
                    checked_integer("max_turns", *max_turns)?,
                );
            }
            if let Some(cwd) = cwd {
                insert_path(&mut table, "cwd", cwd)?;
            }
        }
    }
    Ok(toml::Value::Table(table))
}

/// Insert one Unicode path into a recovery table without lossy conversion.
fn insert_path(
    table: &mut toml::map::Map<String, toml::Value>,
    key: &str,
    path: &std::path::Path,
) -> Result<(), BridgeError> {
    let value = path.to_str().ok_or_else(|| {
        BridgeError::Config(format!(
            "failed to export recovery roster: {key} is not valid Unicode"
        ))
    })?;
    table.insert(key.to_string(), toml::Value::String(value.to_string()));
    Ok(())
}

/// Insert an optional string while preserving TOML's absent-value semantics.
fn insert_optional_string(
    table: &mut toml::map::Map<String, toml::Value>,
    key: &str,
    value: &Option<String>,
) {
    if let Some(value) = value {
        table.insert(key.to_string(), toml::Value::String(value.clone()));
    }
}

/// Convert an ordered list of strings into a TOML array.
fn string_array(values: &[String]) -> toml::Value {
    toml::Value::Array(values.iter().cloned().map(toml::Value::String).collect())
}

/// Convert an unsigned recovery value into TOML's signed integer domain.
fn checked_integer(field: &str, value: impl TryInto<i64>) -> Result<toml::Value, BridgeError> {
    value.try_into().map(toml::Value::Integer).map_err(|_| {
        BridgeError::Config(format!(
            "failed to export recovery roster: {field} exceeds TOML integer range"
        ))
    })
}

/// Keep generic harnesses isolated from ambient process credentials unless explicitly overridden.
fn default_env_clear() -> bool {
    true
}

/// Accept environment names that cannot make process construction panic.
fn valid_command_env_name(name: &str) -> bool {
    !name.is_empty()
        && !name
            .chars()
            .any(|character| matches!(character, '=' | '\0'))
}

/// Validate one external-harness argument template at the configuration boundary.
fn validate_command_template(
    username: &str,
    field: &str,
    template: &[String],
) -> Result<(), BridgeError> {
    let placeholders = template
        .iter()
        .filter(|arg| arg.as_str() == "{prompt}")
        .count();
    if template.is_empty() || placeholders != 1 {
        return Err(BridgeError::Config(format!(
            "command executor {field} for {username} must contain exactly one whole-element {{prompt}} placeholder"
        )));
    }
    Ok(())
}

/// A repository an approved task may execute against.
#[derive(Debug, Deserialize, Clone)]
pub struct WorkspaceConfig {
    /// Logical workspace name, matched against the project name.
    pub name: String,
    /// Absolute path to the repository working tree.
    pub path: PathBuf,
    /// Optional CARGO_TARGET_DIR to keep build artifacts off the source tree.
    pub cargo_target_dir: Option<PathBuf>,
}

/// Execution-mode runtime settings.
#[derive(Debug, Deserialize, Clone)]
pub struct ExecutionSettings {
    /// Root directory under which per-task git worktrees are created.
    pub worktrees_root: PathBuf,
    /// Maximum number of simultaneous execution sessions.
    #[serde(default = "default_max_concurrent")]
    pub max_concurrent_executions: usize,
    /// Seconds a pending approval lives before it expires.
    #[serde(default = "default_approval_ttl")]
    pub approval_ttl_secs: u64,
    /// Wall-clock limit per execution session in seconds (0 = no limit).
    #[serde(default = "default_exec_timeout")]
    pub max_runtime_secs: u64,
}

/// Optional settings for the HTTP-backed Pistis capability oracle.
#[derive(Debug, Deserialize, Clone)]
pub struct PistisConfig {
    /// Whether the bridge should prefer the Pistis-backed oracle.
    pub enabled: bool,
    /// Base URL for the Pistis orchestrator API.
    pub orchestrator_url: String,
    /// cred reference for the orchestrator bearer token.
    pub auth_token_cred: String,
    /// Matrix room id or alias the oracle should query.
    pub room: String,
}

/// Default simultaneous execution sessions.
fn default_max_concurrent() -> usize {
    1
}

/// Default approval time-to-live in seconds.
fn default_approval_ttl() -> u64 {
    1800
}

/// Default per-session wall-clock limit in seconds.
fn default_exec_timeout() -> u64 {
    1800
}

/// Control HTTP server settings.
#[derive(Debug, Deserialize, Clone)]
pub struct ControlConfig {
    /// Address to bind the control server to (e.g., 127.0.0.1:3210).
    pub bind_addr: String,
    /// Bearer token required on control requests.
    pub auth_token: String,
    /// Whether a non-loopback bind is explicitly acknowledged as externally protected.
    #[serde(default)]
    pub allow_insecure_remote: bool,
}

/// Implements fail-closed validation for the execution-approval boundary.
impl ControlConfig {
    /// Validate the listener and token, returning the parsed bind address.
    pub fn validate(&self, reserved_secrets: &[&str]) -> Result<SocketAddr, BridgeError> {
        let bind_addr = self.bind_addr.parse::<SocketAddr>().map_err(|error| {
            BridgeError::Config(format!(
                "invalid control.bind_addr {:?}: {error}",
                self.bind_addr
            ))
        })?;
        if !bind_addr.ip().is_loopback() && !self.allow_insecure_remote {
            return Err(BridgeError::Config(format!(
                "refusing non-loopback control.bind_addr {bind_addr}; set control.allow_insecure_remote=true only behind a trusted TLS boundary"
            )));
        }
        if !(32..=256).contains(&self.auth_token.len()) {
            return Err(BridgeError::Config(
                "control.auth_token must contain 32 to 256 bytes".to_string(),
            ));
        }
        if !self.auth_token.is_ascii()
            || self.auth_token.trim() != self.auth_token
            || self.auth_token.chars().any(char::is_whitespace)
        {
            return Err(BridgeError::Config(
                "control.auth_token must contain only non-whitespace ASCII characters".to_string(),
            ));
        }
        if reserved_secrets.contains(&self.auth_token.as_str()) {
            return Err(BridgeError::Config(
                "control.auth_token must differ from Rift JWT and bridge secrets".to_string(),
            ));
        }
        Ok(bind_addr)
    }
}

#[cfg(test)]
/// Unit tests for bridge configuration parsing.
mod tests {
    use super::BridgeConfig;
    use uuid::Uuid;

    /// Verifies the new execution-mode config blocks parse and are optional.
    #[test]
    fn test_execution_config_blocks_parse() {
        let toml = r#"
            [rift]
            api_url = "http://localhost:3200"
            ws_url = "ws://localhost:3200/ws"
            jwt_secret = "secret"
            bridge_secret = "bridge-secret"
            server_id = "00000000-0000-0000-0000-000000000001"
            channel_id = "00000000-0000-0000-0000-000000000002"

            [bridge]
            cooldown_secs = 30
            turn_budget = 5
            thread_ceiling = 30
            context_window = 50
            jitter_range_ms = [2000, 8000]

            [capabilities]
            architect = ["fs_read", "fs_write", "bash"]

            [[workspaces]]
            name = "rift"
            path = "/tmp/rift"

            [execution]
            worktrees_root = "/tmp/rift/.worktrees"
            max_concurrent_executions = 1
            approval_ttl_secs = 1800

            [control]
            bind_addr = "127.0.0.1:3210"
            auth_token = "set-me"

            [[agents]]
            name = "Architect"
            username = "architect"
            base_chance = 0.5
            system_prompt = "You are an architect."
            executor = { type = "ClaudeCode", binary = "/usr/bin/claude" }
        "#;

        let config: BridgeConfig = toml::from_str(toml).expect("config should parse");
        assert_eq!(config.capabilities.get("architect").unwrap().len(), 3);
        assert_eq!(config.workspaces[0].name, "rift");
        assert_eq!(
            config.execution.as_ref().unwrap().max_concurrent_executions,
            1
        );
        assert!(config.pistis.is_none());
        assert_eq!(config.control.as_ref().unwrap().bind_addr, "127.0.0.1:3210");
    }

    /// Verifies the control boundary accepts a strong token on loopback.
    #[test]
    fn control_config_accepts_safe_loopback_settings() {
        let control = super::ControlConfig {
            bind_addr: "[::1]:3210".to_string(),
            auth_token: "control-token-that-is-at-least-32-bytes".to_string(),
            allow_insecure_remote: false,
        };
        assert!(control
            .validate(&["another-strong-secret-value-here"])
            .is_ok());
    }

    /// Verifies remote listeners and unsafe tokens fail closed.
    #[test]
    fn control_config_rejects_unsafe_boundary_settings() {
        let token = "control-token-that-is-at-least-32-bytes".to_string();
        let mut control = super::ControlConfig {
            bind_addr: "0.0.0.0:3210".to_string(),
            auth_token: token.clone(),
            allow_insecure_remote: false,
        };
        assert!(control.validate(&[]).is_err());
        control.allow_insecure_remote = true;
        assert!(control.validate(&[]).is_ok());
        control.bind_addr = "127.0.0.1:3210".to_string();
        control.auth_token = "weak".to_string();
        assert!(control.validate(&[]).is_err());
        control.auth_token = "token with whitespace that is long enough".to_string();
        assert!(control.validate(&[]).is_err());
        control.auth_token = "é".repeat(32);
        assert!(control.validate(&[]).is_err());
        control.auth_token = token.clone();
        assert!(control.validate(&[&token]).is_err());
    }

    /// Verifies a legacy config without the new blocks still parses.
    #[test]
    fn test_legacy_config_without_execution_blocks_parses() {
        let toml = r#"
            [rift]
            api_url = "http://localhost:3200"
            ws_url = "ws://localhost:3200/ws"
            jwt_secret = "secret"
            bridge_secret = "bridge-secret"
            server_id = "00000000-0000-0000-0000-000000000001"
            channel_id = "00000000-0000-0000-0000-000000000002"

            [bridge]
            cooldown_secs = 30
            turn_budget = 5
            thread_ceiling = 30
            context_window = 50
            jitter_range_ms = [2000, 8000]

            [[agents]]
            name = "Architect"
            username = "architect"
            base_chance = 0.5
            system_prompt = "You are an architect."
            executor = { type = "ClaudeCode", binary = "/usr/bin/claude" }
        "#;

        let config: BridgeConfig = toml::from_str(toml).expect("legacy config should parse");
        assert!(config.capabilities.is_empty());
        assert!(config.workspaces.is_empty());
        assert!(config.execution.is_none());
        assert!(config.pistis.is_none());
        assert!(config.control.is_none());
        // Legacy configs predate the slot/cascade/echo knobs: defaults apply.
        assert_eq!(config.bridge.slot_width_ms, 6000);
        assert_eq!(config.bridge.slot_jitter_ms, 4000);
        assert_eq!(config.bridge.max_cascade_rounds, 8);
        assert!((config.bridge.echo_similarity_threshold - 0.5).abs() < 1e-9);
        assert!((config.bridge.peer_response_damp - 0.4).abs() < 1e-9);
        // Legacy configs also predate the embedding and stimulus blocks:
        // both must stay absent, preserving pre-embedding behavior.
        assert!(config.embedding.is_none());
        assert!(config.stimulus.is_none());
    }

    /// Verifies the embedding and stimulus blocks parse with defaults filled.
    #[test]
    fn test_embedding_and_stimulus_blocks_parse() {
        let toml = r#"
            [rift]
            api_url = "http://localhost:3200"
            ws_url = "ws://localhost:3200/ws"
            jwt_secret = "secret"
            bridge_secret = "bridge-secret"
            server_id = "00000000-0000-0000-0000-000000000001"
            channel_id = "00000000-0000-0000-0000-000000000002"

            [bridge]
            cooldown_secs = 30
            turn_budget = 5
            thread_ceiling = 30
            context_window = 50

            [embedding]
            url = "http://127.0.0.1:11434/v1/embeddings"
            model = "bge-m3"

            [stimulus]
            enabled = true
            poll_secs = 30

            [[agents]]
            name = "Architect"
            username = "architect"
            base_chance = 0.5
            system_prompt = "You are an architect."
            executor = { type = "ClaudeCode", binary = "/usr/bin/claude" }
        "#;

        let config: BridgeConfig = toml::from_str(toml).expect("config should parse");
        let emb = config.embedding.expect("embedding block");
        assert_eq!(
            emb.url.as_deref(),
            Some("http://127.0.0.1:11434/v1/embeddings")
        );
        assert_eq!(emb.model, "bge-m3");
        assert!(emb.api_key_env.is_none());
        assert!((emb.semantic_threshold - 0.85).abs() < 1e-9);
        assert!((emb.reignition_threshold - 0.85).abs() < 1e-9);
        assert!((emb.reignition_damp - 0.3).abs() < 1e-9);
        assert_eq!(emb.reignition_ttl_secs, 21600);
        let stim = config.stimulus.expect("stimulus block");
        assert!(stim.enabled);
        assert_eq!(stim.poll_secs, 30);
        assert_eq!(stim.reflection_after_secs, 14400);
        assert_eq!(stim.chiasm_cooldown_secs, 900);
        assert_eq!(stim.git_cooldown_secs, 300);
        assert_eq!(stim.axon_cooldown_secs, 600);
        assert_eq!(stim.loom_cooldown_secs, 600);
        assert_eq!(stim.max_per_hour, 6);
    }

    /// Verifies an embedding block without a URL selects the in-process-ready
    /// defaults instead of requiring a fake endpoint value.
    #[test]
    fn test_embedding_block_without_url_uses_local_defaults() {
        let toml = r#"
            [rift]
            api_url = "http://localhost:3200"
            ws_url = "ws://localhost:3200/ws"
            jwt_secret = "secret"
            bridge_secret = "bridge-secret"
            server_id = "00000000-0000-0000-0000-000000000001"
            channel_id = "00000000-0000-0000-0000-000000000002"

            [bridge]
            cooldown_secs = 30
            turn_budget = 5
            thread_ceiling = 30
            context_window = 50

            [embedding]

            [[agents]]
            name = "Architect"
            username = "architect"
            base_chance = 0.5
            system_prompt = "You are an architect."
            executor = { type = "ClaudeCode", binary = "/usr/bin/claude" }
        "#;

        let config: BridgeConfig = toml::from_str(toml).expect("config should parse");
        let emb = config.embedding.expect("embedding block");
        assert!(emb.url.is_none());
        assert_eq!(emb.model, "bge-m3");
        assert!((emb.semantic_threshold - 0.85).abs() < 1e-9);
    }

    /// Managed loading supplies every connection coordinate without TOML placeholders.
    #[test]
    fn managed_room_config_needs_only_agent_behavior() {
        let path = std::env::temp_dir().join(format!(
            "henosis-managed-room-config-{}.toml",
            Uuid::new_v4()
        ));
        std::fs::write(
            &path,
            r#"
                [bridge]
                cooldown_secs = 30
                turn_budget = 5
                thread_ceiling = 30
                context_window = 50

                [[agents]]
                name = "Architect"
                username = "architect"
                base_chance = 0.5
                system_prompt = "You are an architect."
                executor = { type = "ClaudeCode", binary = "/usr/bin/claude" }
            "#,
        )
        .expect("write temporary managed config");
        let server_id = Uuid::new_v4();
        let channel_id = Uuid::new_v4();
        let config = BridgeConfig::load_for_managed_room(
            &path,
            "http://127.0.0.1:3200".to_string(),
            "ws://127.0.0.1:3200/ws".to_string(),
            "j".repeat(32),
            "b".repeat(32),
            server_id,
            channel_id,
        )
        .expect("managed configuration");
        std::fs::remove_file(&path).expect("remove temporary managed config");
        assert_eq!(config.rift.server_id, server_id);
        assert_eq!(config.rift.channel_id, channel_id);
        assert_eq!(config.agents.len(), 1);
    }

    /// A generated roster can omit bridge tuning and still defaults to an empty child environment.
    #[test]
    fn managed_command_roster_uses_safe_defaults() {
        let parsed: BridgeConfig = toml::from_str(
            r#"
                [[agents]]
                name = "Adapter"
                username = "adapter"
                base_chance = 1.0
                system_prompt = "Discuss safely."

                executor = { type = "Command", binary = "/opt/bin/adapter", discuss_args = ["--henosis-discuss", "{prompt}"], execute_args = ["--henosis-execute", "{prompt}"] }
            "#,
        )
        .expect("generated roster parses");

        assert_eq!(parsed.bridge.turn_budget, 5);
        match &parsed.agents[0].executor {
            super::ExecutorConfig::Command {
                env_clear,
                inherit_env,
                ..
            } => {
                assert!(*env_clear);
                assert!(inherit_env.is_empty());
            }
            _ => panic!("expected command executor"),
        }
    }

    /// A command harness cannot reuse its execution mode for ordinary discussion turns.
    #[test]
    fn managed_command_roster_rejects_identical_modes() {
        let path = std::env::temp_dir().join(format!(
            "henosis-managed-command-config-{}.toml",
            Uuid::new_v4()
        ));
        std::fs::write(
            &path,
            r#"
                [[agents]]
                name = "Unsafe"
                username = "unsafe"
                base_chance = 1.0
                system_prompt = "Unsafe fixture."
                executor = { type = "Command", binary = "codex", discuss_args = ["exec", "{prompt}"], execute_args = ["exec", "{prompt}"] }
            "#,
        )
        .expect("write temporary managed config");
        let result = BridgeConfig::load_for_managed_room(
            &path,
            "http://127.0.0.1:3200".to_string(),
            "ws://127.0.0.1:3200/ws".to_string(),
            "j".repeat(32),
            "b".repeat(32),
            Uuid::new_v4(),
            Uuid::new_v4(),
        );
        std::fs::remove_file(&path).expect("remove temporary managed config");

        assert!(
            matches!(result, Err(super::BridgeError::Config(message)) if message.contains("distinct discussion and execution modes"))
        );
    }

    /// Command timeouts and inherited environment names fail at config load, not process spawn.
    #[test]
    fn managed_command_roster_rejects_invalid_process_settings() {
        let executor_type_key = "type";
        let invalid = [
            ("max_runtime_secs = 0", "max_runtime_secs greater than zero"),
            ("inherit_env = [\"BAD=NAME\"]", "invalid environment name"),
        ];

        for (setting, expected) in invalid {
            let path = std::env::temp_dir().join(format!(
                "henosis-invalid-command-config-{}.toml",
                Uuid::new_v4()
            ));
            std::fs::write(
                &path,
                format!(
                    r#"
                        [[agents]]
                        name = "Invalid"
                        username = "invalid"
                        base_chance = 1.0
                        system_prompt = "Invalid fixture."

                        [agents.executor]
                        {executor_type_key} = "Command"
                        binary = "adapter"
                        discuss_args = ["--henosis-discuss", "{{prompt}}"]
                        execute_args = ["--henosis-execute", "{{prompt}}"]
                        {setting}
                    "#
                ),
            )
            .expect("write invalid managed config");
            let result = BridgeConfig::load_for_managed_room(
                &path,
                "http://127.0.0.1:3200".to_string(),
                "ws://127.0.0.1:3200/ws".to_string(),
                "j".repeat(32),
                "b".repeat(32),
                Uuid::new_v4(),
                Uuid::new_v4(),
            );
            std::fs::remove_file(&path).expect("remove invalid managed config");

            assert!(
                matches!(result, Err(super::BridgeError::Config(message)) if message.contains(expected)),
                "setting {setting:?} must report {expected:?}"
            );
        }
    }

    /// Recovery export is stable, parseable, and omits explicit runtime secret fields.
    #[test]
    fn recovery_roster_export_is_deterministic_and_omits_runtime_secrets() {
        let config: BridgeConfig = toml::from_str(
            r#"
                [rift]
                jwt_secret = "rift-jwt-secret-value"
                bridge_secret = "rift-bridge-secret-value"

                [[agents]]
                name = "Adapter"
                username = "adapter"
                base_chance = 0.7
                system_prompt = "Use the adapter."

                [agents.executor]
                # Command executor fixture.
                "type" = "Command"
                binary = "/opt/bin/adapter"
                discuss_args = ["--discuss", "{prompt}"]
                execute_args = ["--execute", "{prompt}"]
                inherit_env = ["PATH"]
                env_clear = true

                [agents.executor.env]
                PRIVATE_VALUE = "command-environment-secret"

                [[agents]]
                name = "Synapse"
                username = "synapse"
                base_chance = 0.3
                system_prompt = "Use Synapse."

                [agents.executor]
                # Synapse executor fixture.
                "type" = "Synapse"
                provider = "anthropic"
                model = "claude-sonnet-4-6"
                host = "https://host-secret@example.invalid?token=host-secret"
                token = "provider-token-secret"
                api_key = "provider-api-key-secret"
                max_tokens = 4096
                max_turns = 4
            "#,
        )
        .expect("secret-bearing source roster parses");

        let first = config.export_roster_toml().expect("recovery export");
        let second = config.export_roster_toml().expect("repeat recovery export");
        assert_eq!(first, second);
        let parsed: BridgeConfig = toml::from_str(&first).expect("exported roster parses");
        assert_eq!(parsed.agents.len(), 2);
        assert_eq!(parsed.agents[0].username, "adapter");
        assert_eq!(parsed.agents[1].username, "synapse");
        assert!(first.contains("Use the adapter."));
        assert!(first.contains("/opt/bin/adapter"));
        assert!(first.contains("--discuss"));
        for excluded_value in [
            "rift-jwt-secret-value",
            "rift-bridge-secret-value",
            "command-environment-secret",
            "host-secret",
            "provider-token-secret",
            "provider-api-key-secret",
        ] {
            assert!(!first.contains(excluded_value));
        }
        assert!(!first.contains("PRIVATE_VALUE"));
        assert!(!first.contains("api_key"));
        assert!(!first.contains("token ="));
    }
}
