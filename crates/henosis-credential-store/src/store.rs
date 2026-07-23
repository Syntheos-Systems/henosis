//! The SQLite-backed Phylax credential store.
//!
//! Absorbed from `kleos-phylax`/`kleos-cred` onto the Henosis substrate: ownership is a
//! [`TenantId`] (every read/write scopes on it, replacing the Kleos `WHERE user_id = ?`
//! predicate), secret values are AES-256-GCM encrypted at the field level before they touch
//! disk, and lifecycle events are typed and published to the in-process [`AxonBus`].
//!
//! This module owns the secret table and its owner-tier administration surface: store, read (the
//! ONLY plaintext path, named loudly and never reachable through the gate), delete, and
//! list-names. Capability policies live in [`crate::policy_store`], the use-without-holding
//! resolve modes in [`crate::resolve`], and the fail-closed gate in [`crate::gate`].

use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};

use rusqlite::{Connection, OptionalExtension};
use syntheos_axon::AxonBus;
use syntheos_contracts::{PrincipalId, TenantId, Timestamp, TypedEvent};
use zeroize::Zeroizing;

use crate::crypto::{self, KEY_SIZE};
use crate::error::PhylaxError;
use crate::events::{SecretDeleted, SecretStored};
use crate::model::SecretData;

/// Ordered schema migrations, applied by `PRAGMA user_version`. Append-only (see the DB convention).
const MIGRATIONS: &[(i64, &str)] = &[
    (1, include_str!("../migrations/V1__phylax.sql")),
    (2, include_str!("../migrations/V2__phylax_policies.sql")),
];

/// Map a generic rusqlite error to an opaque backend error.
pub(crate) fn berr(e: impl std::fmt::Display) -> PhylaxError {
    PhylaxError::Backend(e.to_string())
}

/// The current time as its stored RFC3339-UTC string (via the contracts wire form -- `Timestamp`
/// has no `Display`, only a serde representation).
pub(crate) fn now_string() -> Result<String, PhylaxError> {
    serde_json::to_value(Timestamp::now())
        .ok()
        .and_then(|v| v.as_str().map(String::from))
        .ok_or_else(|| PhylaxError::Backend("timestamp serialize".into()))
}

/// The credential store.
///
/// Share it as `Arc<PhylaxStore>`; all methods take `&self`. The master key is held for the
/// store's lifetime and zeroized on drop.
pub struct PhylaxStore {
    /// The one connection, serialized by a `Mutex` (rusqlite `Connection` is `Send`, not `Sync`).
    conn: Mutex<Connection>,
    /// The bus secret/policy lifecycle events are published onto.
    bus: Arc<AxonBus>,
    /// The 32-byte AES-256-GCM master key every secret value is encrypted under.
    master_key: Zeroizing<[u8; KEY_SIZE]>,
}

/// Opens the credential store and manages encrypted secret records.
impl PhylaxStore {
    /// Open (or create) a store at `path` under `master_key`.
    pub fn open(
        path: impl AsRef<Path>,
        bus: Arc<AxonBus>,
        master_key: [u8; KEY_SIZE],
    ) -> Result<Self, PhylaxError> {
        let conn = Connection::open(path).map_err(berr)?;
        Self::from_conn(conn, bus, master_key)
    }

    /// Open an in-memory store (tests).
    pub fn open_in_memory(
        bus: Arc<AxonBus>,
        master_key: [u8; KEY_SIZE],
    ) -> Result<Self, PhylaxError> {
        let conn = Connection::open_in_memory().map_err(berr)?;
        Self::from_conn(conn, bus, master_key)
    }

    /// Apply migrations and wrap the connection.
    fn from_conn(
        mut conn: Connection,
        bus: Arc<AxonBus>,
        master_key: [u8; KEY_SIZE],
    ) -> Result<Self, PhylaxError> {
        apply_migrations(&mut conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
            bus,
            master_key: Zeroizing::new(master_key),
        })
    }

    /// Lock the connection, recovering from a poisoned mutex. Crate-internal so sibling modules
    /// (policy store, resolve modes) share the one serialized connection.
    pub(crate) fn lock_conn(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The master key, for the crate-internal resolve modes that decrypt in-process.
    pub(crate) fn master_key(&self) -> &[u8; KEY_SIZE] {
        &self.master_key
    }

    /// Publish a phylax event, fire-and-forget. A publish failure is logged, never fatal --
    /// telemetry must not change a credential operation's outcome.
    fn emit<E: TypedEvent>(&self, event: &E, tenant: TenantId, principal: PrincipalId) {
        if let Err(e) = self.bus.publish_event(event, tenant, principal) {
            tracing::warn!(error = %e, kind = E::KIND, "failed to publish phylax event");
        }
    }

    /// Store (insert or overwrite) a secret. Owner-tier administration.
    ///
    /// `actor` is the principal performing the write, recorded on the audit event. The secret
    /// value is encrypted before it touches disk.
    pub fn store_secret(
        &self,
        tenant: &TenantId,
        actor: &PrincipalId,
        category: &str,
        name: &str,
        data: &SecretData,
    ) -> Result<(), PhylaxError> {
        // Serialize then encrypt; the plaintext JSON is zeroized on drop.
        let plaintext = Zeroizing::new(
            serde_json::to_vec(data).map_err(|e| PhylaxError::Encryption(e.to_string()))?,
        );
        let blob = crypto::encrypt(&self.master_key, &plaintext)?;
        let now = now_string()?;

        {
            let conn = self.lock_conn();
            conn.execute(
                "INSERT INTO phylax_secrets
                   (tenant, category, name, secret_ciphertext, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?5)
                 ON CONFLICT(tenant, category, name) DO UPDATE SET
                   secret_ciphertext = excluded.secret_ciphertext,
                   updated_at = excluded.updated_at",
                rusqlite::params![tenant.to_string(), category, name, blob, now],
            )
            .map_err(berr)?;
        }

        self.emit(
            &SecretStored {
                category: category.to_string(),
                name: name.to_string(),
            },
            *tenant,
            *actor,
        );
        Ok(())
    }

    /// Read and decrypt a secret. THE ONLY PLAINTEXT PATH.
    ///
    /// This is owner-tier secret administration -- it is never reachable through the dispatcher
    /// gate, and the use-without-holding resolve modes never call it. Named loudly so a future
    /// reader cannot wire it into an agent-reachable route by accident.
    pub fn read_secret_admin(
        &self,
        tenant: &TenantId,
        category: &str,
        name: &str,
    ) -> Result<SecretData, PhylaxError> {
        let blob = self.load_ciphertext(tenant, category, name)?;
        let plaintext = crypto::decrypt(&self.master_key, &blob)?;
        serde_json::from_slice(&plaintext).map_err(|e| PhylaxError::Decryption(e.to_string()))
    }

    /// Fetch a secret's raw ciphertext blob, or a [`PhylaxError::SecretNotFound`].
    ///
    /// Crate-internal: the resolve modes (later slice) use this to decrypt in-process without
    /// exposing a plaintext read on the public surface.
    pub(crate) fn load_ciphertext(
        &self,
        tenant: &TenantId,
        category: &str,
        name: &str,
    ) -> Result<Vec<u8>, PhylaxError> {
        let conn = self.lock_conn();
        conn.query_row(
            "SELECT secret_ciphertext FROM phylax_secrets
             WHERE tenant = ?1 AND category = ?2 AND name = ?3",
            rusqlite::params![tenant.to_string(), category, name],
            |row| row.get::<_, Vec<u8>>(0),
        )
        .optional()
        .map_err(berr)?
        .ok_or_else(|| PhylaxError::SecretNotFound {
            category: category.to_string(),
            name: name.to_string(),
        })
    }

    /// Decrypt a stored secret for in-process use by the resolve modes. Crate-internal so the
    /// plaintext path stays off the public surface.
    pub(crate) fn load_secret(
        &self,
        tenant: &TenantId,
        category: &str,
        name: &str,
    ) -> Result<SecretData, PhylaxError> {
        let blob = self.load_ciphertext(tenant, category, name)?;
        let plaintext = crypto::decrypt(self.master_key(), &blob)?;
        serde_json::from_slice(&plaintext).map_err(|e| PhylaxError::Decryption(e.to_string()))
    }

    /// Delete a secret. Owner-tier administration. Errors if it does not exist.
    pub fn delete_secret(
        &self,
        tenant: &TenantId,
        actor: &PrincipalId,
        category: &str,
        name: &str,
    ) -> Result<(), PhylaxError> {
        let affected = {
            let conn = self.lock_conn();
            conn.execute(
                "DELETE FROM phylax_secrets WHERE tenant = ?1 AND category = ?2 AND name = ?3",
                rusqlite::params![tenant.to_string(), category, name],
            )
            .map_err(berr)?
        };
        if affected == 0 {
            return Err(PhylaxError::SecretNotFound {
                category: category.to_string(),
                name: name.to_string(),
            });
        }
        self.emit(
            &SecretDeleted {
                category: category.to_string(),
                name: name.to_string(),
            },
            *tenant,
            *actor,
        );
        Ok(())
    }

    /// List the (category, name) pairs a tenant has stored. Names only -- never values.
    pub fn list_secret_names(
        &self,
        tenant: &TenantId,
    ) -> Result<Vec<(String, String)>, PhylaxError> {
        let conn = self.lock_conn();
        let mut stmt = conn
            .prepare(
                "SELECT category, name FROM phylax_secrets
                 WHERE tenant = ?1 ORDER BY category, name",
            )
            .map_err(berr)?;
        let rows = stmt
            .query_map(rusqlite::params![tenant.to_string()], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(berr)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(berr)
    }
}

/// Apply every migration whose version exceeds `PRAGMA user_version`, each in its own
/// transaction, bumping `user_version` as it goes. Idempotent.
fn apply_migrations(conn: &mut Connection) -> Result<(), PhylaxError> {
    let mut version: i64 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .map_err(berr)?;
    for (v, sql) in MIGRATIONS {
        if *v > version {
            let tx = conn.transaction().map_err(berr)?;
            tx.execute_batch(sql)
                .map_err(|e| PhylaxError::Backend(format!("migration V{v} failed: {e}")))?;
            tx.pragma_update(None, "user_version", *v).map_err(berr)?;
            tx.commit().map_err(berr)?;
            version = *v;
        }
    }
    Ok(())
}

#[cfg(test)]
/// Verifies encrypted secret persistence, lookup, and lifecycle behavior.
mod tests {
    use super::*;

    /// Build an in-memory store with a random key.
    fn store() -> PhylaxStore {
        PhylaxStore::open_in_memory(Arc::new(AxonBus::new()), *crypto::generate_key())
            .expect("open store")
    }

    /// A stored secret round-trips through the admin read path.
    #[test]
    fn store_and_read_round_trip() {
        let s = store();
        let tenant = TenantId::new();
        let actor = PrincipalId::new();
        let data = SecretData::ApiKey {
            key: "sk-abc123".into(),
            endpoint: None,
            notes: None,
        };
        s.store_secret(&tenant, &actor, "prod", "stripe", &data)
            .expect("store");
        let got = s
            .read_secret_admin(&tenant, "prod", "stripe")
            .expect("read");
        assert_eq!(got, data);
    }

    /// The stored ciphertext column never contains the plaintext secret bytes.
    #[test]
    fn stored_value_is_encrypted_on_disk() {
        let s = store();
        let tenant = TenantId::new();
        let actor = PrincipalId::new();
        s.store_secret(
            &tenant,
            &actor,
            "prod",
            "db",
            &SecretData::Note {
                content: "super-secret".into(),
            },
        )
        .expect("store");
        let blob = s.load_ciphertext(&tenant, "prod", "db").expect("load");
        let haystack = String::from_utf8_lossy(&blob);
        assert!(
            !haystack.contains("super-secret"),
            "plaintext must not appear in the stored ciphertext"
        );
    }

    /// Store-over-existing overwrites rather than duplicating (UNIQUE upsert).
    #[test]
    fn store_overwrites_existing() {
        let s = store();
        let tenant = TenantId::new();
        let actor = PrincipalId::new();
        for content in ["v1", "v2"] {
            s.store_secret(
                &tenant,
                &actor,
                "prod",
                "rotating",
                &SecretData::Note {
                    content: content.into(),
                },
            )
            .expect("store");
        }
        assert_eq!(s.list_secret_names(&tenant).expect("list").len(), 1);
        let got = s
            .read_secret_admin(&tenant, "prod", "rotating")
            .expect("read");
        assert_eq!(
            got,
            SecretData::Note {
                content: "v2".into()
            }
        );
    }

    /// Secrets are tenant-isolated: another tenant cannot read or see them.
    #[test]
    fn secrets_are_tenant_isolated() {
        let s = store();
        let owner = TenantId::new();
        let other = TenantId::new();
        let actor = PrincipalId::new();
        s.store_secret(
            &owner,
            &actor,
            "prod",
            "db",
            &SecretData::Note {
                content: "x".into(),
            },
        )
        .expect("store");
        assert!(matches!(
            s.read_secret_admin(&other, "prod", "db"),
            Err(PhylaxError::SecretNotFound { .. })
        ));
        assert!(s.list_secret_names(&other).expect("list").is_empty());
    }

    /// Delete removes the secret; a second delete reports not-found.
    #[test]
    fn delete_removes_secret() {
        let s = store();
        let tenant = TenantId::new();
        let actor = PrincipalId::new();
        s.store_secret(
            &tenant,
            &actor,
            "prod",
            "db",
            &SecretData::Note {
                content: "x".into(),
            },
        )
        .expect("store");
        s.delete_secret(&tenant, &actor, "prod", "db")
            .expect("delete");
        assert!(matches!(
            s.delete_secret(&tenant, &actor, "prod", "db"),
            Err(PhylaxError::SecretNotFound { .. })
        ));
        assert!(s.list_secret_names(&tenant).expect("list").is_empty());
    }

    /// A store opened under a different key cannot decrypt the first store's data. Proves the
    /// ciphertext is bound to the master key, not merely obfuscated.
    #[test]
    fn wrong_master_key_cannot_decrypt() {
        let bus = Arc::new(AxonBus::new());
        let key_a = *crypto::generate_key();
        let tenant = TenantId::new();
        let actor = PrincipalId::new();

        // Write under key_a into a temp file, then reopen the same file under key_b.
        let dir = std::env::temp_dir().join(format!("phylax-test-{}", tenant.as_uuid()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let path = dir.join("phylax.db");
        {
            let a = PhylaxStore::open(&path, bus.clone(), key_a).expect("open a");
            a.store_secret(
                &tenant,
                &actor,
                "prod",
                "db",
                &SecretData::Note {
                    content: "x".into(),
                },
            )
            .expect("store");
        }
        let key_b = *crypto::generate_key();
        let b = PhylaxStore::open(&path, bus, key_b).expect("open b");
        assert!(matches!(
            b.read_secret_admin(&tenant, "prod", "db"),
            Err(PhylaxError::Decryption(_))
        ));
        std::fs::remove_dir_all(&dir).ok();
    }
}
