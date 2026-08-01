//! Executor implementations.

use std::sync::Arc;

use crate::config::{AgentConfig, ExecutorConfig};
use crate::error::BridgeError;
use crate::executor::AgentExecutor;

pub mod claude_code;
pub mod codex;
pub mod synapse;

pub use claude_code::ClaudeCodeExecutor;
pub use codex::CodexExecutor;
pub use synapse::build_synapse_executor;

/// Construct one executor from a materialized agent configuration.
pub fn build_executor(config: &AgentConfig) -> Result<Arc<dyn AgentExecutor>, BridgeError> {
    match &config.executor {
        ExecutorConfig::ClaudeCode {
            binary,
            model,
            max_tokens,
        } => Ok(Arc::new(
            ClaudeCodeExecutor::new(binary.clone(), model.clone(), *max_tokens)
                .with_execution_mode(config.execution_mode.clone()),
        )),
        ExecutorConfig::Codex {
            binary,
            model,
            reasoning_effort,
        } => Ok(Arc::new(
            CodexExecutor::new(binary.clone(), model.clone(), reasoning_effort.clone())
                .with_execution_mode(config.execution_mode.clone()),
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
        } => build_synapse_executor(
            provider,
            model.clone(),
            host.clone(),
            token.clone(),
            api_key.clone(),
            *max_tokens,
            *max_turns,
            cwd.clone(),
        )
        .map(|executor| Arc::new(executor) as Arc<dyn AgentExecutor>)
        .map_err(|error| {
            BridgeError::Config(format!(
                "failed to build SynapseExecutor for {}: {error}",
                config.name
            ))
        }),
    }
}
