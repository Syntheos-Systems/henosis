//! Per-session git worktree isolation. Each concurrent session gets its own
//! worktree so agents cannot stomp each other's files. Non-git directories are
//! returned unchanged with `isolated = false`.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};

/// The working directory assigned to a session plus whether it is isolated.
#[derive(Debug, Clone)]
pub struct SessionWorktree {
    pub path: PathBuf,
    pub isolated: bool,
}

/// True when `dir` is inside a git work tree.
///
/// Returns `false` both when `dir` is not a repo AND when `git` cannot be
/// spawned (e.g. not on PATH); the latter is logged at trace level.
pub fn is_git_repo(dir: &Path) -> bool {
    match Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", "--is-inside-work-tree"])
        .output()
    {
        Ok(o) => o.status.success() && String::from_utf8_lossy(&o.stdout).trim() == "true",
        Err(e) => {
            log::trace!("git spawn failed for is_git_repo({}): {e}", dir.display());
            false
        }
    }
}

/// Create an isolated worktree for `session_id` rooted at `base`. If `base` is
/// a git repo, runs `git worktree add` under `~/.synapse/worktrees/<id>` on a
/// new detached branch from HEAD. Otherwise returns `base` unchanged with
/// `isolated = false`.
///
/// Blocking: spawns `git` subprocesses synchronously. Call from a
/// `spawn_blocking` context if invoked on a latency-sensitive async path.
///
/// # Errors
/// Returns `Err` if the home directory cannot be resolved, the worktrees
/// directory cannot be created, or `git worktree add` fails (including when the
/// target path already exists).
pub fn prepare(base: &Path, session_id: u64) -> Result<SessionWorktree> {
    if !is_git_repo(base) {
        return Ok(SessionWorktree {
            path: base.to_path_buf(),
            isolated: false,
        });
    }

    let root = dirs::home_dir()
        .context("no home dir")?
        .join(".synapse")
        .join("worktrees");
    std::fs::create_dir_all(&root).context("create worktrees dir")?;
    // Include the OS pid so paths do not collide across concurrent processes or
    // restarts (session_id is a per-process counter that resets to 1 each run).
    let path = root.join(format!("session-{}-{session_id}", std::process::id()));

    let out = Command::new("git")
        .arg("-C")
        .arg(base)
        .args(["worktree", "add", "--detach"])
        .arg(&path)
        .arg("HEAD")
        .output()
        .context("spawn git worktree add")?;
    if !out.status.success() {
        anyhow::bail!(
            "git worktree add failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    Ok(SessionWorktree {
        path,
        isolated: true,
    })
}

/// Remove a worktree created by `prepare`. `base` is the original repo. Returns
/// `Ok(false)` if the worktree had uncommitted changes and was left in place.
pub fn remove(base: &Path, worktree: &SessionWorktree) -> Result<bool> {
    if !worktree.isolated {
        return Ok(true);
    }
    let out = Command::new("git")
        .arg("-C")
        .arg(base)
        .args(["worktree", "remove"])
        .arg(&worktree.path)
        .output()
        .context("spawn git worktree remove")?;
    if out.status.success() {
        Ok(true)
    } else {
        let stderr = String::from_utf8_lossy(&out.stderr);
        // git refuses to remove a worktree with uncommitted/untracked changes;
        // that is the intentional "left in place" case. Anything else is a real error.
        if stderr.contains("contains modified or untracked files") || stderr.contains("is dirty") {
            Ok(false)
        } else {
            anyhow::bail!("git worktree remove failed: {}", stderr.trim());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn init_repo(dir: &Path) {
        for args in [
            vec!["init", "-q"],
            vec!["config", "user.email", "t@t"],
            vec!["config", "user.name", "t"],
        ] {
            assert!(
                Command::new("git")
                    .arg("-C")
                    .arg(dir)
                    .args(&args)
                    .status()
                    .unwrap()
                    .success()
            );
        }
        std::fs::write(dir.join("f.txt"), "hi").unwrap();
        assert!(
            Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(["add", "."])
                .status()
                .unwrap()
                .success()
        );
        assert!(
            Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(["commit", "-q", "-m", "init"])
                .status()
                .unwrap()
                .success()
        );
    }

    #[test]
    fn non_git_dir_is_not_isolated() {
        let tmp = std::env::temp_dir().join(format!("syn-nongit-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let wt = prepare(&tmp, 1).unwrap();
        assert!(!wt.isolated);
        assert_eq!(wt.path, tmp);
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn git_repo_gets_isolated_worktree() {
        let tmp = std::env::temp_dir().join(format!("syn-git-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        init_repo(&tmp);

        let wt = prepare(&tmp, 42).unwrap();
        assert!(wt.isolated);
        assert!(wt.path.exists());
        assert!(wt.path.join("f.txt").exists());
        assert_ne!(wt.path, tmp);

        // Clean worktree removes successfully.
        assert!(remove(&tmp, &wt).unwrap());
        assert!(!wt.path.exists());
        // Belt-and-suspenders: ensure the worktree dir is gone even if a prior
        // assertion changed behavior.
        std::fs::remove_dir_all(&wt.path).ok();

        std::fs::remove_dir_all(&tmp).ok();
    }
}
