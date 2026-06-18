//! Bridge and agent roster configuration.

use serde::Deserialize;
use std::collections::HashMap;
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

/// In-process Kleos backend settings (Story 4.4). When `in_process` is true the
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
    /// Minimum delay between agent posts in seconds.
    pub cooldown_secs: u64,
    /// Maximum turns per agent per topic before budget exhaustion.
    pub turn_budget: u32,
    /// Hard ceiling on total turns per conversation thread.
    pub thread_ceiling: u32,
    /// Number of recent messages to include in context.
    pub context_window: usize,
    /// Delay range for jittered response timing (min_ms, max_ms).
    pub jitter_range_ms: (u64, u64),
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
            jitter_range_ms: (2000, 8000),
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
        toml::from_str(&content)
            .map_err(|e| BridgeError::Config(format!("failed to parse {}: {}", path.display(), e)))
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

    /// Verifies a legacy config without the new blocks still parses.
    #[test]
    fn test_legacy_config_without_execution_blocks_parses() {
        let toml = r#"
            [rift]
            api_url = "http://localhost:3200"
            ws_url = "ws://localhost:3200/ws"
            jwt_secret = "secret"
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
    }
}
