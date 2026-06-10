//! SQLite forge database -- opens the on-disk DB, applies the initial schema,
//! and runs incremental migrations so older databases gain new columns without
//! data loss. All tools share one `Database` instance per process.

use rusqlite::{Connection, Result as SqliteResult};
use std::fs;
use std::path::Path;

/// Thin wrapper around a `rusqlite::Connection` that owns the forge DB file.
/// Callers borrow the inner connection via `conn()` to execute queries.
pub struct Database {
    conn: Connection,
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
                .prepare(&format!("SELECT {} FROM {} LIMIT 0", col, table))
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
