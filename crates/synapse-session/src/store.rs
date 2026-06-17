//! SQLite session store with FTS5 search.
//!
//! Connection is wrapped in a std::sync::Mutex so SessionStore is Send + Sync,
//! allowing it to be shared via Arc across async tasks.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use synapse_provider::{ChatMessage, ContentBlock, Role};

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// A conversation session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: i64,
    pub project: String,
    pub model: String,
    pub summary: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

/// A single turn within a session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Turn {
    pub id: i64,
    pub session_id: i64,
    pub role: String,
    /// Full content blocks serialized as JSON.
    pub content_json: String,
    /// Plain text extracted from content blocks (for display and FTS).
    pub plain_text: String,
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub timestamp: i64,
}

/// A search hit across sessions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResult {
    pub session_id: i64,
    pub turn_id: i64,
    pub role: String,
    pub snippet: String,
    pub project: String,
    pub model: String,
    pub timestamp: i64,
    pub rank: f64,
}

/// A single LLM usage event (one request to one provider).
///
/// Persisted to the `usage` table on every turn so `/cost` and external
/// reporting can aggregate spend without scanning the old `usage.jsonl`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageRecord {
    pub timestamp: i64,
    pub session_id: Option<i64>,
    pub model: String,
    pub provider: String,
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub cache_read_tokens: u32,
    pub cache_write_tokens: u32,
    pub cost_usd: f64,
}

/// Aggregated usage totals over a time window.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UsageTotals {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub cost_usd: f64,
    pub event_count: u64,
}

// ---------------------------------------------------------------------------
// SessionStore
// ---------------------------------------------------------------------------

pub struct SessionStore {
    conn: Mutex<Connection>,
    path: PathBuf,
}

/// Adds inherent behavior for `SessionStore`.
impl SessionStore {
    /// Open (or create) the session database at the given path.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create session db dir: {}", parent.display()))?;
        }

        let conn = Connection::open(path)
            .with_context(|| format!("open session db: {}", path.display()))?;

        conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA foreign_keys = ON;")
            .context("set pragmas")?;

        let store = Self {
            conn: Mutex::new(conn),
            path: path.to_owned(),
        };
        store.migrate()?;
        Ok(store)
    }

    /// Open the default session database at ~/.synapse/sessions.db.
    pub fn open_default() -> Result<Self> {
        let path = dirs::home_dir()
            .context("no home dir")?
            .join(".synapse")
            .join("sessions.db");
        Self::open(&path)
    }

    /// Path to the database file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Lock the database connection.
    fn db(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().expect("session db mutex poisoned")
    }

    // -----------------------------------------------------------------------
    // Schema migration
    // -----------------------------------------------------------------------

    fn migrate(&self) -> Result<()> {
        self.db().execute_batch(
            "CREATE TABLE IF NOT EXISTS sessions (
                id          INTEGER PRIMARY KEY AUTOINCREMENT,
                project     TEXT NOT NULL DEFAULT '',
                model       TEXT NOT NULL DEFAULT '',
                summary     TEXT,
                created_at  INTEGER NOT NULL,
                updated_at  INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS turns (
                id            INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id    INTEGER NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
                role          TEXT NOT NULL,
                content_json  TEXT NOT NULL,
                plain_text    TEXT NOT NULL DEFAULT '',
                input_tokens  INTEGER NOT NULL DEFAULT 0,
                output_tokens INTEGER NOT NULL DEFAULT 0,
                timestamp     INTEGER NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_turns_session ON turns(session_id);
            CREATE INDEX IF NOT EXISTS idx_sessions_project ON sessions(project);
            CREATE INDEX IF NOT EXISTS idx_sessions_updated ON sessions(updated_at DESC);

            CREATE VIRTUAL TABLE IF NOT EXISTS turns_fts USING fts5(
                plain_text,
                content = 'turns',
                content_rowid = 'id',
                tokenize = 'porter unicode61'
            );

            -- Triggers to keep FTS in sync
            CREATE TRIGGER IF NOT EXISTS turns_ai AFTER INSERT ON turns BEGIN
                INSERT INTO turns_fts(rowid, plain_text) VALUES (new.id, new.plain_text);
            END;

            CREATE TRIGGER IF NOT EXISTS turns_ad AFTER DELETE ON turns BEGIN
                INSERT INTO turns_fts(turns_fts, rowid, plain_text) VALUES ('delete', old.id, old.plain_text);
            END;

            CREATE TRIGGER IF NOT EXISTS turns_au AFTER UPDATE OF plain_text ON turns BEGIN
                INSERT INTO turns_fts(turns_fts, rowid, plain_text) VALUES ('delete', old.id, old.plain_text);
                INSERT INTO turns_fts(rowid, plain_text) VALUES (new.id, new.plain_text);
            END;

            -- One row per LLM request. Replaces ~/.synapse/usage.jsonl so the
            -- cost view, future analytics, and the Eidolon activity surface
            -- can query rather than parse line-by-line.
            CREATE TABLE IF NOT EXISTS usage (
                id                  INTEGER PRIMARY KEY AUTOINCREMENT,
                timestamp           INTEGER NOT NULL,
                session_id          INTEGER REFERENCES sessions(id) ON DELETE SET NULL,
                model               TEXT NOT NULL DEFAULT '',
                provider            TEXT NOT NULL DEFAULT '',
                input_tokens        INTEGER NOT NULL DEFAULT 0,
                output_tokens       INTEGER NOT NULL DEFAULT 0,
                cache_read_tokens   INTEGER NOT NULL DEFAULT 0,
                cache_write_tokens  INTEGER NOT NULL DEFAULT 0,
                cost_usd            REAL    NOT NULL DEFAULT 0.0
            );

            CREATE INDEX IF NOT EXISTS idx_usage_timestamp ON usage(timestamp DESC);
            CREATE INDEX IF NOT EXISTS idx_usage_session ON usage(session_id);
            CREATE INDEX IF NOT EXISTS idx_usage_model ON usage(model);"
        ).context("migrate session schema")?;

        Ok(())
    }

    // -----------------------------------------------------------------------
    // Session CRUD
    // -----------------------------------------------------------------------

    /// Create a new session. Returns the session ID.
    pub fn create_session(&self, project: &str, model: &str) -> Result<i64> {
        let now = unix_now();
        let db = self.db();
        db.execute(
            "INSERT INTO sessions (project, model, created_at, updated_at) VALUES (?1, ?2, ?3, ?4)",
            params![project, model, now, now],
        )
        .context("insert session")?;
        Ok(db.last_insert_rowid())
    }

    /// Get a session by ID.
    pub fn get_session(&self, id: i64) -> Result<Option<Session>> {
        self.db().query_row(
            "SELECT id, project, model, summary, created_at, updated_at FROM sessions WHERE id = ?1",
            params![id],
            |row| Ok(Session {
                id: row.get(0)?,
                project: row.get(1)?,
                model: row.get(2)?,
                summary: row.get(3)?,
                created_at: row.get(4)?,
                updated_at: row.get(5)?,
            }),
        ).optional().context("get session")
    }

    /// List recent sessions, most recent first.
    pub fn list_sessions(&self, limit: usize, offset: usize) -> Result<Vec<Session>> {
        let db = self.db();
        let mut stmt = db
            .prepare(
                "SELECT id, project, model, summary, created_at, updated_at
             FROM sessions ORDER BY updated_at DESC LIMIT ?1 OFFSET ?2",
            )
            .context("prepare list sessions")?;

        let rows = stmt
            .query_map(params![limit as i64, offset as i64], |row| {
                Ok(Session {
                    id: row.get(0)?,
                    project: row.get(1)?,
                    model: row.get(2)?,
                    summary: row.get(3)?,
                    created_at: row.get(4)?,
                    updated_at: row.get(5)?,
                })
            })
            .context("list sessions")?;

        rows.collect::<std::result::Result<Vec<_>, _>>()
            .context("collect sessions")
    }

    /// Update session summary and touch updated_at.
    pub fn update_summary(&self, session_id: i64, summary: &str) -> Result<()> {
        self.db()
            .execute(
                "UPDATE sessions SET summary = ?1, updated_at = ?2 WHERE id = ?3",
                params![summary, unix_now(), session_id],
            )
            .context("update summary")?;
        Ok(())
    }

    /// Delete a session and all its turns.
    pub fn delete_session(&self, session_id: i64) -> Result<()> {
        self.db()
            .execute("DELETE FROM sessions WHERE id = ?1", params![session_id])
            .context("delete session")?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Turn persistence
    // -----------------------------------------------------------------------

    /// Persist a single turn. Returns the turn ID.
    pub fn insert_turn(
        &self,
        session_id: i64,
        message: &ChatMessage,
        input_tokens: u32,
        output_tokens: u32,
    ) -> Result<i64> {
        let role = role_str(&message.role);
        let content_json = serde_json::to_string(&message.content).context("serialize content")?;
        let plain_text = extract_plain_text(&message.content);
        let now = unix_now();

        let db = self.db();
        db.execute(
            "INSERT INTO turns (session_id, role, content_json, plain_text, input_tokens, output_tokens, timestamp)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![session_id, role, content_json, plain_text, input_tokens, output_tokens, now],
        ).context("insert turn")?;

        let turn_id = db.last_insert_rowid();

        // Touch session updated_at
        db.execute(
            "UPDATE sessions SET updated_at = ?1 WHERE id = ?2",
            params![now, session_id],
        )
        .context("touch session")?;

        Ok(turn_id)
    }

    /// Load all turns for a session, ordered by timestamp.
    pub fn load_turns(&self, session_id: i64) -> Result<Vec<Turn>> {
        let db = self.db();
        let mut stmt = db.prepare(
            "SELECT id, session_id, role, content_json, plain_text, input_tokens, output_tokens, timestamp
             FROM turns WHERE session_id = ?1 ORDER BY timestamp ASC, id ASC"
        ).context("prepare load turns")?;

        let rows = stmt
            .query_map(params![session_id], |row| {
                Ok(Turn {
                    id: row.get(0)?,
                    session_id: row.get(1)?,
                    role: row.get(2)?,
                    content_json: row.get(3)?,
                    plain_text: row.get(4)?,
                    input_tokens: row.get(5)?,
                    output_tokens: row.get(6)?,
                    timestamp: row.get(7)?,
                })
            })
            .context("load turns")?;

        rows.collect::<std::result::Result<Vec<_>, _>>()
            .context("collect turns")
    }

    /// Reconstruct ChatMessages from stored turns (for context reload).
    pub fn load_messages(&self, session_id: i64) -> Result<Vec<ChatMessage>> {
        let turns = self.load_turns(session_id)?;
        let mut messages = Vec::with_capacity(turns.len());

        for turn in turns {
            let role = parse_role(&turn.role);
            let content: Vec<ContentBlock> = serde_json::from_str(&turn.content_json)
                .with_context(|| format!("deserialize turn {} content", turn.id))?;
            messages.push(ChatMessage { role, content });
        }

        Ok(messages)
    }

    /// Get total token counts for a session.
    pub fn session_token_counts(&self, session_id: i64) -> Result<(u32, u32)> {
        self.db()
            .query_row(
                "SELECT COALESCE(SUM(input_tokens), 0), COALESCE(SUM(output_tokens), 0)
             FROM turns WHERE session_id = ?1",
                params![session_id],
                |row| Ok((row.get::<_, u32>(0)?, row.get::<_, u32>(1)?)),
            )
            .context("session token counts")
    }

    /// Count turns in a session.
    pub fn turn_count(&self, session_id: i64) -> Result<usize> {
        self.db()
            .query_row(
                "SELECT COUNT(*) FROM turns WHERE session_id = ?1",
                params![session_id],
                |row| row.get::<_, usize>(0),
            )
            .context("turn count")
    }

    // -----------------------------------------------------------------------
    // FTS5 search
    // -----------------------------------------------------------------------

    /// Search across all sessions using FTS5. Returns matching turns with context.
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<SearchResult>> {
        let sanitized = sanitize_fts_query(query);
        if sanitized.is_empty() {
            return Ok(Vec::new());
        }

        let db = self.db();
        let mut stmt = db
            .prepare(
                "SELECT t.session_id, t.id, t.role, snippet(turns_fts, 0, '>>>', '<<<', '...', 48),
                    s.project, s.model, t.timestamp, rank
             FROM turns_fts
             JOIN turns t ON t.id = turns_fts.rowid
             JOIN sessions s ON s.id = t.session_id
             WHERE turns_fts MATCH ?1
             ORDER BY rank
             LIMIT ?2",
            )
            .context("prepare search")?;

        let rows = stmt
            .query_map(params![sanitized, limit as i64], |row| {
                Ok(SearchResult {
                    session_id: row.get(0)?,
                    turn_id: row.get(1)?,
                    role: row.get(2)?,
                    snippet: row.get(3)?,
                    project: row.get(4)?,
                    model: row.get(5)?,
                    timestamp: row.get(6)?,
                    rank: row.get(7)?,
                })
            })
            .context("search")?;

        rows.collect::<std::result::Result<Vec<_>, _>>()
            .context("collect search results")
    }

    /// Search within a specific project.
    pub fn search_project(
        &self,
        query: &str,
        project: &str,
        limit: usize,
    ) -> Result<Vec<SearchResult>> {
        let sanitized = sanitize_fts_query(query);
        if sanitized.is_empty() {
            return Ok(Vec::new());
        }

        let db = self.db();
        let mut stmt = db
            .prepare(
                "SELECT t.session_id, t.id, t.role, snippet(turns_fts, 0, '>>>', '<<<', '...', 48),
                    s.project, s.model, t.timestamp, rank
             FROM turns_fts
             JOIN turns t ON t.id = turns_fts.rowid
             JOIN sessions s ON s.id = t.session_id
             WHERE turns_fts MATCH ?1 AND s.project = ?2
             ORDER BY rank
             LIMIT ?3",
            )
            .context("prepare project search")?;

        let rows = stmt
            .query_map(params![sanitized, project, limit as i64], |row| {
                Ok(SearchResult {
                    session_id: row.get(0)?,
                    turn_id: row.get(1)?,
                    role: row.get(2)?,
                    snippet: row.get(3)?,
                    project: row.get(4)?,
                    model: row.get(5)?,
                    timestamp: row.get(6)?,
                    rank: row.get(7)?,
                })
            })
            .context("project search")?;

        rows.collect::<std::result::Result<Vec<_>, _>>()
            .context("collect project search results")
    }

    /// Get the most recent session for a project (for session resume).
    pub fn latest_session_for_project(&self, project: &str) -> Result<Option<Session>> {
        self.db()
            .query_row(
                "SELECT id, project, model, summary, created_at, updated_at
             FROM sessions WHERE project = ?1
             ORDER BY updated_at DESC LIMIT 1",
                params![project],
                |row| {
                    Ok(Session {
                        id: row.get(0)?,
                        project: row.get(1)?,
                        model: row.get(2)?,
                        summary: row.get(3)?,
                        created_at: row.get(4)?,
                        updated_at: row.get(5)?,
                    })
                },
            )
            .optional()
            .context("latest session for project")
    }

    /// Count total sessions.
    pub fn session_count(&self) -> Result<usize> {
        self.db()
            .query_row("SELECT COUNT(*) FROM sessions", [], |row| {
                row.get::<_, usize>(0)
            })
            .context("session count")
    }

    // -----------------------------------------------------------------------
    // Usage tracking
    // -----------------------------------------------------------------------

    /// Record a single LLM request's token usage and cost. Non-fatal callers
    /// should swallow errors -- usage telemetry must never block real work.
    pub fn insert_usage(&self, rec: &UsageRecord) -> Result<i64> {
        let db = self.db();
        db.execute(
            "INSERT INTO usage
             (timestamp, session_id, model, provider,
              input_tokens, output_tokens, cache_read_tokens, cache_write_tokens, cost_usd)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                rec.timestamp,
                rec.session_id,
                rec.model,
                rec.provider,
                rec.input_tokens,
                rec.output_tokens,
                rec.cache_read_tokens,
                rec.cache_write_tokens,
                rec.cost_usd,
            ],
        )
        .context("insert usage")?;
        Ok(db.last_insert_rowid())
    }

    /// Sum usage from `since_ts` (unix seconds) to now. `None` aggregates over
    /// the full table. Used by `/cost today` and future activity dashboards.
    pub fn usage_totals_since(&self, since_ts: Option<i64>) -> Result<UsageTotals> {
        let db = self.db();
        let row: (i64, i64, i64, i64, f64, i64) = match since_ts {
            Some(ts) => db.query_row(
                "SELECT COALESCE(SUM(input_tokens), 0),
                        COALESCE(SUM(output_tokens), 0),
                        COALESCE(SUM(cache_read_tokens), 0),
                        COALESCE(SUM(cache_write_tokens), 0),
                        COALESCE(SUM(cost_usd), 0.0),
                        COUNT(*)
                 FROM usage WHERE timestamp >= ?1",
                params![ts],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                    ))
                },
            ),
            None => db.query_row(
                "SELECT COALESCE(SUM(input_tokens), 0),
                        COALESCE(SUM(output_tokens), 0),
                        COALESCE(SUM(cache_read_tokens), 0),
                        COALESCE(SUM(cache_write_tokens), 0),
                        COALESCE(SUM(cost_usd), 0.0),
                        COUNT(*)
                 FROM usage",
                [],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                    ))
                },
            ),
        }
        .context("usage totals")?;

        Ok(UsageTotals {
            input_tokens: row.0 as u64,
            output_tokens: row.1 as u64,
            cache_read_tokens: row.2 as u64,
            cache_write_tokens: row.3 as u64,
            cost_usd: row.4,
            event_count: row.5 as u64,
        })
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

/// Handles `role_str` behavior.
fn role_str(role: &Role) -> &'static str {
    match role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    }
}

/// Handles `parse_role` behavior.
fn parse_role(s: &str) -> Role {
    match s {
        "system" => Role::System,
        "assistant" => Role::Assistant,
        "tool" => Role::Tool,
        _ => Role::User,
    }
}

/// Extract searchable plain text from content blocks.
fn extract_plain_text(blocks: &[ContentBlock]) -> String {
    let mut parts = Vec::new();
    for block in blocks {
        match block {
            ContentBlock::Text { text } => parts.push(text.as_str()),
            ContentBlock::ToolUse { name, input, .. } => {
                parts.push(name.as_str());
                // Include tool input values for searchability
                if let Some(obj) = input.as_object() {
                    for (_, v) in obj {
                        if let Some(s) = v.as_str() {
                            parts.push(s);
                        }
                    }
                }
            }
            ContentBlock::ToolResult { content, .. } => {
                // Truncate large tool results to avoid bloating FTS
                if content.len() <= 2000 {
                    parts.push(content.as_str());
                } else {
                    parts.push(&content[..2000]);
                }
            }
        }
    }
    parts.join(" ")
}

/// FTS5 operators and keywords that must be stripped from user queries.
const FTS5_KEYWORDS: &[&str] = &["AND", "OR", "NOT", "NEAR"];

/// Sanitize an FTS5 query: strip special chars and FTS5 operators, ensure valid tokens.
fn sanitize_fts_query(query: &str) -> String {
    let tokens: Vec<&str> = query
        .split_whitespace()
        .filter(|t| {
            t.len() >= 2
                && t.chars()
                    .all(|c| c.is_alphanumeric() || c == '_' || c == '-' || c == '.')
                && !FTS5_KEYWORDS.contains(&t.to_uppercase().as_str())
        })
        .collect();

    if tokens.is_empty() {
        return String::new();
    }

    // Join with spaces for implicit AND in FTS5
    tokens.join(" ")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Handles `temp_store` behavior.
    fn temp_store() -> SessionStore {
        SessionStore::open(Path::new(":memory:")).expect("open in-memory db")
    }

    /// Handles `text_message` behavior.
    fn text_message(role: Role, text: &str) -> ChatMessage {
        ChatMessage {
            role,
            content: vec![ContentBlock::Text {
                text: text.to_string(),
            }],
        }
    }

    /// Handles `create_and_get_session` behavior.
    #[test]
    fn create_and_get_session() {
        let store = temp_store();
        let id = store
            .create_session("myproject", "claude-sonnet-4-20250514")
            .unwrap();
        assert!(id > 0);

        let session = store.get_session(id).unwrap().unwrap();
        assert_eq!(session.project, "myproject");
        assert_eq!(session.model, "claude-sonnet-4-20250514");
        assert!(session.summary.is_none());
    }

    /// Handles `insert_and_load_turns` behavior.
    #[test]
    fn insert_and_load_turns() {
        let store = temp_store();
        let sid = store.create_session("test", "test-model").unwrap();

        let user_msg = text_message(Role::User, "fix the bug in main.rs");
        let asst_msg = text_message(Role::Assistant, "I found the issue and fixed it.");

        store.insert_turn(sid, &user_msg, 100, 0).unwrap();
        store.insert_turn(sid, &asst_msg, 0, 200).unwrap();

        let turns = store.load_turns(sid).unwrap();
        assert_eq!(turns.len(), 2);
        assert_eq!(turns[0].role, "user");
        assert_eq!(turns[1].role, "assistant");
        assert!(turns[0].plain_text.contains("fix the bug"));
        assert!(turns[1].plain_text.contains("found the issue"));
    }

    /// Handles `load_messages_roundtrip` behavior.
    #[test]
    fn load_messages_roundtrip() {
        let store = temp_store();
        let sid = store.create_session("test", "m").unwrap();

        let original = vec![
            text_message(Role::User, "hello"),
            ChatMessage {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::Text {
                        text: "let me check".to_string(),
                    },
                    ContentBlock::ToolUse {
                        id: "t1".to_string(),
                        name: "bash".to_string(),
                        input: serde_json::json!({"command": "ls"}),
                    },
                ],
            },
            ChatMessage {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "t1".to_string(),
                    content: "file1.rs\nfile2.rs".to_string(),
                    is_error: false,
                }],
            },
        ];

        for msg in &original {
            store.insert_turn(sid, msg, 0, 0).unwrap();
        }

        let loaded = store.load_messages(sid).unwrap();
        assert_eq!(loaded.len(), 3);

        // Verify content roundtrips correctly
        let orig_json = serde_json::to_string(&original).unwrap();
        let loaded_json = serde_json::to_string(&loaded).unwrap();
        assert_eq!(orig_json, loaded_json);
    }

    /// Handles `fts_search` behavior.
    #[test]
    fn fts_search() {
        let store = temp_store();
        let s1 = store.create_session("alpha", "m").unwrap();
        let s2 = store.create_session("beta", "m").unwrap();

        store
            .insert_turn(
                s1,
                &text_message(Role::User, "deploy the nginx reverse proxy"),
                0,
                0,
            )
            .unwrap();
        store
            .insert_turn(
                s1,
                &text_message(Role::Assistant, "nginx is configured and running"),
                0,
                0,
            )
            .unwrap();
        store
            .insert_turn(
                s2,
                &text_message(Role::User, "fix the database migration"),
                0,
                0,
            )
            .unwrap();

        let results = store.search("nginx", 10).unwrap();
        assert_eq!(results.len(), 2);
        assert!(results.iter().all(|r| r.session_id == s1));

        let results = store.search("database migration", 10).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].session_id, s2);
    }

    /// Handles `search_project_filter` behavior.
    #[test]
    fn search_project_filter() {
        let store = temp_store();
        let s1 = store.create_session("ion", "m").unwrap();
        let s2 = store.create_session("engram", "m").unwrap();

        store
            .insert_turn(s1, &text_message(Role::User, "add wgpu renderer"), 0, 0)
            .unwrap();
        store
            .insert_turn(s2, &text_message(Role::User, "add wgpu renderer"), 0, 0)
            .unwrap();

        let results = store.search_project("wgpu renderer", "ion", 10).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].project, "ion");
    }

    /// Handles `token_counts` behavior.
    #[test]
    fn token_counts() {
        let store = temp_store();
        let sid = store.create_session("test", "m").unwrap();

        store
            .insert_turn(sid, &text_message(Role::User, "hello"), 150, 0)
            .unwrap();
        store
            .insert_turn(sid, &text_message(Role::Assistant, "hi"), 0, 300)
            .unwrap();
        store
            .insert_turn(sid, &text_message(Role::User, "more"), 200, 0)
            .unwrap();

        let (input, output) = store.session_token_counts(sid).unwrap();
        assert_eq!(input, 350);
        assert_eq!(output, 300);
    }

    /// Handles `list_sessions_ordered` behavior.
    #[test]
    fn list_sessions_ordered() {
        let store = temp_store();
        let first = store.create_session("first", "m").unwrap();
        let _second = store.create_session("second", "m").unwrap();

        // Force different updated_at via direct SQL since both created in same second
        store
            .db()
            .execute(
                "UPDATE sessions SET updated_at = updated_at - 10 WHERE id = ?1",
                params![first],
            )
            .unwrap();

        let sessions = store.list_sessions(10, 0).unwrap();
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].project, "second");
    }

    /// Handles `update_summary` behavior.
    #[test]
    fn update_summary() {
        let store = temp_store();
        let sid = store.create_session("test", "m").unwrap();
        store.update_summary(sid, "Fixed the nginx config").unwrap();

        let session = store.get_session(sid).unwrap().unwrap();
        assert_eq!(session.summary.as_deref(), Some("Fixed the nginx config"));
    }

    /// Handles `delete_session_cascades` behavior.
    #[test]
    fn delete_session_cascades() {
        let store = temp_store();
        let sid = store.create_session("test", "m").unwrap();
        store
            .insert_turn(sid, &text_message(Role::User, "hello"), 0, 0)
            .unwrap();
        store
            .insert_turn(sid, &text_message(Role::Assistant, "hi"), 0, 0)
            .unwrap();

        store.delete_session(sid).unwrap();
        assert!(store.get_session(sid).unwrap().is_none());
        assert_eq!(store.load_turns(sid).unwrap().len(), 0);
    }

    /// Handles `latest_session_for_project` behavior.
    #[test]
    fn latest_session_for_project() {
        let store = temp_store();
        let s1 = store.create_session("ion", "m1").unwrap();
        let s2 = store.create_session("ion", "m2").unwrap();

        // Force s1 to be older so s2 is clearly the latest
        store
            .db()
            .execute(
                "UPDATE sessions SET updated_at = updated_at - 10 WHERE id = ?1",
                params![s1],
            )
            .unwrap();

        let latest = store.latest_session_for_project("ion").unwrap().unwrap();
        assert_eq!(latest.id, s2);
        assert_eq!(latest.model, "m2");
    }

    /// Handles `sanitize_fts_strips_special` behavior.
    #[test]
    fn sanitize_fts_strips_special() {
        assert_eq!(sanitize_fts_query("hello world"), "hello world");
        assert_eq!(sanitize_fts_query("a b"), ""); // too short
        assert_eq!(sanitize_fts_query("nginx OR proxy"), "nginx proxy");
        assert_eq!(sanitize_fts_query("file.rs"), "file.rs");
        assert_eq!(sanitize_fts_query("\"injected\""), ""); // quotes stripped
    }

    /// Handles `tool_use_content_is_searchable` behavior.
    #[test]
    fn tool_use_content_is_searchable() {
        let store = temp_store();
        let sid = store.create_session("test", "m").unwrap();

        let msg = ChatMessage {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: "t1".to_string(),
                name: "bash".to_string(),
                input: serde_json::json!({"command": "cargo build --release"}),
            }],
        };
        store.insert_turn(sid, &msg, 0, 0).unwrap();

        let results = store.search("cargo build", 10).unwrap();
        assert_eq!(results.len(), 1);
    }

    /// Handles `session_count` behavior.
    #[test]
    fn session_count() {
        let store = temp_store();
        assert_eq!(store.session_count().unwrap(), 0);
        store.create_session("a", "m").unwrap();
        store.create_session("b", "m").unwrap();
        assert_eq!(store.session_count().unwrap(), 2);
    }

    /// Handles `usage_insert_and_aggregate` behavior.
    #[test]
    fn usage_insert_and_aggregate() {
        let store = temp_store();
        let sid = store.create_session("test", "m").unwrap();

        store
            .insert_usage(&UsageRecord {
                timestamp: 1_700_000_000,
                session_id: Some(sid),
                model: "claude-opus-4".into(),
                provider: "anthropic".into(),
                input_tokens: 100,
                output_tokens: 200,
                cache_read_tokens: 50,
                cache_write_tokens: 25,
                cost_usd: 0.0123,
            })
            .unwrap();

        store
            .insert_usage(&UsageRecord {
                timestamp: 1_700_000_100,
                session_id: Some(sid),
                model: "claude-opus-4".into(),
                provider: "anthropic".into(),
                input_tokens: 300,
                output_tokens: 400,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
                cost_usd: 0.0456,
            })
            .unwrap();

        let totals = store.usage_totals_since(None).unwrap();
        assert_eq!(totals.event_count, 2);
        assert_eq!(totals.input_tokens, 400);
        assert_eq!(totals.output_tokens, 600);
        assert_eq!(totals.cache_read_tokens, 50);
        assert_eq!(totals.cache_write_tokens, 25);
        assert!((totals.cost_usd - 0.0579).abs() < 1e-9);

        // Window filter: only the second event lands inside [1_700_000_050, now).
        let scoped = store.usage_totals_since(Some(1_700_000_050)).unwrap();
        assert_eq!(scoped.event_count, 1);
        assert_eq!(scoped.input_tokens, 300);
    }

    /// Handles `usage_survives_session_delete` behavior.
    #[test]
    fn usage_survives_session_delete() {
        // ON DELETE SET NULL on usage.session_id -- usage history outlives
        // the conversation it came from, so /cost stays accurate across cleanups.
        let store = temp_store();
        let sid = store.create_session("test", "m").unwrap();
        store
            .insert_usage(&UsageRecord {
                timestamp: 1,
                session_id: Some(sid),
                model: "m".into(),
                provider: "p".into(),
                input_tokens: 1,
                output_tokens: 2,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
                cost_usd: 0.5,
            })
            .unwrap();

        store.delete_session(sid).unwrap();
        let totals = store.usage_totals_since(None).unwrap();
        assert_eq!(totals.event_count, 1);
        assert!((totals.cost_usd - 0.5).abs() < 1e-9);
    }
}
