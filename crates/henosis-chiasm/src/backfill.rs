//! Imports legacy task data by mapping legacy owner keys to principals.
//!
//! Reads a legacy Kleos SQLite database, mints one `PrincipalKind::Human` principal per distinct
//! legacy `user_id` via the canonical [`PrincipalDirectory`], records the mapping in
//! `chiasm_legacy_user_id_map`, and imports the legacy chiasm tables onto the Henosis store:
//! tasks (new `TaskId`s, owner = mapped principal), their change history, and their dependency
//! edges (both remapped through `chiasm_legacy_task_id_map`).
//!
//! The live Kleos deployment splits chiasm data across databases with independent
//! AUTOINCREMENT id spaces (the shared monolith plus per-tenant shards), so every import
//! carries an operator-chosen **source label** and the task-id map keys on
//! `(source, legacy_task_id)`. Owner keys are NOT source-scoped: Kleos user ids are
//! registry-global, and the same key maps to the same Human principal from every source.
//!
//! Import rules:
//! - **Runs once per source, not at startup.** Re-running is safe: the map tables make
//!   it idempotent per source.
//! - **No on-demand minting.** Every principal is minted here; a legacy row that cannot be
//!   handled fails the run with an explicit [`ChiasmError::Backfill`] naming the problem.
//! - **The map tables are import artifacts** and are retained for idempotency.
//!
//! Deliberate scope choices: legacy `agent` strings are NOT minted as principals -- agent
//! identity is Soma's domain; the label is preserved per task in
//! `chiasm_legacy_task_id_map.legacy_agent` so a later pass can resolve assignees. Path claims
//! are NOT imported (transient TTL leases, all expired by migration time). Kleos guardrail
//! columns (`condition`, `guardrail_url`, `guardrail_retries`) are not part of the Henosis task
//! model and are dropped. No Axon events are emitted because the import is not live traffic.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use rusqlite::{Connection, OpenFlags};
use syntheos_contracts::{PrincipalId, PrincipalKind, TaskId, TenantId, Timestamp};
use syntheos_identity::PrincipalDirectory;

use crate::error::ChiasmError;
use crate::model::{Task, TaskStatus};
use crate::store::{apply_migrations, berr, insert_task, ts_from_db, ts_to_db};

/// Options controlling a backfill run.
#[derive(Debug, Clone, Default)]
pub struct BackfillOptions {
    /// When true (the safe default for a first pass), validate and count everything but write
    /// nothing and enroll nothing.
    pub dry_run: bool,
}

/// The outcome of a backfill run. In a dry run the counts report what WOULD happen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackfillReport {
    /// The operator-chosen label of the source database this run imported from.
    pub source: String,
    /// Legacy owner key -> principal, covering both reused (prior-run) and newly minted
    /// mappings. Empty of new entries in a dry run.
    pub principals_by_legacy_user: BTreeMap<i64, PrincipalId>,
    /// How many principals were (or would be) newly minted this run.
    pub principals_minted: usize,
    /// How many legacy tasks were (or would be) imported this run.
    pub tasks_imported: usize,
    /// How many legacy tasks were already imported by a prior run and skipped.
    pub tasks_skipped: usize,
    /// How many history rows were (or would be) imported this run.
    pub updates_imported: usize,
    /// How many dependency edges were (or would be) imported this run.
    pub dependencies_imported: usize,
    /// Whether this was a dry run.
    pub dry_run: bool,
}

/// A fully validated legacy task row, parsed into Henosis types before any write happens.
struct LegacyTask {
    /// The legacy i64 primary key.
    legacy_id: i64,
    /// The legacy stringly agent label (preserved in the task-id map, not minted).
    agent: String,
    /// Project the task groups under.
    project: String,
    /// Task title.
    title: String,
    /// Parsed status (legacy tokens match [`TaskStatus`] one-to-one).
    status: TaskStatus,
    /// Progress note.
    summary: Option<String>,
    /// Expected-output description.
    expected_output: Option<String>,
    /// Output format hint.
    output_format: String,
    /// Submitted output.
    output: Option<String>,
    /// Plan text.
    plan: Option<String>,
    /// Reviewer feedback.
    feedback: Option<String>,
    /// Last heartbeat, converted to a [`Timestamp`].
    last_heartbeat: Option<Timestamp>,
    /// Heartbeat interval seconds (legacy column `heartbeat_interval`).
    heartbeat_interval_secs: i64,
    /// Creation time, converted.
    created_at: Timestamp,
    /// Last-modification time, converted.
    updated_at: Timestamp,
    /// The legacy owner key this task maps through.
    legacy_user_id: i64,
}

/// A validated legacy history row.
struct LegacyUpdate {
    /// The legacy task this entry belongs to.
    legacy_task_id: i64,
    /// Parsed status token.
    status: TaskStatus,
    /// Summary recorded at this point.
    summary: Option<String>,
    /// When the change was recorded, converted.
    created_at: Timestamp,
}

/// A validated legacy dependency edge.
struct LegacyDep {
    /// The dependent legacy task.
    legacy_task_id: i64,
    /// The legacy task it depends on.
    legacy_depends_on: i64,
    /// When the edge was created, converted.
    created_at: Timestamp,
}

/// Parse a legacy timestamp: either RFC3339 (rows written by newer Kleos code) or SQLite's
/// `datetime('now')` form `YYYY-MM-DD HH:MM:SS`, which is always UTC.
fn legacy_ts(s: &str) -> Result<Timestamp, ChiasmError> {
    if let Ok(ts) = ts_from_db(s) {
        return Ok(ts);
    }
    let fmt = time::macros::format_description!("[year]-[month]-[day] [hour]:[minute]:[second]");
    let parsed = time::PrimitiveDateTime::parse(s, &fmt)
        .map_err(|e| ChiasmError::Backfill(format!("unparseable legacy timestamp {s:?}: {e}")))?;
    Ok(Timestamp::from_utc(parsed.assume_utc()))
}

/// Whether `name` exists as a table in `conn` (legacy DBs predating the dependencies
/// migration lack `chiasm_task_dependencies`).
fn table_exists(conn: &Connection, name: &str) -> Result<bool, ChiasmError> {
    conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
        rusqlite::params![name],
        |r| r.get::<_, i64>(0),
    )
    .map(|n| n > 0)
    .map_err(berr)
}

/// Read and validate every legacy task row up front, so the run fails before any write or
/// enrollment when the legacy data is bad (convention 3.3).
fn read_legacy_tasks(legacy: &Connection) -> Result<Vec<LegacyTask>, ChiasmError> {
    let mut stmt = legacy
        .prepare(
            "SELECT id, agent, project, title, status, summary, expected_output, output_format, \
             output, plan, feedback, last_heartbeat, heartbeat_interval, created_at, updated_at, \
             user_id FROM chiasm_tasks ORDER BY id ASC",
        )
        .map_err(|e| ChiasmError::Backfill(format!("legacy chiasm_tasks unreadable: {e}")))?;
    // Raw column tuples first (the rusqlite closure must return rusqlite::Result), then parse.
    type Raw = (
        i64,
        String,
        String,
        String,
        String,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<i64>,
        String,
        String,
        i64,
    );
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
                r.get(7)?,
                r.get(8)?,
                r.get(9)?,
                r.get(10)?,
                r.get(11)?,
                r.get(12)?,
                r.get(13)?,
                r.get(14)?,
                r.get(15)?,
            ))
        })
        .map_err(berr)?;
    let mut out = Vec::new();
    for row in rows {
        let raw: Raw = row.map_err(berr)?;
        let status = TaskStatus::parse(&raw.4).map_err(|e| {
            ChiasmError::Backfill(format!("legacy task {} has bad status: {e}", raw.0))
        })?;
        out.push(LegacyTask {
            legacy_id: raw.0,
            agent: raw.1,
            project: raw.2,
            title: raw.3,
            status,
            summary: raw.5,
            expected_output: raw.6,
            output_format: raw.7.unwrap_or_else(|| "raw".to_string()),
            output: raw.8,
            plan: raw.9,
            feedback: raw.10,
            last_heartbeat: raw.11.as_deref().map(legacy_ts).transpose()?,
            heartbeat_interval_secs: raw.12.unwrap_or(300),
            created_at: legacy_ts(&raw.13)?,
            updated_at: legacy_ts(&raw.14)?,
            legacy_user_id: raw.15,
        });
    }
    Ok(out)
}

/// Read and validate every legacy history row. The legacy `agent` label on updates is dropped
/// (the Henosis history table does not carry one).
fn read_legacy_updates(legacy: &Connection) -> Result<Vec<LegacyUpdate>, ChiasmError> {
    let mut stmt = legacy
        .prepare(
            "SELECT id, task_id, status, summary, created_at FROM chiasm_task_updates \
             ORDER BY id ASC",
        )
        .map_err(|e| {
            ChiasmError::Backfill(format!("legacy chiasm_task_updates unreadable: {e}"))
        })?;
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Option<String>>(3)?,
                r.get::<_, String>(4)?,
            ))
        })
        .map_err(berr)?;
    let mut out = Vec::new();
    for row in rows {
        let (id, legacy_task_id, status, summary, created_at) = row.map_err(berr)?;
        out.push(LegacyUpdate {
            legacy_task_id,
            status: TaskStatus::parse(&status).map_err(|e| {
                ChiasmError::Backfill(format!("legacy update {id} has bad status: {e}"))
            })?,
            summary,
            created_at: legacy_ts(&created_at)?,
        });
    }
    Ok(out)
}

/// Read and validate every legacy dependency edge, if the table exists.
fn read_legacy_deps(legacy: &Connection) -> Result<Vec<LegacyDep>, ChiasmError> {
    if !table_exists(legacy, "chiasm_task_dependencies")? {
        return Ok(Vec::new());
    }
    let mut stmt = legacy
        .prepare(
            "SELECT task_id, depends_on, created_at FROM chiasm_task_dependencies ORDER BY id ASC",
        )
        .map_err(berr)?;
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
            ))
        })
        .map_err(berr)?;
    let mut out = Vec::new();
    for row in rows {
        let (legacy_task_id, legacy_depends_on, created_at) = row.map_err(berr)?;
        out.push(LegacyDep {
            legacy_task_id,
            legacy_depends_on,
            created_at: legacy_ts(&created_at)?,
        });
    }
    Ok(out)
}

/// Run the legacy import from ONE Kleos SQLite database at `legacy_db` into the
/// Henosis chiasm store at `target_db`, minting principals in `directory` and homing every
/// imported task under `tenant`. `source` labels which Kleos database this is (monolith vs a
/// tenant shard); each source has its own legacy id space and its own idempotency scope.
///
/// All target writes happen in one transaction, so a failed run leaves the store untouched.
/// (Principals enrolled before a transaction failure would remain in the directory; the next
/// run reuses committed mappings only, so verify the directory if a run fails mid-way.)
pub async fn backfill_from_kleos(
    legacy_db: &Path,
    target_db: &Path,
    directory: &dyn PrincipalDirectory,
    tenant: TenantId,
    source: &str,
    options: BackfillOptions,
) -> Result<BackfillReport, ChiasmError> {
    if source.trim().is_empty() {
        return Err(ChiasmError::Backfill(
            "a non-empty source label is required (e.g. 'monolith', 'tenant-1')".to_string(),
        ));
    }
    let legacy = Connection::open_with_flags(
        legacy_db,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|e| ChiasmError::Backfill(format!("open legacy db {legacy_db:?}: {e}")))?;
    let mut target = Connection::open(target_db).map_err(berr)?;
    target
        .pragma_update(None, "foreign_keys", true)
        .map_err(berr)?;
    apply_migrations(&mut target)?;

    // Step 1: read + validate ALL legacy data before touching anything.
    let tasks = read_legacy_tasks(&legacy)?;
    let updates = read_legacy_updates(&legacy)?;
    let deps = read_legacy_deps(&legacy)?;

    // Step 2: load prior-run state so re-runs are idempotent.
    let mut user_map: BTreeMap<i64, PrincipalId> = {
        let mut stmt = target
            .prepare("SELECT user_id, principal_id FROM chiasm_legacy_user_id_map")
            .map_err(berr)?;
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))
            .map_err(berr)?;
        let mut map = BTreeMap::new();
        for row in rows {
            let (legacy_key, pid) = row.map_err(berr)?;
            map.insert(
                legacy_key,
                pid.parse::<PrincipalId>().map_err(|e| {
                    ChiasmError::Backfill(format!("corrupt mapped principal {pid:?}: {e}"))
                })?,
            );
        }
        map
    };
    let mut task_map: BTreeMap<i64, TaskId> = {
        let mut stmt = target
            .prepare(
                "SELECT legacy_task_id, task_id FROM chiasm_legacy_task_id_map WHERE source = ?1",
            )
            .map_err(berr)?;
        let rows = stmt
            .query_map(rusqlite::params![source], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
            })
            .map_err(berr)?;
        let mut map = BTreeMap::new();
        for row in rows {
            let (legacy_id, tid) = row.map_err(berr)?;
            map.insert(
                legacy_id,
                tid.parse::<TaskId>().map_err(|e| {
                    ChiasmError::Backfill(format!("corrupt mapped task id {tid:?}: {e}"))
                })?,
            );
        }
        map
    };

    let pending_users: BTreeSet<i64> = tasks
        .iter()
        .map(|t| t.legacy_user_id)
        .filter(|u| !user_map.contains_key(u))
        .collect();
    let pending_tasks: Vec<&LegacyTask> = tasks
        .iter()
        .filter(|t| !task_map.contains_key(&t.legacy_id))
        .collect();
    let pending_legacy_ids: BTreeSet<i64> = pending_tasks.iter().map(|t| t.legacy_id).collect();
    // History rows ride with their task: a task imported by a prior run brought its history.
    let pending_updates: Vec<&LegacyUpdate> = updates
        .iter()
        .filter(|u| pending_legacy_ids.contains(&u.legacy_task_id))
        .collect();
    // Edges are deduplicated by the UNIQUE(task_id, depends_on) constraint, so re-attempting
    // prior-run edges is harmless; only count edges with at least one newly imported endpoint.
    let pending_deps: Vec<&LegacyDep> = deps
        .iter()
        .filter(|d| {
            pending_legacy_ids.contains(&d.legacy_task_id)
                || pending_legacy_ids.contains(&d.legacy_depends_on)
        })
        .collect();

    if options.dry_run {
        return Ok(BackfillReport {
            source: source.to_string(),
            principals_by_legacy_user: user_map,
            principals_minted: pending_users.len(),
            tasks_imported: pending_tasks.len(),
            tasks_skipped: tasks.len() - pending_tasks.len(),
            updates_imported: pending_updates.len(),
            dependencies_imported: pending_deps.len(),
            dry_run: true,
        });
    }

    // Step 3: mint principals for unmapped legacy keys. The legacy key is a tenant/owner key
    // in single-operator Kleos, so it classifies as the operating Human (convention 3.2).
    let minted = pending_users.len();
    for legacy_key in pending_users {
        let principal = directory
            .enroll(
                PrincipalKind::Human,
                Some(format!("legacy-user-{legacy_key}")),
            )
            .await
            .map_err(|e| {
                ChiasmError::Backfill(format!("enroll for legacy key {legacy_key}: {e}"))
            })?;
        user_map.insert(legacy_key, principal.id);
    }

    // Step 4: one transaction for every target write.
    let tx = target.transaction().map_err(berr)?;
    for (legacy_key, pid) in &user_map {
        tx.execute(
            "INSERT OR IGNORE INTO chiasm_legacy_user_id_map (user_id, principal_id) \
             VALUES (?1, ?2)",
            rusqlite::params![legacy_key, pid.to_string()],
        )
        .map_err(berr)?;
    }
    let mut tasks_imported = 0;
    for t in &pending_tasks {
        let id = TaskId::new();
        let principal_id = *user_map.get(&t.legacy_user_id).ok_or_else(|| {
            ChiasmError::Backfill(format!(
                "legacy task {} has unmapped owner key {}",
                t.legacy_id, t.legacy_user_id
            ))
        })?;
        insert_task(
            &tx,
            &Task {
                id,
                tenant,
                principal_id,
                assignee: None,
                project: t.project.clone(),
                title: t.title.clone(),
                status: t.status,
                summary: t.summary.clone(),
                expected_output: t.expected_output.clone(),
                output_format: t.output_format.clone(),
                output: t.output.clone(),
                plan: t.plan.clone(),
                feedback: t.feedback.clone(),
                last_heartbeat: t.last_heartbeat,
                heartbeat_interval_secs: t.heartbeat_interval_secs,
                created_at: t.created_at,
                updated_at: t.updated_at,
            },
        )?;
        tx.execute(
            "INSERT INTO chiasm_legacy_task_id_map (source, legacy_task_id, task_id, legacy_agent) \
             VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![source, t.legacy_id, id.to_string(), &t.agent],
        )
        .map_err(berr)?;
        task_map.insert(t.legacy_id, id);
        tasks_imported += 1;
    }
    let mut updates_imported = 0;
    for u in &pending_updates {
        let task_id = task_map.get(&u.legacy_task_id).ok_or_else(|| {
            ChiasmError::Backfill(format!(
                "legacy update references unmapped task {}",
                u.legacy_task_id
            ))
        })?;
        tx.execute(
            "INSERT INTO chiasm_task_updates (task_id, status, summary, created_at) \
             VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![
                task_id.to_string(),
                u.status.as_str(),
                &u.summary,
                ts_to_db(&u.created_at)?,
            ],
        )
        .map_err(berr)?;
        updates_imported += 1;
    }
    let mut dependencies_imported = 0;
    for d in &pending_deps {
        let (Some(task_id), Some(depends_on)) = (
            task_map.get(&d.legacy_task_id),
            task_map.get(&d.legacy_depends_on),
        ) else {
            return Err(ChiasmError::Backfill(format!(
                "legacy dependency {} -> {} references an unmapped task",
                d.legacy_task_id, d.legacy_depends_on
            )));
        };
        dependencies_imported += tx
            .execute(
                "INSERT OR IGNORE INTO chiasm_task_dependencies (task_id, depends_on, created_at) \
                 VALUES (?1, ?2, ?3)",
                rusqlite::params![
                    task_id.to_string(),
                    depends_on.to_string(),
                    ts_to_db(&d.created_at)?,
                ],
            )
            .map_err(berr)?;
    }
    tx.commit().map_err(berr)?;

    Ok(BackfillReport {
        source: source.to_string(),
        principals_by_legacy_user: user_map,
        principals_minted: minted,
        tasks_imported,
        tasks_skipped: tasks.len() - tasks_imported,
        updates_imported,
        dependencies_imported,
        dry_run: false,
    })
}

#[cfg(test)]
/// Tests legacy-task import validation and idempotency.
mod tests {
    use super::*;
    use std::sync::Arc;
    use syntheos_axon::AxonBus;
    use syntheos_identity::InMemoryDirectory;

    use crate::model::TaskFilter;
    use crate::store::ChiasmStore;

    /// The live Kleos chiasm DDL (post-migration-69 column set), as a test fixture.
    const LEGACY_DDL: &str = "
        CREATE TABLE chiasm_tasks (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            agent TEXT NOT NULL,
            project TEXT NOT NULL,
            title TEXT NOT NULL,
            status TEXT NOT NULL DEFAULT 'active',
            summary TEXT,
            expected_output TEXT,
            output_format TEXT DEFAULT 'raw',
            output TEXT,
            condition TEXT,
            guardrail_url TEXT,
            guardrail_retries INTEGER NOT NULL DEFAULT 0,
            plan TEXT,
            feedback TEXT,
            last_heartbeat TEXT,
            heartbeat_interval INTEGER NOT NULL DEFAULT 300,
            assigned INTEGER NOT NULL DEFAULT 1,
            user_id INTEGER NOT NULL DEFAULT 1,
            created_at TEXT NOT NULL DEFAULT (datetime('now')),
            updated_at TEXT NOT NULL DEFAULT (datetime('now'))
        );
        CREATE TABLE chiasm_task_updates (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            task_id INTEGER NOT NULL REFERENCES chiasm_tasks(id) ON DELETE CASCADE,
            agent TEXT NOT NULL,
            status TEXT NOT NULL,
            summary TEXT,
            created_at TEXT NOT NULL DEFAULT (datetime('now'))
        );
        CREATE TABLE chiasm_task_dependencies (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            task_id INTEGER NOT NULL REFERENCES chiasm_tasks(id) ON DELETE CASCADE,
            depends_on INTEGER NOT NULL REFERENCES chiasm_tasks(id) ON DELETE CASCADE,
            created_at TEXT NOT NULL DEFAULT (datetime('now')),
            UNIQUE(task_id, depends_on)
        );
    ";

    /// Paths for a fresh (legacy, target) database pair, unique per test.
    fn db_pair(tag: &str) -> (std::path::PathBuf, std::path::PathBuf) {
        let dir = std::env::temp_dir();
        let nonce = TaskId::new();
        (
            dir.join(format!("henosis-backfill-legacy-{tag}-{nonce}.sqlite")),
            dir.join(format!("henosis-backfill-target-{tag}-{nonce}.sqlite")),
        )
    }

    /// Create a legacy fixture: 3 tasks across 2 legacy owner keys, history, and one edge.
    fn build_legacy_fixture(path: &Path) {
        let conn = Connection::open(path).expect("legacy fixture");
        conn.execute_batch(LEGACY_DDL).expect("legacy ddl");
        conn.execute_batch(
            "INSERT INTO chiasm_tasks (agent, project, title, status, summary, user_id, created_at, updated_at)
             VALUES ('claude-code', 'henosis', 'ship release checklist', 'active', 'in progress', 1,
                     '2026-06-01 10:00:00', '2026-06-02 11:30:00');
             INSERT INTO chiasm_tasks (agent, project, title, status, user_id, created_at, updated_at)
             VALUES ('synapse', 'henosis', 'review release checklist', 'blocked', 1,
                     '2026-06-01 10:05:00', '2026-06-01 10:05:00');
             INSERT INTO chiasm_tasks (agent, project, title, status, user_id, created_at, updated_at)
             VALUES ('codex', 'kleos', 'other-owner task', 'completed', 2,
                     '2026-05-20 08:00:00', '2026-05-21 09:00:00');
             INSERT INTO chiasm_task_updates (task_id, agent, status, summary, created_at)
             VALUES (1, 'claude-code', 'active', 'started', '2026-06-01 10:00:00');
             INSERT INTO chiasm_task_updates (task_id, agent, status, summary, created_at)
             VALUES (2, 'synapse', 'blocked', 'waiting on 1', '2026-06-01 10:06:00');
             INSERT INTO chiasm_task_dependencies (task_id, depends_on, created_at)
             VALUES (2, 1, '2026-06-01 10:06:00');",
        )
        .expect("legacy rows");
    }

    /// Remove the test databases.
    fn cleanup(paths: &[&Path]) {
        for p in paths {
            let _ = std::fs::remove_file(p);
        }
    }

    #[tokio::test]
    /// Confirms dry runs validate and count rows without mutating storage.
    async fn dry_run_counts_without_writing() {
        let (legacy, target) = db_pair("dry");
        build_legacy_fixture(&legacy);
        let dir = InMemoryDirectory::new();

        let report = backfill_from_kleos(
            &legacy,
            &target,
            &dir,
            TenantId::new(),
            "monolith",
            BackfillOptions { dry_run: true },
        )
        .await
        .expect("dry run");
        assert!(report.dry_run);
        assert_eq!(report.principals_minted, 2, "2 distinct legacy owner keys");
        assert_eq!(report.tasks_imported, 3);
        assert_eq!(report.updates_imported, 2);
        assert_eq!(report.dependencies_imported, 1);
        // Nothing was enrolled and nothing was written.
        assert!(dir.list().await.expect("list").is_empty());
        let store = ChiasmStore::open(&target, Arc::new(AxonBus::new())).expect("open");
        for principal in report.principals_by_legacy_user.values() {
            assert!(store
                .list(*principal, TaskFilter::default())
                .await
                .expect("list")
                .is_empty());
        }
        cleanup(&[&legacy, &target]);
    }

    #[tokio::test]
    /// Confirms an applied import projects tasks into owner-scoped records.
    async fn apply_imports_everything_principal_correct() {
        let (legacy, target) = db_pair("apply");
        build_legacy_fixture(&legacy);
        let dir = InMemoryDirectory::new();
        let tenant = TenantId::new();

        let report = backfill_from_kleos(
            &legacy,
            &target,
            &dir,
            tenant,
            "monolith",
            BackfillOptions::default(),
        )
        .await
        .expect("apply");
        assert!(!report.dry_run);
        assert_eq!(report.principals_minted, 2);
        assert_eq!(report.tasks_imported, 3);
        assert_eq!(report.tasks_skipped, 0);
        assert_eq!(report.updates_imported, 2);
        assert_eq!(report.dependencies_imported, 1);

        // Each distinct legacy key minted one Human principal, with the convention's display.
        let principals = dir.list().await.expect("list");
        assert_eq!(principals.len(), 2);
        assert!(principals.iter().all(|p| p.kind == PrincipalKind::Human));
        let owner1 = report.principals_by_legacy_user[&1];
        let owner2 = report.principals_by_legacy_user[&2];
        assert_ne!(owner1, owner2);

        // The imported tasks are owner-scoped, statuses and timestamps converted.
        let store = ChiasmStore::open(&target, Arc::new(AxonBus::new())).expect("open");
        let mine = store
            .list(owner1, TaskFilter::default())
            .await
            .expect("list");
        assert_eq!(mine.len(), 2, "legacy key 1 owned two tasks");
        let ship = mine
            .iter()
            .find(|t| t.title == "ship release checklist")
            .expect("ship task");
        assert_eq!(ship.status, TaskStatus::Active);
        assert_eq!(ship.tenant, tenant);
        assert!(
            ship.assignee.is_none(),
            "agent labels are not minted as assignees"
        );
        assert_eq!(
            ship.created_at,
            legacy_ts("2026-06-01 10:00:00").expect("ts"),
            "datetime('now') form converted to a UTC Timestamp"
        );
        let theirs = store
            .list(owner2, TaskFilter::default())
            .await
            .expect("list");
        assert_eq!(theirs.len(), 1);
        assert_eq!(theirs[0].status, TaskStatus::Completed);

        // History rode along (owner-scoped read).
        let blocked = store
            .list(
                owner1,
                TaskFilter {
                    status: Some(TaskStatus::Blocked),
                    ..Default::default()
                },
            )
            .await
            .expect("list")
            .pop()
            .expect("blocked task");
        let history = store
            .history(owner1, blocked.id, 10)
            .await
            .expect("history");
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].summary.as_deref(), Some("waiting on 1"));

        // The dependency edge was remapped onto the minted TaskIds.
        let deps = store
            .get_dependencies(owner1, blocked.id)
            .await
            .expect("deps");
        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0].depends_on, ship.id);

        // The legacy agent label is preserved in the task-id map for Soma's later pass.
        let raw = Connection::open(&target).expect("raw");
        let agent: String = raw
            .query_row(
                "SELECT legacy_agent FROM chiasm_legacy_task_id_map WHERE legacy_task_id = 1",
                [],
                |r| r.get(0),
            )
            .expect("map row");
        assert_eq!(agent, "claude-code");
        cleanup(&[&legacy, &target]);
    }

    #[tokio::test]
    /// Confirms rerunning an import does not duplicate projected records.
    async fn rerun_is_idempotent() {
        let (legacy, target) = db_pair("rerun");
        build_legacy_fixture(&legacy);
        let dir = InMemoryDirectory::new();
        let tenant = TenantId::new();

        let first = backfill_from_kleos(
            &legacy,
            &target,
            &dir,
            tenant,
            "monolith",
            BackfillOptions::default(),
        )
        .await
        .expect("first run");
        let second = backfill_from_kleos(
            &legacy,
            &target,
            &dir,
            tenant,
            "monolith",
            BackfillOptions::default(),
        )
        .await
        .expect("second run");
        assert_eq!(second.principals_minted, 0);
        assert_eq!(second.tasks_imported, 0);
        assert_eq!(second.tasks_skipped, 3);
        assert_eq!(second.updates_imported, 0);
        assert_eq!(second.dependencies_imported, 0);
        // The mappings are stable across runs and no extra principals appeared.
        assert_eq!(
            second.principals_by_legacy_user,
            first.principals_by_legacy_user
        );
        assert_eq!(dir.list().await.expect("list").len(), 2);
        cleanup(&[&legacy, &target]);
    }

    #[tokio::test]
    /// Confirms invalid legacy statuses fail before any writes occur.
    async fn bad_legacy_status_fails_before_any_write() {
        let (legacy, target) = db_pair("badstatus");
        build_legacy_fixture(&legacy);
        {
            let conn = Connection::open(&legacy).expect("legacy");
            conn.execute(
                "INSERT INTO chiasm_tasks (agent, project, title, status, user_id) \
                 VALUES ('x', 'p', 'corrupt', 'bogus-status', 1)",
                [],
            )
            .expect("insert");
        }
        let dir = InMemoryDirectory::new();
        let err = backfill_from_kleos(
            &legacy,
            &target,
            &dir,
            TenantId::new(),
            "monolith",
            BackfillOptions::default(),
        )
        .await
        .expect_err("must fail on bad status");
        assert!(matches!(err, ChiasmError::Backfill(_)));
        // Validation happens before minting or writing: directory empty, map table empty.
        assert!(dir.list().await.expect("list").is_empty());
        let raw = Connection::open(&target).expect("raw");
        let mapped: i64 = raw
            .query_row("SELECT COUNT(*) FROM chiasm_legacy_user_id_map", [], |r| {
                r.get(0)
            })
            .expect("count");
        assert_eq!(mapped, 0);
        cleanup(&[&legacy, &target]);
    }

    #[tokio::test]
    /// Confirms imports tolerate source databases without dependency tables.
    async fn legacy_db_without_deps_table_imports_tasks() {
        let (legacy, target) = db_pair("nodeps");
        {
            let conn = Connection::open(&legacy).expect("legacy");
            // An older legacy schema: no chiasm_task_dependencies table at all.
            conn.execute_batch(
                "CREATE TABLE chiasm_tasks (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    agent TEXT NOT NULL, project TEXT NOT NULL, title TEXT NOT NULL,
                    status TEXT NOT NULL DEFAULT 'active', summary TEXT,
                    expected_output TEXT, output_format TEXT DEFAULT 'raw', output TEXT,
                    plan TEXT, feedback TEXT, last_heartbeat TEXT,
                    heartbeat_interval INTEGER NOT NULL DEFAULT 300,
                    user_id INTEGER NOT NULL DEFAULT 1,
                    created_at TEXT NOT NULL DEFAULT (datetime('now')),
                    updated_at TEXT NOT NULL DEFAULT (datetime('now')));
                 CREATE TABLE chiasm_task_updates (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    task_id INTEGER NOT NULL, agent TEXT NOT NULL, status TEXT NOT NULL,
                    summary TEXT, created_at TEXT NOT NULL DEFAULT (datetime('now')));
                 INSERT INTO chiasm_tasks (agent, project, title, user_id) VALUES ('a', 'p', 't', 1);",
            )
            .expect("old schema");
        }
        let dir = InMemoryDirectory::new();
        let report = backfill_from_kleos(
            &legacy,
            &target,
            &dir,
            TenantId::new(),
            "monolith",
            BackfillOptions::default(),
        )
        .await
        .expect("apply");
        assert_eq!(report.tasks_imported, 1);
        assert_eq!(report.dependencies_imported, 0);
        cleanup(&[&legacy, &target]);
    }

    /// THE two-source regression: the monolith and a tenant shard have independent
    /// AUTOINCREMENT id spaces, so their task ids overlap numerically. Before the
    /// (source, legacy_task_id) key, the second import silently skipped every overlapping id.
    #[tokio::test]
    async fn two_sources_with_overlapping_ids_both_import_fully() {
        let (monolith, target) = db_pair("twosrc-a");
        let (shard, _unused) = db_pair("twosrc-b");
        build_legacy_fixture(&monolith); // tasks with ids 1..=3
        build_legacy_fixture(&shard); // ALSO ids 1..=3, different logical tasks
        {
            // Make the shard's content distinguishable.
            let conn = Connection::open(&shard).expect("shard");
            conn.execute("UPDATE chiasm_tasks SET title = 'shard: ' || title", [])
                .expect("retitle");
        }
        let dir = InMemoryDirectory::new();
        let tenant = TenantId::new();

        let first = backfill_from_kleos(
            &monolith,
            &target,
            &dir,
            tenant,
            "monolith",
            BackfillOptions::default(),
        )
        .await
        .expect("monolith import");
        assert_eq!(first.tasks_imported, 3);
        let second = backfill_from_kleos(
            &shard,
            &target,
            &dir,
            tenant,
            "tenant-1",
            BackfillOptions::default(),
        )
        .await
        .expect("shard import");
        // Every shard row imports despite the id overlap -- NO silent skips.
        assert_eq!(second.tasks_imported, 3);
        assert_eq!(second.tasks_skipped, 0);
        // Same registry-global owner key resolved to the SAME Human principal across sources.
        assert_eq!(
            first.principals_by_legacy_user[&1],
            second.principals_by_legacy_user[&1]
        );

        // The target holds all six tasks; per-source idempotency still works.
        let raw = Connection::open(&target).expect("raw");
        let total: i64 = raw
            .query_row("SELECT COUNT(*) FROM chiasm_tasks", [], |r| r.get(0))
            .expect("count");
        assert_eq!(total, 6);
        let rerun = backfill_from_kleos(
            &shard,
            &target,
            &dir,
            tenant,
            "tenant-1",
            BackfillOptions::default(),
        )
        .await
        .expect("shard re-run");
        assert_eq!(rerun.tasks_imported, 0);
        assert_eq!(rerun.tasks_skipped, 3);
        cleanup(&[&monolith, &shard, &target]);
    }

    /// An empty source label is an explicit error, not a default.
    #[tokio::test]
    async fn empty_source_label_rejected() {
        let (legacy, target) = db_pair("nosrc");
        build_legacy_fixture(&legacy);
        let dir = InMemoryDirectory::new();
        let err = backfill_from_kleos(
            &legacy,
            &target,
            &dir,
            TenantId::new(),
            "  ",
            BackfillOptions::default(),
        )
        .await
        .expect_err("blank source");
        assert!(matches!(err, ChiasmError::Backfill(_)));
        cleanup(&[&legacy, &target]);
    }
}
