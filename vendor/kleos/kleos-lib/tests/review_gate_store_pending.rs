//! Review-gate store plumbing: `memory::store` must resolve the gate status once
//! and surface it on `StoreResult.pending`, and the persisted row's `status` must
//! match that decision. This is the single source of truth the store and inbox
//! routes rely on to defer fact/entity/brain derivation for unreviewed memories
//! (spawn_post_store_derivation is skipped while pending and re-run on approve).
//!
//! Dedicated single-test binary: the KLEOS_REVIEW_GATE_* switches are read once
//! into process-wide LazyLock statics, so the gate is enabled here before the
//! first store and this file holds exactly one test to avoid a cross-test env
//! race on that one-shot initialization.

use kleos_lib::db::Database;
use kleos_lib::memory;
use kleos_lib::memory::types::StoreRequest;
use rusqlite::params;

/// Build an embedding-free store request for one owned test memory at a given
/// importance.
fn store_req(content: &str, user_id: i64, importance: i32) -> StoreRequest {
    StoreRequest {
        content: content.to_string(),
        category: "general".to_string(),
        source: "test".to_string(),
        importance,
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

/// Read a memory's persisted review-gate status directly.
async fn status_of(db: &Database, id: i64) -> String {
    db.read(move |conn| {
        Ok(conn.query_row(
            "SELECT status FROM memories WHERE id = ?1",
            params![id],
            |r| r.get::<_, String>(0),
        )?)
    })
    .await
    .expect("read status")
}

/// With the gate enabled and an importance threshold of 7, a high-importance
/// store is held for review and `StoreResult.pending` reports it; a low-importance
/// store is approved and reports not pending. In both cases the persisted row's
/// status matches the flag, proving store resolves the decision once.
#[tokio::test]
async fn store_surfaces_gate_status_and_persists_it() {
    // Enable the gate BEFORE the first store so the LazyLock statics initialize
    // with these values. Empty source allowlist so only the importance threshold
    // gates, independent of any KLEOS_REVIEW_GATE_SOURCES inherited from the env.
    std::env::set_var("KLEOS_REVIEW_GATE_ENABLED", "1");
    std::env::set_var("KLEOS_REVIEW_GATE_SOURCES", "");
    std::env::set_var("KLEOS_REVIEW_GATE_IMPORTANCE_THRESHOLD", "7");

    let db = Database::connect_memory().await.expect("in-mem db");
    let user_id = 1;

    // Importance 9 > 7: gated, so the memory is held for review.
    let gated = memory::store(
        &db,
        store_req("high-importance guess", user_id, 9),
        None,
        false,
    )
    .await
    .expect("store gated");
    assert!(gated.created, "a new memory was created");
    assert!(
        gated.pending,
        "importance 9 above the threshold must be held for review (pending)"
    );
    assert_eq!(
        status_of(&db, gated.id).await,
        "pending",
        "the persisted status must match StoreResult.pending (single source of truth)"
    );

    // Importance 3 <= 7: not gated, so the memory is immediately approved.
    let ungated = memory::store(&db, store_req("routine note", user_id, 3), None, false)
        .await
        .expect("store ungated");
    assert!(
        !ungated.pending,
        "importance 3 below the threshold must not be gated"
    );
    assert_eq!(
        status_of(&db, ungated.id).await,
        "approved",
        "an ungated memory persists as approved"
    );
}
