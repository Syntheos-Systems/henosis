//! A lightweight reference to a Chiasm task. Not the full task record; just enough
//! to thread task identity through a request context.

use serde::{Deserialize, Serialize};

use crate::ids::{TaskId, TenantId};

/// A handle to a Chiasm task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
}
