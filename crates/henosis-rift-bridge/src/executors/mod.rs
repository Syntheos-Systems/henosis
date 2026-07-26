//! Executor implementations.

pub mod claude_code;
pub mod command;
pub mod synapse;

pub use claude_code::ClaudeCodeExecutor;
pub use command::{CommandExecutor, ProgressFormat};
pub use synapse::build_synapse_executor;
