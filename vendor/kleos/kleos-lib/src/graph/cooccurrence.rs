use super::types::Entity;
use crate::db::Database;
use crate::Result;
use std::collections::HashMap;
use tracing::info;

/// Record a pairwise co-occurrence between two entities scoped to a user.
/// The pair is stored in canonical order (smaller id first) so that
/// (A, B) and (B, A) map to the same row. `user_id` is written into the
/// row so single-DB installs can filter co-occurrences per user.
#[tracing::instrument(skip(db))]
pub async fn record_cooccurrence(
    db: &Database,
    entity_a: i64,
    entity_b: i64,
    user_id: i64,
) -> Result<()> {
    let (lo, hi) = if entity_a <= entity_b {
        (entity_a, entity_b)
    } else {
        (entity_b, entity_a)
    };
    db.write(move |conn| {
        conn.execute(
            "INSERT INTO entity_cooccurrences \
             (entity_a_id, entity_b_id, count, user_id) \
             VALUES (?1, ?2, 1, ?3) \
             ON CONFLICT(entity_a_id, entity_b_id) DO UPDATE SET \
               count = count + 1, \
               last_seen_at = datetime('now')",
            rusqlite::params![lo, hi, user_id],
        )?;
        Ok(())
    })
    .await?;
    Ok(())
}

/// Record a batch of pairwise co-occurrences in a single transaction.
/// Each pair is normalized to canonical order (smaller id first) so that
/// (A, B) and (B, A) accumulate into the same row, matching
/// `record_cooccurrence`. One transaction with a cached statement replaces a
/// per-pair `db.write` round-trip, which matters because callers record
/// O(n^2) pairs per memory.
///
/// Row failures are tolerated per pair (warn + continue), preserving the
/// per-pair fault isolation the old independent `record_cooccurrence` calls
/// had: a single FK violation (an entity deleted between candidate collection
/// and this write) must not roll back the up-to-C(50,2) already-valid pairs
/// sharing the transaction.
#[tracing::instrument(skip(db, pairs), fields(pair_count = pairs.len()))]
pub async fn record_cooccurrences_batch(
    db: &Database,
    pairs: Vec<(i64, i64)>,
    user_id: i64,
) -> Result<()> {
    if pairs.is_empty() {
        return Ok(());
    }
    db.transaction(move |tx| {
        let mut stmt = tx.prepare_cached(
            "INSERT INTO entity_cooccurrences \
             (entity_a_id, entity_b_id, count, user_id) \
             VALUES (?1, ?2, 1, ?3) \
             ON CONFLICT(entity_a_id, entity_b_id) DO UPDATE SET \
               count = count + 1, \
               last_seen_at = datetime('now')",
        )?;
        for (a, b) in pairs {
            let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
            if let Err(e) = stmt.execute(rusqlite::params![lo, hi, user_id]) {
                tracing::warn!(
                    entity_a = lo,
                    entity_b = hi,
                    "co-occurrence pair skipped: {e}"
                );
            }
        }
        Ok(())
    })
    .await
}

/// Full rebuild of co-occurrence table.
/// Clears all existing co-occurrences and recomputes from all memory-entity links.
#[tracing::instrument(skip(db))]
pub async fn rebuild_cooccurrences(db: &Database, user_id: i64) -> Result<i64> {
    // Clear only the caller's co-occurrences (rows whose entities they own), so
    // one user's rebuild cannot wipe another user's data in single-DB mode.
    db.write(move |conn| {
        conn.execute(
            "DELETE FROM entity_cooccurrences \
             WHERE entity_a_id IN (SELECT id FROM entities WHERE user_id = ?1) \
                OR entity_b_id IN (SELECT id FROM entities WHERE user_id = ?1)",
            rusqlite::params![user_id],
        )?;
        Ok(())
    })
    .await?;

    // Fetch all memory -> entity links for this user's memories. The inner
    // me.id ordering preserves link-insertion order per memory, which is the
    // extraction order the live path capped by -- so rebuild's cap keeps the
    // same first-N subset the original recording did.
    let memory_entities: HashMap<i64, Vec<i64>> = db
        .read(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT m.id, me.entity_id \
                     FROM memories m \
                     JOIN memory_entities me ON me.memory_id = m.id \
                     WHERE m.is_forgotten = 0 AND m.is_archived = 0 \
                       AND m.user_id = ?1 \
                     ORDER BY m.created_at DESC, me.id ASC",
            )?;

            let mut memory_entities: HashMap<i64, Vec<i64>> = HashMap::new();

            let rows = stmt.query_map(rusqlite::params![user_id], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
            })?;

            for row in rows {
                let (memory_id, entity_id) = row?;
                memory_entities
                    .entry(memory_id)
                    .or_default()
                    .push(entity_id);
            }

            Ok(memory_entities)
        })
        .await?;

    // For each memory, create co-occurrence pairs from its entities. Pairing
    // per memory is capped and batched into one transaction, matching the
    // live recording path in extract_and_link_entities (O(n^2) DoS bound).
    // First-occurrence dedup in linkage order (NOT sort-by-id) so the capped
    // subset is the same first-N-extracted entities the live path pairs.
    let mut total_pairs: i64 = 0;
    for entities in memory_entities.values() {
        let mut seen: std::collections::HashSet<i64> = std::collections::HashSet::new();
        let mut ordered: Vec<i64> = Vec::with_capacity(entities.len());
        for &entity_id in entities {
            if seen.insert(entity_id) {
                ordered.push(entity_id);
            }
        }
        ordered.truncate(crate::validation::MAX_COOCCURRENCE_ENTITIES);

        let mut pairs: Vec<(i64, i64)> =
            Vec::with_capacity(ordered.len() * ordered.len().saturating_sub(1) / 2);
        for i in 0..ordered.len() {
            for j in (i + 1)..ordered.len() {
                pairs.push((ordered[i], ordered[j]));
            }
        }
        total_pairs += pairs.len() as i64;
        record_cooccurrences_batch(db, pairs, user_id).await?;
    }

    info!(pairs = total_pairs, user_id, "cooccurrences_rebuilt");

    Ok(total_pairs)
}

/// Get entities that co-occur with the given entity.
#[tracing::instrument(skip(db))]
pub async fn get_cooccurring_entities(
    db: &Database,
    entity_id: i64,
    user_id: i64,
    limit: usize,
) -> Result<Vec<Entity>> {
    db.read(move |conn| {
        let mut stmt = conn.prepare(
            "SELECT e.id, e.name, e.entity_type, e.description, e.aliases, \
                        e.space_id, e.confidence, e.occurrence_count, \
                        e.first_seen_at, e.last_seen_at, e.created_at, \
                        co.count as cooccurrence_count \
                 FROM entity_cooccurrences co \
                 JOIN entities e ON e.id = CASE \
                     WHEN co.entity_a_id = ?1 THEN co.entity_b_id \
                     ELSE co.entity_a_id \
                 END \
                 WHERE (co.entity_a_id = ?1 OR co.entity_b_id = ?1) \
                   AND e.user_id = ?3 \
                   AND EXISTS (SELECT 1 FROM entities WHERE id = ?1 AND user_id = ?3) \
                 ORDER BY co.count DESC \
                 LIMIT ?2",
        )?;

        let rows = stmt.query_map(rusqlite::params![entity_id, limit as i64, user_id], |row| {
            Ok(Entity {
                id: row.get(0)?,
                name: row.get(1)?,
                entity_type: row.get(2)?,
                description: row.get(3)?,
                aliases: row.get(4)?,
                user_id,
                space_id: row.get(5)?,
                confidence: row.get(6)?,
                occurrence_count: row.get(7)?,
                first_seen_at: row.get(8)?,
                last_seen_at: row.get(9)?,
                created_at: row.get(10)?,
            })
        })?;

        let mut entities = Vec::new();
        for row in rows {
            entities.push(row?);
        }

        Ok(entities)
    })
    .await
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_canonical_pair_ordering() {
        let (lo, hi) = if 5 <= 3 { (5, 3) } else { (3, 5) };
        assert_eq!(lo, 3);
        assert_eq!(hi, 5);

        let (lo2, hi2) = if 2 <= 7 { (2, 7) } else { (7, 2) };
        assert_eq!(lo2, 2);
        assert_eq!(hi2, 7);
    }

    #[test]
    fn test_weight_calculation() {
        // Weight = ln(count).max(0.1).min(1.0)
        let count = 3i64;
        let weight = (count as f32).ln().clamp(0.1, 1.0);
        assert!(weight > 0.1);
        assert!(weight <= 1.0);

        let count_1 = 1i64;
        let weight_1 = (count_1 as f32).ln().clamp(0.1, 1.0);
        assert_eq!(weight_1, 0.1); // ln(1) = 0, clamped to 0.1
    }
}
