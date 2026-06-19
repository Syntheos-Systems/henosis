//! Skill materializations -- track which kind:agent skills have been
//! written to disk so Claude Code's harness can pick them up next session.
//!
//! Background: Claude Code only registers Task subagents from
//! `~/.claude/agents/<name>.md` at session start. The Skills Cloud (v50+)
//! lets us pull an agent definition from Kleos *during* a session, but the
//! harness won't know about it until we materialize it (write the .md to
//! the canonical agents dir) and the user starts a new session.
//!
//! This module is the bookkeeping side: the actual filesystem write is
//! the CLI's job (`kleos-cli skill materialize <id>`). The DB row tells us:
//! "we have written agent X to path Y; the content hash at write time was H".
//! The CLI can then detect drift between the Kleos row and disk on each
//! materialize invocation and avoid clobbering hand-edited .md files.

use crate::db::Database;
use crate::Result;
use rusqlite::params;
use serde::{Deserialize, Serialize};

// One materialization record. PK is `skill_id` so a skill is materialized
// at most once at a time; updating the record on re-materialize is the
// expected path.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillMaterialization {
    pub skill_id: i64,
    pub target_path: String,
    pub materialized_at: String,
    pub content_hash_at_materialize: String,
}

// Record (or refresh) a materialization. Idempotent: re-materializing the
// same skill updates the path / hash / timestamp.
pub async fn record(
    db: &Database,
    skill_id: i64,
    target_path: &str,
    content_hash: &str,
) -> Result<()> {
    let path = target_path.to_string();
    let hash = content_hash.to_string();
    db.write(move |conn| {
        conn.execute(
            "INSERT INTO skill_materializations \
             (skill_id, target_path, content_hash_at_materialize) \
             VALUES (?1, ?2, ?3) \
             ON CONFLICT(skill_id) DO UPDATE SET \
                target_path = excluded.target_path, \
                content_hash_at_materialize = excluded.content_hash_at_materialize, \
                materialized_at = datetime('now')",
            params![skill_id, path, hash],
        )?;
        Ok(())
    })
    .await
}

// Look up a materialization by skill id. None means the skill has never
// been written to disk (or the row was cleared).
pub async fn get(db: &Database, skill_id: i64) -> Result<Option<SkillMaterialization>> {
    db.read(move |conn| {
        let mut stmt = conn.prepare(
            "SELECT skill_id, target_path, materialized_at, content_hash_at_materialize \
                 FROM skill_materializations WHERE skill_id = ?1",
        )?;
        let mut rows = stmt.query(params![skill_id])?;
        if let Some(row) = rows.next()? {
            Ok(Some(SkillMaterialization {
                skill_id: row.get(0)?,
                target_path: row.get(1)?,
                materialized_at: row.get(2)?,
                content_hash_at_materialize: row.get(3)?,
            }))
        } else {
            Ok(None)
        }
    })
    .await
}

// List every active materialization. Used by the Tauri browse app to
// flag which agent skills are currently on disk.
pub async fn list_all(db: &Database) -> Result<Vec<SkillMaterialization>> {
    db.read(move |conn| {
        let mut stmt = conn.prepare(
            "SELECT skill_id, target_path, materialized_at, content_hash_at_materialize \
                 FROM skill_materializations ORDER BY materialized_at DESC",
        )?;
        let rows = stmt.query_map(params![], |r| {
            Ok(SkillMaterialization {
                skill_id: r.get(0)?,
                target_path: r.get(1)?,
                materialized_at: r.get(2)?,
                content_hash_at_materialize: r.get(3)?,
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    })
    .await
}

// Drop the row. Caller is responsible for unlinking the on-disk file.
// Separating bookkeeping from filesystem ops keeps this module trivially
// testable and lets the CLI handle real-world filesystem errors.
pub async fn forget(db: &Database, skill_id: i64) -> Result<usize> {
    db.write(move |conn| {
        let n = conn.execute(
            "DELETE FROM skill_materializations WHERE skill_id = ?1",
            params![skill_id],
        )?;
        Ok(n)
    })
    .await
}
