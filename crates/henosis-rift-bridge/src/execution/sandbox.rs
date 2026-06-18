//! Git worktree sandbox management.

use std::path::{Path, PathBuf};

use tokio::process::Command;

use crate::config::WorkspaceConfig;
use crate::error::BridgeError;
use crate::executor::ExecutionSandbox;

/// Build the git branch name for a task, honoring the executor convention
/// `agent/{agent}/...`.
pub fn branch_name(agent: &str, task_id: &str) -> String {
    format!("agent/{agent}/task-{task_id}")
}

/// Build the worktree directory path for a task under the worktrees root.
pub fn worktree_path(worktrees_root: &Path, task_id: &str) -> PathBuf {
    worktrees_root.join(format!("task-{task_id}"))
}

/// Resolve which workspace a task runs against: prefer an exact name match,
/// otherwise the first configured workspace.
pub fn resolve_workspace<'a>(
    workspaces: &'a [WorkspaceConfig],
    project_name: &str,
) -> Option<&'a WorkspaceConfig> {
    workspaces
        .iter()
        .find(|w| w.name == project_name)
        .or_else(|| workspaces.first())
}

/// Creates and tears down per-task git worktrees.
pub struct SandboxManager {
    /// Root directory for all task worktrees.
    worktrees_root: PathBuf,
    /// Wall-clock limit applied to each session, in seconds.
    max_runtime_secs: u64,
}

/// Worktree lifecycle operations.
impl SandboxManager {
    /// Build a manager rooted at the given worktrees directory.
    pub fn new(worktrees_root: PathBuf, max_runtime_secs: u64) -> Self {
        Self {
            worktrees_root,
            max_runtime_secs,
        }
    }

    /// Create a fresh git worktree on a new branch for the task and return the
    /// `ExecutionSandbox` describing it.
    ///
    /// Runs `git -C <repo> worktree add -b <branch> <path> HEAD`.
    pub async fn create(
        &self,
        workspace: &WorkspaceConfig,
        agent: &str,
        task_id: &str,
    ) -> Result<ExecutionSandbox, BridgeError> {
        let branch = branch_name(agent, task_id);
        let path = worktree_path(&self.worktrees_root, task_id);

        let output = Command::new("git")
            .arg("-C")
            .arg(&workspace.path)
            .arg("worktree")
            .arg("add")
            .arg("-b")
            .arg(&branch)
            .arg(&path)
            .arg("HEAD")
            .output()
            .await
            .map_err(|e| BridgeError::Sandbox(format!("git worktree add failed to spawn: {e}")))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(BridgeError::Sandbox(format!(
                "git worktree add failed: {stderr}"
            )));
        }

        Ok(ExecutionSandbox {
            branch,
            working_dir: path,
            max_runtime_secs: self.max_runtime_secs,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{branch_name, resolve_workspace, worktree_path};
    use crate::config::WorkspaceConfig;
    use std::path::PathBuf;

    /// Builds two workspaces for resolution tests.
    fn workspaces() -> Vec<WorkspaceConfig> {
        vec![
            WorkspaceConfig {
                name: "rift".into(),
                path: PathBuf::from("/tmp/rift"),
                cargo_target_dir: None,
            },
            WorkspaceConfig {
                name: "synapse".into(),
                path: PathBuf::from("/tmp/synapse"),
                cargo_target_dir: None,
            },
        ]
    }

    /// Verifies the branch name follows the agent/{agent}/task-{id} convention.
    #[test]
    fn test_branch_name_format() {
        assert_eq!(branch_name("architect", "42"), "agent/architect/task-42");
    }

    /// Verifies the worktree path is task-scoped under the worktrees root.
    #[test]
    fn test_worktree_path_format() {
        let root = PathBuf::from("/tmp/rift/.worktrees");
        assert_eq!(
            worktree_path(&root, "42"),
            PathBuf::from("/tmp/rift/.worktrees/task-42")
        );
    }

    /// Verifies workspace resolution prefers an exact project-name match.
    #[test]
    fn test_resolve_workspace_prefers_name_match() {
        let ws = workspaces();
        let resolved = resolve_workspace(&ws, "synapse").unwrap();
        assert_eq!(resolved.name, "synapse");
    }

    /// Verifies resolution falls back to the first workspace when no name matches.
    #[test]
    fn test_resolve_workspace_falls_back_to_first() {
        let ws = workspaces();
        let resolved = resolve_workspace(&ws, "unknown").unwrap();
        assert_eq!(resolved.name, "rift");
    }

    /// Verifies resolution returns None when no workspaces are configured.
    #[test]
    fn test_resolve_workspace_none_when_empty() {
        assert!(resolve_workspace(&[], "rift").is_none());
    }
}
