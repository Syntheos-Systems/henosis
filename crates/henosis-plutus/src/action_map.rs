//! Maps a `ToolInvocation` to an `ActionClass` (required `Permission` + optional `QuotaDimension`).
//!
//! Unknown `(tool, action)` pairs map to the most restrictive class -- `AgentExecute` permission
//! (requires at least `Member` role) with `ToolCalls` quota dimension -- so an unrecognized action
//! is gated rather than waved through. This is the fail-closed contract for the action map.

use syntheos_contracts::ToolInvocation;

use crate::quota::QuotaDimension;
use crate::rbac::Permission;

/// The authorization class an invocation resolves to.
///
/// The gate checks `can(role, permission)` and, when `quota_dimension` is `Some`,
/// calls `check_and_increment` for that dimension.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionClass {
    /// The permission the principal must hold to perform this action.
    pub permission: Permission,
    /// The quota dimension to charge if the action is permitted, or `None` when the action
    /// is not separately metered (e.g. a read-only search has no daily counter of its own).
    pub quota_dimension: Option<QuotaDimension>,
}

/// Resolve a `ToolInvocation` to its `ActionClass`.
///
/// Known `(tool, action)` pairs are mapped explicitly. Any unrecognized pair
/// falls through to the most restrictive class (`AgentExecute` + `ToolCalls`),
/// ensuring that an unknown action is denied for low-privilege roles rather than
/// silently permitted. This is the fail-closed policy for the action map.
pub fn map_invocation(inv: &ToolInvocation) -> ActionClass {
    match (inv.tool.as_str(), inv.action.as_str()) {
        // Memory operations on the cognitive core (Kleos).
        ("kleos", "memory_store") => ActionClass {
            permission: Permission::MemoryStore,
            quota_dimension: Some(QuotaDimension::MemoryStores),
        },
        ("kleos", "memory_search") => ActionClass {
            permission: Permission::MemorySearch,
            quota_dimension: None, // reads are not metered
        },
        ("kleos", "context_build") => ActionClass {
            permission: Permission::MemorySearch,
            quota_dimension: None,
        },
        ("kleos", "handoff_dump") | ("kleos", "handoff_restore") => ActionClass {
            permission: Permission::MemoryStore,
            quota_dimension: Some(QuotaDimension::MemoryStores),
        },

        // Task submission to the Chiasm scheduler.
        ("chiasm", "create_task") | (_, "task_submit") => ActionClass {
            permission: Permission::TaskSubmit,
            quota_dimension: Some(QuotaDimension::Tasks),
        },

        // Org read (directory lookups, membership queries).
        (_, "org_read") | ("soma", "list_agents") | ("soma", "get_agent") => ActionClass {
            permission: Permission::OrgRead,
            quota_dimension: None,
        },

        // Secret resolution via Phylax.
        ("phylax", _) | (_, "secret_read") | (_, "secret_resolve") => ActionClass {
            permission: Permission::SecretRead,
            quota_dimension: Some(QuotaDimension::ToolCalls),
        },

        // Billing management.
        ("billing", _) | (_, "billing_manage") => ActionClass {
            permission: Permission::BillingManage,
            quota_dimension: None,
        },

        // Generic tool invocations (any tool + action not matched above).
        // Counted against ToolCalls quota; requires Member or higher.
        (_, "tool_invoke") => ActionClass {
            permission: Permission::ToolInvoke,
            quota_dimension: Some(QuotaDimension::ToolCalls),
        },

        // Unknown pairs: fail closed. Requires AgentExecute (Member or higher) and charges
        // ToolCalls quota. This ensures an unrecognized action is gated, not waved through.
        // Viewer and Billing roles cannot perform AgentExecute, so they are always denied.
        _ => ActionClass {
            permission: Permission::AgentExecute,
            quota_dimension: Some(QuotaDimension::ToolCalls),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Build a minimal `ToolInvocation` for tests.
    fn inv(tool: &str, action: &str) -> ToolInvocation {
        ToolInvocation {
            tool: tool.to_owned(),
            action: action.to_owned(),
            args: json!({}),
        }
    }

    /// Known pairs resolve to their expected class.
    #[test]
    fn known_pairs_resolve_correctly() {
        let ms = map_invocation(&inv("kleos", "memory_store"));
        assert_eq!(ms.permission, Permission::MemoryStore);
        assert_eq!(ms.quota_dimension, Some(QuotaDimension::MemoryStores));

        let search = map_invocation(&inv("kleos", "memory_search"));
        assert_eq!(search.permission, Permission::MemorySearch);
        assert_eq!(search.quota_dimension, None);

        let task = map_invocation(&inv("chiasm", "create_task"));
        assert_eq!(task.permission, Permission::TaskSubmit);
        assert_eq!(task.quota_dimension, Some(QuotaDimension::Tasks));

        let secret = map_invocation(&inv("phylax", "resolve"));
        assert_eq!(secret.permission, Permission::SecretRead);
        assert_eq!(secret.quota_dimension, Some(QuotaDimension::ToolCalls));
    }

    /// Unknown pairs are mapped to the most restrictive fail-closed class.
    #[test]
    fn unknown_pair_maps_to_fail_closed_class() {
        let unknown = map_invocation(&inv("mystery", "thing"));
        assert_eq!(unknown.permission, Permission::AgentExecute);
        assert_eq!(unknown.quota_dimension, Some(QuotaDimension::ToolCalls));
    }

    /// Another unknown tool.
    #[test]
    fn another_unknown_pair_is_fail_closed() {
        let unknown = map_invocation(&inv("some_future_tool", "unrecognized_action"));
        assert_eq!(unknown.permission, Permission::AgentExecute);
    }
}
