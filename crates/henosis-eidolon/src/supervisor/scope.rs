//! File-scope checking: edits outside an allow-listed path set.
//!
//! Ported (copy-and-own) from Kleos `eidolon-supervisor/src/checks/scope.rs` with one
//! deviation: Kleos shipped this check `#[allow(dead_code)]` and never wired it; Henosis wires
//! it whenever the supervisor config carries a non-empty `allowed_paths` (empty = disabled,
//! same contract as the gate's empty pattern list).

use super::{Severity, Violation};

/// Fire when the entry edits a file outside every allowed path prefix. An empty allow-list
/// disables the check.
pub fn check_file_scope(entry: &serde_json::Value, allowed_paths: &[String]) -> Vec<Violation> {
    if allowed_paths.is_empty() {
        return Vec::new();
    }
    let file_path = match extract_file_path(entry) {
        Some(p) => p,
        None => return Vec::new(),
    };
    // Component-wise prefix match, not string prefix: `str::starts_with` would
    // let `/etc/config_backup` escape an allowed `/etc/config`. `Path::starts_with`
    // only matches whole path components.
    let edited = std::path::Path::new(&file_path);
    let in_scope = allowed_paths
        .iter()
        .any(|allowed| edited.starts_with(allowed));
    if !in_scope {
        return vec![Violation {
            rule_id: "scope-violation".into(),
            severity: Severity::Warning,
            message: format!("Edit outside allowed scope: {file_path}"),
            context: file_path,
            session_id: None,
        }];
    }
    Vec::new()
}

/// The edited file path, when the entry is an Edit/Write/NotebookEdit tool use.
fn extract_file_path(entry: &serde_json::Value) -> Option<String> {
    let obj = entry.as_object()?;
    let tool_name = obj
        .get("tool_name")
        .or(obj.get("name"))
        .and_then(|v| v.as_str())?;
    if !matches!(tool_name, "Edit" | "Write" | "NotebookEdit") {
        return None;
    }
    let input = obj.get("tool_input").or(obj.get("input"))?;
    input
        .get("file_path")
        .or(input.get("filePath"))
        .and_then(|v| v.as_str())
        .map(String::from)
}

/// Unit tests for file-scope checking.
#[cfg(test)]
mod tests {
    use super::*;

    /// Build an Edit tool-use entry touching `path`.
    fn edit(path: &str) -> serde_json::Value {
        serde_json::json!({"tool_name": "Edit", "tool_input": {"file_path": path}})
    }

    /// A sibling sharing only a string prefix must NOT be considered in-scope:
    /// `/etc/config_backup` escapes an allowed `/etc/config` under str prefixing.
    #[test]
    fn string_prefix_sibling_is_out_of_scope() {
        let allowed = vec!["/etc/config".to_string()];
        let v = check_file_scope(&edit("/etc/config_backup/secrets"), &allowed);
        assert_eq!(v.len(), 1, "sibling-prefix path must be a violation");
    }

    /// A genuine child component is in-scope.
    #[test]
    fn real_child_is_in_scope() {
        let allowed = vec!["/etc/config".to_string()];
        let v = check_file_scope(&edit("/etc/config/app.toml"), &allowed);
        assert!(v.is_empty(), "a real child path must be allowed");
    }

    /// An empty allow-list disables the check.
    #[test]
    fn empty_allowlist_disables() {
        let v = check_file_scope(&edit("/anywhere/at/all"), &[]);
        assert!(v.is_empty());
    }
}
