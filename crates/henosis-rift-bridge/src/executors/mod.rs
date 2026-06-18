//! Executor implementations.

pub mod claude_code;
pub mod synapse;

pub use claude_code::ClaudeCodeExecutor;
pub use synapse::build_synapse_executor;
