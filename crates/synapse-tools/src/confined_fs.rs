//! Capability-relative filesystem access for model-invoked tools.

use anyhow::{Context, Result, bail};
use cap_std::ambient_authority;
use cap_std::fs::{Dir, DirEntry, Metadata};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

/// Retains the task-root capability opened before untrusted model execution begins.
pub struct ToolExecutionContext {
    /// Stable directory handle that cannot be redirected by replacing the ambient path.
    root: Arc<Dir>,
    /// Human-readable working directory supplied by the embedding host.
    cwd: PathBuf,
}

/// Opens and exposes the stable authority shared by every tool in one agent session.
impl ToolExecutionContext {
    /// Opens the task root once and retains that exact directory for subsequent tool calls.
    pub fn new(cwd: PathBuf) -> Result<Self> {
        let root = Dir::open_ambient_dir(&cwd, ambient_authority())
            .with_context(|| format!("opening task root {}", cwd.display()))?;
        Ok(Self {
            root: Arc::new(root),
            cwd,
        })
    }

    /// Returns the host-provided working directory for gates, tools, and diagnostics.
    pub fn cwd(&self) -> &Path {
        &self.cwd
    }
}

/// Binds one validated relative path to an opened task-root capability.
pub(crate) struct ConfinedPath {
    /// Open directory capability that prevents traversal outside the task root.
    root: Arc<Dir>,
    /// Validated path interpreted only relative to `root`.
    relative: PathBuf,
    /// Human-readable path used in tool output.
    display: PathBuf,
}

/// Provides capability-relative filesystem operations for a validated path.
impl ConfinedPath {
    /// Validates one model-supplied path against a retained task-root capability.
    pub(crate) fn new(
        context: &ToolExecutionContext,
        supplied: &str,
        allow_root: bool,
    ) -> Result<Self> {
        let relative = validate_relative_path(supplied, allow_root)?;
        let display = context.cwd.join(&relative);
        Ok(Self {
            root: Arc::clone(&context.root),
            relative,
            display,
        })
    }

    /// Reads the entire file without granting ambient filesystem access.
    pub(crate) fn read(&self) -> std::io::Result<Vec<u8>> {
        self.root.read(&self.relative)
    }

    /// Reads the entire UTF-8 file without granting ambient filesystem access.
    pub(crate) fn read_to_string(&self) -> std::io::Result<String> {
        self.root.read_to_string(&self.relative)
    }

    /// Creates missing in-root parents and writes the complete file contents.
    pub(crate) fn write(&self, contents: &[u8]) -> std::io::Result<()> {
        if let Some(parent) = self.relative.parent()
            && !parent.as_os_str().is_empty()
        {
            self.root.create_dir_all(parent)?;
        }
        self.root.write(&self.relative, contents)
    }

    /// Returns metadata for the confined target.
    pub(crate) fn metadata(&self) -> std::io::Result<Metadata> {
        self.root.metadata(&self.relative)
    }

    /// Opens the confined target as a directory capability.
    pub(crate) fn open_dir(&self) -> std::io::Result<Dir> {
        self.root.open_dir(&self.relative)
    }

    /// Returns the display path associated with this confined target.
    pub(crate) fn display(&self) -> &Path {
        &self.display
    }
}

/// Walks regular files beneath an opened directory without following symlinks.
///
/// The visitor receives a capability-bound entry and its path relative to the
/// supplied directory. Returning `true` stops the walk early.
pub(crate) fn visit_files<F>(
    directory: &Dir,
    relative_dir: &Path,
    visitor: &mut F,
) -> std::io::Result<bool>
where
    F: FnMut(&DirEntry, &Path) -> bool,
{
    for entry in directory.entries()? {
        let Ok(entry) = entry else {
            continue;
        };
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        let relative = relative_dir.join(entry.file_name());

        if file_type.is_symlink() {
            continue;
        }
        if file_type.is_dir() {
            let Ok(child) = entry.open_dir() else {
                continue;
            };
            if visit_files(&child, &relative, visitor)? {
                return Ok(true);
            }
        } else if file_type.is_file() && visitor(&entry, &relative) {
            return Ok(true);
        }
    }

    Ok(false)
}

/// Rejects paths that could address anything outside an opened task root.
fn validate_relative_path(supplied: &str, allow_root: bool) -> Result<PathBuf> {
    let path = Path::new(supplied);
    if path.is_absolute() {
        bail!("path must be relative to the task root");
    }

    let mut relative = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => relative.push(part),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                bail!("path must not contain parent traversal or a filesystem root");
            }
        }
    }

    if relative.as_os_str().is_empty() {
        if allow_root {
            relative.push(".");
        } else {
            bail!("path must name a file beneath the task root");
        }
    }

    Ok(relative)
}

/// Exercises lexical and capability-enforced path confinement.
#[cfg(test)]
mod tests {
    use super::*;

    /// Confirms absolute and parent-traversal paths are rejected.
    #[test]
    fn rejects_paths_outside_the_task_root() {
        let root = tempfile::tempdir().expect("task root");
        let context =
            ToolExecutionContext::new(root.path().to_path_buf()).expect("execution context");

        assert!(ConfinedPath::new(&context, "/etc/passwd", false).is_err());
        assert!(ConfinedPath::new(&context, "../outside", false).is_err());
        assert!(ConfinedPath::new(&context, "nested/../../outside", false).is_err());
    }

    /// Confirms a missing nested target can be created inside the task root.
    #[test]
    fn writes_missing_nested_target_inside_root() {
        let root = tempfile::tempdir().expect("task root");
        let context =
            ToolExecutionContext::new(root.path().to_path_buf()).expect("execution context");
        let target =
            ConfinedPath::new(&context, "nested/file.txt", false).expect("confined target");

        target.write(b"inside").expect("confined write");

        assert_eq!(
            std::fs::read(root.path().join("nested/file.txt")).expect("written file"),
            b"inside"
        );
    }

    /// Confirms a pre-existing symlink cannot redirect a read outside the root.
    #[cfg(unix)]
    #[test]
    fn rejects_symlink_read_escape() {
        let root = tempfile::tempdir().expect("task root");
        let outside = tempfile::tempdir().expect("outside root");
        std::fs::write(outside.path().join("secret.txt"), b"secret").expect("outside file");
        std::os::unix::fs::symlink(outside.path(), root.path().join("escape"))
            .expect("escape symlink");
        let context =
            ToolExecutionContext::new(root.path().to_path_buf()).expect("execution context");
        let target =
            ConfinedPath::new(&context, "escape/secret.txt", false).expect("confined target");

        assert!(target.read().is_err());
    }

    /// Confirms a pre-existing symlink cannot redirect a write outside the root.
    #[cfg(unix)]
    #[test]
    fn rejects_symlink_write_escape() {
        let root = tempfile::tempdir().expect("task root");
        let outside = tempfile::tempdir().expect("outside root");
        std::os::unix::fs::symlink(outside.path(), root.path().join("escape"))
            .expect("escape symlink");
        let context =
            ToolExecutionContext::new(root.path().to_path_buf()).expect("execution context");
        let target =
            ConfinedPath::new(&context, "escape/created.txt", false).expect("confined target");

        assert!(target.write(b"outside").is_err());
        assert!(!outside.path().join("created.txt").exists());
    }

    /// Confirms replacing the ambient task-root path cannot redirect a retained capability.
    #[cfg(unix)]
    #[test]
    fn retained_root_survives_ambient_path_replacement() {
        let workspace = tempfile::tempdir().expect("workspace");
        let root = workspace.path().join("task");
        let retained = workspace.path().join("retained");
        let outside = workspace.path().join("outside");
        std::fs::create_dir(&root).expect("task root");
        std::fs::create_dir(&outside).expect("outside root");
        let context = ToolExecutionContext::new(root.clone()).expect("execution context");

        std::fs::rename(&root, &retained).expect("move original root");
        std::os::unix::fs::symlink(&outside, &root).expect("replace ambient root");
        let target = ConfinedPath::new(&context, "proof.txt", false).expect("confined target");
        target.write(b"retained").expect("write retained root");

        assert_eq!(
            std::fs::read(retained.join("proof.txt")).expect("retained file"),
            b"retained"
        );
        assert!(!outside.join("proof.txt").exists());
    }

    /// Confirms Windows drive, UNC, device, and rooted paths are rejected.
    #[cfg(windows)]
    #[test]
    fn rejects_windows_root_and_prefix_forms() {
        for supplied in [
            r"C:\Windows\system.ini",
            r"C:relative.txt",
            r"\Windows\system.ini",
            r"\\server\share\secret.txt",
            r"\\?\C:\Windows\system.ini",
        ] {
            assert!(
                validate_relative_path(supplied, false).is_err(),
                "accepted Windows escape form: {supplied}"
            );
        }
    }

    /// Confirms a Windows directory link cannot redirect reads or writes outside the root.
    #[cfg(windows)]
    #[test]
    fn rejects_windows_directory_link_escape() {
        let root = tempfile::tempdir().expect("task root");
        let outside = tempfile::tempdir().expect("outside root");
        std::fs::write(outside.path().join("secret.txt"), b"secret").expect("outside file");
        std::os::windows::fs::symlink_dir(outside.path(), root.path().join("escape"))
            .expect("escape directory link");
        let context =
            ToolExecutionContext::new(root.path().to_path_buf()).expect("execution context");
        let read_target =
            ConfinedPath::new(&context, "escape/secret.txt", false).expect("read target");
        let write_target =
            ConfinedPath::new(&context, "escape/created.txt", false).expect("write target");

        assert!(read_target.read().is_err());
        assert!(write_target.write(b"outside").is_err());
        assert!(!outside.path().join("created.txt").exists());
    }

    /// Confirms an NTFS junction cannot redirect reads or writes outside the root.
    #[cfg(windows)]
    #[test]
    fn rejects_windows_junction_escape() {
        let root = tempfile::tempdir().expect("task root");
        let outside = tempfile::tempdir().expect("outside root");
        let junction = root.path().join("escape");
        std::fs::write(outside.path().join("secret.txt"), b"secret").expect("outside file");
        let output = std::process::Command::new("cmd")
            .arg("/C")
            .arg("mklink")
            .arg("/J")
            .arg(&junction)
            .arg(outside.path())
            .output()
            .expect("create junction");
        assert!(
            output.status.success(),
            "mklink failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let context =
            ToolExecutionContext::new(root.path().to_path_buf()).expect("execution context");
        let read_target =
            ConfinedPath::new(&context, "escape/secret.txt", false).expect("read target");
        let write_target =
            ConfinedPath::new(&context, "escape/created.txt", false).expect("write target");

        assert!(read_target.read().is_err());
        assert!(write_target.write(b"outside").is_err());
        assert!(!outside.path().join("created.txt").exists());
    }
}
