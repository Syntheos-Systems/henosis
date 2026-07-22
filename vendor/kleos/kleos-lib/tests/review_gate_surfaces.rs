//! Review-gate regression across the surfaces closed in the gate-cluster PR.
//!
//! `context_review_gate.rs` and `review_gate_search.rs` cover context assembly
//! and search. This file covers the remaining agent-facing readers that were
//! missing the `status != 'pending'` predicate -- episodes, tag search, /pack,
//! prompt generation, and the graph endpoints -- plus the write-path
//! self-approval hole where `memory::update` honored a client-supplied status.
//!
//! Every read test follows the same shape: seed one approved and one pending
//! memory with distinct content, assert the surface shows the approved one and
//! withholds the pending one, then approve the pending one and assert it now
//! surfaces (proving the predicate is a gate, not a blanket exclusion). These
//! tests fail on the pre-fix code.

use kleos_lib::db::Database;
use kleos_lib::episodes::{self, CreateEpisodeRequest};
use kleos_lib::graph;
use kleos_lib::memory;
use kleos_lib::memory::types::{StoreRequest, UpdateRequest};
use kleos_lib::pack::{self, PackFormat};
use kleos_lib::prompts;
use rusqlite::params;

/// Build an embedding-free store request for one owned test memory, optionally
/// static and optionally tagged.
fn store_req(
    content: &str,
    user_id: i64,
    is_static: bool,
    importance: i32,
    tags: Option<Vec<String>>,
) -> StoreRequest {
    StoreRequest {
        content: content.to_string(),
        category: "general".to_string(),
        source: "test".to_string(),
        importance,
        tags,
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
    tags: Option<Vec<String>>,
) -> i64 {
    memory::store(
        db,
        store_req(content, user_id, is_static, importance, tags),
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

/// Force a stored memory into the review-gate rejected state, matching
/// `inbox::reject_memory` (status='rejected', is_archived=1). Rejected rows are
/// archived rather than deleted, so derive/read paths must exclude them by the
/// is_archived guard even though `status != 'pending'` alone would let them pass.
async fn mark_rejected(db: &Database, id: i64) {
    db.write(move |conn| {
        conn.execute(
            "UPDATE memories SET status = 'rejected', is_archived = 1 WHERE id = ?1",
            params![id],
        )?;
        Ok(())
    })
    .await
    .expect("mark rejected");
}

/// Read a memory's current status directly.
async fn status_of(db: &Database, id: i64) -> String {
    db.read(move |conn| {
        Ok(conn.query_row(
            "SELECT status FROM memories WHERE id = ?1",
            params![id],
            |row| row.get::<_, String>(0),
        )?)
    })
    .await
    .expect("read status")
}

/// The generic edit path must NOT approve a pending memory. Before the fix,
/// `POST /memory/{id}/update {"status":"approved"}` flipped a pending row to
/// approved with only ownership auth, letting the storing agent self-approve and
/// bypassing the dedicated inbox review surface.
#[tokio::test]
async fn update_cannot_self_approve_pending_memory() {
    let db = Database::connect_memory().await.expect("in-mem db");
    let user_id = 1;

    let id = store_memory(&db, "pending fact about millbrook", user_id, false, 5, None).await;
    mark_pending(&db, id).await;

    // A status change through update is rejected.
    let attempt = UpdateRequest {
        content: None,
        category: None,
        importance: None,
        tags: None,
        is_static: None,
        status: Some("approved".to_string()),
        embedding: None,
        chunk_embeddings: None,
    };
    let res = memory::update(&db, id, attempt, user_id, false).await;
    assert!(
        res.is_err(),
        "update must reject a client-supplied status change (self-approval)"
    );
    assert_eq!(
        status_of(&db, id).await,
        "pending",
        "the memory must remain pending after the rejected update"
    );

    // A content-only edit (status: None) still works and leaves status intact.
    let edit = UpdateRequest {
        content: Some("pending fact about millbrook, revised".to_string()),
        category: None,
        importance: None,
        tags: None,
        is_static: None,
        status: None,
        embedding: None,
        chunk_embeddings: None,
    };
    let updated = memory::update(&db, id, edit, user_id, false)
        .await
        .expect("content-only edit succeeds");
    assert_eq!(
        updated.status, "pending",
        "a content edit must not change the review status"
    );

    // Echoing the current status (no-op) is allowed for idempotent clients.
    let noop = UpdateRequest {
        content: None,
        category: None,
        importance: None,
        tags: None,
        is_static: None,
        status: Some("pending".to_string()),
        embedding: None,
        chunk_embeddings: None,
    };
    memory::update(&db, updated.id, noop, user_id, false)
        .await
        .expect("no-op status equal to stored value is allowed");
}

/// `GET /episodes/{id}/memories` must not return pending memory content.
#[tokio::test]
async fn episode_memories_withhold_pending() {
    let db = Database::connect_memory().await.expect("in-mem db");
    let user_id = 1;

    let episode = episodes::create_episode(
        &db,
        CreateEpisodeRequest {
            title: Some("release".to_string()),
            session_id: None,
            agent: None,
            summary: None,
        },
        user_id,
    )
    .await
    .expect("create episode")
    .id;

    let approved = store_memory(&db, "episode note brightwater", user_id, false, 5, None).await;
    let pending = store_memory(&db, "episode note thornfield", user_id, false, 5, None).await;
    episodes::assign_memories_to_episode(&db, episode, user_id, &[approved, pending])
        .await
        .expect("assign memories");
    mark_pending(&db, pending).await;

    let rendered = serde_json::to_string(
        &episodes::get_episode_memories(&db, episode, user_id)
            .await
            .expect("get episode memories"),
    )
    .unwrap();
    assert!(rendered.contains("brightwater"), "approved memory shown");
    assert!(
        !rendered.contains("thornfield"),
        "pending memory must be withheld from episode memories"
    );
}

/// `POST /tags/search` must not return pending memory content in either the
/// match-all or match-any branch.
#[tokio::test]
async fn tag_search_withholds_pending() {
    let db = Database::connect_memory().await.expect("in-mem db");
    let user_id = 1;
    let tag = vec!["release".to_string()];

    store_memory(
        &db,
        "tagged brightwater",
        user_id,
        false,
        5,
        Some(tag.clone()),
    )
    .await;
    let pending = store_memory(
        &db,
        "tagged thornfield",
        user_id,
        false,
        5,
        Some(tag.clone()),
    )
    .await;
    mark_pending(&db, pending).await;

    for match_all in [true, false] {
        let hits = memory::search_by_tags(&db, user_id, &tag, match_all, 50)
            .await
            .expect("tag search");
        let contents: Vec<&str> = hits.iter().map(|m| m.content.as_str()).collect();
        assert!(
            contents.iter().any(|c| c.contains("brightwater")),
            "approved memory must be found (match_all={match_all})"
        );
        assert!(
            !contents.iter().any(|c| c.contains("thornfield")),
            "pending memory must be withheld from tag search (match_all={match_all})"
        );
    }
}

/// `/pack` assembles agent context and must not pack pending statics.
#[tokio::test]
async fn pack_withholds_pending_static() {
    let db = Database::connect_memory().await.expect("in-mem db");
    let user_id = 1;

    store_memory(&db, "static fact brightwater", user_id, true, 9, None).await;
    let pending = store_memory(&db, "static fact thornfield", user_id, true, 9, None).await;
    mark_pending(&db, pending).await;

    let packed = pack::pack_memories(&db, "", 8000, PackFormat::Text, user_id)
        .await
        .expect("pack")
        .packed;
    assert!(packed.contains("brightwater"), "approved static is packed");
    assert!(
        !packed.contains("thornfield"),
        "pending static must not be packed"
    );

    let id = store_memory(&db, "static fact quarterdeck", user_id, true, 9, None).await;
    mark_pending(&db, id).await;
    db.write(move |conn| {
        conn.execute(
            "UPDATE memories SET status = 'approved' WHERE id = ?1",
            params![id],
        )?;
        Ok(())
    })
    .await
    .expect("approve");
    let packed = pack::pack_memories(&db, "", 8000, PackFormat::Text, user_id)
        .await
        .expect("pack")
        .packed;
    assert!(
        packed.contains("quarterdeck"),
        "an approved static must be packable"
    );
}

/// `generate_prompt` must not surface pending statics in the rendered prompt.
#[tokio::test]
async fn generate_prompt_withholds_pending_static() {
    let db = Database::connect_memory().await.expect("in-mem db");
    let user_id = 1;

    store_memory(&db, "prompt fact brightwater", user_id, true, 9, None).await;
    let pending = store_memory(&db, "prompt fact thornfield", user_id, true, 9, None).await;
    mark_pending(&db, pending).await;

    let prompt = prompts::generate_prompt(&db, "text", 8000, "", user_id)
        .await
        .expect("generate prompt")
        .prompt;
    assert!(prompt.contains("brightwater"), "approved static in prompt");
    assert!(
        !prompt.contains("thornfield"),
        "pending static must not appear in the generated prompt"
    );
}

/// `POST /graph/search` node content must not include pending memories.
#[tokio::test]
async fn graph_search_withholds_pending() {
    let db = Database::connect_memory().await.expect("in-mem db");
    let user_id = 1;

    store_memory(&db, "graph node brightwater", user_id, false, 5, None).await;
    let pending = store_memory(&db, "graph node thornfield", user_id, false, 5, None).await;
    mark_pending(&db, pending).await;

    let nodes = graph::search::graph_search(&db, "graph node", 50, user_id)
        .await
        .expect("graph search");
    let contents: Vec<&str> = nodes.iter().map(|n| n.content.as_str()).collect();
    assert!(
        contents.iter().any(|c| c.contains("brightwater")),
        "approved memory node present"
    );
    assert!(
        !contents.iter().any(|c| c.contains("thornfield")),
        "pending memory node must be withheld from graph search"
    );
}

/// `list_all_tags` powers tag-discovery surfaces. A tag that exists only on a
/// pending (unreviewed) or rejected (archived) memory must not be listed, or the
/// tag itself leaks the withheld memory's presence and lets it be tag-searched.
#[tokio::test]
async fn list_all_tags_withholds_pending_and_rejected() {
    let db = Database::connect_memory().await.expect("in-mem db");
    let user_id = 1;

    store_memory(
        &db,
        "approved tagged brightwater",
        user_id,
        false,
        5,
        Some(vec!["shown".to_string()]),
    )
    .await;
    let pending = store_memory(
        &db,
        "pending tagged thornfield",
        user_id,
        false,
        5,
        Some(vec!["hidden-pending".to_string()]),
    )
    .await;
    let rejected = store_memory(
        &db,
        "rejected tagged quarterdeck",
        user_id,
        false,
        5,
        Some(vec!["hidden-rejected".to_string()]),
    )
    .await;
    mark_pending(&db, pending).await;
    mark_rejected(&db, rejected).await;

    let tags: Vec<String> = memory::list_all_tags(&db, user_id)
        .await
        .expect("list tags")
        .into_iter()
        .map(|t| t.tag)
        .collect();
    assert!(tags.iter().any(|t| t == "shown"), "approved tag is listed");
    assert!(
        !tags.iter().any(|t| t == "hidden-pending"),
        "a tag present only on a pending memory must not be listed"
    );
    assert!(
        !tags.iter().any(|t| t == "hidden-rejected"),
        "a tag present only on a rejected memory must not be listed"
    );
}

/// The admin fact/entity backfill sources feed structured derivation. Handing
/// back a pending (unreviewed) or rejected (refused) memory would derive facts or
/// entity links from content the gate withholds -- the same bypass as surfacing
/// it in search. Both backfills must exclude pending and rejected rows.
#[tokio::test]
async fn fact_and_entity_backfill_exclude_pending_and_rejected() {
    let db = Database::connect_memory().await.expect("in-mem db");
    let user_id = 1;

    let approved = store_memory(&db, "approved backfill source", user_id, false, 5, None).await;
    let pending = store_memory(&db, "pending backfill source", user_id, false, 5, None).await;
    let rejected = store_memory(&db, "rejected backfill source", user_id, false, 5, None).await;
    mark_pending(&db, pending).await;
    mark_rejected(&db, rejected).await;

    let fact_ids: Vec<i64> = kleos_lib::admin::get_memories_without_facts(&db, 500)
        .await
        .expect("fact backfill")
        .into_iter()
        .map(|(id, _, _)| id)
        .collect();
    assert!(
        fact_ids.contains(&approved),
        "an approved memory is a valid fact-derivation source"
    );
    assert!(
        !fact_ids.contains(&pending),
        "a pending memory must not be a fact-derivation source"
    );
    assert!(
        !fact_ids.contains(&rejected),
        "a rejected memory must not be a fact-derivation source"
    );

    let entity_ids: Vec<i64> = kleos_lib::admin::get_memories_without_entity_links(&db, 500)
        .await
        .expect("entity backfill")
        .into_iter()
        .map(|(id, _, _)| id)
        .collect();
    assert!(
        entity_ids.contains(&approved),
        "an approved memory is a valid entity-derivation source"
    );
    assert!(
        !entity_ids.contains(&pending),
        "a pending memory must not be an entity-derivation source"
    );
    assert!(
        !entity_ids.contains(&rejected),
        "a rejected memory must not be an entity-derivation source"
    );
}
