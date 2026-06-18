//! Per-agent growth file store.
//!
//! Growth files are project-scoped, per-agent notes that persist across turns
//! and get injected into discussion context. They are distinct from Kleos
//! shared memory: growth is the agent's own running scratchpad for this project.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;

use crate::error::BridgeError;
use crate::types::AgentId;

/// Loads and appends per-agent growth files under a project-scoped root.
pub struct GrowthStore {
    /// Directory that holds one growth file per agent.
    root: PathBuf,
}

/// Implements project-scoped growth-file load/append for agents.
impl GrowthStore {
    /// Construct a store rooted at `root` (one file per agent lives here).
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    /// Load an agent's growth file contents, or an empty string if none exists.
    pub fn load(&self, agent: &AgentId) -> Result<String, BridgeError> {
        let path = self.root.join(format!("{}.md", agent.0));

        match fs::read_to_string(path) {
            Ok(contents) => Ok(contents),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(String::new()),
            Err(error) => Err(error.into()),
        }
    }

    /// Append a line to an agent's growth file, creating it (mode 0600) if absent.
    pub fn append(&self, agent: &AgentId, line: &str) -> Result<(), BridgeError> {
        fs::create_dir_all(&self.root)?;

        let path = self.root.join(format!("{}.md", agent.0));
        let mut options = OpenOptions::new();
        options.append(true).create(true);

        #[cfg(unix)]
        options.mode(0o600);

        let mut file = options.open(path)?;
        writeln!(file, "{line}")?;

        Ok(())
    }
}

/// Tests growth-file behavior against isolated temporary roots.
#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use std::time::{SystemTime, UNIX_EPOCH};
    use uuid::Uuid;

    /// Creates a unique temporary root for a growth-store test.
    fn temp_root(test_name: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("test clock should be after Unix epoch")
            .as_nanos();

        std::env::temp_dir().join(format!(
            "rift-growth-{test_name}-{}-{unique}",
            std::process::id()
        ))
    }

    /// Creates a fresh random agent ID for a growth-store test.
    fn agent_id() -> AgentId {
        AgentId(Uuid::new_v4())
    }

    /// Verifies that missing growth files load as empty content.
    #[test]
    fn load_missing_file_returns_empty_string() {
        let root = temp_root("missing");
        let store = GrowthStore::new(root.clone());
        let agent = agent_id();

        let contents = store.load(&agent).expect("missing load should succeed");

        assert_eq!(contents, "");
        let _ = fs::remove_dir_all(root);
    }

    /// Verifies that appending a line persists it with a trailing newline.
    #[test]
    fn append_then_load_round_trips_line() {
        let root = temp_root("round-trip");
        let store = GrowthStore::new(root.clone());
        let agent = agent_id();

        store
            .append(&agent, "remember the tacos")
            .expect("append should succeed");

        let contents = store.load(&agent).expect("load should succeed");

        assert_eq!(contents, "remember the tacos\n");
        let _ = fs::remove_dir_all(root);
    }

    /// Verifies that append preserves prior lines and creates secure Unix files.
    #[test]
    fn appends_accumulate_and_created_file_is_private_on_unix() {
        let root = temp_root("accumulate");
        let store = GrowthStore::new(root.clone());
        let agent = agent_id();

        store.append(&agent, "first line").expect("first append");
        store.append(&agent, "second line").expect("second append");

        let contents = store.load(&agent).expect("load should succeed");
        assert_eq!(contents, "first line\nsecond line\n");

        #[cfg(unix)]
        {
            let path = root.join(format!("{}.md", agent.0));
            let mode = fs::metadata(path)
                .expect("growth file metadata should exist")
                .permissions()
                .mode()
                & 0o777;

            assert_eq!(mode, 0o600);
        }

        let _ = fs::remove_dir_all(root);
    }
}
