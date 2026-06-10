//! The typed envelope every Axon event is wrapped in, plus the `TypedEvent` trait
//! that lets services emit strongly-typed events instead of hand-rolled JSON.

use serde::{Deserialize, Serialize};

use crate::ids::{EventId, PrincipalId, TenantId};
use crate::time::Timestamp;

/// The envelope every Axon event travels in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AxonEnvelope {
    /// Stable identity of this event.
    pub id: EventId,
    /// Coarse routing channel (e.g. `tool`, `task`, `credential`).
    pub channel: String,
    /// Specific event kind (e.g. `tool.invoked`, `task.completed`).
    pub kind: String,
    /// Tenant the event belongs to.
    pub tenant: TenantId,
    /// Principal that caused the event.
    pub principal: PrincipalId,
    /// When the event occurred (RFC3339).
    pub occurred_at: Timestamp,
    /// Event-specific payload as free-form JSON.
    pub payload: serde_json::Value,
}

/// Implemented by strongly-typed event structs so services emit typed events.
/// `to_envelope` fills `channel`/`kind` from the associated consts and
/// serializes `self` into the payload.
pub trait TypedEvent: Serialize {
    /// The coarse routing channel for this event type.
    const CHANNEL: &'static str;
    /// The specific kind string for this event type.
    const KIND: &'static str;

    /// Wrap this event in an [`AxonEnvelope`], stamping a fresh id and the current time.
    ///
    /// Returns an error if the event's `Serialize` implementation fails (for
    /// example, a payload containing a map with non-string keys). Most event
    /// structs serialize infallibly, but the substrate does not assume it.
    fn to_envelope(
        &self,
        tenant: TenantId,
        principal: PrincipalId,
    ) -> Result<AxonEnvelope, serde_json::Error> {
        Ok(AxonEnvelope {
            id: EventId::new(),
            channel: Self::CHANNEL.to_string(),
            kind: Self::KIND.to_string(),
            tenant,
            principal,
            occurred_at: Timestamp::now(),
            payload: serde_json::to_value(self)?,
        })
    }
}

/// Tests for Axon envelope and typed event wire contracts.
#[cfg(test)]
mod tests {
    use super::*;

    /// A trivial event used only to exercise the `TypedEvent` default impl.
    #[derive(Serialize)]
    struct ToolInvoked {
        /// Tool name included in the test payload.
        tool: String,
    }

    /// Implement `TypedEvent` for the test event.
    impl TypedEvent for ToolInvoked {
        const CHANNEL: &'static str = "tool";
        const KIND: &'static str = "tool.invoked";
    }

    /// `TypedEvent::to_envelope` fills routing fields from associated constants.
    #[test]
    fn into_envelope_fills_channel_and_kind() {
        let ev = ToolInvoked {
            tool: "kleos".to_string(),
        };
        let env = ev
            .to_envelope(TenantId::new(), PrincipalId::new())
            .expect("event serializes");
        assert_eq!(env.channel, "tool");
        assert_eq!(env.kind, "tool.invoked");
        assert_eq!(env.payload, serde_json::json!({ "tool": "kleos" }));
    }

    /// Axon envelopes roundtrip with the current wire shape.
    #[test]
    fn envelope_roundtrip() {
        let env = AxonEnvelope {
            id: EventId::new(),
            channel: "task".to_string(),
            kind: "task.completed".to_string(),
            tenant: TenantId::new(),
            principal: PrincipalId::new(),
            occurred_at: Timestamp::now(),
            payload: serde_json::json!({ "ok": true }),
        };
        let json = serde_json::to_string(&env).expect("serialize");
        let back: AxonEnvelope = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(env, back);
    }

    /// Axon envelopes reject misspelled routing or context fields.
    #[test]
    fn envelope_rejects_unknown_fields() {
        let env = AxonEnvelope {
            id: EventId::new(),
            channel: "task".to_string(),
            kind: "task.completed".to_string(),
            tenant: TenantId::new(),
            principal: PrincipalId::new(),
            occurred_at: Timestamp::now(),
            payload: serde_json::json!({ "ok": true }),
        };
        let mut value = serde_json::to_value(env).expect("serialize");
        value["principle"] = serde_json::json!("wrong");
        let err = serde_json::from_value::<AxonEnvelope>(value).expect_err("unknown field");
        assert!(err.to_string().contains("unknown field"));
    }
}
