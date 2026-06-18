use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Unique agent identifier within the bridge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AgentId(pub Uuid);

/// Types of messages in the room.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum MessageType {
    /// Human user message.
    User,
    /// AI agent message.
    Agent,
    /// External stimulus injected by the bridge.
    Stimulus,
    /// System notification.
    System,
}

impl MessageType {
    /// Database string representation.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Agent => "agent",
            Self::Stimulus => "stimulus",
            Self::System => "system",
        }
    }
}

/// A message received from the Rift WebSocket gateway.
#[derive(Debug, Clone, Deserialize)]
pub struct RoomMessage {
    /// Message ID.
    pub id: Uuid,
    /// Channel the message was posted in.
    pub channel_id: Uuid,
    /// Author's Rift user ID.
    pub author_id: Uuid,
    /// Author's username.
    pub author_username: String,
    /// Message text content.
    pub content: String,
    /// Type discriminator (user, agent, stimulus, system).
    pub message_type: String,
    /// When the message was created.
    pub created_at: DateTime<Utc>,
}

/// An agent's response to post back to the room.
#[derive(Debug, Clone)]
pub struct AgentResponse {
    /// Which agent generated this response.
    pub agent_id: AgentId,
    /// Target channel.
    pub channel_id: Uuid,
    /// Response text content.
    pub content: String,
}

/// Current state of an agent in the room.
#[derive(Debug, Clone, PartialEq)]
pub enum AgentState {
    /// Ready to respond to messages.
    Idle,
    /// Currently generating a response.
    Thinking,
    /// Cooling down after a recent post.
    Cooldown {
        /// When the cooldown expires.
        until: DateTime<Utc>,
    },
}
