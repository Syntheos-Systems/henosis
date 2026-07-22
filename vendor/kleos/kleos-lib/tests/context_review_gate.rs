//! Review-gate regression: `status = 'pending'` memories must never reach an
//! agent through ASSEMBLED CONTEXT, not just through search.
//!
//! `review_gate_search.rs` covers the retrieval choke point. This file covers the
//! other way memory reaches a model: `context::assemble_context`, which injects
//! memories directly rather than searching for them.
//!
//! Guards the fix in `kleos-lib/src/context/deps.rs`. The gate was enforced only
//! by `status != 'pending'` predicates in the search/list/recall/timeline SQL;
//! every reader in `context/deps.rs` was missing it (`grep -rn "status"
//! kleos-lib/src/context/` returned nothing). So a high-importance memory was
//! correctly withheld from search and then injected into the prompt anyway --
//! `get_static_memories` even ordered by `importance DESC`, making the memories
//! the gate exists to hold back the FIRST ones injected. These tests fail on the
//! pre-fix code.

use kleos_lib::context::{assemble_context, ContextOptions};
use kleos_lib::db::Database;
use kleos_lib::memory;
use kleos_lib::memory::types::StoreRequest;
use rusqlite::params;

/// Build an embedding-free store request for one owned test memory. `is_static`
/// selects which injection block the memory lands in: static facts vs the recent
/// (temporal) block.
fn store_req(content: &str, user_id: i64, is_static: bool, importance: i32) -> StoreRequest {
    StoreRequest {
        content: content.to_string(),
        category: "general".to_string(),
        source: "test".to_string(),
        importance,
        tags: None,
        embedding: None,
        chunk_embeddings: None,
        session_id: None,
        is_static: Some(is_static),
        user_id: Some(user_id),
        space_id: None,
        parent_memory_id: None,
        sync_id: None,
        artifacts: None,
        created_at: None,
    }
}

/// Persist one owned test memory and return its row id.
async fn store_memory(
    db: &Database,
    content: &str,
    user_id: i64,
    is_static: bool,
    importance: i32,
) -> i64 {
    memory::store(
        db,
        store_req(content, user_id, is_static, importance),
        None,
        false,
    )
    .await
    .expect("store memory")
    .id
}

/// Force a stored memory into the review-gate pending state. Written directly
/// rather than via the env-var gate: `REVIEW_GATE_*` are `LazyLock` statics read
/// once per process, so flipping the environment mid-test is unreliable.
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

/// Approve a pending memory, as the inbox does.
async fn mark_approved(db: &Database, id: i64) {
    db.write(move |conn| {
        conn.execute(
            "UPDATE memories SET status = 'approved' WHERE id = ?1",
            params![id],
        )?;
        Ok(())
    })
    .await
    .expect("mark approved");
}

/// Assemble context for `user_id` with the static and recent blocks enabled and
/// no embedder/LLM/reranker, and return the rendered prompt text. The relevance
/// floor is pinned to 0.0 so the positive control cannot be dropped by scoring.
async fn assemble(db: &Database, user_id: i64, query: &str) -> String {
    assemble_context(
        db,
        ContextOptions {
            query: query.to_string(),
            include_static: Some(true),
            include_recent: Some(true),
            min_relevance: Some(0.0),
            token_budget: Some(8000),
            ..Default::default()
        },
        user_id,
        None,
        None,
        None,
    )
    .await
    .expect("assemble context")
    .context
}

/// A pending high-importance STATIC memory must not be injected, while its
/// approved sibling still is. This is the exact bypass the gate existed to stop:
/// static facts are injected verbatim and ranked by importance, so the gated
/// memory was previously the first thing to reach the model.
#[tokio::test]
async fn assembled_context_withholds_pending_static_memory() {
    let db = Database::connect_memory().await.expect("in-mem db");
    let user_id = 1;

    store_memory(
        &db,
        "the deploy host for production is codenamed brightwater",
        user_id,
        true,
        9,
    )
    .await;
    let pending = store_memory(
        &db,
        "the deploy host for production is codenamed thornfield",
        user_id,
        true,
        9,
    )
    .await;
    mark_pending(&db, pending).await;

    let content = assemble(&db, user_id, "production deploy host").await;

    assert!(
        content.contains("brightwater"),
        "approved static memory must be injected (context: {content})"
    );
    assert!(
        !content.contains("thornfield"),
        "pending static memory must NOT be injected into assembled context (context: {content})"
    );

    // Approving it makes it injectable: proves the predicate is a gate, not a
    // blanket exclusion of static memories.
    mark_approved(&db, pending).await;
    let content = assemble(&db, user_id, "production deploy host").await;
    assert!(
        content.contains("thornfield"),
        "memory must be injected once approved (context: {content})"
    );
}

/// A pending memory must also be withheld from the recent/temporal block. A
/// memory stored seconds ago is the most likely thing to be awaiting review, so
/// this is the gate's likeliest leak.
#[tokio::test]
async fn assembled_context_withholds_pending_recent_memory() {
    let db = Database::connect_memory().await.expect("in-mem db");
    let user_id = 1;

    store_memory(
        &db,
        "the migration finished at quarterdeck",
        user_id,
        false,
        5,
    )
    .await;
    let pending = store_memory(
        &db,
        "the migration finished at millbrook",
        user_id,
        false,
        5,
    )
    .await;
    mark_pending(&db, pending).await;

    let content = assemble(&db, user_id, "migration").await;

    assert!(
        content.contains("quarterdeck"),
        "approved recent memory must be injected (context: {content})"
    );
    assert!(
        !content.contains("millbrook"),
        "pending recent memory must NOT be injected into assembled context (context: {content})"
    );
}
