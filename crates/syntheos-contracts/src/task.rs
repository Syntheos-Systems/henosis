//! A lightweight reference to a Chiasm task. Not the full task record; just enough
//! to thread task identity through a request context.

use serde::{Deserialize, Serialize};

use crate::ids::{TaskId, TenantId};

/// A handle to a Chiasm task.
///
/// `deny_unknown_fields`: `TaskRef` rides the gate-authorization boundary as
/// `RequestContext.task`, so a misspelled or injected field must hard-error,
/// not deserialize silently -- consistent with the other wire structs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskRef {
    /// Stable identity of the task.
    pub id: TaskId,
    /// Tenant the task belongs to.
    pub tenant: TenantId,
    /// Optional human-readable task title.
    pub title: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn taskref_roundtrip() {
        let t = TaskRef {
            id: TaskId::new(),
            tenant: TenantId::new(),
            title: Some("ship the contracts crate".to_string()),
        };
        let json = serde_json::to_string(&t).expect("serialize");
        let back: TaskRef = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(t, back);
    }

    /// A misspelled or injected field on a `TaskRef` payload is rejected, not
    /// silently dropped -- closes the boundary gap the re-audit found (the one
    /// wire struct the deny_unknown_fields sweep missed).
    #[test]
    fn taskref_rejects_unknown_fields() {
        let json = format!(
            "{{\"id\":\"{}\",\"tenant\":\"{}\",\"title\":\"ship\",\"titel\":\"typo\"}}",
            TaskId::new().as_uuid(),
            TenantId::new().as_uuid()
        );
        let err = serde_json::from_str::<TaskRef>(&json).expect_err("unknown field");
        assert!(err.to_string().contains("unknown field"));
    }
}
