//! SQLite forge database -- opens the on-disk DB, applies the initial schema,
//! and runs incremental migrations so older databases gain new columns without
//! data loss. All tools share one `Database` instance per process.

use rusqlite::{Connection, Result as SqliteResult};
use std::fs;
use std::path::{Path, PathBuf};

/// Thin wrapper around a `rusqlite::Connection` that owns the forge DB file.
/// Callers borrow the inner connection via `conn()` to execute queries.
pub struct Database {
    conn: Connection,
}

/// Resolve the Crucible database path with new names first and legacy state preserved.
pub fn default_database_path() -> PathBuf {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let state_base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| home.as_ref().map(|path| path.join(".local").join("state")))
        .unwrap_or_else(|| PathBuf::from("."));
    let new_default = state_base.join("crucible").join("crucible.db");
    let legacy_default = home
        .as_ref()
        .map(|path| path.join(".agent-forge").join("forge.db"))
        .unwrap_or_else(|| PathBuf::from(".agent-forge/forge.db"));

    select_database_path(
        std::env::var_os("CRUCIBLE_DB")
            .map(|path| expand_home(PathBuf::from(path), home.as_deref())),
        std::env::var_os("AGENT_FORGE_DB")
            .map(|path| expand_home(PathBuf::from(path), home.as_deref())),
        new_default,
        legacy_default,
    )
}

/// Expand an explicit `~/` prefix without changing absolute or ordinary relative paths.
fn expand_home(path: PathBuf, home: Option<&Path>) -> PathBuf {
    let Some(path_text) = path.to_str() else {
        return path;
    };
    let Some(relative) = path_text.strip_prefix("~/") else {
        return path;
    };
    home.map(|base| base.join(relative)).unwrap_or(path)
}

/// Apply environment precedence and reuse existing legacy state when no override is present.
fn select_database_path(
    crucible_override: Option<PathBuf>,
    legacy_override: Option<PathBuf>,
    new_default: PathBuf,
    legacy_default: PathBuf,
) -> PathBuf {
    crucible_override.or(legacy_override).unwrap_or_else(|| {
        if new_default.exists() || !legacy_default.exists() {
            new_default
        } else {
            legacy_default
        }
    })
}

/// Open, initialise, and migrate the forge database.
impl Database {
    /// Open (or create) the forge DB at `path`, create parent directories as
    /// needed, apply the full schema, and run any pending migrations.
    pub fn open(path: &Path) -> SqliteResult<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).ok();
        }
        let conn = Connection::open(path)?;
        let db = Self { conn };
        db.init_schema()?;
        Ok(db)
    }

    /// Create all core tables (specs, hypotheses, checkpoints, session_learns,
    /// approaches, verifications) if they do not already exist, then run migrations.
    fn init_schema(&self) -> SqliteResult<()> {
        self.conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS specs (
                id TEXT PRIMARY KEY,
                created_at INTEGER NOT NULL,
                task_description TEXT NOT NULL,
                task_type TEXT NOT NULL,
                acceptance_criteria TEXT NOT NULL,
                interface_contract TEXT,
                edge_cases TEXT,
                files_to_touch TEXT,
                dependencies TEXT,
                status TEXT DEFAULT 'active',
                completed_at INTEGER,
                status_note TEXT
            );

            CREATE TABLE IF NOT EXISTS hypotheses (
                id TEXT PRIMARY KEY,
                created_at INTEGER NOT NULL,
                bug_description TEXT NOT NULL,
                hypothesis TEXT NOT NULL,
                confidence REAL NOT NULL,
                outcome TEXT,
                outcome_notes TEXT,
                verified_at INTEGER,
                spec_id TEXT REFERENCES specs(id)
            );

            CREATE TABLE IF NOT EXISTS checkpoints (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL UNIQUE,
                created_at INTEGER NOT NULL,
                git_ref TEXT,
                files_snapshot TEXT,
                description TEXT
            );

            CREATE TABLE IF NOT EXISTS session_learns (
                id TEXT PRIMARY KEY,
                created_at INTEGER NOT NULL,
                discovery TEXT NOT NULL,
                context TEXT,
                tags TEXT,
                spec_id TEXT REFERENCES specs(id)
            );

            CREATE TABLE IF NOT EXISTS approaches (
                id TEXT PRIMARY KEY,
                spec_id TEXT REFERENCES specs(id),
                created_at INTEGER NOT NULL,
                name TEXT NOT NULL,
                description TEXT NOT NULL,
                pros TEXT,
                cons TEXT,
                score REAL,
                chosen INTEGER DEFAULT 0
            );

            CREATE TABLE IF NOT EXISTS verifications (
                id TEXT PRIMARY KEY,
                spec_id TEXT REFERENCES specs(id),
                created_at INTEGER NOT NULL,
                command TEXT NOT NULL,
                exit_code INTEGER NOT NULL,
                success INTEGER NOT NULL,
                duration_ms INTEGER,
                criteria_index INTEGER,
                stdout TEXT,
                stderr TEXT
            );
            "#,
        )?;

        // Migrations for existing databases
        self.migrate()
    }

    /// Apply incremental column additions to existing databases. Each migration
    /// is guarded by a probe query so it is safe to re-run on an up-to-date DB.
    fn migrate(&self) -> SqliteResult<()> {
        let has_column = |table: &str, col: &str| -> bool {
            self.conn
                .prepare(&format!("SELECT {col} FROM {table} LIMIT 0"))
                .is_ok()
        };

        if !has_column("hypotheses", "spec_id") {
            self.conn.execute_batch(
                "ALTER TABLE hypotheses ADD COLUMN spec_id TEXT REFERENCES specs(id);",
            )?;
        }
        if !has_column("session_learns", "spec_id") {
            self.conn.execute_batch(
                "ALTER TABLE session_learns ADD COLUMN spec_id TEXT REFERENCES specs(id);",
            )?;
        }
        if !has_column("specs", "completed_at") {
            self.conn
                .execute_batch("ALTER TABLE specs ADD COLUMN completed_at INTEGER;")?;
        }
        if !has_column("specs", "status_note") {
            self.conn
                .execute_batch("ALTER TABLE specs ADD COLUMN status_note TEXT;")?;
        }
        Ok(())
    }

    /// Return a shared reference to the underlying `rusqlite::Connection` for
    /// direct query execution by callers.
    pub fn conn(&self) -> &Connection {
        &self.conn
    }
}

/// Regression tests for Crucible database-path migration behavior.
#[cfg(test)]
mod tests {
    use super::{expand_home, select_database_path};
    use std::path::PathBuf;

    /// The primary override takes precedence over every legacy location.
    #[test]
    fn primary_override_wins() {
        let selected = select_database_path(
            Some(PathBuf::from("/new/explicit.db")),
            Some(PathBuf::from("/legacy/explicit.db")),
            PathBuf::from("/new/default.db"),
            PathBuf::from("/legacy/default.db"),
        );

        assert_eq!(selected, PathBuf::from("/new/explicit.db"));
    }

    /// The legacy override remains supported when no primary override is set.
    #[test]
    fn legacy_override_is_compatible() {
        let selected = select_database_path(
            None,
            Some(PathBuf::from("/legacy/explicit.db")),
            PathBuf::from("/new/default.db"),
            PathBuf::from("/legacy/default.db"),
        );

        assert_eq!(selected, PathBuf::from("/legacy/explicit.db"));
    }

    /// Existing legacy state is reused when the new default has not been created.
    #[test]
    fn existing_legacy_default_is_reused() {
        let directory = tempfile::tempdir().expect("tempdir");
        let new_default = directory.path().join("new/crucible.db");
        let legacy_default = directory.path().join("legacy/forge.db");
        std::fs::create_dir_all(legacy_default.parent().expect("parent")).expect("mkdir");
        std::fs::write(&legacy_default, b"existing").expect("write");

        let selected = select_database_path(None, None, new_default, legacy_default.clone());

        assert_eq!(selected, legacy_default);
    }

    /// Explicit home-relative paths expand consistently for CLI and in-process consumers.
    #[test]
    fn expands_home_relative_override() {
        assert_eq!(
            expand_home(
                PathBuf::from("~/.config/crucible.db"),
                Some(std::path::Path::new("/home/tester")),
            ),
            PathBuf::from("/home/tester/.config/crucible.db"),
        );
    }
}
