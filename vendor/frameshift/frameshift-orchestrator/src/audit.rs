//! Explainable audit log of persona transitions.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader};
use std::path::Path;

use chrono::Utc;
use serde::{Deserialize, Serialize};

use crate::error::OrchestratorError;

/// A single recorded persona transition event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Transition {
    /// RFC3339 timestamp of when the transition occurred.
    pub timestamp: String,

    /// The persona active before the transition, or `None` on first selection.
    pub from: Option<String>,

    /// The persona switched to.
    pub to: String,

    /// Confidence score at the time of the switch.
    pub confidence: f32,

    /// Human-readable rationale explaining why the switch occurred.
    pub rationale: String,
}

/// An in-memory log of persona transitions backed by a JSON-lines file.
#[derive(Debug, Clone, Default)]
pub struct AuditLog {
    /// All loaded transition entries, in chronological order.
    entries: Vec<Transition>,
}

/// Loads, appends, and queries persona transition audit records.
impl AuditLog {
    /// Maximum number of audit entries retained in memory when loading. Entries
    /// older than the most recent `MAX_AUDIT_ENTRIES` are dropped so a log grown
    /// over a long-lived deployment cannot exhaust memory on load.
    const MAX_AUDIT_ENTRIES: usize = 50_000;

    /// Load an audit log from a JSON-lines file.
    ///
    /// Returns an empty `AuditLog` if the file does not exist. Each non-empty
    /// line must be a valid JSON object matching `Transition`. Only the most
    /// recent `MAX_AUDIT_ENTRIES` are retained.
    pub fn load(path: &Path) -> Result<Self, OrchestratorError> {
        Self::load_capped(path, Self::MAX_AUDIT_ENTRIES)
    }

    /// Load at most `max` most-recent entries, streaming the file line by line
    /// so the full file contents are never held in memory at once.
    fn load_capped(path: &Path, max: usize) -> Result<Self, OrchestratorError> {
        if !path.exists() {
            return Ok(AuditLog::default());
        }
        let file = std::fs::File::open(path)?;
        let reader = BufReader::new(file);
        let mut entries: VecDeque<Transition> = VecDeque::new();
        for line in reader.lines() {
            let line = line?;
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let t: Transition = serde_json::from_str(trimmed)?;
            entries.push_back(t);
            if max > 0 && entries.len() > max {
                entries.pop_front();
            }
        }
        Ok(AuditLog {
            entries: entries.into(),
        })
    }

    /// Append a transition to both the in-memory log and the backing file.
    ///
    /// The entry is serialized as a single JSON line and appended to `path`.
    /// Parent directories are created if they do not exist.
    pub fn append(&mut self, path: &Path, t: Transition) -> Result<(), OrchestratorError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let line = serde_json::to_string(&t)? + "\n";
        // Append to file.
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        file.write_all(line.as_bytes())?;
        self.entries.push(t);
        Ok(())
    }

    /// Return a slice of the most recent `n` transitions, or all if fewer than `n` exist.
    pub fn recent(&self, n: usize) -> &[Transition] {
        let len = self.entries.len();
        if n >= len {
            &self.entries
        } else {
            &self.entries[len - n..]
        }
    }
}

/// Build a monotonic RFC3339 timestamp string for audit log entries.
///
/// Uses `chrono::Utc::now()` formatted as RFC3339.
pub fn now_timestamp() -> String {
    Utc::now().to_rfc3339()
}

#[cfg(test)]
/// Verifies audit persistence, retention, and timestamp behavior.
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// Helper to build a test Transition.
    fn make_transition(from: Option<&str>, to: &str) -> Transition {
        Transition {
            timestamp: now_timestamp(),
            from: from.map(|s| s.to_string()),
            to: to.to_string(),
            confidence: 0.8,
            rationale: format!("switched to {to}"),
        }
    }

    /// Load returns empty log when file does not exist.
    #[test]
    fn load_missing_returns_empty() {
        let tmp = TempDir::new().unwrap();
        let log = AuditLog::load(&tmp.path().join("audit.jsonl")).unwrap();
        assert_eq!(log.entries.len(), 0);
    }

    /// Append adds entry to in-memory log and persists to file.
    #[test]
    fn append_persists_and_appears_in_memory() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("audit.jsonl");

        let mut log = AuditLog::load(&path).unwrap();
        log.append(&path, make_transition(None, "rust-expert"))
            .unwrap();
        assert_eq!(log.entries.len(), 1);

        // Reload and verify persistence.
        let loaded = AuditLog::load(&path).unwrap();
        assert_eq!(loaded.entries.len(), 1);
        assert_eq!(loaded.entries[0].to, "rust-expert");
    }

    /// Multiple appends are all persisted and loadable.
    #[test]
    fn multiple_appends_all_persisted() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("audit.jsonl");

        let mut log = AuditLog::load(&path).unwrap();
        log.append(&path, make_transition(None, "alpha")).unwrap();
        log.append(&path, make_transition(Some("alpha"), "beta"))
            .unwrap();
        log.append(&path, make_transition(Some("beta"), "gamma"))
            .unwrap();

        let loaded = AuditLog::load(&path).unwrap();
        assert_eq!(loaded.entries.len(), 3);
        assert_eq!(loaded.entries[2].to, "gamma");
    }

    /// recent(n) returns at most n entries from the end.
    #[test]
    fn recent_returns_last_n() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("audit.jsonl");

        let mut log = AuditLog::load(&path).unwrap();
        for i in 0..5 {
            log.append(&path, make_transition(None, &format!("p{i}")))
                .unwrap();
        }

        let recent = log.recent(2);
        assert_eq!(recent.len(), 2);
        assert_eq!(recent[0].to, "p3");
        assert_eq!(recent[1].to, "p4");
    }

    /// recent(n) when n > len returns all entries.
    #[test]
    fn recent_more_than_len_returns_all() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("audit.jsonl");

        let mut log = AuditLog::load(&path).unwrap();
        log.append(&path, make_transition(None, "only")).unwrap();

        assert_eq!(log.recent(100).len(), 1);
    }

    /// load_capped retains only the most recent `max` entries from the file.
    #[test]
    fn load_capped_keeps_most_recent() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("audit.jsonl");

        let mut log = AuditLog::default();
        for i in 0..5 {
            log.append(&path, make_transition(None, &format!("p{i}")))
                .unwrap();
        }

        let loaded = AuditLog::load_capped(&path, 3).unwrap();
        assert_eq!(loaded.entries.len(), 3, "only the 3 most recent retained");
        assert_eq!(loaded.entries.first().unwrap().to, "p2");
        assert_eq!(loaded.entries.last().unwrap().to, "p4");
    }
}
