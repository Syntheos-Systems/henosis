//! A proposed action plus the context the dispatcher threads through every gate.

use serde::{Deserialize, Serialize};

use crate::ids::{PrincipalId, TenantId};
use crate::task::TaskRef;

/// A proposed action, resolved from skill/adapter registries before authorization.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
}

#[cfg(test)]
mod tests {
    use super::*;

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
        };
        let json = serde_json::to_string(&ctx).expect("serialize");
        let back: RequestContext = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(ctx, back);
    }
}
