//! Skill bundles -- named, ordered collections of skills.
//!
//! Two flavors:
//! - **Auto-generated** (one per plugin) so users can "load all of
//!   superpowers" or browse a plugin's contributions as a unit.
//! - **User-curated** (e.g. "frontend pack" mixing elite-frontend-ux,
//!   frontend-design, ui-mobile-design) for cross-plugin workflows.
//!
//! Schema lives in tenant migration v50: `skill_bundles` (id, name UNIQUE,
//! description, auto_generated flag, created_at, updated_at) +
//! `skill_bundle_members` (bundle_id, skill_id, added_at; PK on the pair).

use crate::db::Database;
use crate::{EngError, Result};
use rusqlite::params;
use serde::{Deserialize, Serialize};

// One bundle row.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillBundle {
    pub id: i64,
    pub name: String,
    pub description: Option<String>,
    pub auto_generated: bool,
    pub created_at: String,
    pub updated_at: String,
}

// A bundle plus the count of skills it contains. Cheaper than fetching
// every member just to render a list.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BundleSummary {
    pub bundle: SkillBundle,
    pub member_count: i64,
}

// Create a new bundle. Returns the row id (existing if name collides --
// idempotent for the importer's per-plugin auto bundles).
pub async fn create_bundle(
    db: &Database,
    name: &str,
    description: Option<&str>,
    auto_generated: bool,
) -> Result<i64> {
    let name = name.trim().to_string();
    if name.is_empty() {
        return Err(EngError::InvalidInput("bundle name cannot be empty".into()));
    }
    let description = description.map(|s| s.to_string());
    let auto_flag: i32 = if auto_generated { 1 } else { 0 };
    db.write(move |conn| {
        // INSERT OR IGNORE so re-running the importer doesn't error out;
        // existing bundles keep their description / created_at.
        conn.execute(
            "INSERT OR IGNORE INTO skill_bundles (name, description, auto_generated) \
             VALUES (?1, ?2, ?3)",
            params![name, description, auto_flag],
        )?;
        let id: i64 = conn.query_row(
            "SELECT id FROM skill_bundles WHERE name = ?1",
            params![name],
            |r| r.get(0),
        )?;
        Ok(id)
    })
    .await
}

// Get a bundle by id.
pub async fn get_bundle(db: &Database, id: i64) -> Result<SkillBundle> {
    db.read(move |conn| {
        conn.query_row(
            "SELECT id, name, description, auto_generated, created_at, updated_at \
             FROM skill_bundles WHERE id = ?1",
            params![id],
            row_to_bundle,
        )
        .map_err(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => {
                EngError::NotFound(format!("bundle {id} not found"))
            }
            other => EngError::DatabaseMessage(other.to_string()),
        })
    })
    .await
}

// Get a bundle by canonical name (e.g. plugin name for auto bundles).
pub async fn get_bundle_by_name(db: &Database, name: &str) -> Result<SkillBundle> {
    let name = name.to_string();
    db.read(move |conn| {
        conn.query_row(
            "SELECT id, name, description, auto_generated, created_at, updated_at \
             FROM skill_bundles WHERE name = ?1",
            params![name],
            row_to_bundle,
        )
        .map_err(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => {
                EngError::NotFound(format!("bundle '{name}' not found"))
            }
            other => EngError::DatabaseMessage(other.to_string()),
        })
    })
    .await
}

// List bundles with their member counts. Newest first.
pub async fn list_bundles(db: &Database, limit: usize) -> Result<Vec<BundleSummary>> {
    let limit_i = limit as i64;
    db.read(move |conn| {
        let mut stmt = conn.prepare(
            "SELECT b.id, b.name, b.description, b.auto_generated, b.created_at, \
                        b.updated_at, \
                        (SELECT COUNT(*) FROM skill_bundle_members m WHERE m.bundle_id = b.id) \
                 FROM skill_bundles b \
                 ORDER BY b.created_at DESC, b.id DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit_i], |r| {
            Ok(BundleSummary {
                bundle: SkillBundle {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    description: r.get(2)?,
                    auto_generated: r.get::<_, i32>(3)? != 0,
                    created_at: r.get(4)?,
                    updated_at: r.get(5)?,
                },
                member_count: r.get(6)?,
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

// Add a skill to a bundle. Idempotent via PK.
pub async fn add_member(db: &Database, bundle_id: i64, skill_id: i64) -> Result<()> {
    db.write(move |conn| {
        conn.execute(
            "INSERT OR IGNORE INTO skill_bundle_members (bundle_id, skill_id) \
             VALUES (?1, ?2)",
            params![bundle_id, skill_id],
        )?;
        Ok(())
    })
    .await
}

// Bulk add. Used by the importer when an entire plugin's worth of skills
// gets ingested in one pass.
pub async fn add_members(db: &Database, bundle_id: i64, skill_ids: &[i64]) -> Result<()> {
    if skill_ids.is_empty() {
        return Ok(());
    }
    let ids = skill_ids.to_vec();
    db.write(move |conn| {
        let tx = conn.unchecked_transaction()?;
        for sid in &ids {
            tx.execute(
                "INSERT OR IGNORE INTO skill_bundle_members (bundle_id, skill_id) \
                 VALUES (?1, ?2)",
                params![bundle_id, sid],
            )?;
        }
        tx.commit()?;
        Ok(())
    })
    .await
}

// Remove one skill from a bundle. Returns rows deleted (0 or 1).
pub async fn remove_member(db: &Database, bundle_id: i64, skill_id: i64) -> Result<usize> {
    db.write(move |conn| {
        let n = conn.execute(
            "DELETE FROM skill_bundle_members WHERE bundle_id = ?1 AND skill_id = ?2",
            params![bundle_id, skill_id],
        )?;
        Ok(n)
    })
    .await
}

// List all skill ids in a bundle, ordered by when they were added.
pub async fn list_members(db: &Database, bundle_id: i64) -> Result<Vec<i64>> {
    db.read(move |conn| {
        let mut stmt = conn.prepare(
            "SELECT skill_id FROM skill_bundle_members \
                 WHERE bundle_id = ?1 ORDER BY added_at ASC, skill_id ASC",
        )?;
        let rows = stmt.query_map(params![bundle_id], |r| r.get::<_, i64>(0))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    })
    .await
}

// Drop a bundle plus its membership rows (FK cascade handles members).
pub async fn delete_bundle(db: &Database, bundle_id: i64) -> Result<()> {
    db.write(move |conn| {
        conn.execute(
            "DELETE FROM skill_bundles WHERE id = ?1",
            params![bundle_id],
        )?;
        Ok(())
    })
    .await
}

// Row mapper kept private; query_row callers go through this so the
// column order stays in one place.
fn row_to_bundle(r: &rusqlite::Row<'_>) -> rusqlite::Result<SkillBundle> {
    Ok(SkillBundle {
        id: r.get(0)?,
        name: r.get(1)?,
        description: r.get(2)?,
        auto_generated: r.get::<_, i32>(3)? != 0,
        created_at: r.get(4)?,
        updated_at: r.get(5)?,
    })
}
