//! Bridge and agent roster configuration.

use serde::Deserialize;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use uuid::Uuid;

use crate::error::BridgeError;

/// Top-level bridge configuration loaded from TOML.
#[derive(Debug, Deserialize)]
pub struct BridgeConfig {
    /// Connection settings for the Rift server.
    pub rift: RiftConfig,
    /// Bridge daemon behavior settings.
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
#[derive(Debug, Deserialize)]
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
#[derive(Debug, Deserialize)]
pub struct RiftConfig {
    /// Rift server base URL (e.g., http://localhost:3200).
    pub api_url: String,
    /// WebSocket URL (e.g., ws://localhost:3200/ws).
    pub ws_url: String,
    /// JWT secret shared with Rift server for agent token issuance.
    pub jwt_secret: String,
    /// Dedicated bearer secret for bridge-only Rift routes.
    pub bridge_secret: String,
    /// Rift server ID to join agents to.
    pub server_id: Uuid,
    /// Channel ID for the team room.
    pub channel_id: Uuid,
    /// How often to poll bridge pause status in seconds.
    pub pause_poll_secs: Option<u64>,
}

/// Bridge daemon settings.
#[derive(Debug, Deserialize)]
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
#[derive(Debug, Deserialize)]
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
}

/// Executor backend configuration (tagged union in TOML).
#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
pub enum ExecutorConfig {
    /// Shell out to `claude -p`. Lightweight, no tool access.
    ClaudeCode {
        /// Absolute path to claude binary.
        binary: PathBuf,
        /// Model to use (e.g., "sonnet").
        model: Option<String>,
        /// Max tokens for response.
        max_tokens: Option<u32>,
    },
    /// Full Synapse agent loop with provider + tool access.
    Synapse {
        /// Provider backend: "foundry-anthropic", "foundry-openai", "claude-max", "anthropic".
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
        let content = std::fs::read_to_string(path).map_err(|e| {
            BridgeError::Config(format!("failed to read {}: {}", path.display(), e))
        })?;
        let config: Self = toml::from_str(&content).map_err(|e| {
            BridgeError::Config(format!("failed to parse {}: {}", path.display(), e))
        })?;
        config.validate_security()?;
        Ok(config)
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
        Ok(())
    }
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
#[derive(Debug, Deserialize)]
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
}
