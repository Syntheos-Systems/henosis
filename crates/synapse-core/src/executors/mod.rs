//! Concrete `AgentExecutor` implementations shipped with Synapse.
//!
//! The public surface is `SynapseExecutor`, which backs the Synapse runtime.
//! Other executors (Hephaestus, ClaudeCode) live in their respective crates.

pub mod synapse_executor;

pub use synapse_executor::SynapseExecutor;
