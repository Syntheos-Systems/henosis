//! The persistent SQLite-backed [`PrincipalDirectory`].
//!
//! This is the unit-6 DB decision (ADR Item 3 / gating story G2): `InMemoryDirectory` cannot
//! survive a restart, but the `user_id -> PrincipalId` backfill the projection convention
//! prescribes must persist to be usable as a one-time migration tool, and Phase 1 Chiasm/Soma
//! extraction needs principal lookup that outlives process restarts.
//!
//! Backend: `rusqlite` (matches Kleos's SQLite tooling). Schema is managed by the kernel-crate
//! migration convention -- `PRAGMA user_version` plus ordered `migrations/Vn__*.sql` files
//! (see `2026-06-10-henosis-db-and-migration-convention.md`). This crate is the reference
//! implementation of that convention; copy `apply_migrations` into each later SQLite kernel crate.
//!
//! Concurrency: a single `Connection` behind a `Mutex` (a principal directory is low-volume).
//! No `.await` is held across the lock. A connection pool can replace the `Mutex` later without
//! changing the `PrincipalDirectory` surface.

use std::path::Path;
use std::sync::Mutex;

use async_trait::async_trait;
use rusqlite::{Connection, OptionalExtension};
use syntheos_contracts::{Principal, PrincipalId, PrincipalKind};

use crate::directory::PrincipalDirectory;
use crate::error::DirectoryError;

/// Ordered schema migrations, applied by `PRAGMA user_version`. Append-only: never edit a shipped
/// entry; add a new `(version, sql)` pair and a matching `migrations/Vn__*.sql` file.
const MIGRATIONS: &[(i64, &str)] = &[(1, include_str!("../migrations/V1__principals.sql"))];

/// A persistent [`PrincipalDirectory`] backed by a single SQLite database.
///
/// Share it as `Arc<SqliteDirectory>` or `Arc<dyn PrincipalDirectory>`; all methods take `&self`.
pub struct SqliteDirectory {
    /// The one connection, serialized by a `Mutex` (rusqlite `Connection` is `Send` but not `Sync`).
    conn: Mutex<Connection>,
}

/// Map a generic rusqlite error to an opaque backend error.
fn backend(e: rusqlite::Error) -> DirectoryError {
    DirectoryError::Backend(e.to_string())
}

impl SqliteDirectory {
    /// Open (creating the file if absent) a directory at `path`, applying any pending migrations.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, DirectoryError> {
        let conn = Connection::open(path).map_err(backend)?;
        Self::from_conn(conn)
    }

    /// Open an ephemeral in-memory directory, applying migrations. For tests and throwaway use.
    pub fn open_in_memory() -> Result<Self, DirectoryError> {
        let conn = Connection::open_in_memory().map_err(backend)?;
        Self::from_conn(conn)
    }

    /// Apply migrations to a fresh connection and wrap it.
    fn from_conn(mut conn: Connection) -> Result<Self, DirectoryError> {
        apply_migrations(&mut conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Insert `principal`, mapping a primary-key/unique violation to [`DirectoryError::AlreadyExists`]
    /// (the persistent analogue of the in-memory directory's `entry()` uniqueness guard) and any
    /// other failure to [`DirectoryError::Backend`].
    fn insert(conn: &Connection, principal: &Principal) -> Result<(), DirectoryError> {
        let kind = serde_json::to_string(&principal.kind)
            .map_err(|e| DirectoryError::Backend(format!("serialize kind: {e}")))?;
        conn.execute(
            "INSERT INTO principals (id, kind, display) VALUES (?1, ?2, ?3)",
            rusqlite::params![principal.id.to_string(), kind, principal.display.as_deref()],
        )
        .map_err(|e| match &e {
            rusqlite::Error::SqliteFailure(f, _)
                if f.code == rusqlite::ErrorCode::ConstraintViolation =>
            {
                DirectoryError::AlreadyExists(principal.id)
            }
            _ => DirectoryError::Backend(e.to_string()),
        })?;
        Ok(())
    }

    /// Enroll a principal under a caller-supplied id. `#[cfg(test)]`-gated, exactly like
    /// `InMemoryDirectory::enroll_with_id`, so the `AlreadyExists` path is testable without
    /// exposing a public choose-your-own-id API.
    #[cfg(test)]
    fn enroll_with_id(
        &self,
        id: PrincipalId,
        kind: PrincipalKind,
        display: Option<String>,
    ) -> Result<Principal, DirectoryError> {
        let principal = Principal { id, kind, display };
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        Self::insert(&conn, &principal)?;
        Ok(principal)
    }
}

/// Reconstruct a [`Principal`] from its stored columns, surfacing a corrupt row as a backend error.
fn parse_principal(
    id_s: &str,
    kind_s: &str,
    display: Option<String>,
) -> Result<Principal, DirectoryError> {
    let id = id_s
        .parse::<PrincipalId>()
        .map_err(|e| DirectoryError::Backend(format!("corrupt principal id {id_s:?}: {e}")))?;
    let kind: PrincipalKind = serde_json::from_str(kind_s)
        .map_err(|e| DirectoryError::Backend(format!("corrupt principal kind {kind_s:?}: {e}")))?;
    Ok(Principal { id, kind, display })
}

/// Apply every migration whose version exceeds the database's `PRAGMA user_version`, each inside
/// its own transaction, bumping `user_version` as it goes. Idempotent: re-opening an up-to-date
/// database applies nothing.
fn apply_migrations(conn: &mut Connection) -> Result<(), DirectoryError> {
    let mut version: i64 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .map_err(backend)?;
    for (v, sql) in MIGRATIONS {
        if *v > version {
            let tx = conn.transaction().map_err(backend)?;
            tx.execute_batch(sql)
                .map_err(|e| DirectoryError::Backend(format!("migration V{v} failed: {e}")))?;
            tx.pragma_update(None, "user_version", *v).map_err(backend)?;
            tx.commit().map_err(backend)?;
            version = *v;
        }
    }
    Ok(())
}

#[async_trait]
impl PrincipalDirectory for SqliteDirectory {
    async fn enroll(
        &self,
        kind: PrincipalKind,
        display: Option<String>,
    ) -> Result<Principal, DirectoryError> {
        let principal = Principal {
            id: PrincipalId::new(),
            kind,
            display,
        };
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        Self::insert(&conn, &principal)?;
        Ok(principal)
    }

    async fn lookup(&self, id: PrincipalId) -> Result<Option<Principal>, DirectoryError> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let row = conn
            .query_row(
                "SELECT id, kind, display FROM principals WHERE id = ?1",
                rusqlite::params![id.to_string()],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, Option<String>>(2)?,
                    ))
                },
            )
            .optional()
            .map_err(backend)?;
        match row {
            Some((id_s, kind_s, display)) => Ok(Some(parse_principal(&id_s, &kind_s, display)?)),
            None => Ok(None),
        }
    }

    async fn list(&self) -> Result<Vec<Principal>, DirectoryError> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let mut stmt = conn
            .prepare("SELECT id, kind, display FROM principals")
            .map_err(backend)?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<String>>(2)?,
                ))
            })
            .map_err(backend)?;
        let mut out = Vec::new();
        for row in rows {
            let (id_s, kind_s, display) = row.map_err(backend)?;
            out.push(parse_principal(&id_s, &kind_s, display)?);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[tokio::test]
    async fn enroll_then_lookup() {
        let dir = SqliteDirectory::open_in_memory().expect("open");
        let p = dir
            .enroll(PrincipalKind::Agent, Some("eidolon".into()))
            .await
            .expect("enroll");
        let got = dir.lookup(p.id).await.expect("lookup").expect("present");
        assert_eq!(got, p);
    }

    #[tokio::test]
    async fn lookup_unknown_is_none() {
        let dir = SqliteDirectory::open_in_memory().expect("open");
        assert!(dir
            .lookup(PrincipalId::new())
            .await
            .expect("lookup")
            .is_none());
    }

    #[tokio::test]
    async fn kind_and_display_roundtrip_through_storage() {
        let dir = SqliteDirectory::open_in_memory().expect("open");
        let p = dir
            .enroll(PrincipalKind::Service, Some("hermes".into()))
            .await
            .expect("enroll");
        let got = dir.lookup(p.id).await.expect("lookup").expect("present");
        assert_eq!(got.kind, PrincipalKind::Service);
        assert_eq!(got.display.as_deref(), Some("hermes"));
    }

    #[tokio::test]
    async fn list_returns_all_enrolled() {
        let dir = SqliteDirectory::open_in_memory().expect("open");
        dir.enroll(PrincipalKind::Agent, None).await.expect("enroll");
        dir.enroll(PrincipalKind::Human, None).await.expect("enroll");
        assert_eq!(dir.list().await.expect("list").len(), 2);
    }

    #[tokio::test]
    async fn usable_as_trait_object() {
        let dir: Arc<dyn PrincipalDirectory> = Arc::new(SqliteDirectory::open_in_memory().expect("open"));
        let p = dir
            .enroll(PrincipalKind::Integration, Some("github".into()))
            .await
            .expect("enroll");
        let got = dir.lookup(p.id).await.expect("lookup").expect("present");
        assert_eq!(got, p);
    }

    /// A duplicate enrollment on the same `PrincipalId` is rejected by the PRIMARY KEY, surfaced as
    /// `AlreadyExists`, and does not overwrite the first record.
    #[tokio::test]
    async fn duplicate_id_is_rejected() {
        let dir = SqliteDirectory::open_in_memory().expect("open");
        let id = PrincipalId::new();
        dir.enroll_with_id(id, PrincipalKind::Agent, Some("first".into()))
            .expect("first enroll must succeed");
        let err = dir
            .enroll_with_id(id, PrincipalKind::Human, Some("collision".into()))
            .expect_err("duplicate enroll must fail");
        assert!(
            matches!(err, DirectoryError::AlreadyExists(eid) if eid == id),
            "expected AlreadyExists({id}), got {err:?}",
        );
        let got = dir.lookup(id).await.expect("lookup").expect("present");
        assert_eq!(got.display.as_deref(), Some("first"));
        assert_eq!(got.kind, PrincipalKind::Agent);
    }

    /// Enrolled principals survive reopening the same database file -- the property the in-memory
    /// directory lacks and the backfill needs.
    #[tokio::test]
    async fn persists_across_reopen() {
        let tmp = std::env::temp_dir().join(format!("henosis-dir-{}.sqlite", PrincipalId::new()));
        let id;
        {
            let dir = SqliteDirectory::open(&tmp).expect("open");
            let p = dir
                .enroll(PrincipalKind::Agent, Some("durable".into()))
                .await
                .expect("enroll");
            id = p.id;
        }
        {
            let dir = SqliteDirectory::open(&tmp).expect("reopen");
            let got = dir.lookup(id).await.expect("lookup").expect("present after reopen");
            assert_eq!(got.display.as_deref(), Some("durable"));
        }
        let _ = std::fs::remove_file(&tmp);
    }

    /// Re-opening runs migrations idempotently (user_version already at head -> no error).
    #[tokio::test]
    async fn migrations_are_idempotent_on_reopen() {
        let tmp = std::env::temp_dir().join(format!("henosis-mig-{}.sqlite", PrincipalId::new()));
        SqliteDirectory::open(&tmp).expect("first open applies V1");
        SqliteDirectory::open(&tmp).expect("second open applies nothing");
        let _ = std::fs::remove_file(&tmp);
    }
}
