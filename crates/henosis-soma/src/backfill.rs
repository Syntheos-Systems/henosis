//! The one-time Kleos soma absorption backfill (projection convention 3.2 + 3.4).
//!
//! Reads a legacy Kleos SQLite database and imports `soma_agents` onto the Henosis presence
//! store. Two identities are involved, and the convention treats them differently:
//!
//! - **The agent itself.** Each legacy agent row becomes a first-class principal: the backfill
//!   mints one `PrincipalKind::Agent` principal per row (display = the legacy name) and the
//!   presence row keys on it. `soma_legacy_agent_map` records `legacy id -> principal` so
//!   re-runs are idempotent and chiasm's preserved `legacy_agent` task labels can later resolve
//!   to assignee principals.
//! - **The legacy owner key.** Per convention 3.4 the soma backfill REUSES the Human principal
//!   chiasm minted for the same legacy `user_id` (read once from chiasm's
//!   `chiasm_legacy_user_id_map` -- the one sanctioned cross-service migration-table read);
//!   only keys chiasm never saw mint a fresh Human here. Presence rows do not carry the owner;
//!   `soma_legacy_user_id_map` preserves the linkage for later attribution (Pistis grants).
//!
//! Legacy JSON columns are sanitized on the way in (the live store parses strictly): a
//! capabilities/drift value that is not an array of strings degrades per-entry (non-strings
//! stringified) or to empty, matching what Kleos's own lenient reader would have produced. A
//! legacy status token that does not parse is an explicit error (convention 3.3) -- bad data
//! fails the run, never silently discarded. All target writes happen in one transaction. No
//! Axon events are emitted: a migration is not live traffic.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use rusqlite::{Connection, OpenFlags};
use syntheos_contracts::{PrincipalId, PrincipalKind, TenantId, Timestamp};
use syntheos_identity::PrincipalDirectory;

use crate::error::SomaError;
use crate::model::PresenceStatus;
use crate::store::{apply_migrations, berr, ts_to_db};

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
    /// Legacy owner key -> Human principal (reused from chiasm, prior runs, or minted here).
    pub owners_by_legacy_user: BTreeMap<i64, PrincipalId>,
    /// How many owner keys were resolved by reusing chiasm's mapping (convention 3.4).
    pub owners_reused_from_chiasm: usize,
    /// How many Human owner principals were (or would be) freshly minted this run.
    pub owners_minted: usize,
    /// How many Agent principals + presence rows were (or would be) imported this run.
    pub agents_imported: usize,
    /// How many legacy agents were already imported by a prior run and skipped.
    pub agents_skipped: usize,
    /// Whether this was a dry run.
    pub dry_run: bool,
}

/// A fully validated legacy agent row, parsed and sanitized before any write happens.
struct LegacyAgent {
    /// The legacy i64 primary key.
    legacy_id: i64,
    /// The legacy agent label.
    name: String,
    /// Coarse category (legacy column `type`).
    agent_type: String,
    /// Optional description.
    description: Option<String>,
    /// Sanitized capability strings.
    capabilities: Vec<String>,
    /// Parsed status (legacy tokens match [`PresenceStatus`] one-to-one).
    status: PresenceStatus,
    /// Sanitized configuration object.
    config: serde_json::Value,
    /// Last heartbeat, converted.
    heartbeat_at: Option<Timestamp>,
    /// Latest quality score.
    quality_score: Option<f64>,
    /// Sanitized drift-flag strings.
    drift_flags: Vec<String>,
    /// Creation time, converted.
    created_at: Timestamp,
    /// Last-modification time, converted.
    updated_at: Timestamp,
    /// The legacy owner key (mapped, never imported).
    legacy_user_id: i64,
}

/// Parse a legacy timestamp: either RFC3339 or SQLite's `datetime('now')` form
/// `YYYY-MM-DD HH:MM:SS`, which is always UTC.
fn legacy_ts(s: &str) -> Result<Timestamp, SomaError> {
    if let Ok(ts) = crate::store::ts_from_db(s) {
        return Ok(ts);
    }
    let fmt = time::macros::format_description!("[year]-[month]-[day] [hour]:[minute]:[second]");
    let parsed = time::PrimitiveDateTime::parse(s, &fmt)
        .map_err(|e| SomaError::Backfill(format!("unparseable legacy timestamp {s:?}: {e}")))?;
    Ok(Timestamp::from_utc(parsed.assume_utc()))
}

/// Sanitize a legacy JSON-array-of-strings column: string entries pass through, other entries
/// stringify, and anything that is not a JSON array degrades to empty -- the shape Kleos's own
/// lenient reader (`parse_json` with an `[]` fallback) would have produced.
fn sanitize_string_array(text: Option<&str>) -> Vec<String> {
    let Some(text) = text else {
        return Vec::new();
    };
    match serde_json::from_str::<serde_json::Value>(text) {
        Ok(serde_json::Value::Array(entries)) => entries
            .into_iter()
            .map(|v| match v {
                serde_json::Value::String(s) => s,
                other => other.to_string(),
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// Sanitize a legacy JSON-object column: a parseable object passes through, anything else
/// degrades to `{}` (the Kleos fallback).
fn sanitize_object(text: Option<&str>) -> serde_json::Value {
    text.and_then(|t| serde_json::from_str::<serde_json::Value>(t).ok())
        .filter(|v| v.is_object())
        .unwrap_or_else(|| serde_json::json!({}))
}

/// Read and validate every legacy agent row up front, so the run fails before any write or
/// enrollment when the legacy data is bad (convention 3.3).
fn read_legacy_agents(legacy: &Connection) -> Result<Vec<LegacyAgent>, SomaError> {
    let mut stmt = legacy
        .prepare(
            "SELECT id, name, type, description, capabilities, status, config, heartbeat_at, \
             quality_score, drift_flags, created_at, updated_at, user_id \
             FROM soma_agents ORDER BY id ASC",
        )
        .map_err(|e| SomaError::Backfill(format!("legacy soma_agents unreadable: {e}")))?;
    // Raw column tuples first (the rusqlite closure must return rusqlite::Result), then parse.
    type Raw = (
        i64,
        String,
        String,
        Option<String>,
        Option<String>,
        String,
        Option<String>,
        Option<String>,
        Option<f64>,
        Option<String>,
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
            ))
        })
        .map_err(berr)?;
    let mut out = Vec::new();
    for row in rows {
        let raw: Raw = row.map_err(berr)?;
        let status = PresenceStatus::parse(&raw.5).map_err(|e| {
            SomaError::Backfill(format!("legacy agent {} ({:?}) has bad status: {e}", raw.0, raw.1))
        })?;
        out.push(LegacyAgent {
            legacy_id: raw.0,
            name: raw.1,
            agent_type: raw.2,
            description: raw.3,
            capabilities: sanitize_string_array(raw.4.as_deref()),
            status,
            config: sanitize_object(raw.6.as_deref()),
            heartbeat_at: raw.7.as_deref().map(legacy_ts).transpose()?,
            quality_score: raw.8,
            drift_flags: sanitize_string_array(raw.9.as_deref()),
            created_at: legacy_ts(&raw.10)?,
            updated_at: legacy_ts(&raw.11)?,
            legacy_user_id: raw.12,
        });
    }
    Ok(out)
}

/// Read a `* -> principal_id` map table keyed by an i64 column.
fn read_i64_map(
    conn: &Connection,
    sql: &str,
    what: &str,
) -> Result<BTreeMap<i64, PrincipalId>, SomaError> {
    let mut stmt = conn.prepare(sql).map_err(berr)?;
    let rows = stmt
        .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))
        .map_err(berr)?;
    let mut map = BTreeMap::new();
    for row in rows {
        let (key, pid) = row.map_err(berr)?;
        map.insert(
            key,
            pid.parse::<PrincipalId>().map_err(|e| {
                SomaError::Backfill(format!("corrupt {what} principal {pid:?}: {e}"))
            })?,
        );
    }
    Ok(map)
}

/// Run the soma absorption backfill from a legacy Kleos SQLite database at `legacy_db` into the
/// Henosis soma store at `target_db`, homing every imported presence under `tenant`.
///
/// `chiasm_db` is the path to the already-backfilled Henosis CHIASM database; when given, owner
/// keys resolve through `chiasm_legacy_user_id_map` first so the same legacy key maps to the
/// same Human principal across services (convention 3.4). Run the chiasm backfill first;
/// passing a chiasm db without that table is an explicit error (wrong file, or chiasm not yet
/// backfilled).
pub async fn backfill_from_kleos(
    legacy_db: &Path,
    target_db: &Path,
    chiasm_db: Option<&Path>,
    directory: &dyn PrincipalDirectory,
    tenant: TenantId,
    options: BackfillOptions,
) -> Result<BackfillReport, SomaError> {
    let legacy = Connection::open_with_flags(
        legacy_db,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|e| SomaError::Backfill(format!("open legacy db {legacy_db:?}: {e}")))?;
    let mut target = Connection::open(target_db).map_err(berr)?;
    target
        .pragma_update(None, "foreign_keys", true)
        .map_err(berr)?;
    apply_migrations(&mut target)?;

    // Phase 1: read + validate + sanitize ALL legacy data before touching anything.
    let agents = read_legacy_agents(&legacy)?;
    // Names must be unique within the target tenant. Legacy uniqueness was (name, user_id), so
    // a cross-owner duplicate collides when both land in one tenant -- explicit error, the
    // operator decides (convention 3.3: never silently mutate).
    {
        let mut seen: BTreeMap<&str, i64> = BTreeMap::new();
        for a in &agents {
            if let Some(first) = seen.insert(a.name.as_str(), a.legacy_id) {
                return Err(SomaError::Backfill(format!(
                    "legacy agents {first} and {} share the name {:?} across owner keys; \
                     resolve the duplicate before importing into one tenant",
                    a.legacy_id, a.name
                )));
            }
        }
    }

    // Phase 2: prior-run state (idempotent re-runs) + the chiasm cross-map (convention 3.4).
    let mut owner_map = read_i64_map(
        &target,
        "SELECT user_id, principal_id FROM soma_legacy_user_id_map",
        "soma owner map",
    )?;
    let agent_map = read_i64_map(
        &target,
        "SELECT legacy_agent_id, principal_id FROM soma_legacy_agent_map",
        "soma agent map",
    )?;
    let chiasm_map: BTreeMap<i64, PrincipalId> = match chiasm_db {
        Some(path) => {
            let chiasm = Connection::open_with_flags(
                path,
                OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )
            .map_err(|e| SomaError::Backfill(format!("open chiasm db {path:?}: {e}")))?;
            read_i64_map(
                &chiasm,
                "SELECT user_id, principal_id FROM chiasm_legacy_user_id_map",
                "chiasm cross-map",
            )
            .map_err(|e| {
                SomaError::Backfill(format!(
                    "chiasm cross-map unreadable (run the chiasm backfill first?): {e}"
                ))
            })?
        }
        None => BTreeMap::new(),
    };

    let pending_agents: Vec<&LegacyAgent> = agents
        .iter()
        .filter(|a| !agent_map.contains_key(&a.legacy_id))
        .collect();
    let pending_owner_keys: BTreeSet<i64> = pending_agents
        .iter()
        .map(|a| a.legacy_user_id)
        .filter(|k| !owner_map.contains_key(k))
        .collect();
    let owners_reused_from_chiasm = pending_owner_keys
        .iter()
        .filter(|k| chiasm_map.contains_key(k))
        .count();
    let owners_minted = pending_owner_keys.len() - owners_reused_from_chiasm;

    if options.dry_run {
        return Ok(BackfillReport {
            owners_by_legacy_user: owner_map,
            owners_reused_from_chiasm,
            owners_minted,
            agents_imported: pending_agents.len(),
            agents_skipped: agents.len() - pending_agents.len(),
            dry_run: true,
        });
    }

    // Phase 3: resolve owner principals (chiasm reuse first, then mint) and mint one Agent
    // principal per pending legacy agent.
    for key in pending_owner_keys {
        let principal = match chiasm_map.get(&key) {
            Some(reused) => *reused,
            None => {
                directory
                    .enroll(PrincipalKind::Human, Some(format!("legacy-user-{key}")))
                    .await
                    .map_err(|e| SomaError::Backfill(format!("enroll owner key {key}: {e}")))?
                    .id
            }
        };
        owner_map.insert(key, principal);
    }
    let mut minted_agents: Vec<(&LegacyAgent, PrincipalId)> = Vec::with_capacity(pending_agents.len());
    for agent in &pending_agents {
        let principal = directory
            .enroll(PrincipalKind::Agent, Some(agent.name.clone()))
            .await
            .map_err(|e| {
                SomaError::Backfill(format!("enroll agent {:?}: {e}", agent.name))
            })?;
        minted_agents.push((agent, principal.id));
    }

    // Phase 4: one transaction for every target write.
    let tx = target.transaction().map_err(berr)?;
    for (key, pid) in &owner_map {
        tx.execute(
            "INSERT OR IGNORE INTO soma_legacy_user_id_map (user_id, principal_id) VALUES (?1, ?2)",
            rusqlite::params![key, pid.to_string()],
        )
        .map_err(berr)?;
    }
    let mut agents_imported = 0;
    for (agent, principal) in &minted_agents {
        tx.execute(
            "INSERT INTO soma_presence (principal_id, tenant, name, agent_type, description, \
             capabilities, status, config, heartbeat_at, quality_score, drift_flags, \
             created_at, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            rusqlite::params![
                principal.to_string(),
                tenant.to_string(),
                &agent.name,
                &agent.agent_type,
                &agent.description,
                serde_json::to_string(&agent.capabilities)
                    .map_err(|e| SomaError::Backfill(format!("capabilities serialize: {e}")))?,
                agent.status.as_str(),
                agent.config.to_string(),
                agent.heartbeat_at.as_ref().map(ts_to_db).transpose()?,
                agent.quality_score,
                serde_json::to_string(&agent.drift_flags)
                    .map_err(|e| SomaError::Backfill(format!("drift_flags serialize: {e}")))?,
                ts_to_db(&agent.created_at)?,
                ts_to_db(&agent.updated_at)?,
            ],
        )
        .map_err(berr)?;
        tx.execute(
            "INSERT INTO soma_legacy_agent_map (legacy_agent_id, principal_id, legacy_name) \
             VALUES (?1, ?2, ?3)",
            rusqlite::params![agent.legacy_id, principal.to_string(), &agent.name],
        )
        .map_err(berr)?;
        agents_imported += 1;
    }
    tx.commit().map_err(berr)?;

    Ok(BackfillReport {
        owners_by_legacy_user: owner_map,
        owners_reused_from_chiasm,
        owners_minted,
        agents_imported,
        agents_skipped: agents.len() - agents_imported,
        dry_run: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use syntheos_axon::AxonBus;
    use syntheos_identity::InMemoryDirectory;

    use crate::model::PresenceFilter;
    use crate::store::SomaStore;

    /// The live Kleos soma_agents DDL, as a test fixture.
    const LEGACY_DDL: &str = "
        CREATE TABLE soma_agents (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            name TEXT NOT NULL,
            type TEXT NOT NULL,
            description TEXT,
            capabilities TEXT DEFAULT '[]',
            status TEXT NOT NULL DEFAULT 'pending',
            config TEXT DEFAULT '{}',
            heartbeat_at TEXT,
            created_at TEXT NOT NULL DEFAULT (datetime('now')),
            updated_at TEXT NOT NULL DEFAULT (datetime('now')),
            quality_score REAL,
            drift_flags TEXT,
            user_id INTEGER NOT NULL DEFAULT 1,
            UNIQUE(name, user_id)
        );
    ";

    /// Paths for a fresh (legacy, target) database pair, unique per test.
    fn db_pair(tag: &str) -> (std::path::PathBuf, std::path::PathBuf) {
        let dir = std::env::temp_dir();
        let nonce = PrincipalId::new();
        (
            dir.join(format!("henosis-soma-backfill-legacy-{tag}-{nonce}.sqlite")),
            dir.join(format!("henosis-soma-backfill-target-{tag}-{nonce}.sqlite")),
        )
    }

    /// Create a legacy fixture: 3 agents (one with messy JSON), one owner key.
    fn build_legacy_fixture(path: &Path) {
        let conn = Connection::open(path).expect("legacy fixture");
        conn.execute_batch(LEGACY_DDL).expect("legacy ddl");
        conn.execute_batch(
            "INSERT INTO soma_agents (name, type, capabilities, status, heartbeat_at, quality_score, user_id, created_at, updated_at)
             VALUES ('claude-code', 'coding', '[\"planning\",\"implementation\"]', 'online',
                     '2026-06-09 12:00:00', 0.85, 1, '2026-04-04 17:30:17', '2026-06-09 12:00:00');
             INSERT INTO soma_agents (name, type, capabilities, status, config, drift_flags, user_id, created_at, updated_at)
             VALUES ('messy', 'cli', '[\"code\", 7]', 'pending', 'not json', NULL, 1,
                     '2026-05-01 08:00:00', '2026-05-01 08:00:00');
             INSERT INTO soma_agents (name, type, status, user_id, created_at, updated_at)
             VALUES ('synapse', 'cli', 'offline', 1, '2026-05-19 02:19:17', '2026-06-10 01:02:28');",
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
    async fn dry_run_counts_without_writing() {
        let (legacy, target) = db_pair("dry");
        build_legacy_fixture(&legacy);
        let dir = InMemoryDirectory::new();

        let report = backfill_from_kleos(
            &legacy,
            &target,
            None,
            &dir,
            TenantId::new(),
            BackfillOptions { dry_run: true },
        )
        .await
        .expect("dry run");
        assert!(report.dry_run);
        assert_eq!(report.owners_minted, 1);
        assert_eq!(report.owners_reused_from_chiasm, 0);
        assert_eq!(report.agents_imported, 3);
        assert!(dir.list().await.expect("list").is_empty(), "nothing enrolled");
        cleanup(&[&legacy, &target]);
    }

    #[tokio::test]
    async fn apply_imports_agents_as_principals_with_sanitized_json() {
        let (legacy, target) = db_pair("apply");
        build_legacy_fixture(&legacy);
        let dir = Arc::new(InMemoryDirectory::new());
        let tenant = TenantId::new();

        let report = backfill_from_kleos(
            &legacy,
            &target,
            None,
            dir.as_ref(),
            tenant,
            BackfillOptions::default(),
        )
        .await
        .expect("apply");
        assert_eq!(report.owners_minted, 1);
        assert_eq!(report.agents_imported, 3);

        // 1 Human owner + 3 Agent principals enrolled.
        let principals = dir.list().await.expect("list");
        assert_eq!(principals.len(), 4);
        assert_eq!(
            principals.iter().filter(|p| p.kind == PrincipalKind::Agent).count(),
            3
        );

        // The presence rows read back through the strict store: sanitization worked.
        let store = SomaStore::open(&target, Arc::new(AxonBus::new()), dir.clone()).expect("open");
        let all = store.list(PresenceFilter::default()).await.expect("list");
        assert_eq!(all.len(), 3);
        let cc = store.get_by_name(tenant, "claude-code").await.expect("get").expect("present");
        assert_eq!(cc.status, PresenceStatus::Online, "legacy status preserved");
        assert_eq!(cc.quality_score, Some(0.85));
        assert!(cc.heartbeat_at.is_some(), "legacy heartbeat converted");
        assert_eq!(cc.capabilities, vec!["planning".to_string(), "implementation".to_string()]);
        let messy = store.get_by_name(tenant, "messy").await.expect("get").expect("present");
        assert_eq!(messy.capabilities, vec!["code".to_string(), "7".to_string()]);
        assert_eq!(messy.config, serde_json::json!({}), "non-object config degraded");
        assert!(messy.drift_flags.is_empty(), "NULL drift_flags degraded");
        cleanup(&[&legacy, &target]);
    }

    #[tokio::test]
    async fn chiasm_cross_map_reuses_owner_principal() {
        let (legacy, target) = db_pair("crossmap");
        build_legacy_fixture(&legacy);
        // A chiasm target db whose backfill already mapped legacy key 1.
        let chiasm_path = std::env::temp_dir()
            .join(format!("henosis-soma-backfill-chiasm-{}.sqlite", PrincipalId::new()));
        let chiasm_owner = PrincipalId::new();
        {
            let conn = Connection::open(&chiasm_path).expect("chiasm fixture");
            conn.execute_batch(
                "CREATE TABLE chiasm_legacy_user_id_map (
                     user_id INTEGER NOT NULL PRIMARY KEY,
                     principal_id TEXT NOT NULL UNIQUE);",
            )
            .expect("ddl");
            conn.execute(
                "INSERT INTO chiasm_legacy_user_id_map (user_id, principal_id) VALUES (1, ?1)",
                rusqlite::params![chiasm_owner.to_string()],
            )
            .expect("row");
        }
        let dir = InMemoryDirectory::new();
        let report = backfill_from_kleos(
            &legacy,
            &target,
            Some(&chiasm_path),
            &dir,
            TenantId::new(),
            BackfillOptions::default(),
        )
        .await
        .expect("apply");
        // The same legacy key resolved to chiasm's principal; no Human was minted here.
        assert_eq!(report.owners_reused_from_chiasm, 1);
        assert_eq!(report.owners_minted, 0);
        assert_eq!(report.owners_by_legacy_user[&1], chiasm_owner);
        assert!(dir
            .list()
            .await
            .expect("list")
            .iter()
            .all(|p| p.kind == PrincipalKind::Agent));
        cleanup(&[&legacy, &target, &chiasm_path]);
    }

    #[tokio::test]
    async fn rerun_is_idempotent() {
        let (legacy, target) = db_pair("rerun");
        build_legacy_fixture(&legacy);
        let dir = InMemoryDirectory::new();
        let tenant = TenantId::new();

        let first =
            backfill_from_kleos(&legacy, &target, None, &dir, tenant, BackfillOptions::default())
                .await
                .expect("first run");
        let second =
            backfill_from_kleos(&legacy, &target, None, &dir, tenant, BackfillOptions::default())
                .await
                .expect("second run");
        assert_eq!(second.agents_imported, 0);
        assert_eq!(second.agents_skipped, 3);
        assert_eq!(second.owners_minted, 0);
        assert_eq!(second.owners_by_legacy_user, first.owners_by_legacy_user);
        assert_eq!(dir.list().await.expect("list").len(), 4, "no duplicate principals");
        cleanup(&[&legacy, &target]);
    }

    #[tokio::test]
    async fn bad_status_fails_before_any_write() {
        let (legacy, target) = db_pair("badstatus");
        build_legacy_fixture(&legacy);
        {
            let conn = Connection::open(&legacy).expect("legacy");
            conn.execute(
                "INSERT INTO soma_agents (name, type, status, user_id) \
                 VALUES ('broken', 'cli', 'wedged', 1)",
                [],
            )
            .expect("insert");
        }
        let dir = InMemoryDirectory::new();
        let err = backfill_from_kleos(
            &legacy,
            &target,
            None,
            &dir,
            TenantId::new(),
            BackfillOptions::default(),
        )
        .await
        .expect_err("must fail on bad status");
        assert!(matches!(err, SomaError::Backfill(_)));
        assert!(dir.list().await.expect("list").is_empty(), "validation precedes minting");
        cleanup(&[&legacy, &target]);
    }

    #[tokio::test]
    async fn cross_owner_name_collision_is_explicit_error() {
        let (legacy, target) = db_pair("collision");
        build_legacy_fixture(&legacy);
        {
            let conn = Connection::open(&legacy).expect("legacy");
            // Same name under a different legacy owner: legal in Kleos, collides in one tenant.
            conn.execute(
                "INSERT INTO soma_agents (name, type, user_id) VALUES ('claude-code', 'cli', 2)",
                [],
            )
            .expect("insert");
        }
        let dir = InMemoryDirectory::new();
        let err = backfill_from_kleos(
            &legacy,
            &target,
            None,
            &dir,
            TenantId::new(),
            BackfillOptions::default(),
        )
        .await
        .expect_err("must fail on name collision");
        assert!(matches!(err, SomaError::Backfill(msg) if msg.contains("claude-code")));
        cleanup(&[&legacy, &target]);
    }
}
