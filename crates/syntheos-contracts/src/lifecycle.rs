//! Action-lifecycle events emitted by the unified dispatcher (`syntheos-dispatch`)
//! onto the Axon bus. In-process reactors -- narration, evaluation, the future
//! durable audit path -- subscribe to these to observe the action stream.
//!
//! They live in `syntheos-contracts` (not in the dispatcher crate) so a subscriber
//! never has to depend on the dispatcher implementation just to read its events.
//!
//! All five travel on the [`ACTION_CHANNEL`]; payloads carry identifying strings only
//! (tool/action/gate/reason/prompt) -- never raw arguments or results, so nothing
//! sensitive lands on the ephemeral bus.

use serde::{Deserialize, Serialize};

use crate::event::TypedEvent;
use crate::TaskId;

/// The coarse channel every action-lifecycle event travels on.
pub const ACTION_CHANNEL: &str = "action";

/// An action entered the dispatcher and is about to run the gate chain.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActionInvoked {
    /// The tool/adapter being invoked (e.g. `kleos`).
    pub tool: String,
    /// The specific action on that tool (e.g. `memory_store`).
    pub action: String,
    /// Chiasm task correlated with this action, when the request is task-scoped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<TaskId>,
}

/// Emit `ActionInvoked` on the action lifecycle channel.
impl TypedEvent for ActionInvoked {
    const CHANNEL: &'static str = ACTION_CHANNEL;
    const KIND: &'static str = "action.invoked";
}

/// An action that passed every gate and executed successfully.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActionCompleted {
    /// The tool/adapter that ran.
    pub tool: String,
    /// The action that ran.
    pub action: String,
    /// Chiasm task correlated with this action, when the request is task-scoped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<TaskId>,
}

/// Emit `ActionCompleted` on the action lifecycle channel.
impl TypedEvent for ActionCompleted {
    const CHANNEL: &'static str = ACTION_CHANNEL;
    const KIND: &'static str = "action.completed";
}

/// An action that passed every gate but failed during execution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActionFailed {
    /// The tool/adapter that was invoked.
    pub tool: String,
    /// The action that was invoked.
    pub action: String,
    /// Human-readable execution error.
    pub error: String,
    /// Chiasm task correlated with this action, when the request is task-scoped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<TaskId>,
}

/// Emit `ActionFailed` on the action lifecycle channel.
impl TypedEvent for ActionFailed {
    const CHANNEL: &'static str = ACTION_CHANNEL;
    const KIND: &'static str = "action.failed";
}

/// An action rejected by a gate before execution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActionDenied {
    /// The tool/adapter that was requested.
    pub tool: String,
    /// The action that was requested.
    pub action: String,
    /// The gate that denied it (its [`crate::Gate::name`]).
    pub gate: String,
    /// Why the gate denied it.
    pub reason: String,
    /// Chiasm task correlated with this action, when the request is task-scoped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<TaskId>,
}

/// Emit `ActionDenied` on the action lifecycle channel.
impl TypedEvent for ActionDenied {
    const CHANNEL: &'static str = ACTION_CHANNEL;
    const KIND: &'static str = "action.denied";
}

/// An action a gate escalated for explicit approval before it may proceed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovalRequired {
    /// The tool/adapter awaiting approval.
    pub tool: String,
    /// The action awaiting approval.
    pub action: String,
    /// The gate that requested approval.
    pub gate: String,
    /// The prompt to show the approver.
    pub prompt: String,
    /// Chiasm task correlated with this action, when the request is task-scoped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<TaskId>,
}

/// Emit `ApprovalRequired` on the action lifecycle channel.
impl TypedEvent for ApprovalRequired {
    const CHANNEL: &'static str = ACTION_CHANNEL;
    const KIND: &'static str = "action.approval_required";
}

/// Tests for action lifecycle event wire contracts.
#[cfg(test)]
mod tests {
    use super::{ActionDenied, ActionInvoked};
    use crate::event::TypedEvent;
    use crate::ids::{PrincipalId, TaskId, TenantId};

    /// Invoked events envelope onto the shared action channel.
    #[test]
    fn invoked_envelopes_onto_action_channel() {
        let ev = ActionInvoked {
            tool: "kleos".into(),
            action: "memory_store".into(),
            task_id: None,
        };
        let env = ev
            .to_envelope(TenantId::new(), PrincipalId::new())
            .expect("event serializes");
        assert_eq!(env.channel, "action");
        assert_eq!(env.kind, "action.invoked");
        assert_eq!(
            env.payload,
            serde_json::json!({ "tool": "kleos", "action": "memory_store" })
        );
    }

    /// Denied events carry audit fields and roundtrip from envelope payloads.
    #[test]
    fn denied_carries_gate_and_reason_and_roundtrips() {
        let ev = ActionDenied {
            tool: "kleos".into(),
            action: "memory_store".into(),
            gate: "phylax".into(),
            reason: "no capability".into(),
            task_id: None,
        };
        let env = ev
            .to_envelope(TenantId::new(), PrincipalId::new())
            .expect("event serializes");
        assert_eq!(env.kind, "action.denied");
        let back: ActionDenied = serde_json::from_value(env.payload).expect("roundtrip");
        assert_eq!(back, ev);
    }

    /// Task-scoped lifecycle events carry the correlation id on the wire.
    #[test]
    fn task_scoped_event_serializes_correlation() {
        let task_id = TaskId::new();
        let ev = ActionInvoked {
            tool: "phylax".into(),
            action: "sign".into(),
            task_id: Some(task_id),
        };
        let env = ev
            .to_envelope(TenantId::new(), PrincipalId::new())
            .expect("event serializes");
        assert_eq!(env.payload["task_id"], task_id.to_string());
    }

    /// Lifecycle events reject misspelled fields.
    #[test]
    fn lifecycle_event_rejects_unknown_fields() {
        let json = r#"{"tool":"kleos","action":"memory_store","toool":"bad"}"#;
        let err = serde_json::from_str::<ActionInvoked>(json).expect_err("unknown field");
        assert!(err.to_string().contains("unknown field"));
    }
}
