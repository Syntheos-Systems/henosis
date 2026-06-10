//! The presence-lifecycle events Soma publishes onto the Axon bus.
//!
//! They live here (a service crate) rather than in `syntheos-contracts` because they are Soma's
//! domain events, but they implement the contracts' [`TypedEvent`] trait so any in-process
//! reactor (narration, evaluation, supervision) can subscribe without depending on Soma.
//! Payloads carry identifying strings and coarse signal only -- never configuration bodies,
//! which may hold connection details that must not land on the ephemeral bus.

use serde::{Deserialize, Serialize};
use syntheos_contracts::TypedEvent;

/// The coarse channel every Soma presence event travels on.
pub const AGENT_CHANNEL: &str = "agent";

/// An agent registered (or re-registered) its presence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentRegistered {
    /// The agent's principal id.
    pub principal_id: String,
    /// Its working label.
    pub name: String,
    /// Its coarse category.
    pub agent_type: String,
}

/// Emit `AgentRegistered` on the agent channel.
impl TypedEvent for AgentRegistered {
    const CHANNEL: &'static str = AGENT_CHANNEL;
    const KIND: &'static str = "agent.registered";
}

/// An agent's presence registration was removed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentDeregistered {
    /// The deregistered agent's principal id.
    pub principal_id: String,
}

/// Emit `AgentDeregistered` on the agent channel.
impl TypedEvent for AgentDeregistered {
    const CHANNEL: &'static str = AGENT_CHANNEL;
    const KIND: &'static str = "agent.deregistered";
}

/// An agent heartbeat landed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentHeartbeat {
    /// The agent's principal id.
    pub principal_id: String,
    /// Its status token after the heartbeat (auto-revival or an override may change it).
    pub status: String,
}

/// Emit `AgentHeartbeat` on the agent channel.
impl TypedEvent for AgentHeartbeat {
    const CHANNEL: &'static str = AGENT_CHANNEL;
    const KIND: &'static str = "agent.heartbeat";
}

/// An agent's status was explicitly set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentStatusChanged {
    /// The agent's principal id.
    pub principal_id: String,
    /// The new status token.
    pub status: String,
}

/// Emit `AgentStatusChanged` on the agent channel.
impl TypedEvent for AgentStatusChanged {
    const CHANNEL: &'static str = AGENT_CHANNEL;
    const KIND: &'static str = "agent.status_changed";
}

/// An agent's quality signal was updated (by Thymus evaluation or supervision).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentQualityUpdated {
    /// The agent's principal id.
    pub principal_id: String,
    /// The new quality score, when one was set.
    pub quality_score: Option<f64>,
    /// How many drift flags the agent now carries.
    pub drift_flag_count: u64,
}

/// Emit `AgentQualityUpdated` on the agent channel.
impl TypedEvent for AgentQualityUpdated {
    const CHANNEL: &'static str = AGENT_CHANNEL;
    const KIND: &'static str = "agent.quality_updated";
}
