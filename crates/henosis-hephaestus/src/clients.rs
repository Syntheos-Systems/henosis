//! Aggregated client bundle held in `AppState`. After the Phase 1 refactor
//! this file is a thin coordinator: the LLM loop lives in `orchestrator.rs`,
//! the provider implementations in `providers/`, and the coordination
//! services (Kleos, Chiasm, Axon, cred) in `services.rs`. `Clients` retains
//! its original method shapes so `tasks.rs` does not need to change in this
//! commit -- subsequent commits can wire `tasks.rs` directly to the
//! orchestrator and `Services` if desired.
//!
//! The two LLM entry points (`anthropic_complete` and `anthropic_resume`)
//! are now provider-agnostic despite their legacy names: they construct the
//! configured provider via the factory and call into `orchestrator::run` /
//! `orchestrator::resume`. The names remain to keep this commit focused on
//! the structural change; renaming to `complete` / `resume` is a follow-up.

use reqwest::Client;
use std::time::Duration;
use thiserror::Error;

use crate::agent_forge::AgentForgeClient;
use crate::anthropic_auth::{AuthError, ProviderChain};
use crate::checkpoint::Checkpoint;
use crate::config::Config;
use crate::gate::GateClient;
use crate::hermes_client::{HermesClient, ToolDef};
use crate::orchestrator::{self, OrchestratorError, OrchestratorResult};
use crate::services::Services;
use crate::streaming::StreamSink;
use crate::tasks::TaskRecord;

/// Error type surfaced to `tasks.rs`. Wraps the orchestrator's errors plus
/// the auth-chain failures the task layer needs to map to 503 at submission
/// time. Variants kept for backwards compatibility with existing call sites.
#[derive(Debug, Error)]
pub enum ClientError {
    /// Provider call failed -- network, non-2xx, parse error, etc. The
    /// `body` carries human-readable detail for logs and error responses.
    #[error("provider error ({status}): {body}")]
    Anthropic {
        /// HTTP status code (0 for non-HTTP failures).
        status: u16,
        /// Human-readable error detail.
        body: String,
    },
    /// Token resolution failed.
    #[error(transparent)]
    Auth(#[from] AuthError),
    /// Generic message from the orchestrator (loop exhaustion, internal
    /// bug, tool-dispatch parse failure, etc.).
    #[error("{0}")]
    Other(String),
}

impl From<OrchestratorError> for ClientError {
    /// Map orchestrator errors into the legacy `ClientError` shape so
    /// `tasks.rs` call sites remain unchanged.
    fn from(value: OrchestratorError) -> Self {
        match value {
            OrchestratorError::Provider(msg) => ClientError::Anthropic {
                status: 0,
                body: msg,
            },
            OrchestratorError::LoopExhausted(_) => ClientError::Other(value.to_string()),
        }
    }
}

/// Backwards-compatible alias. `tasks.rs` imports `AnthropicResult`; the
/// orchestrator's actual type is `OrchestratorResult`. They are the same.
pub type AnthropicResult = OrchestratorResult;

/// Aggregated client bundle. Owns config, provider auth chain, Hermes,
/// Eidolon gate, agent-forge wrapper, and the coordination services bundle.
/// Handed to handlers via `AppState`.
pub struct Clients {
    /// Runtime configuration.
    cfg: Config,
    /// Anthropic OAuth credential chain (Plutus -> credentials file).
    auth: ProviderChain,
    /// Hermes tool gateway client.
    hermes: HermesClient,
    /// Eidolon gate client.
    gate: GateClient,
    /// agent-forge CLI wrapper.
    agent_forge: AgentForgeClient,
    /// Coordination service bundle (Kleos, Chiasm, Axon, cred).
    services: Services,
}

impl Clients {
    /// Construct from a Config. Builds a single shared reqwest client used
    /// by every dependent client so the connection pool is unified across
    /// LLM, Hermes, gate checks, Kleos, Chiasm, and Axon.
    pub fn new(cfg: Config) -> Self {
        let http = Client::builder()
            .connect_timeout(Duration::from_secs(2))
            .timeout(cfg.llm_timeout)
            .build()
            .expect("reqwest client build");
        let auth = ProviderChain::from_config(&cfg);
        let hermes = HermesClient::new(&cfg.hermes_url, http.clone());
        let gate = GateClient::new(&cfg.eidolon_url, http.clone());
        let agent_forge =
            AgentForgeClient::new(cfg.agent_forge_bin.clone(), cfg.agent_forge_db.clone());
        let services = Services::new(http, cfg.clone());
        Self {
            cfg,
            auth,
            hermes,
            gate,
            agent_forge,
            services,
        }
    }

    /// Borrow the underlying `Config`.
    pub fn config(&self) -> &Config {
        &self.cfg
    }

    /// Borrow the agent-forge client.
    pub fn agent_forge(&self) -> &AgentForgeClient {
        &self.agent_forge
    }

    /// Borrow the Services bundle.
    pub fn services(&self) -> &Services {
        &self.services
    }

    /// Resolve an Anthropic OAuth bearer token for the current tenant. Used
    /// at task submission time to fail fast if no provider can mint a token.
    pub async fn anthropic_token(&self, tenant_id: Option<&str>) -> Result<String, AuthError> {
        self.auth.token(tenant_id).await
    }

    /// Forwarded to Services::cred_get.
    pub async fn cred_get(&self, slot: &str) -> Option<String> {
        self.services.cred_get(slot).await
    }

    /// Mirror a TaskRecord to Kleos. Forwarded to Services.
    pub async fn kleos_store_task(&self, task: &TaskRecord) {
        self.services.kleos_store_task(task).await;
    }

    /// Search Kleos for in-flight tasks at startup. Forwarded to Services.
    pub async fn kleos_recover_tasks(&self) -> Vec<serde_json::Value> {
        self.services.kleos_recover_tasks().await
    }

    /// Persist a per-turn checkpoint to Kleos. Forwarded to Services.
    pub async fn kleos_store_checkpoint(&self, cp: &Checkpoint) {
        self.services.kleos_store_checkpoint(cp).await;
    }

    /// Load the most recent checkpoint for a task. Forwarded to Services.
    pub async fn kleos_load_latest_checkpoint(&self, task_id: &str) -> Option<Checkpoint> {
        self.services.kleos_load_latest_checkpoint(task_id).await
    }

    /// Mirror the user/assistant thread to Kleos. Forwarded to Services.
    pub async fn kleos_store_thread(&self, session_id: &str, content: &str) {
        self.services.kleos_store_thread(session_id, content).await;
    }

    /// Create a Chiasm task. Forwarded to Services.
    pub async fn chiasm_create_task(
        &self,
        title: &str,
        summary: &str,
        project: &str,
    ) -> Option<i64> {
        self.services
            .chiasm_create_task(title, summary, project)
            .await
    }

    /// Submit a Chiasm task output. Forwarded to Services.
    pub async fn chiasm_submit_output(&self, chiasm_id: i64, output: &str) {
        self.services.chiasm_submit_output(chiasm_id, output).await;
    }

    /// Publish an event to Axon. Forwarded to Services.
    pub async fn axon_publish(&self, channel: &str, action: &str, payload: serde_json::Value) {
        self.services.axon_publish(channel, action, payload).await;
    }

    /// Run a fresh tool-use loop with the configured provider. Legacy name
    /// preserved; body provider-agnostic. `stream` is optional -- the
    /// orchestrator emits SSE events to it when present.
    #[allow(clippy::too_many_arguments)]
    pub async fn anthropic_complete(
        &self,
        tenant_id: Option<&str>,
        task_id: Option<&str>,
        prompt: &str,
        extra_system: Option<&str>,
        tools: &[ToolDef],
        max_turns: usize,
        stream: Option<&StreamSink>,
    ) -> Result<AnthropicResult, ClientError> {
        let provider = crate::providers::build_provider(
            &self.cfg,
            self.auth.clone(),
            self.services.http().clone(),
            &self.services,
            tenant_id.map(String::from),
        )
        .await
        .map_err(|e| ClientError::Other(format!("build_provider: {e}")))?;

        Ok(orchestrator::run(
            provider,
            &self.services,
            &self.hermes,
            &self.gate,
            &self.cfg,
            tenant_id,
            task_id,
            extra_system,
            tools,
            prompt,
            max_turns,
            stream,
        )
        .await?)
    }

    /// Resume a tool-use loop from an existing message history. Legacy name
    /// preserved; body provider-agnostic. `stream` is optional.
    #[allow(clippy::too_many_arguments)]
    pub async fn anthropic_resume(
        &self,
        tenant_id: Option<&str>,
        task_id: Option<&str>,
        extra_system: Option<&str>,
        messages: Vec<serde_json::Value>,
        tools: &[ToolDef],
        max_turns: usize,
        start_step: u32,
        stream: Option<&StreamSink>,
    ) -> Result<AnthropicResult, ClientError> {
        let provider = crate::providers::build_provider(
            &self.cfg,
            self.auth.clone(),
            self.services.http().clone(),
            &self.services,
            tenant_id.map(String::from),
        )
        .await
        .map_err(|e| ClientError::Other(format!("build_provider: {e}")))?;

        Ok(orchestrator::resume(
            provider,
            &self.services,
            &self.hermes,
            &self.gate,
            &self.cfg,
            tenant_id,
            task_id,
            extra_system,
            tools,
            messages,
            max_turns,
            start_step,
            stream,
        )
        .await?)
    }
}
