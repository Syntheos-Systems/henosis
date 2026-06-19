//! Instance-level access grants for Space Sharing.
//!
//! In a sharded (multi-tenant) deployment each `user_id` owns a physically
//! separate database shard. An `instance_grants` row lets a grantee reach an
//! owner's ENTIRE shard at a given access level. Enforcement happens at the
//! single `resolve_db_for_user` chokepoint in kleos-server: a request names a
//! target owner (act-as), the chokepoint authorizes (caller is owner, caller
//! holds Admin, or a grant covers the requested access), then resolves the
//! owner's shard. One place enforces all delegated access.
//!
//! The grants registry lives in the control/global DB (`state.db`) because it
//! maps cross-shard (grantee -> owner) relationships that no single tenant
//! shard can hold.

use crate::db::Database;
use crate::{EngError, Result};
use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};

/// Access level a grant conveys over an owner's shard.
///
/// A two-level lattice where `Write` implies `Read`: a write grant satisfies a
/// read requirement, but not the reverse.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InstanceAccess {
    /// Read-only delegated access to the owner's shard.
    Read,
    /// Read and write delegated access to the owner's shard.
    Write,
}

impl InstanceAccess {
    /// Lattice rank used for `satisfies` comparison (Read=0, Write=1).
    fn rank(self) -> u8 {
        match self {
            Self::Read => 0,
            Self::Write => 1,
        }
    }

    /// True when this access level meets or exceeds the required minimum.
    pub fn satisfies(self, min: InstanceAccess) -> bool {
        self.rank() >= min.rank()
    }

    /// Canonical lowercase string stored in the `access` column.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
        }
    }
}

impl std::str::FromStr for InstanceAccess {
    type Err = EngError;

    /// Parse the canonical `read`/`write` token (case-insensitive).
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "read" => Ok(Self::Read),
            "write" => Ok(Self::Write),
            other => Err(EngError::InvalidInput(format!(
                "invalid instance access level: {other}"
            ))),
        }
    }
}

/// A single delegated-access grant over an owner's whole shard.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstanceGrant {
    /// The shard owner whose data the grant exposes.
    pub owner_user_id: i64,
    /// The user who receives delegated access.
    pub grantee_user_id: i64,
    /// The level of access the grant conveys.
    pub access: InstanceAccess,
    /// The user (owner or admin) who created the grant.
    pub granted_by: i64,
    /// Creation timestamp (`datetime('now')`).
    pub created_at: String,
}

/// Create or update a grant. Upserts on the `(owner, grantee)` primary key so
/// re-granting at a different access level updates in place.
///
/// Rejects a self-grant (`owner == grantee`): a user already has full access to
/// their own shard, so a self-grant would be a meaningless row that could mask
/// authorization bugs.
pub async fn grant_instance_access(
    db: &Database,
    owner_user_id: i64,
    grantee_user_id: i64,
    access: InstanceAccess,
    granted_by: i64,
) -> Result<()> {
    // A user already has full access to their own shard; a self-grant is a
    // meaningless row that could mask an authorization bug, so reject it.
    if owner_user_id == grantee_user_id {
        return Err(EngError::InvalidInput(
            "cannot grant instance access to the owner themselves".into(),
        ));
    }
    let access_str = access.as_str();
    db.write(move |conn| {
        // Upsert on the (owner, grantee) primary key: re-granting updates the
        // access level and attribution in place and preserves created_at.
        conn.execute(
            "INSERT INTO instance_grants
                 (owner_user_id, grantee_user_id, access, granted_by)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(owner_user_id, grantee_user_id)
             DO UPDATE SET access = excluded.access, granted_by = excluded.granted_by",
            rusqlite::params![owner_user_id, grantee_user_id, access_str, granted_by],
        )?;
        Ok(())
    })
    .await
}

/// Revoke a grant. Idempotent: revoking a grant that does not exist is a no-op
/// and returns `Ok`.
pub async fn revoke_instance_access(
    db: &Database,
    owner_user_id: i64,
    grantee_user_id: i64,
) -> Result<()> {
    db.write(move |conn| {
        // DELETE of a non-existent row affects zero rows and is not an error,
        // which gives the desired idempotent revoke.
        conn.execute(
            "DELETE FROM instance_grants
             WHERE owner_user_id = ?1 AND grantee_user_id = ?2",
            rusqlite::params![owner_user_id, grantee_user_id],
        )?;
        Ok(())
    })
    .await
}

/// Look up the access level a grantee holds over an owner's shard, if any.
///
/// This is the chokepoint hot path: it runs on every act-as request where the
/// caller is neither the owner nor an Admin. Keyed on the `(owner, grantee)`
/// primary key for an index-only lookup.
pub async fn lookup_instance_grant(
    db: &Database,
    owner_user_id: i64,
    grantee_user_id: i64,
) -> Result<Option<InstanceAccess>> {
    let row: Option<String> = db
        .read(move |conn| {
            conn.query_row(
                "SELECT access FROM instance_grants
                 WHERE owner_user_id = ?1 AND grantee_user_id = ?2",
                rusqlite::params![owner_user_id, grantee_user_id],
                |r| r.get::<_, String>(0),
            )
            .optional()
            .map_err(EngError::from)
        })
        .await?;
    // A row with a corrupt access token is a fail-closed condition: surface it
    // rather than silently treating it as no grant.
    row.map(|s| s.parse::<InstanceAccess>()).transpose()
}

/// List every grant an owner has issued, newest first. Backs the owner/admin
/// management UI.
pub async fn list_grants_for_owner(
    db: &Database,
    owner_user_id: i64,
) -> Result<Vec<InstanceGrant>> {
    db.read(move |conn| {
        let mut stmt = conn.prepare(
            "SELECT owner_user_id, grantee_user_id, access, granted_by, created_at
             FROM instance_grants
             WHERE owner_user_id = ?1
             ORDER BY created_at DESC, grantee_user_id ASC",
        )?;
        let grants = stmt
            .query_map(rusqlite::params![owner_user_id], |row| {
                let access_str: String = row.get(2)?;
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    access_str,
                    row.get::<_, i64>(3)?,
                    row.get::<_, String>(4)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
            .into_iter()
            .map(|(owner, grantee, access_str, granted_by, created_at)| {
                Ok(InstanceGrant {
                    owner_user_id: owner,
                    grantee_user_id: grantee,
                    access: access_str.parse::<InstanceAccess>()?,
                    granted_by,
                    created_at,
                })
            })
            .collect::<Result<Vec<InstanceGrant>>>()?;
        Ok(grants)
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// In-memory control DB with all global migrations applied (including v84).
    async fn control_db() -> Database {
        Database::connect_memory()
            .await
            .expect("in-memory control db")
    }

    #[test]
    fn access_lattice_write_implies_read() {
        // Write satisfies both Read and Write requirements.
        assert!(InstanceAccess::Write.satisfies(InstanceAccess::Read));
        assert!(InstanceAccess::Write.satisfies(InstanceAccess::Write));
        // Read satisfies only Read, never Write.
        assert!(InstanceAccess::Read.satisfies(InstanceAccess::Read));
        assert!(!InstanceAccess::Read.satisfies(InstanceAccess::Write));
    }

    #[test]
    fn access_parses_canonical_tokens() {
        assert_eq!(
            "read".parse::<InstanceAccess>().unwrap(),
            InstanceAccess::Read
        );
        assert_eq!(
            "WRITE".parse::<InstanceAccess>().unwrap(),
            InstanceAccess::Write
        );
        assert!("admin".parse::<InstanceAccess>().is_err());
    }

    #[tokio::test]
    async fn grant_then_lookup_roundtrips() {
        let db = control_db().await;
        // Owner 10 grants grantee 20 read access, granted by admin 1.
        grant_instance_access(&db, 10, 20, InstanceAccess::Read, 1)
            .await
            .expect("grant succeeds");

        let found = lookup_instance_grant(&db, 10, 20)
            .await
            .expect("lookup succeeds");
        assert_eq!(found, Some(InstanceAccess::Read));

        // A grantee with no grant resolves to None.
        let missing = lookup_instance_grant(&db, 10, 99)
            .await
            .expect("lookup succeeds");
        assert_eq!(missing, None);
    }

    #[tokio::test]
    async fn grant_upserts_access_level() {
        let db = control_db().await;
        grant_instance_access(&db, 10, 20, InstanceAccess::Read, 1)
            .await
            .expect("initial read grant");
        // Re-granting at write must update in place, not create a duplicate row.
        grant_instance_access(&db, 10, 20, InstanceAccess::Write, 1)
            .await
            .expect("upgrade to write grant");

        let found = lookup_instance_grant(&db, 10, 20).await.unwrap();
        assert_eq!(found, Some(InstanceAccess::Write));

        let grants = list_grants_for_owner(&db, 10).await.unwrap();
        assert_eq!(grants.len(), 1, "upsert must not duplicate the row");
    }

    #[tokio::test]
    async fn revoke_removes_grant_and_is_idempotent() {
        let db = control_db().await;
        grant_instance_access(&db, 10, 20, InstanceAccess::Write, 1)
            .await
            .unwrap();
        revoke_instance_access(&db, 10, 20).await.expect("revoke");
        assert_eq!(lookup_instance_grant(&db, 10, 20).await.unwrap(), None);
        // Revoking again is a harmless no-op.
        revoke_instance_access(&db, 10, 20)
            .await
            .expect("idempotent revoke");
    }

    #[tokio::test]
    async fn self_grant_is_rejected() {
        let db = control_db().await;
        let err = grant_instance_access(&db, 10, 10, InstanceAccess::Read, 1)
            .await
            .expect_err("self-grant must be rejected");
        assert!(matches!(err, EngError::InvalidInput(_)));
    }

    #[tokio::test]
    async fn list_grants_for_owner_scopes_to_owner() {
        let db = control_db().await;
        // Owner 10 issues two grants; owner 11 issues one.
        grant_instance_access(&db, 10, 20, InstanceAccess::Read, 1)
            .await
            .unwrap();
        grant_instance_access(&db, 10, 21, InstanceAccess::Write, 1)
            .await
            .unwrap();
        grant_instance_access(&db, 11, 20, InstanceAccess::Read, 1)
            .await
            .unwrap();

        let owner_10 = list_grants_for_owner(&db, 10).await.unwrap();
        assert_eq!(owner_10.len(), 2, "owner 10 has exactly two grants");
        assert!(owner_10.iter().all(|g| g.owner_user_id == 10));
    }
}
