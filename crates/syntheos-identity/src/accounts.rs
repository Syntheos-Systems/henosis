//! Operator-account management layered on top of [`SqliteDirectory`].
//!
//! Adds `create_account`, `verify_login`, and `get_account` to
//! [`SqliteDirectory`] using the `operator_account` table created by the V2
//! migration. Passwords are hashed with Argon2id; the plaintext is never stored.
//!
//! Timing safety: `verify_login` always runs an argon2 verification (against a
//! dummy hash when the email is unknown) so response time does not leak whether
//! an account exists.

use argon2::{
    password_hash::{rand_core::OsRng, PasswordHash, PasswordVerifier, SaltString},
    Argon2, PasswordHasher,
};
use rusqlite::OptionalExtension;
use syntheos_contracts::PrincipalId;

use crate::error::DirectoryError;
use crate::sqlite::SqliteDirectory;

/// A view of an operator account that never includes the password hash.
///
/// Returned by [`SqliteDirectory::get_account`]. All fields are owned so the
/// value can be held independently of the store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperatorAccount {
    /// Login handle; always lowercase (normalised at insert time).
    pub email: String,
    /// The principal this account is bound to in the identity directory.
    pub principal: PrincipalId,
    /// When `true` the account is suspended -- `verify_login` will return `None`.
    pub disabled: bool,
    /// ISO-8601 UTC creation timestamp as recorded by SQLite `datetime('now')`.
    pub created_at: String,
}

/// Process-wide dummy PHC hash for timing-equalisation on unknown emails.
///
/// Computed once on first use via [`std::sync::OnceLock`]; never changes after
/// initialisation so it is safe to share across threads.
static DUMMY_HASH: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// Return the process-wide dummy PHC hash string, generating it on first call.
///
/// The hash is a valid Argon2id hash of a fixed sentinel password. It is used
/// only to equalise the timing of `verify_login` for missing email addresses;
/// the result is always discarded.
fn dummy_hash() -> &'static str {
    DUMMY_HASH.get_or_init(|| {
        let salt = SaltString::generate(&mut OsRng);
        Argon2::default()
            .hash_password(b"__dummy__", &salt)
            .expect("dummy hash generation must not fail")
            .to_string()
    })
}

/// Hash a plaintext password with Argon2id defaults, returning the PHC string.
///
/// Uses a freshly-generated random salt on every call. The returned string is
/// suitable for storage and later verification via [`verify_password`].
fn hash_password(password: &str) -> Result<String, DirectoryError> {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| DirectoryError::Backend(format!("argon2 hash failed: {e}")))
}

/// Verify `password` against a stored PHC string.
///
/// Returns `true` on a successful match, `false` when the password is wrong.
/// Propagates structural errors (corrupt stored hash) as [`DirectoryError::Backend`].
fn verify_password(phc: &str, password: &str) -> Result<bool, DirectoryError> {
    let parsed = PasswordHash::new(phc)
        .map_err(|e| DirectoryError::Backend(format!("corrupt stored hash: {e}")))?;
    Ok(Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok())
}

/// Provides persistent operator-account creation, verification, and lookup.
impl SqliteDirectory {
    /// Return whether at least one operator account exists without exposing account metadata.
    pub fn has_operator_accounts(&self) -> Result<bool, DirectoryError> {
        let conn = self.conn.lock().unwrap_or_else(|error| error.into_inner());
        conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM operator_account LIMIT 1)",
            [],
            |row| row.get::<_, bool>(0),
        )
        .map_err(|error| DirectoryError::Backend(error.to_string()))
    }

    /// Create an operator account bound to `principal`.
    ///
    /// `email` is normalised to lowercase before storage. `password` is hashed
    /// with Argon2id; the plaintext is not stored. Returns
    /// [`DirectoryError::Backend`] when the email is already registered.
    pub fn create_account(
        &self,
        email: &str,
        password: &str,
        principal: PrincipalId,
    ) -> Result<(), DirectoryError> {
        let email_lc = email.to_lowercase();
        let hash = hash_password(password)?;
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        conn.execute(
            "INSERT INTO operator_account (email, password_hash, principal_id) \
             VALUES (?1, ?2, ?3)",
            rusqlite::params![email_lc, hash, principal.to_string()],
        )
        .map_err(|e| match &e {
            rusqlite::Error::SqliteFailure(f, _)
                if f.code == rusqlite::ErrorCode::ConstraintViolation =>
            {
                DirectoryError::Backend(format!("operator account already exists: {email_lc}"))
            }
            _ => DirectoryError::Backend(e.to_string()),
        })?;
        Ok(())
    }

    /// Verify a login attempt, returning the bound principal on success.
    ///
    /// Returns `Some(principal)` only when:
    /// - the email exists (compared lowercase),
    /// - the password matches the stored Argon2id hash, and
    /// - the account is not disabled.
    ///
    /// Returns `None` in all other cases. Timing is kept uniform: an argon2
    /// verification always runs, even for unknown email addresses, to prevent
    /// user-enumeration via response latency.
    pub fn verify_login(
        &self,
        email: &str,
        password: &str,
    ) -> Result<Option<PrincipalId>, DirectoryError> {
        let email_lc = email.to_lowercase();

        // Fetch the row while holding the lock; release immediately afterwards
        // so the slow argon2 verification does not block other operations.
        let row = {
            let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
            conn.query_row(
                "SELECT password_hash, principal_id, disabled \
                 FROM operator_account WHERE email = ?1",
                rusqlite::params![email_lc],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, i64>(2)?,
                    ))
                },
            )
            .optional()
            .map_err(|e| DirectoryError::Backend(e.to_string()))?
        };

        match row {
            None => {
                // Unknown email -- run a dummy verify to equalise timing.
                let _ = verify_password(dummy_hash(), password);
                Ok(None)
            }
            Some((phc, principal_str, disabled)) => {
                let matches = verify_password(&phc, password)?;
                if matches && disabled == 0 {
                    let id = principal_str.parse::<PrincipalId>().map_err(|e| {
                        DirectoryError::Backend(format!("corrupt principal_id: {e}"))
                    })?;
                    Ok(Some(id))
                } else {
                    Ok(None)
                }
            }
        }
    }

    /// Look up an operator account by email without exposing the password hash.
    ///
    /// Returns `None` when no account with the given email exists. The email is
    /// compared lowercase (matching the storage normalisation of `create_account`).
    pub fn get_account(&self, email: &str) -> Result<Option<OperatorAccount>, DirectoryError> {
        let email_lc = email.to_lowercase();
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let row = conn
            .query_row(
                "SELECT email, principal_id, disabled, created_at \
                 FROM operator_account WHERE email = ?1",
                rusqlite::params![email_lc],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, i64>(2)?,
                        r.get::<_, String>(3)?,
                    ))
                },
            )
            .optional()
            .map_err(|e| DirectoryError::Backend(e.to_string()))?;

        match row {
            None => Ok(None),
            Some((email, principal_str, disabled, created_at)) => {
                let principal = principal_str
                    .parse::<PrincipalId>()
                    .map_err(|e| DirectoryError::Backend(format!("corrupt principal_id: {e}")))?;
                Ok(Some(OperatorAccount {
                    email,
                    principal,
                    disabled: disabled != 0,
                    created_at,
                }))
            }
        }
    }
}

/// Unit tests for operator account authentication and lifecycle behavior.
#[cfg(test)]
mod tests {
    use super::*;

    /// A created account verifies with the right password and rejects the wrong one;
    /// a disabled account never verifies.
    #[test]
    fn operator_account_create_and_verify() {
        let dir = SqliteDirectory::open_in_memory().expect("open");
        assert!(!dir.has_operator_accounts().expect("empty account census"));
        let p = PrincipalId::new();
        dir.create_account("Op@example.com", "hunter2", p)
            .expect("create");
        assert!(dir
            .has_operator_accounts()
            .expect("populated account census"));
        // Email is case-insensitive; password must match.
        assert_eq!(
            dir.verify_login("op@example.com", "hunter2")
                .expect("verify"),
            Some(p)
        );
        assert_eq!(
            dir.verify_login("op@example.com", "wrong").expect("verify"),
            None
        );
        assert_eq!(
            dir.verify_login("nobody@example.com", "hunter2")
                .expect("verify"),
            None
        );
    }

    /// A disabled account always returns None from verify_login, even with the correct password.
    #[test]
    fn disabled_account_cannot_verify() {
        let dir = SqliteDirectory::open_in_memory().expect("open");
        let p = PrincipalId::new();
        dir.create_account("admin@example.com", "secret", p)
            .expect("create");
        // Disable the account directly via SQL.
        {
            let conn = dir.conn.lock().unwrap();
            conn.execute(
                "UPDATE operator_account SET disabled = 1 WHERE email = 'admin@example.com'",
                [],
            )
            .expect("disable");
        }
        assert_eq!(
            dir.verify_login("admin@example.com", "secret")
                .expect("verify"),
            None,
            "disabled account must not verify"
        );
    }

    /// get_account returns the struct without the hash; absent email returns None.
    #[test]
    fn get_account_round_trip() {
        let dir = SqliteDirectory::open_in_memory().expect("open");
        let p = PrincipalId::new();
        dir.create_account("user@example.com", "pw", p)
            .expect("create");
        let acct = dir
            .get_account("user@example.com")
            .expect("get")
            .expect("present");
        assert_eq!(acct.email, "user@example.com");
        assert_eq!(acct.principal, p);
        assert!(!acct.disabled);
        assert!(dir
            .get_account("nobody@example.com")
            .expect("get")
            .is_none());
    }
}
