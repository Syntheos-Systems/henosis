//! File-scope checking: edits outside an allow-listed path set.
//!
//! The check runs whenever the supervisor configuration carries a non-empty `allowed_paths`.
//! An empty list disables it, matching the gate's empty pattern-list behavior.
//! This remains a detection-only, post-hoc check: it resolves filesystem identity to report an
//! escape but never creates, modifies, blocks, or removes the edited path. Existing targets are
//! canonicalized directly; a new leaf is checked through its existing parent for compatibility
//! with Write/Edit events observed before the leaf becomes visible or after it is removed.

use std::path::{Component, Path, PathBuf};

use super::{Severity, Violation};

/// Return whether a path is absolute and contains no lexical parent traversal.
fn is_absolute_without_traversal(path: &Path) -> bool {
    path.is_absolute()
        && !path
            .components()
            .any(|component| matches!(component, Component::ParentDir))
}

/// Resolve an absolute, traversal-free path that already exists.
fn canonical_existing_path(path: &Path) -> Option<PathBuf> {
    if !is_absolute_without_traversal(path) {
        return None;
    }
    std::fs::canonicalize(path).ok()
}

/// Resolve an edited target, allowing only a missing leaf beneath an existing directory.
fn canonical_edited_path(path: &Path) -> Option<PathBuf> {
    if !is_absolute_without_traversal(path) {
        return None;
    }
    match std::fs::canonicalize(path) {
        Ok(existing) => return Some(existing),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return None,
    }
    match std::fs::symlink_metadata(path) {
        Ok(_) => return None,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return None,
    }
    let leaf = path.file_name()?;
    let parent = canonical_existing_path(path.parent()?)?;
    if !std::fs::metadata(&parent).ok()?.is_dir() {
        return None;
    }
    Some(parent.join(leaf))
}

/// Fire when the entry edits a file outside every canonical allowed root.
///
/// An empty allow-list disables the detection-only check. Relative, traversing, and
/// unresolvable paths fail closed as violations. Canonical resolution makes the final
/// [`Path::starts_with`] comparison component-wise and resistant to symlink escapes.
pub fn check_file_scope(entry: &serde_json::Value, allowed_paths: &[String]) -> Vec<Violation> {
    if allowed_paths.is_empty() {
        return Vec::new();
    }
    let file_path = match extract_file_path(entry) {
        Some(p) => p,
        None => return Vec::new(),
    };
    let edited = canonical_edited_path(Path::new(&file_path));
    let in_scope = edited.is_some_and(|edited| {
        allowed_paths.iter().any(|allowed| {
            canonical_existing_path(Path::new(allowed)).is_some_and(|root| edited.starts_with(root))
        })
    });
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
    use std::path::Path;

    use super::*;

    /// An automatically cleaned, uniquely named directory for filesystem-scope tests.
    struct TestDir(PathBuf);

    /// Creates and exposes test directories without adding a test-only dependency.
    impl TestDir {
        /// Create a unique directory below the operating system's temporary directory.
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "eidolon-scope-{}",
                syntheos_contracts::EventId::new()
            ));
            std::fs::create_dir(&path).expect("create test directory");
            Self(path)
        }

        /// Return the test directory path.
        fn path(&self) -> &Path {
            &self.0
        }
    }

    /// Removes each temporary directory and its test-only contents.
    impl Drop for TestDir {
        /// Clean up the uniquely owned test directory.
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Build an Edit tool-use entry touching `path`.
    fn edit(path: &str) -> serde_json::Value {
        serde_json::json!({"tool_name": "Edit", "tool_input": {"file_path": path}})
    }

    /// A sibling sharing only a string prefix must NOT be considered in-scope:
    /// `/etc/config_backup` escapes an allowed `/etc/config` under str prefixing.
    #[test]
    fn string_prefix_sibling_is_out_of_scope() {
        let temp = TestDir::new();
        let allowed_root = temp.path().join("config");
        let sibling = temp.path().join("config_backup");
        std::fs::create_dir(&allowed_root).expect("create allowed root");
        std::fs::create_dir(&sibling).expect("create sibling");
        let target = sibling.join("secrets");
        std::fs::write(&target, "secret").expect("create sibling target");
        let allowed = vec![allowed_root.to_string_lossy().into_owned()];
        let v = check_file_scope(&edit(&target.to_string_lossy()), &allowed);
        assert_eq!(v.len(), 1, "sibling-prefix path must be a violation");
    }

    /// A genuine child component is in-scope.
    #[test]
    fn real_child_is_in_scope() {
        let temp = TestDir::new();
        let target = temp.path().join("app.toml");
        std::fs::write(&target, "content").expect("create existing child");
        let allowed = vec![temp.path().to_string_lossy().into_owned()];
        let v = check_file_scope(&edit(&target.to_string_lossy()), &allowed);
        assert!(v.is_empty(), "a real child path must be allowed");
    }

    /// A new leaf beneath an existing allowed directory remains in scope.
    #[test]
    fn new_child_is_in_scope() {
        let temp = TestDir::new();
        let target = temp.path().join("new.toml");
        let allowed = vec![temp.path().to_string_lossy().into_owned()];
        let v = check_file_scope(&edit(&target.to_string_lossy()), &allowed);
        assert!(v.is_empty(), "a new direct child must be allowed");
    }

    /// Parent traversal is rejected even when its lexical prefix is allowed.
    #[test]
    fn parent_traversal_is_out_of_scope() {
        let temp = TestDir::new();
        let allowed_root = temp.path().join("allowed");
        std::fs::create_dir(&allowed_root).expect("create allowed root");
        let target = allowed_root.join("../escaped.txt");
        let allowed = vec![allowed_root.to_string_lossy().into_owned()];
        let v = check_file_scope(&edit(&target.to_string_lossy()), &allowed);
        assert_eq!(v.len(), 1, "parent traversal must be a violation");
    }

    /// A relative edited path cannot be proven to reside under an absolute allowed root.
    #[test]
    fn relative_path_is_out_of_scope() {
        let temp = TestDir::new();
        let allowed = vec![temp.path().to_string_lossy().into_owned()];
        let v = check_file_scope(&edit("relative.txt"), &allowed);
        assert_eq!(v.len(), 1, "relative path must be a violation");
    }

    /// A missing leaf with a nonexistent immediate parent cannot be resolved safely.
    #[test]
    fn nonexistent_parent_is_out_of_scope() {
        let temp = TestDir::new();
        let target = temp.path().join("missing").join("new.toml");
        let allowed = vec![temp.path().to_string_lossy().into_owned()];
        let v = check_file_scope(&edit(&target.to_string_lossy()), &allowed);
        assert_eq!(v.len(), 1, "unresolvable parent must be a violation");
    }

    /// A configured root that cannot be canonicalized never authorizes an edit.
    #[test]
    fn nonexistent_allowed_root_authorizes_nothing() {
        let temp = TestDir::new();
        let target = temp.path().join("file.toml");
        std::fs::write(&target, "content").expect("create existing target");
        let allowed = vec![temp
            .path()
            .join("missing-root")
            .to_string_lossy()
            .into_owned()];
        let v = check_file_scope(&edit(&target.to_string_lossy()), &allowed);
        assert_eq!(v.len(), 1, "unresolvable root must not authorize edits");
    }

    /// A symlink below an allowed root cannot redirect an edit outside that root.
    #[cfg(unix)]
    #[test]
    fn symlink_escape_is_out_of_scope() {
        use std::os::unix::fs::symlink;

        let temp = TestDir::new();
        let allowed_root = temp.path().join("allowed");
        let outside = temp.path().join("outside");
        std::fs::create_dir(&allowed_root).expect("create allowed root");
        std::fs::create_dir(&outside).expect("create outside directory");
        let outside_target = outside.join("secret");
        std::fs::write(&outside_target, "secret").expect("create outside target");
        symlink(&outside, allowed_root.join("redirect")).expect("create escaping symlink");
        let target = allowed_root.join("redirect").join("secret");
        let allowed = vec![allowed_root.to_string_lossy().into_owned()];
        let v = check_file_scope(&edit(&target.to_string_lossy()), &allowed);
        assert_eq!(v.len(), 1, "symlink escape must be a violation");
    }

    /// A dangling symlink leaf is not mistaken for a legitimate new child file.
    #[cfg(unix)]
    #[test]
    fn dangling_symlink_escape_is_out_of_scope() {
        use std::os::unix::fs::symlink;

        let temp = TestDir::new();
        let allowed_root = temp.path().join("allowed");
        let outside = temp.path().join("outside");
        std::fs::create_dir(&allowed_root).expect("create allowed root");
        std::fs::create_dir(&outside).expect("create outside directory");
        let target = allowed_root.join("redirect");
        symlink(outside.join("future-secret"), &target).expect("create dangling symlink");
        let allowed = vec![allowed_root.to_string_lossy().into_owned()];
        let v = check_file_scope(&edit(&target.to_string_lossy()), &allowed);
        assert_eq!(v.len(), 1, "dangling symlink must be a violation");
    }

    /// An empty allow-list disables the check.
    #[test]
    fn empty_allowlist_disables() {
        let v = check_file_scope(&edit("/anywhere/at/all"), &[]);
        assert!(v.is_empty());
    }
}
