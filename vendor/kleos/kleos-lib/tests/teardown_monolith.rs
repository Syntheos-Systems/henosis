//! Monolith deprovision sweep regression.
//!
//! In single-DB (shared) mode a deprovisioned user's data lives in the monolith
//! rather than a shard file, so `delete_monolith_rows` must remove every row the
//! user owns. The pre-fix implementation deleted a hardcoded 5-table list and
//! relied on `ON DELETE CASCADE` for the rest -- but most monolith user-data
//! tables (memories, episodes, skills, ...) carry a plain `user_id` with no
//! cascade FK, so dozens of tables were left orphaned. These tests assert the
//! dynamic sweep catches them while leaving other users untouched.

use kleos_lib::db::Database;
use kleos_lib::memory::types::StoreRequest;
use kleos_lib::memory::{self};
use kleos_lib::tenant::teardown::delete_monolith_rows;

/// Build a shared monolith in-memory database.
async fn monolith() -> Database {
    Database::connect_memory().await.expect("monolith db")
}

/// Construct a minimal `StoreRequest` owned by `user_id`.
fn store_req(content: &str, user_id: i64) -> StoreRequest {
    StoreRequest {
        content: content.to_string(),
        category: "general".to_string(),
        source: "test".to_string(),
        importance: 5,
        user_id: Some(user_id),
        ..Default::default()
    }
}

/// Count a user's non-deleted rows in the memories table.
async fn count_memories(db: &Database, user_id: i64) -> i64 {
    db.read(move |conn| {
        Ok(conn.query_row(
            "SELECT COUNT(*) FROM memories WHERE user_id = ?1",
            rusqlite::params![user_id],
            |r| r.get(0),
        )?)
    })
    .await
    .expect("count memories")
}

/// delete_monolith_rows must sweep the memories table (plain user_id, no cascade
/// FK -- missed by the pre-fix hardcoded list) for the target user ONLY, leaving
/// every other tenant's rows intact. Content is deliberately distinct so the
/// store-time simhash near-dup collapse does not skew the counts.
#[tokio::test]
async fn delete_monolith_rows_sweeps_plain_user_id_tables() {
    let db = monolith().await;

    for c in [
        "alice apple pie recipe with cinnamon",
        "alice notes on quantum entanglement",
    ] {
        memory::store(&db, store_req(c, 10), None, false)
            .await
            .expect("store alice");
    }
    for c in [
        "bob mountain climbing expedition log",
        "bob jazz saxophone practice routine",
    ] {
        memory::store(&db, store_req(c, 20), None, false)
            .await
            .expect("store bob");
    }

    let alice_before = count_memories(&db, 10).await;
    let bob_before = count_memories(&db, 20).await;
    assert_eq!(alice_before, 2, "alice should have 2 distinct memories");
    assert_eq!(bob_before, 2, "bob should have 2 distinct memories");

    let counts = delete_monolith_rows(&db, 20).await.expect("delete bob");

    assert_eq!(
        count_memories(&db, 20).await,
        0,
        "bob's memories must be deleted by the sweep"
    );
    assert_eq!(
        count_memories(&db, 10).await,
        alice_before,
        "alice's memories must be untouched by bob's deprovision"
    );
    assert_eq!(
        counts.get("memories").copied().unwrap_or(0),
        bob_before as usize,
        "the memories table must be in the swept set with bob's row count: {counts:?}"
    );
}

/// The reserved owner account (user_id = 1) must never be deprovisioned.
#[tokio::test]
async fn delete_monolith_rows_refuses_owner() {
    let db = monolith().await;
    assert!(
        delete_monolith_rows(&db, 1).await.is_err(),
        "must refuse to delete owner user_id=1"
    );
}
