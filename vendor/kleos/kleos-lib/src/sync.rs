//! Sync receive -- apply changes from another kleos instance.

use serde::{Deserialize, Serialize};

use crate::db::Database;
use crate::memory;
use crate::memory::types::StoreRequest;
use crate::Result;

/// One change record received from a peer kleos instance during sync.
#[derive(Debug, Deserialize)]
pub struct SyncReceiveChange {
    pub sync_id: String,
    pub change_type: String,
    pub content: Option<String>,
    pub category: Option<String>,
    pub importance: Option<i32>,
    pub timestamp: Option<String>,
}

/// Summary of how many received sync changes were applied versus skipped.
#[derive(Debug, Serialize)]
pub struct SyncReceiveResult {
    pub applied: i64,
    pub skipped: i64,
}

/// Apply a batch of received sync changes for `user_id`, deduping inserts and
/// scoping every read and write to that tenant's rows.
#[tracing::instrument(skip(db, changes), fields(change_count = changes.len()))]
pub async fn receive_sync(
    db: &Database,
    user_id: i64,
    changes: Vec<SyncReceiveChange>,
) -> Result<SyncReceiveResult> {
    let mut applied = 0i64;
    let mut skipped = 0i64;

    for change in &changes {
        match change.change_type.as_str() {
            "insert" | "update" => {
                let content = match change.content.as_deref().filter(|c| !c.trim().is_empty()) {
                    Some(c) => c.to_string(),
                    None => {
                        skipped += 1;
                        continue;
                    }
                };

                let sync_id = change.sync_id.clone();
                // Scope the dedup probe to this tenant. In shared-monolith mode sync_id is
                // not globally unique, so an unscoped probe (a) leaks the existence of
                // another tenant's row and (b) wrongly skips this tenant's own insert when a
                // different tenant already holds the same sync_id.
                let exists = db
                    .read(move |conn| {
                        let mut stmt = conn.prepare(
                            "SELECT id FROM memories WHERE sync_id = ?1 AND user_id = ?2",
                        )?;
                        let mut rows = stmt.query(rusqlite::params![sync_id, user_id])?;
                        Ok(rows.next()?.is_some())
                    })
                    .await?;
                if exists {
                    skipped += 1;
                    continue;
                }

                let req = StoreRequest {
                    content,
                    category: change
                        .category
                        .clone()
                        .unwrap_or_else(|| "general".to_string()),
                    source: "sync".to_string(),
                    importance: change.importance.unwrap_or(5),
                    user_id: Some(user_id),
                    sync_id: Some(change.sync_id.clone()),
                    ..Default::default()
                };
                memory::store(db, req, None, false).await?;
                applied += 1;
            }
            "delete" => {
                let sync_id = change.sync_id.clone();
                // Scope the soft-delete to this tenant. Without the user_id predicate any
                // tenant could forget another tenant's memory by guessing/replaying a
                // sync_id in shared-monolith mode.
                let affected = db
                    .write(move |conn| {
                        Ok(conn.execute(
                            "UPDATE memories SET is_forgotten = 1 \
                             WHERE sync_id = ?1 AND user_id = ?2",
                            rusqlite::params![sync_id, user_id],
                        )?)
                    })
                    .await?;
                if affected > 0 {
                    applied += 1;
                } else {
                    skipped += 1;
                }
            }
            _ => {
                skipped += 1;
            }
        }
    }

    Ok(SyncReceiveResult { applied, skipped })
}
