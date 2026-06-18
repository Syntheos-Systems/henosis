use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use uuid::Uuid;

use super::attachment::Attachment;

/// Raw message row as stored in the database.
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct Message {
    /// Unique message identifier.
    pub id: Uuid,
    /// Channel this message belongs to.
    pub channel_id: Uuid,
    /// User who authored the message.
    pub author_id: Uuid,
    /// Text content of the message.
    pub content: String,
    /// Set if the message has been edited; null otherwise.
    pub edited_at: Option<DateTime<Utc>>,
    /// Timestamp when the message was created.
    pub created_at: DateTime<Utc>,
    /// Type of message (user, agent, stimulus, system).
    pub message_type: String,
}

/// Message with author info joined from the users table.
#[derive(Debug, Serialize, FromRow)]
pub struct MessageWithAuthor {
    /// Unique message identifier.
    pub id: Uuid,
    /// Channel this message belongs to.
    pub channel_id: Uuid,
    /// User who authored the message.
    pub author_id: Uuid,
    /// Text content of the message.
    pub content: String,
    /// Set if the message has been edited; null otherwise.
    pub edited_at: Option<DateTime<Utc>>,
    /// Timestamp when the message was created.
    pub created_at: DateTime<Utc>,
    /// Type of message (user, agent, stimulus, system).
    pub message_type: String,
    /// Username of the author.
    pub author_username: String,
    /// Optional display name of the author.
    pub author_display_name: Option<String>,
    /// Optional avatar URL of the author.
    pub author_avatar_url: Option<String>,
}

/// Full message response including attachments, ready to serialize for the API.
#[derive(Debug, Serialize)]
pub struct MessageResponse {
    /// Unique message identifier.
    pub id: Uuid,
    /// Channel this message belongs to.
    pub channel_id: Uuid,
    /// User who authored the message.
    pub author_id: Uuid,
    /// Text content of the message.
    pub content: String,
    /// Set if the message has been edited; null otherwise.
    pub edited_at: Option<DateTime<Utc>>,
    /// Timestamp when the message was created.
    pub created_at: DateTime<Utc>,
    /// Type of message (user, agent, stimulus, system).
    pub message_type: String,
    /// Username of the author.
    pub author_username: String,
    /// Optional display name of the author.
    pub author_display_name: Option<String>,
    /// Optional avatar URL of the author.
    pub author_avatar_url: Option<String>,
    /// List of file attachments on this message.
    pub attachments: Vec<Attachment>,
}

impl MessageResponse {
    /// Assemble a MessageResponse from a joined message row and its attachments.
    pub fn from_msg(msg: MessageWithAuthor, attachments: Vec<Attachment>) -> Self {
        Self {
            id: msg.id,
            channel_id: msg.channel_id,
            author_id: msg.author_id,
            content: msg.content,
            edited_at: msg.edited_at,
            created_at: msg.created_at,
            message_type: msg.message_type,
            author_username: msg.author_username,
            author_display_name: msg.author_display_name,
            author_avatar_url: msg.author_avatar_url,
            attachments,
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct SendMessageRequest {
    pub content: Option<String>,
    pub attachment_ids: Option<Vec<Uuid>>,
}

#[derive(Debug, Deserialize)]
pub struct EditMessageRequest {
    pub content: String,
}

#[derive(Debug, Deserialize)]
pub struct MessageQuery {
    pub before: Option<Uuid>,
    pub after: Option<Uuid>,
    pub limit: Option<i64>,
}
