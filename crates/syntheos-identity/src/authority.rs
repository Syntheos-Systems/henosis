//! Server-owned machine-token and operator-refresh credential storage.
//!
//! Each persisted secret is a SHA-256 digest. Public metadata deliberately omits the digest and
//! the cleartext credential, which is returned only by the issuing or rotating operation.

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use rusqlite::{OptionalExtension, Transaction, TransactionBehavior};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use syntheos_contracts::{PrincipalId, TenantId};
use uuid::Uuid;
use zeroize::{Zeroize, ZeroizeOnDrop};

use crate::error::DirectoryError;
use crate::sqlite::SqliteDirectory;

/// Maximum byte length accepted for a machine-token management label.
const MAX_LABEL_BYTES: usize = 128;
/// Maximum number of scope values accepted for one machine token.
const MAX_SCOPE_COUNT: usize = 64;
/// Maximum byte length accepted for one machine-token scope.
const MAX_SCOPE_BYTES: usize = 128;
/// Maximum combined byte length accepted for all machine-token scopes.
const MAX_SCOPES_BYTES: usize = 4096;

/// A fixed authority secret that clears its bytes automatically when dropped.
#[derive(Zeroize, ZeroizeOnDrop)]
struct SecretBytes([u8; 32]);

/// A machine-token metadata record that never exposes its credential secret or digest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MachineToken {
    /// Stable record identifier encoded into the credential wire form.
    pub id: Uuid,
    /// Tenant that owns and scopes the credential.
    pub tenant: TenantId,
    /// Principal authenticated by this credential.
    pub principal: PrincipalId,
    /// Human-readable management label.
    pub label: String,
    /// Permission scope strings granted to this credential.
    pub scopes: Vec<String>,
    /// Unix timestamp at issuance.
    pub created_at: i64,
    /// Optional Unix timestamp after which authentication rejects this credential.
    pub expires_at: Option<i64>,
    /// Unix timestamp at revocation, when revoked.
    pub revoked_at: Option<i64>,
    /// Unix timestamp of the most recent successful authentication.
    pub last_used_at: Option<i64>,
}

/// A newly issued machine credential with its one-time cleartext token.
#[derive(PartialEq, Eq, Zeroize, ZeroizeOnDrop)]
pub struct MachineTokenIssued {
    /// The credential returned only at issuance in `hen_v1_<uuid>.<base64url-secret>` form.
    pub token: String,
    /// The safely listable record metadata.
    #[zeroize(skip)]
    pub metadata: MachineToken,
}

/// A safely listable operator refresh-session record without its opaque secret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperatorRefreshSession {
    /// Stable record identifier for management operations.
    pub id: Uuid,
    /// Durable family identifier shared by every rotation descendant.
    pub family_id: Uuid,
    /// Tenant that owns and scopes this session.
    pub tenant: TenantId,
    /// Operator principal authenticated by this session.
    pub principal: PrincipalId,
    /// Unix timestamp at issuance.
    pub created_at: i64,
    /// Optional Unix timestamp after which refresh is rejected.
    pub expires_at: Option<i64>,
    /// Unix timestamp at revocation, when revoked.
    pub revoked_at: Option<i64>,
    /// Unix timestamp of the most recent successful use.
    pub last_used_at: Option<i64>,
}

/// A newly issued operator refresh secret with safely listable session metadata.
#[derive(PartialEq, Eq, Zeroize, ZeroizeOnDrop)]
pub struct RefreshSessionIssued {
    /// Opaque base64url-no-pad encoding of exactly 32 random bytes, returned once.
    pub token: String,
    /// The safely listable session record.
    #[zeroize(skip)]
    pub metadata: OperatorRefreshSession,
}

/// Compute the fixed SHA-256 digest stored for a raw credential secret.
fn digest(secret: &[u8]) -> [u8; 32] {
    Sha256::digest(secret).into()
}

/// Generate a cryptographically random secret whose buffer clears on drop.
fn random_secret() -> SecretBytes {
    SecretBytes(rand::random())
}

/// Decode an exact-size base64url-no-pad secret while clearing the decoder buffer.
fn decode_secret(value: &str) -> Option<SecretBytes> {
    let mut decoded = URL_SAFE_NO_PAD.decode(value).ok()?;
    if decoded.len() != 32 {
        decoded.zeroize();
        return None;
    }
    let bytes: [u8; 32] = decoded
        .as_slice()
        .try_into()
        .expect("decoded secret length was checked");
    decoded.zeroize();
    Some(SecretBytes(bytes))
}

/// Reject unbounded machine-token labels, scopes, and already-expired issuance requests.
fn validate_machine_issuance(
    label: &str,
    scopes: &[String],
    expires_at: Option<i64>,
    now: i64,
) -> Result<(), DirectoryError> {
    if label.is_empty() || label.len() > MAX_LABEL_BYTES {
        return Err(DirectoryError::Backend(
            "invalid machine token label".into(),
        ));
    }
    if scopes.len() > MAX_SCOPE_COUNT
        || scopes
            .iter()
            .any(|scope| scope.is_empty() || scope.len() > MAX_SCOPE_BYTES)
        || scopes.iter().map(String::len).sum::<usize>() > MAX_SCOPES_BYTES
    {
        return Err(DirectoryError::Backend(
            "invalid machine token scopes".into(),
        ));
    }
    if expires_at.is_some_and(|expires| expires <= now) {
        return Err(DirectoryError::Backend(
            "machine token expiry must be in the future".into(),
        ));
    }
    Ok(())
}

/// Convert a stored tenant UUID string into its strongly typed contract identifier.
fn tenant_from_db(value: String) -> Result<TenantId, DirectoryError> {
    value
        .parse()
        .map_err(|error| DirectoryError::Backend(format!("corrupt authority tenant: {error}")))
}

/// Convert a stored principal UUID string into its strongly typed contract identifier.
fn principal_from_db(value: String) -> Result<PrincipalId, DirectoryError> {
    value
        .parse()
        .map_err(|error| DirectoryError::Backend(format!("corrupt authority principal: {error}")))
}

/// Convert a stored UUID record identifier into a UUID.
fn record_id_from_db(value: String) -> Result<Uuid, DirectoryError> {
    Uuid::parse_str(&value)
        .map_err(|error| DirectoryError::Backend(format!("corrupt authority record id: {error}")))
}

/// Convert a machine-token row into safe metadata.
fn machine_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<MachineToken> {
    let id = row.get::<_, String>(0)?;
    let tenant = row.get::<_, String>(1)?;
    let principal = row.get::<_, String>(2)?;
    let scopes = row.get::<_, String>(4)?;
    let parse_error = |message: String| {
        rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                message,
            )),
        )
    };
    Ok(MachineToken {
        id: record_id_from_db(id).map_err(|error| parse_error(error.to_string()))?,
        tenant: tenant_from_db(tenant).map_err(|error| parse_error(error.to_string()))?,
        principal: principal_from_db(principal).map_err(|error| parse_error(error.to_string()))?,
        label: row.get(3)?,
        scopes: serde_json::from_str(&scopes).map_err(|error| parse_error(error.to_string()))?,
        created_at: row.get(5)?,
        expires_at: row.get(6)?,
        revoked_at: row.get(7)?,
        last_used_at: row.get(8)?,
    })
}

/// Convert an operator-refresh row into safe metadata.
fn refresh_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<OperatorRefreshSession> {
    let id = row.get::<_, String>(0)?;
    let family_id = row.get::<_, String>(1)?;
    let tenant = row.get::<_, String>(2)?;
    let principal = row.get::<_, String>(3)?;
    let parse_error = |message: String| {
        rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                message,
            )),
        )
    };
    Ok(OperatorRefreshSession {
        id: record_id_from_db(id).map_err(|error| parse_error(error.to_string()))?,
        family_id: record_id_from_db(family_id).map_err(|error| parse_error(error.to_string()))?,
        tenant: tenant_from_db(tenant).map_err(|error| parse_error(error.to_string()))?,
        principal: principal_from_db(principal).map_err(|error| parse_error(error.to_string()))?,
        created_at: row.get(4)?,
        expires_at: row.get(5)?,
        revoked_at: row.get(6)?,
        last_used_at: row.get(7)?,
    })
}

/// Read a machine-token record by id and its stored digest for authentication.
fn machine_auth_row(
    tx: &Transaction<'_>,
    id: Uuid,
) -> Result<Option<(Vec<u8>, MachineToken)>, DirectoryError> {
    tx.query_row(
        "SELECT id, tenant_id, principal_id, label, scopes_json, created_at, expires_at, revoked_at, last_used_at, secret_hash FROM machine_token WHERE id = ?1",
        rusqlite::params![id.to_string()],
        |row| Ok((row.get(9)?, machine_from_row(row)?)),
    )
    .optional()
    .map_err(|error| DirectoryError::Backend(error.to_string()))
}

/// Read a refresh-session record by digest for authentication or rotation.
fn refresh_auth_row(
    tx: &Transaction<'_>,
    secret_hash: &[u8; 32],
) -> Result<Option<OperatorRefreshSession>, DirectoryError> {
    tx.query_row(
        "SELECT id, family_id, tenant_id, principal_id, created_at, expires_at, revoked_at, last_used_at FROM operator_refresh_session WHERE secret_hash = ?1",
        rusqlite::params![secret_hash.as_slice()],
        refresh_from_row,
    )
    .optional()
    .map_err(|error| DirectoryError::Backend(error.to_string()))
}

/// Check that a refresh family exists, matches its owner, and has no revocation tombstone.
fn refresh_family_is_active(
    tx: &Transaction<'_>,
    session: &OperatorRefreshSession,
) -> Result<bool, DirectoryError> {
    tx.query_row(
        "SELECT 1 FROM operator_refresh_family WHERE id = ?1 AND tenant_id = ?2 AND principal_id = ?3 AND revoked_at IS NULL",
        rusqlite::params![
            session.family_id.to_string(),
            session.tenant.to_string(),
            session.principal.to_string()
        ],
        |_| Ok(()),
    )
    .optional()
    .map(|row| row.is_some())
    .map_err(|error| DirectoryError::Backend(error.to_string()))
}

/// Tombstone one scoped refresh family and revoke every session currently linked to it.
fn revoke_refresh_family_in_tx(
    tx: &Transaction<'_>,
    tenant: TenantId,
    principal: PrincipalId,
    family_id: Uuid,
    now: i64,
) -> Result<bool, DirectoryError> {
    let family_found = tx
        .execute(
            "UPDATE operator_refresh_family SET revoked_at = COALESCE(revoked_at, ?1) WHERE id = ?2 AND tenant_id = ?3 AND principal_id = ?4",
            rusqlite::params![
                now,
                family_id.to_string(),
                tenant.to_string(),
                principal.to_string()
            ],
        )
        .map_err(|error| DirectoryError::Backend(error.to_string()))?
        == 1;
    if !family_found {
        return Ok(false);
    }
    tx.execute(
        "UPDATE operator_refresh_session SET revoked_at = COALESCE(revoked_at, ?1) WHERE family_id = ?2 AND tenant_id = ?3 AND principal_id = ?4",
        rusqlite::params![
            now,
            family_id.to_string(),
            tenant.to_string(),
            principal.to_string()
        ],
    )
    .map_err(|error| DirectoryError::Backend(error.to_string()))?;
    Ok(true)
}

/// Implements persistent authority credential operations over the identity database.
impl SqliteDirectory {
    /// Issue a machine token and return the cleartext wire credential only from this call.
    pub fn create_machine_token(
        &self,
        tenant: TenantId,
        principal: PrincipalId,
        label: impl Into<String>,
        scopes: Vec<String>,
        expires_at: Option<i64>,
        now: i64,
    ) -> Result<MachineTokenIssued, DirectoryError> {
        let id = Uuid::new_v4();
        let label = label.into();
        validate_machine_issuance(&label, &scopes, expires_at, now)?;
        let secret = random_secret();
        let metadata = MachineToken {
            id,
            tenant,
            principal,
            label,
            scopes,
            created_at: now,
            expires_at,
            revoked_at: None,
            last_used_at: None,
        };
        let scopes_json = serde_json::to_string(&metadata.scopes)
            .map_err(|error| DirectoryError::Backend(format!("serialize token scopes: {error}")))?;
        let secret_hash = digest(&secret.0);
        let conn = self.conn.lock().unwrap_or_else(|error| error.into_inner());
        conn.execute(
            "INSERT INTO machine_token (id, secret_hash, tenant_id, principal_id, label, scopes_json, created_at, expires_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            rusqlite::params![id.to_string(), secret_hash.as_slice(), tenant.to_string(), principal.to_string(), metadata.label, scopes_json, now, expires_at],
        ).map_err(|error| DirectoryError::Backend(error.to_string()))?;
        Ok(MachineTokenIssued {
            token: format!("hen_v1_{id}.{}", URL_SAFE_NO_PAD.encode(&secret.0)),
            metadata,
        })
    }

    /// Authenticate a machine-token wire credential and record its successful use.
    pub fn authenticate_machine_token(
        &self,
        credential: &str,
        now: i64,
    ) -> Result<Option<MachineToken>, DirectoryError> {
        let Some(rest) = credential.strip_prefix("hen_v1_") else {
            return Ok(None);
        };
        let Some((id_text, secret_text)) = rest.split_once('.') else {
            return Ok(None);
        };
        if secret_text.contains('.') {
            return Ok(None);
        }
        let Ok(id) = Uuid::parse_str(id_text) else {
            return Ok(None);
        };
        let Some(secret) = decode_secret(secret_text) else {
            return Ok(None);
        };
        let candidate = digest(&secret.0);
        let mut conn = self.conn.lock().unwrap_or_else(|error| error.into_inner());
        let tx = conn
            .transaction()
            .map_err(|error| DirectoryError::Backend(error.to_string()))?;
        let Some((stored_digest, mut metadata)) = machine_auth_row(&tx, id)? else {
            return Ok(None);
        };
        let digest_matches =
            stored_digest.len() == 32 && stored_digest.ct_eq(candidate.as_slice()).unwrap_u8() == 1;
        let active = metadata.revoked_at.is_none()
            && metadata.expires_at.is_none_or(|expires| expires > now);
        if !digest_matches || !active {
            return Ok(None);
        }
        tx.execute(
            "UPDATE machine_token SET last_used_at = ?1 WHERE id = ?2",
            rusqlite::params![now, id.to_string()],
        )
        .map_err(|error| DirectoryError::Backend(error.to_string()))?;
        tx.commit()
            .map_err(|error| DirectoryError::Backend(error.to_string()))?;
        metadata.last_used_at = Some(now);
        Ok(Some(metadata))
    }

    /// List safely visible machine-token metadata scoped to one tenant.
    pub fn list_machine_tokens(
        &self,
        tenant: TenantId,
    ) -> Result<Vec<MachineToken>, DirectoryError> {
        let conn = self.conn.lock().unwrap_or_else(|error| error.into_inner());
        let mut statement = conn.prepare("SELECT id, tenant_id, principal_id, label, scopes_json, created_at, expires_at, revoked_at, last_used_at FROM machine_token WHERE tenant_id = ?1 ORDER BY created_at DESC")
            .map_err(|error| DirectoryError::Backend(error.to_string()))?;
        let rows = statement
            .query_map(rusqlite::params![tenant.to_string()], machine_from_row)
            .map_err(|error| DirectoryError::Backend(error.to_string()))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|error| DirectoryError::Backend(error.to_string()))
    }

    /// Revoke one machine token only when its record belongs to `tenant`.
    pub fn revoke_machine_token(
        &self,
        tenant: TenantId,
        id: Uuid,
        now: i64,
    ) -> Result<bool, DirectoryError> {
        let conn = self.conn.lock().unwrap_or_else(|error| error.into_inner());
        let changed = conn.execute("UPDATE machine_token SET revoked_at = COALESCE(revoked_at, ?1) WHERE id = ?2 AND tenant_id = ?3", rusqlite::params![now, id.to_string(), tenant.to_string()])
            .map_err(|error| DirectoryError::Backend(error.to_string()))?;
        Ok(changed == 1)
    }

    /// Check whether authenticated machine-token metadata grants a requested scope.
    pub fn machine_token_has_scope(token: &MachineToken, scope: &str) -> bool {
        token.scopes.iter().any(|granted| granted == scope)
    }

    /// Issue an opaque 32-byte operator refresh secret and safely visible metadata.
    pub fn issue_operator_refresh(
        &self,
        tenant: TenantId,
        principal: PrincipalId,
        expires_at: Option<i64>,
        now: i64,
    ) -> Result<RefreshSessionIssued, DirectoryError> {
        let id = Uuid::new_v4();
        let family_id = Uuid::new_v4();
        if expires_at.is_some_and(|expires| expires <= now) {
            return Err(DirectoryError::Backend(
                "refresh expiry must be in the future".into(),
            ));
        }
        let secret = random_secret();
        let metadata = OperatorRefreshSession {
            id,
            family_id,
            tenant,
            principal,
            created_at: now,
            expires_at,
            revoked_at: None,
            last_used_at: None,
        };
        let secret_hash = digest(&secret.0);
        let mut conn = self.conn.lock().unwrap_or_else(|error| error.into_inner());
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| DirectoryError::Backend(error.to_string()))?;
        tx.execute(
            "INSERT INTO operator_refresh_family (id, tenant_id, principal_id, created_at) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![
                family_id.to_string(),
                tenant.to_string(),
                principal.to_string(),
                now
            ],
        )
        .map_err(|error| DirectoryError::Backend(error.to_string()))?;
        tx.execute("INSERT INTO operator_refresh_session (id, family_id, secret_hash, tenant_id, principal_id, created_at, expires_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            rusqlite::params![id.to_string(), family_id.to_string(), secret_hash.as_slice(), tenant.to_string(), principal.to_string(), now, expires_at])
            .map_err(|error| DirectoryError::Backend(error.to_string()))?;
        tx.commit()
            .map_err(|error| DirectoryError::Backend(error.to_string()))?;
        Ok(RefreshSessionIssued {
            token: URL_SAFE_NO_PAD.encode(&secret.0),
            metadata,
        })
    }

    /// Authenticate an opaque refresh secret and record its successful use.
    pub fn authenticate_operator_refresh(
        &self,
        token: &str,
        now: i64,
    ) -> Result<Option<OperatorRefreshSession>, DirectoryError> {
        let Some(secret) = decode_secret(token) else {
            return Ok(None);
        };
        let secret_hash = digest(&secret.0);
        let mut conn = self.conn.lock().unwrap_or_else(|error| error.into_inner());
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| DirectoryError::Backend(error.to_string()))?;
        let Some(mut session) = refresh_auth_row(&tx, &secret_hash)? else {
            return Ok(None);
        };
        if session.revoked_at.is_some()
            || session.expires_at.is_some_and(|expires| expires <= now)
            || !refresh_family_is_active(&tx, &session)?
        {
            return Ok(None);
        }
        tx.execute(
            "UPDATE operator_refresh_session SET last_used_at = ?1 WHERE id = ?2",
            rusqlite::params![now, session.id.to_string()],
        )
        .map_err(|error| DirectoryError::Backend(error.to_string()))?;
        tx.commit()
            .map_err(|error| DirectoryError::Backend(error.to_string()))?;
        session.last_used_at = Some(now);
        Ok(Some(session))
    }

    /// Rotate an active refresh secret atomically, revoking its predecessor before minting the replacement.
    pub fn rotate_operator_refresh(
        &self,
        tenant: TenantId,
        principal: PrincipalId,
        token: &str,
        expires_at: Option<i64>,
        now: i64,
    ) -> Result<Option<RefreshSessionIssued>, DirectoryError> {
        if expires_at.is_some_and(|expires| expires <= now) {
            return Err(DirectoryError::Backend(
                "refresh expiry must be in the future".into(),
            ));
        }
        let Some(secret) = decode_secret(token) else {
            return Ok(None);
        };
        let secret_hash = digest(&secret.0);
        let mut conn = self.conn.lock().unwrap_or_else(|error| error.into_inner());
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| DirectoryError::Backend(error.to_string()))?;
        let Some(previous) = refresh_auth_row(&tx, &secret_hash)? else {
            return Ok(None);
        };
        let active = previous.tenant == tenant
            && previous.principal == principal
            && previous.revoked_at.is_none()
            && previous.expires_at.is_none_or(|expires| expires > now)
            && refresh_family_is_active(&tx, &previous)?;
        if !active {
            return Ok(None);
        }
        let changed = tx.execute("UPDATE operator_refresh_session SET revoked_at = ?1, last_used_at = ?1 WHERE id = ?2 AND family_id = ?3 AND tenant_id = ?4 AND principal_id = ?5 AND revoked_at IS NULL", rusqlite::params![now, previous.id.to_string(), previous.family_id.to_string(), tenant.to_string(), principal.to_string()])
            .map_err(|error| DirectoryError::Backend(error.to_string()))?;
        if changed != 1 {
            return Ok(None);
        }
        let id = Uuid::new_v4();
        let replacement_secret = random_secret();
        let replacement_hash = digest(&replacement_secret.0);
        tx.execute("INSERT INTO operator_refresh_session (id, family_id, secret_hash, tenant_id, principal_id, created_at, expires_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)", rusqlite::params![id.to_string(), previous.family_id.to_string(), replacement_hash.as_slice(), tenant.to_string(), principal.to_string(), now, expires_at])
            .map_err(|error| DirectoryError::Backend(error.to_string()))?;
        tx.commit()
            .map_err(|error| DirectoryError::Backend(error.to_string()))?;
        Ok(Some(RefreshSessionIssued {
            token: URL_SAFE_NO_PAD.encode(&replacement_secret.0),
            metadata: OperatorRefreshSession {
                id,
                family_id: previous.family_id,
                tenant,
                principal,
                created_at: now,
                expires_at,
                revoked_at: None,
                last_used_at: None,
            },
        }))
    }

    /// List safely visible refresh-session metadata scoped to one tenant.
    pub fn list_operator_refreshes(
        &self,
        tenant: TenantId,
    ) -> Result<Vec<OperatorRefreshSession>, DirectoryError> {
        let conn = self.conn.lock().unwrap_or_else(|error| error.into_inner());
        let mut statement = conn.prepare("SELECT id, family_id, tenant_id, principal_id, created_at, expires_at, revoked_at, last_used_at FROM operator_refresh_session WHERE tenant_id = ?1 ORDER BY created_at DESC")
            .map_err(|error| DirectoryError::Backend(error.to_string()))?;
        let rows = statement
            .query_map(rusqlite::params![tenant.to_string()], refresh_from_row)
            .map_err(|error| DirectoryError::Backend(error.to_string()))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|error| DirectoryError::Backend(error.to_string()))
    }

    /// Revoke one operator refresh session only when its record belongs to `tenant`.
    pub fn revoke_operator_refresh(
        &self,
        tenant: TenantId,
        id: Uuid,
        now: i64,
    ) -> Result<bool, DirectoryError> {
        let conn = self.conn.lock().unwrap_or_else(|error| error.into_inner());
        let changed = conn.execute("UPDATE operator_refresh_session SET revoked_at = COALESCE(revoked_at, ?1) WHERE id = ?2 AND tenant_id = ?3", rusqlite::params![now, id.to_string(), tenant.to_string()])
            .map_err(|error| DirectoryError::Backend(error.to_string()))?;
        Ok(changed == 1)
    }

    /// Atomically tombstone one tenant- and principal-scoped refresh family.
    pub fn revoke_operator_refresh_family(
        &self,
        tenant: TenantId,
        principal: PrincipalId,
        family_id: Uuid,
        now: i64,
    ) -> Result<bool, DirectoryError> {
        let mut conn = self.conn.lock().unwrap_or_else(|error| error.into_inner());
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| DirectoryError::Backend(error.to_string()))?;
        let found = revoke_refresh_family_in_tx(&tx, tenant, principal, family_id, now)?;
        tx.commit()
            .map_err(|error| DirectoryError::Backend(error.to_string()))?;
        Ok(found)
    }

    /// Resolve any persisted refresh secret and atomically tombstone its scoped family.
    pub fn revoke_operator_refresh_family_by_token(
        &self,
        token: &str,
        now: i64,
    ) -> Result<bool, DirectoryError> {
        let Some(secret) = decode_secret(token) else {
            return Ok(false);
        };
        let secret_hash = digest(&secret.0);
        let mut conn = self.conn.lock().unwrap_or_else(|error| error.into_inner());
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| DirectoryError::Backend(error.to_string()))?;
        let Some(session) = refresh_auth_row(&tx, &secret_hash)? else {
            return Ok(false);
        };
        let found = revoke_refresh_family_in_tx(
            &tx,
            session.tenant,
            session.principal,
            session.family_id,
            now,
        )?;
        tx.commit()
            .map_err(|error| DirectoryError::Backend(error.to_string()))?;
        Ok(found)
    }
}

/// Unit tests for authority credential storage behavior.
#[cfg(test)]
mod tests {
    use std::sync::{Arc, Barrier};

    use super::*;

    /// Machine credentials authenticate once issued, update usage, honor scopes, and reject replay after revocation.
    #[test]
    fn machine_token_lifecycle() {
        let directory = SqliteDirectory::open_in_memory().expect("open");
        let tenant = TenantId::new();
        let principal = PrincipalId::new();
        let issued = directory
            .create_machine_token(
                tenant,
                principal,
                "worker",
                vec!["jobs.read".into()],
                Some(200),
                100,
            )
            .expect("issue");
        let authenticated = directory
            .authenticate_machine_token(&issued.token, 150)
            .expect("auth")
            .expect("active");
        assert_eq!(authenticated.last_used_at, Some(150));
        assert!(SqliteDirectory::machine_token_has_scope(
            &authenticated,
            "jobs.read"
        ));
        assert!(
            directory
                .revoke_machine_token(tenant, issued.metadata.id, 151)
                .expect("revoke")
        );
        assert!(
            directory
                .authenticate_machine_token(&issued.token, 152)
                .expect("auth")
                .is_none()
        );
    }

    /// Refresh rotation revokes the predecessor and leaves only the new secret usable.
    #[test]
    fn refresh_rotation_is_single_use() {
        let directory = SqliteDirectory::open_in_memory().expect("open");
        let tenant = TenantId::new();
        let principal = PrincipalId::new();
        let issued = directory
            .issue_operator_refresh(tenant, principal, Some(200), 100)
            .expect("issue");
        let replacement = directory
            .rotate_operator_refresh(tenant, principal, &issued.token, Some(300), 150)
            .expect("rotate")
            .expect("active");
        assert_eq!(replacement.metadata.family_id, issued.metadata.family_id);
        assert!(
            directory
                .authenticate_operator_refresh(&issued.token, 151)
                .expect("auth")
                .is_none()
        );
        assert!(
            directory
                .authenticate_operator_refresh(&replacement.token, 151)
                .expect("auth")
                .is_some()
        );
    }

    /// Malformed credentials reject, and already-expired credentials cannot be issued.
    #[test]
    fn machine_authentication_rejects_invalid_and_expired_credentials() {
        let directory = SqliteDirectory::open_in_memory().expect("open");
        let tenant = TenantId::new();
        let expired = directory.create_machine_token(
            tenant,
            PrincipalId::new(),
            "expired",
            Vec::new(),
            Some(100),
            100,
        );
        assert!(expired.is_err());
        let issued = directory
            .create_machine_token(
                tenant,
                PrincipalId::new(),
                "active",
                Vec::new(),
                Some(200),
                100,
            )
            .expect("issue");
        assert!(
            directory
                .authenticate_machine_token("hen_v1_not-a-uuid.abc", 100)
                .expect("malformed")
                .is_none()
        );
        assert!(
            directory
                .authenticate_machine_token(&issued.token, 200)
                .expect("expired")
                .is_none()
        );
        let metadata = directory
            .list_machine_tokens(tenant)
            .expect("list")
            .into_iter()
            .next()
            .expect("record");
        assert_eq!(metadata.last_used_at, None);
    }

    /// Already-expired refresh-session requests are rejected before any secret is generated.
    #[test]
    fn refresh_issuance_rejects_expired_expiry() {
        let directory = SqliteDirectory::open_in_memory().expect("open");
        assert!(
            directory
                .issue_operator_refresh(TenantId::new(), PrincipalId::new(), Some(100), 100)
                .is_err()
        );
    }

    /// Oversized labels and scopes are rejected before authority metadata is persisted.
    #[test]
    fn machine_issuance_rejects_unbounded_metadata() {
        let directory = SqliteDirectory::open_in_memory().expect("open");
        let tenant = TenantId::new();
        assert!(
            directory
                .create_machine_token(
                    tenant,
                    PrincipalId::new(),
                    "x".repeat(MAX_LABEL_BYTES + 1),
                    Vec::new(),
                    None,
                    100,
                )
                .is_err()
        );
        assert!(
            directory
                .create_machine_token(
                    tenant,
                    PrincipalId::new(),
                    "bounded",
                    vec!["x".repeat(MAX_SCOPE_BYTES + 1)],
                    None,
                    100,
                )
                .is_err()
        );
        assert!(
            directory
                .list_machine_tokens(tenant)
                .expect("list")
                .is_empty()
        );
    }

    /// Family revocation requires the exact tenant and principal that own the durable family.
    #[test]
    fn refresh_family_revocation_is_owner_scoped() {
        let directory = SqliteDirectory::open_in_memory().expect("open");
        let tenant = TenantId::new();
        let principal = PrincipalId::new();
        let issued = directory
            .issue_operator_refresh(tenant, principal, Some(300), 100)
            .expect("issue");
        assert!(
            !directory
                .revoke_operator_refresh_family(
                    TenantId::new(),
                    principal,
                    issued.metadata.family_id,
                    110,
                )
                .expect("wrong tenant")
        );
        assert!(
            !directory
                .revoke_operator_refresh_family(
                    tenant,
                    PrincipalId::new(),
                    issued.metadata.family_id,
                    110,
                )
                .expect("wrong principal")
        );
        assert!(
            directory
                .authenticate_operator_refresh(&issued.token, 120)
                .expect("still active")
                .is_some()
        );
        assert!(
            directory
                .revoke_operator_refresh_family(tenant, principal, issued.metadata.family_id, 121)
                .expect("revoke")
        );
        assert!(
            directory
                .authenticate_operator_refresh(&issued.token, 122)
                .expect("revoked")
                .is_none()
        );
    }

    /// Independent connections serialize rotation and logout so no successor remains active.
    #[test]
    fn refresh_rotation_racing_family_logout_leaves_no_active_session() {
        let path =
            std::env::temp_dir().join(format!("syntheos-refresh-race-{}.sqlite", Uuid::new_v4()));
        let rotate_store = SqliteDirectory::open(&path).expect("open rotate connection");
        let logout_store = SqliteDirectory::open(&path).expect("open logout connection");
        let tenant = TenantId::new();
        let principal = PrincipalId::new();
        let issued = rotate_store
            .issue_operator_refresh(tenant, principal, Some(300), 100)
            .expect("issue");
        let old_token = issued.token.clone();
        let logout_token = issued.token.clone();
        let barrier = Arc::new(Barrier::new(2));
        let rotate_barrier = Arc::clone(&barrier);
        let rotate = std::thread::spawn(move || {
            rotate_barrier.wait();
            rotate_store
                .rotate_operator_refresh(tenant, principal, &old_token, Some(400), 150)
                .expect("rotate")
        });
        let logout = std::thread::spawn(move || {
            barrier.wait();
            logout_store
                .revoke_operator_refresh_family_by_token(&logout_token, 150)
                .expect("family logout")
        });
        let replacement = rotate.join().expect("rotate thread");
        assert!(logout.join().expect("logout thread"));

        let observer = SqliteDirectory::open(&path).expect("open observer");
        assert!(
            observer
                .authenticate_operator_refresh(&issued.token, 151)
                .expect("old token")
                .is_none()
        );
        if let Some(replacement) = replacement {
            assert_eq!(replacement.metadata.family_id, issued.metadata.family_id);
            assert!(
                observer
                    .authenticate_operator_refresh(&replacement.token, 151)
                    .expect("replacement")
                    .is_none()
            );
        }
        drop(observer);
        let _ = std::fs::remove_file(path);
    }
}
