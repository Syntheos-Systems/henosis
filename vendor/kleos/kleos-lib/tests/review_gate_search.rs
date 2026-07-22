//! Review-gate regression: `status = 'pending'` memories must never surface
//! through the semantic search / recall choke point (`hybrid_search`), not only
//! through the `list` path that `memory::tests::recall_withholds_pending_memories`
//! already covers.
//!
//! Guards the fix in `kleos-lib/src/memory/search.rs`: `hydrate_candidates`
//! filtered `status != 'pending'` but the hydration loop only *updated* existing
//! candidates and never *removed* the ones that failed to hydrate, so a pending
//! memory surfaced by the FTS (or vector) channel stayed in the pool, survived
//! every downstream retain, and reached `fetch_memories_batch` -- which had no
//! status predicate. The test drives the FTS channel (no embedder needed) so it
//! fails on the pre-fix code and passes once pending is dropped at hydration and
//! backstopped at the content fetch.

use kleos_lib::db::Database;
use kleos_lib::memory;
use kleos_lib::memory::search::hybrid_search;
use kleos_lib::memory::types::{SearchRequest, StoreRequest};
use rusqlite::params;

/// Build a minimal, embedding-free store request for one owned test memory.
fn store_req(content: &str, user_id: i64) -> StoreRequest {
    StoreRequest {
        content: content.to_string(),
        category: "general".to_string(),
        source: "test".to_string(),
        importance: 5,
        tags: None,
        embedding: None,
        chunk_embeddings: None,
        session_id: None,
        is_static: Some(false),
        user_id: Some(user_id),
        space_id: None,
        parent_memory_id: None,
        sync_id: None,
        artifacts: None,
        created_at: None,
    }
}

/// Persist one owned test memory (FTS-synced via trigger) and return its row id.
async fn store_memory(db: &Database, content: &str, user_id: i64) -> i64 {
    memory::store(db, store_req(content, user_id), None, false)
        .await
        .expect("store memory")
        .id
}

/// Force a stored memory into the review-gate pending state.
async fn mark_pending(db: &Database, id: i64) {
    db.write(move |conn| {
        conn.execute(
            "UPDATE memories SET status = 'pending' WHERE id = ?1",
            params![id],
        )?;
        Ok(())
    })
    .await
    .expect("mark pending");
}

/// A pending memory that matches the query on the FTS channel must be withheld
/// from `hybrid_search` output, while the approved sibling still surfaces.
#[tokio::test]
async fn hybrid_search_withholds_pending_memories() {
    let db = Database::connect_memory().await.expect("in-mem db");
    let user_id = 1;

    let approved = store_memory(
        &db,
        "apple pie recipe with a flaky butter crust and cinnamon sugar",
        user_id,
    )
    .await;
    let pending = store_memory(
        &db,
        "apple pie recipe awaiting review, tart green apples and fresh nutmeg",
        user_id,
    )
    .await;
    mark_pending(&db, pending).await;

    let results = hybrid_search(
        &db,
        SearchRequest {
            query: "apple pie recipe".to_string(),
            user_id: Some(user_id),
            limit: Some(10),
            ..Default::default()
        },
    )
    .await
    .expect("search");

    let ids: Vec<i64> = results.iter().map(|r| r.memory.id).collect();
    assert!(
        ids.contains(&approved),
        "approved memory must surface through search (ids: {ids:?})"
    );
    assert!(
        !ids.contains(&pending),
        "pending memory must NOT leak through the search choke point (ids: {ids:?})"
    );
}
