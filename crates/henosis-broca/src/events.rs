//! The narration events Broca publishes onto the Axon bus.
//!
//! They live here (a service crate) rather than in `syntheos-contracts` because they are
//! Broca's domain events, but they implement the contracts' [`TypedEvent`] trait so any
//! in-process reactor can subscribe without depending on Broca. Payloads carry identifying
//! strings and the narrative sentence only -- never the raw action payload, which may hold
//! detail that must not land on the ephemeral bus.

use serde::{Deserialize, Serialize};
use syntheos_contracts::TypedEvent;

/// The coarse channel every Broca narration event travels on.
pub const NARRATION_CHANNEL: &str = "narration";

/// An action was recorded in the narration log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActionLogged {
    /// The log row id.
    pub action_id: i64,
    /// The acting agent's principal id.
    pub principal_id: String,
    /// Originating service name.
    pub service: String,
    /// Action type token.
    pub action: String,
    /// The narrative sentence, when one was derived at log time.
    pub narrative: Option<String>,
}

/// Emit `ActionLogged` on the narration channel.
impl TypedEvent for ActionLogged {
    const CHANNEL: &'static str = NARRATION_CHANNEL;
    const KIND: &'static str = "narration.logged";
}
