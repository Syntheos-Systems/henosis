pub use super::types::BrainPattern;

use crate::db::Database;
use crate::{EngError, Result};

// ---------------------------------------------------------------------------
// Blob serialization (little-endian f32)
// ---------------------------------------------------------------------------

pub fn pattern_to_blob(pattern: &[f32]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(pattern.len() * 4);
    for &f in pattern {
        buf.extend_from_slice(&f.to_le_bytes());
    }
    buf
}

pub fn blob_to_pattern(blob: &[u8]) -> Vec<f32> {
    blob.chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

// ---------------------------------------------------------------------------
// Database CRUD
// ---------------------------------------------------------------------------

/// Insert or replace a pattern row. Uses INSERT OR REPLACE so calling
/// with an existing id updates in place.
#[tracing::instrument(skip(db, pattern), fields(pattern_id = pattern.id, user_id = pattern.user_id, strength = pattern.strength, importance = pattern.importance))]
pub async fn store_pattern(db: &Database, pattern: &BrainPattern) -> Result<()> {
    let blob = pattern_to_blob(&pattern.pattern);
    let id = pattern.id;
    let user_id = pattern.user_id;
    let strength = pattern.strength as f64;
    let importance = pattern.importance;
    let access_count = pattern.access_count;
    let last_activated_at = pattern.last_activated_at.clone();
    let created_at = pattern.created_at.clone();

    db.write(move |conn| {
        conn.execute(
            "INSERT OR REPLACE INTO brain_patterns \
             (id, user_id, pattern, strength, importance, access_count, \
              last_activated_at, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            rusqlite::params![
                id,
                user_id,
                blob,
                strength,
                importance,
                access_count,
                last_activated_at,
                created_at
            ],
        )?;
        Ok(())
    })
    .await
}

/// Load a single pattern by id and user_id.
#[tracing::instrument(skip(db), fields(pattern_id = id, user_id))]
pub async fn get_pattern(db: &Database, id: i64, user_id: i64) -> Result<BrainPattern> {
    db.read(move |conn| {
        let mut stmt = conn.prepare(
            "SELECT id, user_id, pattern, strength, importance, access_count, \
                        last_activated_at, created_at \
                 FROM brain_patterns WHERE id = ?1 AND user_id = ?2",
        )?;

        let mut rows = stmt.query(rusqlite::params![id, user_id])?;

        let row = rows
            .next()?
            .ok_or_else(|| EngError::NotFound(format!("brain pattern {}", id)))?;

        row_to_pattern(row)
    })
    .await
}

/// Load all patterns across every user. Used to populate the in-memory
/// network at startup so a single shared backend can serve multi-tenant
/// queries with per-tenant filtering at recall time.
#[tracing::instrument(skip(db))]
pub async fn list_all_patterns(db: &Database) -> Result<Vec<BrainPattern>> {
    db.read(move |conn| {
        let mut stmt = conn.prepare(
            "SELECT id, user_id, pattern, strength, importance, access_count, \
                        last_activated_at, created_at \
                 FROM brain_patterns \
                 ORDER BY id",
        )?;

        let patterns = stmt
            .query_map([], |row| Ok(row_to_pattern(row)))?
            .map(|r| r.map_err(EngError::from).and_then(|inner| inner))
            .collect::<Result<Vec<BrainPattern>>>()?;

        Ok(patterns)
    })
    .await
}

/// Load all patterns for a user. Used to populate the in-memory network
/// at startup.
#[tracing::instrument(skip(db), fields(user_id))]
pub async fn list_patterns(db: &Database, user_id: i64) -> Result<Vec<BrainPattern>> {
    db.read(move |conn| {
        let mut stmt = conn.prepare(
            "SELECT id, user_id, pattern, strength, importance, access_count, \
                        last_activated_at, created_at \
                 FROM brain_patterns WHERE user_id = ?1 \
                 ORDER BY id",
        )?;

        let patterns = stmt
            .query_map(rusqlite::params![user_id], |row| Ok(row_to_pattern(row)))?
            .map(|r| r.map_err(EngError::from).and_then(|inner| inner))
            .collect::<Result<Vec<BrainPattern>>>()?;

        Ok(patterns)
    })
    .await
}

/// Update the strength (decay_factor) of a pattern.
#[tracing::instrument(skip(db), fields(pattern_id = id, user_id, strength))]
pub async fn update_strength(db: &Database, id: i64, user_id: i64, strength: f32) -> Result<()> {
    db.write(move |conn| {
        let affected = conn.execute(
            "UPDATE brain_patterns SET strength = ?1 WHERE id = ?2 AND user_id = ?3",
            rusqlite::params![strength as f64, id, user_id],
        )?;

        if affected == 0 {
            return Err(EngError::NotFound(format!("brain pattern {}", id)));
        }
        Ok(())
    })
    .await
}

/// Increment access_count and set last_activated_at to now.
#[tracing::instrument(skip(db), fields(pattern_id = id, user_id))]
pub async fn touch_pattern(db: &Database, id: i64, user_id: i64) -> Result<()> {
    db.write(move |conn| {
        conn.execute(
            "UPDATE brain_patterns \
             SET access_count = access_count + 1, \
                 last_activated_at = datetime('now') \
             WHERE id = ?1 AND user_id = ?2",
            rusqlite::params![id, user_id],
        )?;
        Ok(())
    })
    .await
}

/// Delete a single pattern.
#[tracing::instrument(skip(db), fields(pattern_id = id, user_id))]
pub async fn delete_pattern(db: &Database, id: i64, user_id: i64) -> Result<()> {
    db.write(move |conn| {
        conn.execute(
            "DELETE FROM brain_patterns WHERE id = ?1 AND user_id = ?2",
            rusqlite::params![id, user_id],
        )?;
        // Also clean up edges referencing this pattern
        conn.execute(
            "DELETE FROM brain_edges WHERE (source_id = ?1 OR target_id = ?1)",
            rusqlite::params![id],
        )?;
        Ok(())
    })
    .await
}

/// Delete all patterns whose strength is below the given threshold.
/// Returns the number of deleted patterns.
#[tracing::instrument(skip(db), fields(user_id, threshold))]
pub async fn delete_weak_patterns(db: &Database, user_id: i64, threshold: f32) -> Result<usize> {
    db.write(move |conn| {
        // First collect IDs so we can clean edges
        let mut stmt =
            conn.prepare("SELECT id FROM brain_patterns WHERE user_id = ?1 AND strength < ?2")?;

        let dead_ids: Vec<i64> = stmt
            .query_map(rusqlite::params![user_id, threshold as f64], |row| {
                row.get(0)
            })?
            .map(|r| r.map_err(EngError::from))
            .collect::<Result<Vec<i64>>>()?;

        if dead_ids.is_empty() {
            return Ok(0);
        }

        let count = dead_ids.len();

        // Delete patterns
        conn.execute(
            "DELETE FROM brain_patterns WHERE user_id = ?1 AND strength < ?2",
            rusqlite::params![user_id, threshold as f64],
        )?;

        // Clean edges referencing dead patterns
        for id in &dead_ids {
            conn.execute(
                "DELETE FROM brain_edges WHERE (source_id = ?1 OR target_id = ?1)",
                rusqlite::params![*id],
            )?;
        }

        Ok(count)
    })
    .await
}

/// Count patterns for a user.
#[tracing::instrument(skip(db), fields(user_id))]
pub async fn count_patterns(db: &Database, user_id: i64) -> Result<i64> {
    db.read(move |conn| {
        Ok(conn.query_row(
            "SELECT COUNT(*) FROM brain_patterns WHERE user_id = ?1",
            rusqlite::params![user_id],
            |row| row.get(0),
        )?)
    })
    .await
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn row_to_pattern(row: &rusqlite::Row<'_>) -> Result<BrainPattern> {
    let id: i64 = row.get(0)?;
    let user_id: i64 = row.get(1)?;
    let blob: Vec<u8> = row.get(2)?;
    let strength: f64 = row.get(3)?;
    let importance: i32 = row.get(4)?;
    let access_count: i32 = row.get(5)?;
    let last_activated_at: Option<String> = row.get(6)?;
    let created_at: String = row.get(7)?;

    Ok(BrainPattern {
        id,
        user_id,
        pattern: blob_to_pattern(&blob),
        strength: strength as f32,
        importance,
        access_count,
        last_activated_at,
        created_at,
    })
}
