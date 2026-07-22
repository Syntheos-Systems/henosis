//! Shared-monolith row-scoping regressions.
//!
//! In single-DB mode (`ENGRAM_TENANT_SHARDING=0`) one monolith serves every
//! user, so a query missing its `user_id` predicate reaches another tenant's
//! rows. These tests seed two users on ONE `Database` and assert the fixed
//! paths -- storage-quota accounting, session append, and skill lineage -- stay
//! scoped to the caller. They fail on the pre-fix code.

use kleos_lib::db::Database;
use kleos_lib::quota::{self, DEFAULT_STORAGE_BYTES_LIMIT};
use kleos_lib::sessions::{self, SessionCreateRequest};
use kleos_lib::skills;
use rusqlite::params;

/// Storage-quota usage must sum only the caller's own artifacts. Before the fix
/// the SUM spanned every tenant, so one tenant filling the quota blocked all
/// others (and their own uploads looked fine against an inflated total).
#[tokio::test]
async fn storage_quota_is_scoped_per_user() {
    let db = Database::connect_memory().await.expect("db");

    // User 10 already holds a full quota's worth of artifact bytes. Only the
    // size counter matters here, so the row carries no blob.
    db.write(move |conn| {
        conn.execute(
            "INSERT INTO artifacts (name, size_bytes, user_id) VALUES ('big', ?1, 10)",
            params![DEFAULT_STORAGE_BYTES_LIMIT],
        )?;
        Ok(())
    })
    .await
    .expect("seed artifact");

    // User 10 is over quota: a further upload is rejected.
    assert!(
        quota::enforce_storage_quota(&db, 10, 1).await.is_err(),
        "user 10 is at the limit and must be rejected"
    );

    // User 20 owns nothing, so their upload must NOT be blocked by user 10's usage.
    quota::enforce_storage_quota(&db, 20, 1)
        .await
        .expect("user 20's quota must be independent of user 10's usage");
}

/// A tenant must not append output to (or bump the updated_at of) another
/// tenant's session. Before the fix the session was verified by bare id.
#[tokio::test]
async fn session_append_is_scoped_per_user() {
    let db = Database::connect_memory().await.expect("db");

    let session = sessions::create_session(
        &db,
        &SessionCreateRequest {
            agent: "owner-agent".to_string(),
        },
        10,
    )
    .await
    .expect("create session for user 10");

    // User 20 cannot append to user 10's session.
    let foreign = sessions::append_output(&db, &session.id, 20, "intruder line").await;
    assert!(
        foreign.is_err(),
        "user 20 must not append to user 10's session"
    );

    // The owner still can, and only their line is recorded.
    sessions::append_output(&db, &session.id, 10, "owner line")
        .await
        .expect("owner append");
    let output = sessions::get_session_output(&db, &session.id)
        .await
        .expect("get output");
    assert_eq!(
        output,
        vec!["owner line".to_string()],
        "only the owner's line"
    );
}

/// Skill lineage must not leak a foreign tenant's parent id, even if the
/// lineage table holds a pre-patch cross-tenant row (the fixed `create_skill`
/// no longer creates such rows, so it can only be simulated by direct insert).
#[tokio::test]
async fn skill_lineage_filters_foreign_parents() {
    let db = Database::connect_memory().await.expect("db");

    // child + a same-tenant parent belong to user 10; the other parent to 20.
    db.write(|conn| {
        conn.execute(
            "INSERT INTO skill_records (id, name, agent, code, user_id) \
             VALUES (1, 'child', 'a', '', 10), \
                    (2, 'parent-same', 'a', '', 10), \
                    (3, 'parent-other', 'a', '', 20)",
            [],
        )?;
        conn.execute(
            "INSERT INTO skill_lineage_parents (skill_id, parent_id) \
             VALUES (1, 2), (1, 3)",
            [],
        )?;
        Ok(())
    })
    .await
    .expect("seed skills + lineage");

    let parents = skills::get_lineage(&db, 1, 10).await.expect("lineage");
    assert_eq!(
        parents,
        vec![2],
        "only the same-tenant parent (2) may be returned, not the foreign one (3)"
    );
}
