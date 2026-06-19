pub use super::types::{BrainEdge, EdgeType};

use crate::db::Database;
use crate::{EngError, Result};

// ---------------------------------------------------------------------------
// Database CRUD
// ---------------------------------------------------------------------------

/// Insert a new edge. If the (source, target, type) triple already exists,
/// update the weight to the max of old and new. Scoped to `user_id`.
#[tracing::instrument(skip(db), fields(source_id, target_id, weight, edge_type = ?edge_type, user_id))]
pub async fn store_edge(
    db: &Database,
    source_id: i64,
    target_id: i64,
    weight: f32,
    edge_type: EdgeType,
    user_id: i64,
) -> Result<()> {
    let edge_type_str = edge_type.to_string();
    db.write(move |conn| {
        conn.execute(
            "INSERT INTO brain_edges (source_id, target_id, weight, edge_type, user_id) \
             VALUES (?1, ?2, ?3, ?4, ?5) \
             ON CONFLICT(source_id, target_id, edge_type) \
             DO UPDATE SET weight = MAX(weight, excluded.weight)",
            rusqlite::params![source_id, target_id, weight as f64, edge_type_str, user_id],
        )?;
        Ok(())
    })
    .await
}

/// Get all edges originating from a given pattern, scoped to `user_id`.
#[tracing::instrument(skip(db), fields(source_id, user_id))]
pub async fn get_edges_from(db: &Database, source_id: i64, user_id: i64) -> Result<Vec<BrainEdge>> {
    db.read(move |conn| {
        let mut stmt = conn.prepare(
            "SELECT id, source_id, target_id, weight, edge_type, user_id, created_at \
                 FROM brain_edges WHERE source_id = ?1 AND user_id = ?2",
        )?;

        let edges = stmt
            .query_map(rusqlite::params![source_id, user_id], |row| {
                Ok(row_to_edge_raw(row))
            })?
            .map(|r| r.map_err(EngError::from).and_then(|inner| inner))
            .collect::<Result<Vec<BrainEdge>>>()?;

        Ok(edges)
    })
    .await
}

/// Get all edges connected to a pattern (either direction), scoped to `user_id`.
#[tracing::instrument(skip(db), fields(pattern_id, user_id))]
pub async fn get_edges_for(db: &Database, pattern_id: i64, user_id: i64) -> Result<Vec<BrainEdge>> {
    db.read(move |conn| {
        let mut stmt = conn.prepare(
            "SELECT id, source_id, target_id, weight, edge_type, user_id, created_at \
                 FROM brain_edges \
                 WHERE (source_id = ?1 OR target_id = ?1) AND user_id = ?2",
        )?;

        let edges = stmt
            .query_map(rusqlite::params![pattern_id, user_id], |row| {
                Ok(row_to_edge_raw(row))
            })?
            .map(|r| r.map_err(EngError::from).and_then(|inner| inner))
            .collect::<Result<Vec<BrainEdge>>>()?;

        Ok(edges)
    })
    .await
}

/// Strengthen an edge by adding a Hebbian boost. The weight is clamped
/// to [0, 1]. Scoped to `user_id` in the UPDATE WHERE clause.
#[tracing::instrument(skip(db), fields(source_id, target_id, edge_type = ?edge_type, boost, user_id))]
pub async fn strengthen_edge(
    db: &Database,
    source_id: i64,
    target_id: i64,
    edge_type: EdgeType,
    boost: f32,
    user_id: i64,
) -> Result<()> {
    let edge_type_str = edge_type.to_string();
    let affected = db
        .write(move |conn| {
            let n = conn.execute(
                "UPDATE brain_edges \
                     SET weight = MIN(1.0, weight + ?1) \
                     WHERE source_id = ?2 AND target_id = ?3 \
                       AND edge_type = ?4 AND user_id = ?5",
                rusqlite::params![boost as f64, source_id, target_id, edge_type_str, user_id],
            )?;
            Ok(n)
        })
        .await?;

    if affected == 0 {
        // Edge doesn't exist yet -- create it with the boost as initial weight.
        store_edge(db, source_id, target_id, boost, edge_type, user_id).await?;
    }
    Ok(())
}

/// Decay all edge weights for `user_id` by multiplying with the given rate.
/// Returns the number of affected edges.
#[tracing::instrument(skip(db), fields(user_id, rate))]
pub async fn decay_edges(db: &Database, user_id: i64, rate: f32) -> Result<usize> {
    db.write(move |conn| {
        let n = conn.execute(
            "UPDATE brain_edges SET weight = weight * ?1 WHERE user_id = ?2",
            rusqlite::params![rate as f64, user_id],
        )?;
        Ok(n)
    })
    .await
}

/// Remove edges for `user_id` whose weight has fallen below the threshold.
/// Returns the number of pruned edges.
#[tracing::instrument(skip(db), fields(user_id, threshold))]
pub async fn prune_edges(db: &Database, user_id: i64, threshold: f32) -> Result<usize> {
    db.write(move |conn| {
        let n = conn.execute(
            "DELETE FROM brain_edges WHERE weight < ?1 AND user_id = ?2",
            rusqlite::params![threshold as f64, user_id],
        )?;
        Ok(n)
    })
    .await
}

/// Count edges belonging to `user_id`.
#[tracing::instrument(skip(db), fields(user_id))]
pub async fn count_edges(db: &Database, user_id: i64) -> Result<i64> {
    db.read(move |conn| {
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM brain_edges WHERE user_id = ?1",
            rusqlite::params![user_id],
            |row| row.get(0),
        )?;
        Ok(count)
    })
    .await
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Map a result row from brain_edges into a `BrainEdge`. Column order must
/// match the SELECT list: id, source_id, target_id, weight, edge_type,
/// user_id, created_at.
fn row_to_edge_raw(row: &rusqlite::Row<'_>) -> Result<BrainEdge> {
    let id: i64 = row.get(0)?;
    let source_id: i64 = row.get(1)?;
    let target_id: i64 = row.get(2)?;
    let weight: f64 = row.get(3)?;
    let edge_type_str: String = row.get(4)?;
    let user_id: i64 = row.get(5)?;
    let created_at: String = row.get(6)?;

    Ok(BrainEdge {
        id,
        source_id,
        target_id,
        weight: weight as f32,
        edge_type: EdgeType::from_str_loose(&edge_type_str),
        user_id,
        created_at,
    })
}
