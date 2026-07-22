//! Regression tests for the `insert_link` similarity-range guard.
//!
//! `memory_links.similarity` feeds PageRank edge weights, which are divided
//! by per-node totals; a zero, negative, out-of-range, or NaN similarity row
//! poisons every downstream rank. `insert_link` is the single funnel for all
//! caller-supplied similarity values (including the batch link route), so it
//! must reject anything outside (0.0, 1.0] -- with NaN caught explicitly,
//! since every range comparison on NaN is false.

use kleos_lib::db::Database;
use kleos_lib::memory;
use kleos_lib::memory::types::StoreRequest;
use kleos_lib::EngError;

/// Owning user for the memories these tests create.
const TEST_USER: i64 = 10;

/// Build a fresh in-memory database.
async fn test_db() -> Database {
    Database::connect_memory().await.expect("in-memory db")
}

/// Store a memory owned by `TEST_USER` and return its row id.
async fn store_memory(db: &Database, content: &str) -> i64 {
    let req = StoreRequest {
        content: content.to_string(),
        category: "general".to_string(),
        source: "test".to_string(),
        importance: 5,
        user_id: Some(TEST_USER),
        ..Default::default()
    };
    memory::store(db, req, None, false)
        .await
        .expect("store memory")
        .id
}

/// Count all rows in `memory_links`.
async fn link_count(db: &Database) -> i64 {
    db.read(move |conn| {
        Ok(conn.query_row("SELECT COUNT(*) FROM memory_links", [], |row| row.get(0))?)
    })
    .await
    .expect("count links")
}

/// Out-of-range and non-finite similarity values are rejected with
/// `InvalidInput` and no row is written.
#[tokio::test]
async fn insert_link_rejects_out_of_range_similarity() {
    let db = test_db().await;
    let a = store_memory(&db, "link similarity source").await;
    let b = store_memory(&db, "link similarity target").await;

    for bad in [0.0, -0.5, 1.5, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        let err = memory::insert_link(&db, a, b, bad, "manual", TEST_USER)
            .await
            .expect_err("similarity outside (0.0, 1.0] must be rejected");
        assert!(
            matches!(err, EngError::InvalidInput(_)),
            "expected InvalidInput for similarity {bad}, got {err:?}"
        );
    }

    assert_eq!(
        link_count(&db).await,
        0,
        "no rejected value may write a row"
    );
}

/// Boundary values inside the accepted range still insert.
#[tokio::test]
async fn insert_link_accepts_valid_similarity() {
    let db = test_db().await;
    let a = store_memory(&db, "valid link source").await;
    let b = store_memory(&db, "valid link target").await;

    memory::insert_link(&db, a, b, 1.0, "manual", TEST_USER)
        .await
        .expect("similarity 1.0 is valid");
    memory::insert_link(&db, b, a, 0.001, "manual", TEST_USER)
        .await
        .expect("small positive similarity is valid");

    assert_eq!(link_count(&db).await, 2, "both valid links must be written");
}
