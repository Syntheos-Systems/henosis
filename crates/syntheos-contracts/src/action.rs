//! A proposed action plus the context the dispatcher threads through every gate.

use serde::{Deserialize, Serialize};

use crate::ids::{PrincipalId, TenantId};
use crate::task::TaskRef;

/// Server-derived authority facts that callers cannot choose through the public API.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthorityContext {
    /// Stable identifier for the authenticated token or operator session.
    pub token_identity: String,
    /// Server-validated public dispatch key used to enforce at-most-once execution.
    pub idempotency_key: String,
    /// Durable approval presented for this exact request, when one exists.
    pub approval_id: Option<String>,
}

/// A proposed action, resolved from skill/adapter registries before authorization.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolInvocation {
    /// The tool or adapter being invoked (e.g. `kleos`).
    pub tool: String,
    /// The specific action on that tool (e.g. `memory_store`).
    pub action: String,
    /// Arguments to the action as free-form JSON.
    pub args: serde_json::Value,
}

/// The context the dispatcher threads through every gate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequestContext {
    /// Tenant the request belongs to.
    pub tenant: TenantId,
    /// Principal making the request.
    pub principal: PrincipalId,
    /// Active persona, if any.
    pub persona: Option<String>,
    /// Session identifier, if any.
    pub session: Option<String>,
    /// Room/channel identifier, if any.
    pub room: Option<String>,
    /// Task this request is part of, if any.
    pub task: Option<TaskRef>,
    /// Workflow this request is part of, if any.
    pub workflow: Option<String>,
    /// Server-derived authenticated authority for the request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authority: Option<AuthorityContext>,
}

/// Tests for action request wire contracts.
#[cfg(test)]
mod tests {
    use super::*;

    /// Tool invocation payloads roundtrip without changing field shape.
    #[test]
    fn tool_invocation_roundtrip() {
        let inv = ToolInvocation {
            tool: "kleos".to_string(),
            action: "memory_store".to_string(),
            args: serde_json::json!({ "content": "hello" }),
        };
        let json = serde_json::to_string(&inv).expect("serialize");
        let back: ToolInvocation = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(inv, back);
    }

    /// Minimal request contexts roundtrip with optional fields absent.
    #[test]
    fn request_context_roundtrip_minimal() {
        let ctx = RequestContext {
            tenant: TenantId::new(),
            principal: PrincipalId::new(),
            persona: None,
            session: None,
            room: None,
            task: None,
            workflow: None,
            authority: None,
        };
        let json = serde_json::to_string(&ctx).expect("serialize");
        let back: RequestContext = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(ctx, back);
    }

    /// Tool invocation payloads reject misspelled fields.
    #[test]
    fn tool_invocation_rejects_unknown_fields() {
        let json = r#"{"tool":"kleos","action":"memory_store","args":{},"argz":{}}"#;
        let err = serde_json::from_str::<ToolInvocation>(json).expect_err("unknown field");
        assert!(err.to_string().contains("unknown field"));
    }

    /// Request contexts reject misspelled optional security context fields.
    #[test]
    fn request_context_rejects_unknown_fields() {
        let json = format!(
            "{{\"tenant\":\"{}\",\"principal\":\"{}\",\"persona\":null,\"session\":null,\"room\":null,\"task\":null,\"workflow\":null,\"sesion\":\"oops\"}}",
            TenantId::new().as_uuid(),
            PrincipalId::new().as_uuid()
        );
        let err = serde_json::from_str::<RequestContext>(&json).expect_err("unknown field");
        assert!(err.to_string().contains("unknown field"));
    }
}
