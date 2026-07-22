//! Projects -- project management with memory linking and scoped search.
//!
//! Ports: projects/db.ts, projects/types.ts, projects/routes.ts (logic)

use crate::db::Database;
use crate::{EngError, Result};
use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};

pub const VALID_PROJECT_STATUSES: &[&str] = &["active", "paused", "completed", "archived"];

/// A project record as returned to callers: identity, metadata, and the count
/// of memories linked to it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectRow {
    pub id: i64,
    pub name: String,
    pub description: Option<String>,
    pub status: String,
    pub metadata: Option<String>,
    pub user_id: i64,
    pub created_at: String,
    pub updated_at: Option<String>,
    pub memory_count: Option<i64>,
}

/// Request body for creating a project; all fields optional so the handler can
/// apply defaults and validation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateProjectBody {
    pub name: Option<String>,
    pub description: Option<String>,
    pub status: Option<String>,
    pub metadata: Option<serde_json::Value>,
}

/// Request body for updating a project; each present field overwrites the
/// stored value, absent fields are left unchanged.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateProjectBody {
    pub name: Option<String>,
    pub description: Option<String>,
    pub status: Option<String>,
    pub metadata: Option<serde_json::Value>,
}

/// Insert a new project owned by `user_id`, returning its id and created_at.
#[tracing::instrument(skip(db, description, metadata), fields(name = %name, status = %status, user_id))]
pub async fn create_project(
    db: &Database,
    name: &str,
    description: Option<&str>,
    status: &str,
    metadata: Option<&str>,
    user_id: i64,
) -> Result<(i64, String)> {
    let name = name.to_string();
    let description = description.map(|s| s.to_string());
    let status = status.to_string();
    let metadata = metadata.map(|s| s.to_string());

    db.write(move |conn| {
        let mut stmt = conn.prepare(
            "INSERT INTO projects (name, description, status, metadata, user_id) \
                 VALUES (?1, ?2, ?3, ?4, ?5) RETURNING id, created_at",
        )?;
        let (id, created_at) = stmt
            .query_row(
                rusqlite::params![name, description, status, metadata, user_id],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
            )
            .map_err(|e| EngError::Internal(e.to_string()))?;
        Ok((id, created_at))
    })
    .await
}

/// Fetch a single project by id, scoped to its owner; None if absent or foreign.
#[tracing::instrument(skip(db), fields(project_id = id, user_id))]
pub async fn get_project(db: &Database, id: i64, user_id: i64) -> Result<Option<ProjectRow>> {
    db.read(move |conn| {
        let mut stmt = conn.prepare(
            "SELECT p.id, p.name, p.description, p.status, p.metadata, \
                 p.user_id, p.created_at, p.updated_at, \
                 (SELECT COUNT(*) FROM memory_projects WHERE project_id = p.id) as memory_count \
                 FROM projects p WHERE p.id = ?1 AND p.user_id = ?2",
        )?;
        let mut rows = stmt.query(rusqlite::params![id, user_id])?;
        match rows.next()? {
            Some(row) => Ok(Some(row_to_project(row)?)),
            None => Ok(None),
        }
    })
    .await
}

/// Project derivation master switch. Default-off; set
/// `KLEOS_PROJECTS_DERIVE_ENABLED=1` to surface projects that exist only as
/// `tasks.project` values (Chiasm activity) alongside the explicit `projects`
/// rows. When unset, `list_projects` returns exactly the explicit rows as
/// before.
static PROJECTS_DERIVE_ENABLED: std::sync::LazyLock<bool> = std::sync::LazyLock::new(|| {
    std::env::var("KLEOS_PROJECTS_DERIVE_ENABLED")
        .map(|v| matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"))
        .unwrap_or(false)
});

/// Deterministic, always-negative synthetic id for a derived project. Negative
/// so it can never collide with a real (positive) `projects.id`, and stable per
/// name so the GUI's React keys do not churn between refetches.
fn derived_project_id(name: &str) -> i64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    name.to_ascii_lowercase().hash(&mut hasher);
    -((hasher.finish() & (i64::MAX as u64)) as i64) - 1
}

/// Append projects that exist only as `tasks.project` values (not as explicit
/// `projects` rows) to `result`, scoped to `user_id`. `existing_lower` holds the
/// lower-cased names already present so explicit rows always win. `memory_count`
/// is set to the task count for that project so the card renders a non-zero
/// badge (and is not hidden by the GUI's zero-count filter). Synthetic rows are
/// reported with status `active`.
fn append_derived_task_projects(
    conn: &rusqlite::Connection,
    user_id: i64,
    existing_lower: &std::collections::HashSet<String>,
    result: &mut Vec<ProjectRow>,
) -> rusqlite::Result<()> {
    let mut stmt = conn.prepare(
        "SELECT project, COUNT(*) AS cnt FROM tasks \
         WHERE user_id = ?1 AND project IS NOT NULL AND TRIM(project) <> '' \
         GROUP BY project ORDER BY project COLLATE NOCASE",
    )?;
    let mut rows = stmt.query(rusqlite::params![user_id])?;
    while let Some(row) = rows.next()? {
        let name: String = row.get(0)?;
        let count: i64 = row.get(1)?;
        if existing_lower.contains(&name.to_ascii_lowercase()) {
            continue;
        }
        result.push(ProjectRow {
            id: derived_project_id(&name),
            name,
            description: Some("Derived from Chiasm task activity".to_string()),
            status: "active".to_string(),
            metadata: None,
            user_id,
            created_at: String::new(),
            updated_at: None,
            memory_count: Some(count),
        });
    }
    Ok(())
}

/// List a user's projects (optionally filtered by status). When project
/// derivation is enabled, also folds in task-only projects (see
/// [`append_derived_task_projects`]).
#[tracing::instrument(skip(db), fields(user_id, status = ?status))]
pub async fn list_projects(
    db: &Database,
    user_id: i64,
    status: Option<&str>,
) -> Result<Vec<ProjectRow>> {
    let status = status.map(|s| s.to_string());
    // Snapshot the flag before entering the blocking closure.
    let derive = *PROJECTS_DERIVE_ENABLED;

    db.read(move |conn| {
        let mut result = Vec::new();
        if let Some(ref s) = status {
            let mut stmt = conn
                .prepare(
                    "SELECT p.id, p.name, p.description, p.status, p.metadata, \
                     p.user_id, p.created_at, p.updated_at, \
                     (SELECT COUNT(*) FROM memory_projects WHERE project_id = p.id) as memory_count \
                     FROM projects p WHERE p.user_id = ?1 AND p.status = ?2 \
                     ORDER BY p.name COLLATE NOCASE",
                )
                ?;
            let mut rows = stmt
                .query(rusqlite::params![user_id, s])
                ?;
            while let Some(row) = rows.next()? {
                result.push(row_to_project(row)?);
            }
        } else {
            let mut stmt = conn
                .prepare(
                    "SELECT p.id, p.name, p.description, p.status, p.metadata, \
                     p.user_id, p.created_at, p.updated_at, \
                     (SELECT COUNT(*) FROM memory_projects WHERE project_id = p.id) as memory_count \
                     FROM projects p WHERE p.user_id = ?1 \
                     ORDER BY p.status = 'active' DESC, p.name COLLATE NOCASE",
                )
                ?;
            let mut rows = stmt
                .query(rusqlite::params![user_id])
                ?;
            while let Some(row) = rows.next()? {
                result.push(row_to_project(row)?);
            }
        }
        // Derived (task-only) projects are synthetic 'active' rows, so only fold
        // them in when the caller is not filtering to some other status.
        let status_admits_active = status.as_deref().map(|s| s == "active").unwrap_or(true);
        if derive && status_admits_active {
            let existing_lower: std::collections::HashSet<String> =
                result.iter().map(|p| p.name.to_ascii_lowercase()).collect();
            append_derived_task_projects(conn, user_id, &existing_lower, &mut result)?;
        }
        Ok(result)
    })
    .await
}

/// Update a project's mutable fields (owner-scoped); absent fields are kept.
#[tracing::instrument(skip(db, name, description, metadata), fields(project_id = id, user_id, status = ?status))]
pub async fn update_project(
    db: &Database,
    id: i64,
    user_id: i64,
    name: Option<&str>,
    description: Option<&str>,
    status: Option<&str>,
    metadata: Option<&str>,
) -> Result<()> {
    let name = name.map(|s| s.to_string());
    let description = description.map(|s| s.to_string());
    let status = status.map(|s| s.to_string());
    let metadata = metadata.map(|s| s.to_string());

    db.write(move |conn| {
        // Return NotFound when no row matched (wrong id or another owner's
        // project) instead of a silent success, matching link_memory/unlink_memory
        // so callers can tell a real update from a no-op.
        let rows = conn.execute(
            "UPDATE projects SET \
             name = COALESCE(?1, name), \
             description = COALESCE(?2, description), \
             status = COALESCE(?3, status), \
             metadata = COALESCE(?4, metadata), \
             updated_at = datetime('now') \
             WHERE id = ?5 AND user_id = ?6",
            rusqlite::params![name, description, status, metadata, id, user_id],
        )?;
        if rows == 0 {
            return Err(EngError::NotFound(format!("project {} not found", id)));
        }
        Ok(())
    })
    .await
}

/// Delete a project by id, scoped to its owner.
#[tracing::instrument(skip(db), fields(project_id = id, user_id))]
pub async fn delete_project(db: &Database, id: i64, user_id: i64) -> Result<()> {
    db.write(move |conn| {
        // Return NotFound when no row matched so a delete of a missing or
        // non-owned project is distinguishable from a real one.
        let rows = conn.execute(
            "DELETE FROM projects WHERE id = ?1 AND user_id = ?2",
            rusqlite::params![id, user_id],
        )?;
        if rows == 0 {
            return Err(EngError::NotFound(format!("project {} not found", id)));
        }
        Ok(())
    })
    .await
}

/// Link a memory to a project after verifying both are owned by `user_id`.
#[tracing::instrument(skip(db), fields(memory_id, project_id, user_id))]
pub async fn link_memory(
    db: &Database,
    memory_id: i64,
    project_id: i64,
    user_id: i64,
) -> Result<()> {
    db.write(move |conn| {
        // Verify project exists AND belongs to this user
        let project_exists: bool = conn
            .query_row(
                "SELECT 1 FROM projects WHERE id = ?1 AND user_id = ?2",
                rusqlite::params![project_id, user_id],
                |_| Ok(true),
            )
            .optional()?
            .unwrap_or(false);

        if !project_exists {
            return Err(EngError::NotFound(
                "project not found or not owned by user".to_string(),
            ));
        }

        // Verify the memory exists AND belongs to this user. Without the
        // user_id predicate a caller could link another tenant's memory into
        // their own project in monolith mode (integrity pollution), and the
        // NotFound-vs-success distinction was a cross-tenant existence oracle.
        let memory_exists: bool = conn
            .query_row(
                "SELECT 1 FROM memories WHERE id = ?1 AND user_id = ?2",
                rusqlite::params![memory_id, user_id],
                |_| Ok(true),
            )
            .optional()?
            .unwrap_or(false);

        if !memory_exists {
            return Err(EngError::NotFound(
                "memory not found or not owned by user".to_string(),
            ));
        }

        conn.execute(
            "INSERT OR IGNORE INTO memory_projects (memory_id, project_id) VALUES (?1, ?2)",
            rusqlite::params![memory_id, project_id],
        )?;
        Ok(())
    })
    .await
}

/// Remove a memory-to-project link after verifying the project's ownership.
#[tracing::instrument(skip(db), fields(memory_id, project_id, user_id))]
pub async fn unlink_memory(
    db: &Database,
    memory_id: i64,
    project_id: i64,
    user_id: i64,
) -> Result<()> {
    db.write(move |conn| {
        // Verify project exists AND belongs to this user before unlinking
        let project_exists: bool = conn
            .query_row(
                "SELECT 1 FROM projects WHERE id = ?1 AND user_id = ?2",
                rusqlite::params![project_id, user_id],
                |_| Ok(true),
            )
            .optional()?
            .unwrap_or(false);

        if !project_exists {
            return Err(EngError::NotFound(
                "project not found or not owned by user".to_string(),
            ));
        }

        conn.execute(
            "DELETE FROM memory_projects WHERE memory_id = ?1 AND project_id = ?2",
            rusqlite::params![memory_id, project_id],
        )?;
        Ok(())
    })
    .await
}

/// Return the ids of memories linked to a project, owner-scoped on both sides.
#[tracing::instrument(skip(db), fields(project_id, user_id))]
pub async fn get_project_memory_ids(
    db: &Database,
    project_id: i64,
    user_id: i64,
) -> Result<Vec<i64>> {
    db.read(move |conn| {
        // Defense-in-depth: enforce ownership before listing memory ids.
        let owned: bool = conn
            .query_row(
                "SELECT 1 FROM projects WHERE id = ?1 AND user_id = ?2",
                rusqlite::params![project_id, user_id],
                |_| Ok(true),
            )
            .optional()?
            .unwrap_or(false);
        if !owned {
            return Ok(Vec::new());
        }

        // Filter the join by m.user_id too: defense in depth so a memory linked
        // before the link_memory fix (or in any cross-tenant state) is never
        // surfaced to a non-owner in monolith mode.
        let mut stmt = conn.prepare(
            "SELECT mp.memory_id FROM memory_projects mp \
                 JOIN memories m ON m.id = mp.memory_id \
                 WHERE mp.project_id = ?1 AND m.user_id = ?2",
        )?;
        let mut rows = stmt.query(rusqlite::params![project_id, user_id])?;
        let mut ids = Vec::new();
        while let Some(row) = rows.next()? {
            ids.push(row.get::<_, i64>(0)?);
        }
        Ok(ids)
    })
    .await
}

/// Map a SQL result row (id, name, description, status, metadata, user_id,
/// created_at, updated_at, memory_count) into a [`ProjectRow`].
fn row_to_project(row: &rusqlite::Row<'_>) -> Result<ProjectRow> {
    Ok(ProjectRow {
        id: row
            .get(0)
            .map_err(|e| crate::EngError::Internal(e.to_string()))?,
        name: row
            .get(1)
            .map_err(|e| crate::EngError::Internal(e.to_string()))?,
        description: row.get(2).unwrap_or(None),
        status: row
            .get(3)
            .map_err(|e| crate::EngError::Internal(e.to_string()))?,
        metadata: row.get(4).unwrap_or(None),
        user_id: row
            .get(5)
            .map_err(|e| crate::EngError::Internal(e.to_string()))?,
        created_at: row
            .get(6)
            .map_err(|e| crate::EngError::Internal(e.to_string()))?,
        updated_at: row.get(7).unwrap_or(None),
        memory_count: row.get(8).unwrap_or(None),
    })
}

/// Unit tests for project CRUD, tenant scoping, and task-project derivation.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Database;

    /// VALID_PROJECT_STATUSES contains the accepted statuses and nothing else.
    #[test]
    fn test_valid_statuses() {
        assert!(VALID_PROJECT_STATUSES.contains(&"active"));
        assert!(VALID_PROJECT_STATUSES.contains(&"archived"));
        assert!(!VALID_PROJECT_STATUSES.contains(&"deleted"));
    }

    /// update_project and delete_project must return NotFound for an id that
    /// does not exist or is not owned by the caller, rather than a silent
    /// success that hides the miss (and, for a wrong owner, cross-tenant writes).
    #[tokio::test]
    async fn update_and_delete_missing_project_return_not_found() {
        let db = Database::connect_memory().await.expect("db");
        let (project_id, _) = create_project(&db, "owned", None, "active", None, 1)
            .await
            .expect("create");

        let miss = update_project(&db, 999_999, 1, Some("x"), None, None, None).await;
        assert!(
            matches!(miss, Err(EngError::NotFound(_))),
            "updating a nonexistent project must be NotFound, got {miss:?}"
        );
        let del_miss = delete_project(&db, 999_999, 1).await;
        assert!(
            matches!(del_miss, Err(EngError::NotFound(_))),
            "deleting a nonexistent project must be NotFound, got {del_miss:?}"
        );

        // Project exists but belongs to user 1; user 2 must not update or delete it.
        let wrong_owner = update_project(&db, project_id, 2, Some("x"), None, None, None).await;
        assert!(
            matches!(wrong_owner, Err(EngError::NotFound(_))),
            "updating another owner's project must be NotFound (no cross-tenant write)"
        );

        // The real owner still succeeds.
        update_project(&db, project_id, 1, Some("renamed"), None, None, None)
            .await
            .expect("owner update succeeds");
        delete_project(&db, project_id, 1)
            .await
            .expect("owner delete succeeds");
    }

    /// Insert a memory owned by `owner` and return its id.
    async fn insert_memory(db: &Database, owner: i64, content: &str) -> i64 {
        let content = content.to_string();
        db.write(move |conn| {
            Ok(conn.query_row(
                "INSERT INTO memories (user_id, content) VALUES (?1, ?2) RETURNING id",
                rusqlite::params![owner, content],
                |r| r.get::<_, i64>(0),
            )?)
        })
        .await
        .expect("insert memory")
    }

    /// A user must not be able to link another tenant's memory to their project
    /// (monolith mode), and the listing must never surface a foreign memory.
    #[tokio::test]
    async fn link_memory_is_tenant_scoped() {
        let db = Database::connect_memory().await.expect("db");
        // Project owned by user 1.
        let (project_id, _) = create_project(&db, "p", None, "active", None, 1)
            .await
            .expect("create project");

        // Linking another user's memory must fail closed.
        let foreign = insert_memory(&db, 2, "foreign").await;
        assert!(
            link_memory(&db, foreign, project_id, 1).await.is_err(),
            "must not link another tenant's memory"
        );

        // Linking an owned memory works, and the listing returns only it.
        let own = insert_memory(&db, 1, "mine").await;
        link_memory(&db, own, project_id, 1)
            .await
            .expect("link own memory");
        let ids = get_project_memory_ids(&db, project_id, 1)
            .await
            .expect("list ids");
        assert_eq!(ids, vec![own], "listing must be scoped to the owner");
    }

    /// Synthetic ids are negative (never collide with real positive ids) and
    /// stable per name so the GUI's React keys do not churn between refetches.
    #[test]
    fn derived_project_id_is_negative_and_stable() {
        let a = derived_project_id("Kleos");
        let b = derived_project_id("Kleos");
        let c = derived_project_id("Synapse");
        assert!(a < 0 && c < 0, "derived ids must be negative");
        assert_eq!(a, b, "same name -> same id");
        assert_ne!(a, c, "distinct names -> distinct ids");
    }

    /// Derivation surfaces task-only projects, scoped to the owner, while
    /// excluding explicit duplicates and skipping blank/NULL project values.
    #[tokio::test]
    async fn derives_task_projects_excluding_explicit_and_blank() {
        let db = Database::connect_memory().await.expect("db");
        db.write(|conn| {
            for (title, project, user) in [
                ("a", Some("Kleos"), 1), // duplicate of an explicit row
                ("b", Some("Kleos"), 1),
                ("c", Some("Synapse"), 1), // task-only -> should derive
                ("d", Some(""), 1),        // blank -> skipped
                ("e", None, 1),            // NULL -> skipped
                ("f", Some("Other"), 2),   // foreign owner -> out of scope
            ] {
                conn.execute(
                    "INSERT INTO tasks (title, project, user_id) VALUES (?1, ?2, ?3)",
                    rusqlite::params![title, project, user],
                )?;
            }
            Ok(())
        })
        .await
        .expect("seed tasks");

        let derived = db
            .read(|conn| {
                let mut result = Vec::new();
                let existing: std::collections::HashSet<String> =
                    ["kleos".to_string()].into_iter().collect();
                append_derived_task_projects(conn, 1, &existing, &mut result)?;
                Ok(result)
            })
            .await
            .expect("derive");

        assert_eq!(derived.len(), 1, "only the task-only project derives");
        assert_eq!(derived[0].name, "Synapse");
        assert_eq!(derived[0].memory_count, Some(1));
        assert_eq!(derived[0].status, "active");
        assert!(derived[0].id < 0, "derived id must be negative");
    }
}
