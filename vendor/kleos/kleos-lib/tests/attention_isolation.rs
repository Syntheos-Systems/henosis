//! Unit tests for attention notes: CRUD correctness and tenant isolation.
//!
//! Every operation must be scoped by user_id — a second tenant must never
//! read, modify, or delete another tenant's notes.

use kleos_lib::attention::{
    create_note, delete_note, get_note, list_notes, update_note, CreateNoteRequest,
    UpdateNoteRequest,
};
use kleos_lib::tenant::{TenantConfig, TenantRegistry};
use std::sync::Arc;

async fn one_db() -> Arc<kleos_lib::tenant::TenantHandle> {
    let dir = tempfile::tempdir().expect("tempdir");
    let registry = TenantRegistry::new(dir.path(), TenantConfig::default(), 128, false, None)
        .expect("registry");
    let handle = registry
        .get_or_create("attention_test")
        .await
        .expect("tenant");
    std::mem::forget(dir);
    handle
}

// --- CRUD -------------------------------------------------------------------

#[tokio::test]
async fn create_and_retrieve() {
    let handle = one_db().await;
    let db = handle.database();

    let note = create_note(
        &db,
        CreateNoteRequest {
            content: "fix the watcher bug".into(),
            priority: Some(8),
        },
        1,
    )
    .await
    .expect("create");

    assert_eq!(note.content, "fix the watcher bug");
    assert_eq!(note.priority, 8);

    let fetched = get_note(&db, note.id, 1).await.expect("get");
    assert_eq!(fetched.id, note.id);
    assert_eq!(fetched.content, note.content);
}

#[tokio::test]
async fn default_priority_is_five() {
    let handle = one_db().await;
    let db = handle.database();

    let note = create_note(
        &db,
        CreateNoteRequest {
            content: "some reminder".into(),
            priority: None,
        },
        1,
    )
    .await
    .expect("create");

    assert_eq!(note.priority, 5);
}

#[tokio::test]
async fn list_ordered_by_priority_then_age() {
    let handle = one_db().await;
    let db = handle.database();

    create_note(
        &db,
        CreateNoteRequest {
            content: "low".into(),
            priority: Some(2),
        },
        1,
    )
    .await
    .expect("create low");
    create_note(
        &db,
        CreateNoteRequest {
            content: "high".into(),
            priority: Some(9),
        },
        1,
    )
    .await
    .expect("create high");
    create_note(
        &db,
        CreateNoteRequest {
            content: "mid".into(),
            priority: Some(5),
        },
        1,
    )
    .await
    .expect("create mid");

    let notes = list_notes(&db, 1, 10).await.expect("list");
    assert_eq!(notes.len(), 3);
    assert_eq!(notes[0].content, "high", "highest priority must come first");
    assert_eq!(notes[1].content, "mid");
    assert_eq!(notes[2].content, "low");
}

#[tokio::test]
async fn update_content_and_priority() {
    let handle = one_db().await;
    let db = handle.database();

    let note = create_note(
        &db,
        CreateNoteRequest {
            content: "original".into(),
            priority: Some(3),
        },
        1,
    )
    .await
    .expect("create");

    let updated = update_note(
        &db,
        note.id,
        UpdateNoteRequest {
            content: Some("updated".into()),
            priority: Some(7),
        },
        1,
    )
    .await
    .expect("update");

    assert_eq!(updated.content, "updated");
    assert_eq!(updated.priority, 7);
}

#[tokio::test]
async fn partial_update_leaves_other_fields_intact() {
    let handle = one_db().await;
    let db = handle.database();

    let note = create_note(
        &db,
        CreateNoteRequest {
            content: "keep this".into(),
            priority: Some(6),
        },
        1,
    )
    .await
    .expect("create");

    let updated = update_note(
        &db,
        note.id,
        UpdateNoteRequest {
            content: None,
            priority: Some(9),
        },
        1,
    )
    .await
    .expect("partial update");

    assert_eq!(updated.content, "keep this", "content must be unchanged");
    assert_eq!(updated.priority, 9);
}

#[tokio::test]
async fn delete_removes_note() {
    let handle = one_db().await;
    let db = handle.database();

    let note = create_note(
        &db,
        CreateNoteRequest {
            content: "done".into(),
            priority: None,
        },
        1,
    )
    .await
    .expect("create");

    delete_note(&db, note.id, 1).await.expect("delete");

    let notes = list_notes(&db, 1, 10).await.expect("list");
    assert!(notes.is_empty(), "list must be empty after delete");
}

// --- Tenant isolation -------------------------------------------------------

#[tokio::test]
async fn list_is_scoped_to_user() {
    const ALICE: i64 = 1;
    const BOB: i64 = 2;

    let handle = one_db().await;
    let db = handle.database();

    create_note(
        &db,
        CreateNoteRequest {
            content: "alice note".into(),
            priority: None,
        },
        ALICE,
    )
    .await
    .expect("alice create");
    create_note(
        &db,
        CreateNoteRequest {
            content: "bob note".into(),
            priority: None,
        },
        BOB,
    )
    .await
    .expect("bob create");

    let alice = list_notes(&db, ALICE, 50).await.expect("alice list");
    let bob = list_notes(&db, BOB, 50).await.expect("bob list");

    assert_eq!(alice.len(), 1);
    assert_eq!(alice[0].content, "alice note");
    assert_eq!(bob.len(), 1);
    assert_eq!(bob[0].content, "bob note");
}

#[tokio::test]
async fn update_by_other_tenant_is_noop() {
    const OWNER: i64 = 1;
    const INTRUDER: i64 = 2;

    let handle = one_db().await;
    let db = handle.database();

    let note = create_note(
        &db,
        CreateNoteRequest {
            content: "owner note".into(),
            priority: Some(5),
        },
        OWNER,
    )
    .await
    .expect("create");

    let result = update_note(
        &db,
        note.id,
        UpdateNoteRequest {
            content: Some("hijacked".into()),
            priority: None,
        },
        INTRUDER,
    )
    .await;

    assert!(result.is_err(), "intruder update must fail");

    let original = get_note(&db, note.id, OWNER).await.expect("get");
    assert_eq!(original.content, "owner note", "content must be unchanged");
}

#[tokio::test]
async fn delete_by_other_tenant_is_noop() {
    const OWNER: i64 = 1;
    const INTRUDER: i64 = 2;

    let handle = one_db().await;
    let db = handle.database();

    let note = create_note(
        &db,
        CreateNoteRequest {
            content: "survives".into(),
            priority: None,
        },
        OWNER,
    )
    .await
    .expect("create");

    let result = delete_note(&db, note.id, INTRUDER).await;
    assert!(result.is_err(), "intruder delete must fail");

    let still_there = list_notes(&db, OWNER, 10).await.expect("list");
    assert_eq!(still_there.len(), 1, "note must still exist for owner");
}

#[tokio::test]
async fn get_by_other_tenant_is_noop() {
    const OWNER: i64 = 1;
    const INTRUDER: i64 = 2;

    let handle = one_db().await;
    let db = handle.database();

    let note = create_note(
        &db,
        CreateNoteRequest {
            content: "private".into(),
            priority: None,
        },
        OWNER,
    )
    .await
    .expect("create");

    let result = get_note(&db, note.id, INTRUDER).await;
    assert!(result.is_err(), "intruder get must fail");
}

// --- Validation -------------------------------------------------------------

#[tokio::test]
async fn priority_out_of_range_is_rejected() {
    let handle = one_db().await;
    let db = handle.database();

    for bad in [0i64, 11, -1, 999] {
        let result = create_note(
            &db,
            CreateNoteRequest {
                content: "test".into(),
                priority: Some(bad),
            },
            1,
        )
        .await;
        assert!(result.is_err(), "priority {bad} must be rejected");
    }
}

#[tokio::test]
async fn priority_boundaries_are_accepted() {
    let handle = one_db().await;
    let db = handle.database();

    for good in [1i64, 5, 10] {
        create_note(
            &db,
            CreateNoteRequest {
                content: format!("prio {good}"),
                priority: Some(good),
            },
            1,
        )
        .await
        .unwrap_or_else(|e| panic!("priority {good} must be accepted: {e}"));
    }
}
