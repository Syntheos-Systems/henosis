//! Capability-policy storage and matching for the resolve modes.
//!
//! Policies are deny-by-default: a resolve mode is permitted only when [`PhylaxStore::match_policy`]
//! finds a policy that names the mode (and, for exec, lists the argv[0]). Matching is by
//! specificity, mirroring the absorbed Kleos rule: a principal-specific policy beats a
//! tenant-wide one, a category+name policy beats a category-only one, which beats a
//! namespace-only one.

use rusqlite::OptionalExtension;
use syntheos_contracts::{PrincipalId, TenantId};

use crate::error::PhylaxError;
use crate::model::{Policy, ResolveMode};
use crate::store::{berr, now_string, PhylaxStore};

/// Validate that an exec allowlist contains only absolute paths. A relative entry would resolve
/// against the daemon's working directory rather than a policy decision.
fn validate_exec_allowlist(allowlist: Option<&[String]>) -> Result<(), PhylaxError> {
    if let Some(paths) = allowlist {
        for p in paths {
            if !p.starts_with('/') {
                return Err(PhylaxError::InvalidInput(format!(
                    "exec allowlist entry '{p}' must be an absolute path"
                )));
            }
        }
    }
    Ok(())
}

impl PhylaxStore {
    /// Create a capability policy. Returns the stored policy with its assigned id.
    ///
    /// `principal` scopes the policy to one principal, or `None` for any principal in the tenant.
    /// `allowed_modes` is the set of resolve modes permitted; `exec_allowlist` (absolute paths
    /// only) gates exec and may be `None` to forbid exec even when listed in `allowed_modes`.
    pub fn create_policy(
        &self,
        tenant: &TenantId,
        principal: Option<&PrincipalId>,
        category: Option<&str>,
        secret_name: Option<&str>,
        allowed_modes: &[ResolveMode],
        exec_allowlist: Option<&[String]>,
    ) -> Result<Policy, PhylaxError> {
        validate_exec_allowlist(exec_allowlist)?;
        let modes_json = serde_json::to_string(allowed_modes)
            .map_err(|e| PhylaxError::Backend(e.to_string()))?;
        let exec_json = exec_allowlist
            .map(serde_json::to_string)
            .transpose()
            .map_err(|e| PhylaxError::Backend(e.to_string()))?;
        let principal_s = principal.map(|p| p.to_string());
        let now = now_string()?;

        let id = {
            let conn = self.lock_conn();
            conn.execute(
                "INSERT INTO phylax_policies
                   (tenant, principal_id, category, secret_name, allowed_modes,
                    exec_allowlist, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                rusqlite::params![
                    tenant.to_string(),
                    principal_s,
                    category,
                    secret_name,
                    modes_json,
                    exec_json,
                    now
                ],
            )
            .map_err(berr)?;
            conn.last_insert_rowid()
        };

        Ok(Policy {
            id,
            principal_id: principal_s,
            category: category.map(str::to_string),
            secret_name: secret_name.map(str::to_string),
            allowed_modes: allowed_modes.to_vec(),
            exec_allowlist: exec_allowlist.map(<[String]>::to_vec),
        })
    }

    /// Delete a policy by id. Errors if it does not exist in this tenant.
    pub fn delete_policy(&self, tenant: &TenantId, id: i64) -> Result<(), PhylaxError> {
        let affected = {
            let conn = self.lock_conn();
            conn.execute(
                "DELETE FROM phylax_policies WHERE id = ?1 AND tenant = ?2",
                rusqlite::params![id, tenant.to_string()],
            )
            .map_err(berr)?
        };
        if affected == 0 {
            return Err(PhylaxError::Backend(format!("policy {id} not found")));
        }
        Ok(())
    }

    /// List every policy in a tenant, most specific first.
    pub fn list_policies(&self, tenant: &TenantId) -> Result<Vec<Policy>, PhylaxError> {
        let conn = self.lock_conn();
        let mut stmt = conn
            .prepare(
                "SELECT id, principal_id, category, secret_name, allowed_modes, exec_allowlist
                 FROM phylax_policies
                 WHERE tenant = ?1
                 ORDER BY (principal_id IS NOT NULL) DESC,
                          (secret_name IS NOT NULL) DESC,
                          (category IS NOT NULL) DESC,
                          id",
            )
            .map_err(berr)?;
        let rows = stmt
            .query_map(rusqlite::params![tenant.to_string()], row_to_policy)
            .map_err(berr)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(berr)
    }

    /// Find the most specific policy matching (tenant, principal, category, name), or `None`.
    ///
    /// Specificity order (mirrors the absorbed Kleos rule): a policy bound to this principal
    /// beats a tenant-wide one; among those, a `secret_name` match beats `category`-only beats
    /// namespace-only. A policy's `principal_id`/`category`/`secret_name` filters match when
    /// they are NULL (wildcard) or equal the request value.
    pub fn match_policy(
        &self,
        tenant: &TenantId,
        principal: &PrincipalId,
        category: &str,
        name: &str,
    ) -> Result<Option<Policy>, PhylaxError> {
        let conn = self.lock_conn();
        let mut stmt = conn
            .prepare(
                "SELECT id, principal_id, category, secret_name, allowed_modes, exec_allowlist
                 FROM phylax_policies
                 WHERE tenant = ?1
                   AND (principal_id IS NULL OR principal_id = ?2)
                   AND (category IS NULL OR category = ?3)
                   AND (secret_name IS NULL OR secret_name = ?4)
                 ORDER BY (principal_id IS NOT NULL) DESC,
                          (secret_name IS NOT NULL) DESC,
                          (category IS NOT NULL) DESC,
                          id
                 LIMIT 1",
            )
            .map_err(berr)?;
        stmt.query_row(
            rusqlite::params![tenant.to_string(), principal.to_string(), category, name],
            row_to_policy,
        )
        .optional()
        .map_err(berr)
    }
}

/// Parse a policy row. The JSON columns degrade safely: an unparseable `allowed_modes` yields no
/// modes (deny everything), and an unparseable `exec_allowlist` yields `None` (exec denied) --
/// never a broader permission.
fn row_to_policy(row: &rusqlite::Row<'_>) -> rusqlite::Result<Policy> {
    let modes_json: String = row.get(4)?;
    let exec_json: Option<String> = row.get(5)?;
    let allowed_modes: Vec<ResolveMode> = serde_json::from_str(&modes_json).unwrap_or_default();
    let exec_allowlist: Option<Vec<String>> = exec_json.and_then(|j| serde_json::from_str(&j).ok());
    Ok(Policy {
        id: row.get(0)?,
        principal_id: row.get(1)?,
        category: row.get(2)?,
        secret_name: row.get(3)?,
        allowed_modes,
        exec_allowlist,
    })
}
