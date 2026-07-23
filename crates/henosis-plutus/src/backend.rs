//! The `PolicyBackend` trait: the four reads `PlutusGate` needs from a policy authority.
//!
//! `PlutusStore` (Postgres) is the production implementation. `LocalPolicyBackend`
//! provides a bounded single-operator development authority. `MockPolicyBackend`
//! (available under the `test-helpers` feature or in `#[cfg(test)]` contexts) lets
//! gate tests run without a live Postgres connection -- satisfying D1 rule 6.

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use syntheos_contracts::{PrincipalId, TenantId};

use crate::quota::{QuotaConfig, QuotaDimension, QuotaOutcome, QuotaTier};
use crate::rbac::Role;
use crate::{PlutusError, Result};

/// The lifecycle status of an org.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrgStatus {
    /// The org is active and may process requests.
    Active,
    /// The org has been suspended; requests are denied until reinstated.
    Suspended,
    /// The org has been deleted; it no longer exists for authorization purposes.
    Deleted,
}

/// Parse the text values stored in the `org.status` column.
impl std::str::FromStr for OrgStatus {
    /// Error type for unrecognized status strings.
    type Err = String;

    /// Map the canonical status string to the enum variant.
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "active" => Ok(OrgStatus::Active),
            "suspended" => Ok(OrgStatus::Suspended),
            "deleted" => Ok(OrgStatus::Deleted),
            other => Err(format!("unknown org status: {other:?}")),
        }
    }
}

/// Display `OrgStatus` as its canonical lowercase text.
impl std::fmt::Display for OrgStatus {
    /// Write the status string.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            OrgStatus::Active => "active",
            OrgStatus::Suspended => "suspended",
            OrgStatus::Deleted => "deleted",
        };
        f.write_str(s)
    }
}

/// The policy reads `PlutusGate` and the operator login flow need from a policy authority.
///
/// Production impl: `PlutusStore` over Postgres.
/// Local development impl: `LocalPolicyBackend` for one operator on loopback.
/// Test impl: `MockPolicyBackend` (no live DB required).
///
/// Every method returns `Result<_>` so the gate can distinguish "policy says no"
/// (a `GateDecision::Deny`) from "authority unreachable" (`Err(GateError)`).
/// Fail-closed is enforced at the gate level: an `Err` from any method denies.
#[async_trait]
pub trait PolicyBackend: Send + Sync {
    /// Look up the active/suspended/deleted status of the org identified by `tenant`.
    ///
    /// Returns `Ok(None)` when no org is found for the tenant (no org = deny).
    async fn org_status(&self, tenant: TenantId) -> Result<Option<OrgStatus>>;

    /// Look up the role of `principal` within `tenant`'s org.
    ///
    /// Returns `Ok(None)` when the principal is not a member of the org (no membership = deny).
    async fn member_role(&self, tenant: TenantId, principal: PrincipalId) -> Result<Option<Role>>;

    /// Resolve which tenant (org) `principal` belongs to by finding their membership row.
    ///
    /// Executes `SELECT tenant_id FROM org_member WHERE principal_id = ? LIMIT 1`.
    /// Returns `Ok(Some(tenant))` when the principal has a membership, or `Ok(None)` when
    /// no membership row exists (no org = deny login). Used by the operator login flow to
    /// map a verified principal to its org before checking org status and role.
    async fn tenant_for_principal(&self, principal: PrincipalId) -> Result<Option<TenantId>>;

    /// Atomically increment the daily usage counter for `dimension` by `amount`
    /// and return whether the result is within the org's configured limit.
    ///
    /// The increment is applied even when the result exceeds the limit; the count
    /// reflects the rejected attempt. This is acceptable for a daily hard cap.
    async fn check_and_increment(
        &self,
        tenant: TenantId,
        dim: QuotaDimension,
        amount: i64,
        today: &str,
    ) -> Result<QuotaOutcome>;

    /// Check whether the per-org token-bucket rate limit allows one more request.
    ///
    /// Refills the bucket based on elapsed time and `rate_limit_rpm`, then takes
    /// one token if available. Returns `true` when the request is within the rate
    /// limit; `false` when the bucket is empty (deny without error).
    async fn rate_limit_ok(
        &self,
        tenant: TenantId,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Result<bool>;
}

/// A loopback-only, single-operator policy authority for local development.
///
/// This backend preserves the same org, membership, RBAC, quota, and token-bucket reads used by
/// [`crate::PlutusGate`] without requiring PostgreSQL. Its counters live in process memory, so
/// production and multi-tenant deployments must use [`crate::PlutusStore`]. The server owns the
/// loopback and billing restrictions that keep this backend inside its development boundary.
pub struct LocalPolicyBackend {
    /// The only tenant recognized by this local authority.
    tenant: TenantId,
    /// The only principal recognized as a member of the local tenant.
    principal: PrincipalId,
    /// The local principal's role within the tenant.
    role: Role,
    /// Quota and rate-limit values selected from the configured tier.
    quota: QuotaConfig,
    /// Daily counters keyed by stable quota dimension and UTC date.
    usage: Mutex<HashMap<(String, String), i64>>,
    /// Token-bucket state initialized on the first request.
    rate_bucket: Mutex<Option<LocalRateBucket>>,
}

/// Mutable token-bucket state protected by [`LocalPolicyBackend::rate_bucket`].
struct LocalRateBucket {
    /// Tokens available after the most recent request.
    tokens: f64,
    /// UTC instant used as the refill baseline.
    last_refill: chrono::DateTime<chrono::Utc>,
}

/// Constructs the bounded local authority used by explicit development installs.
impl LocalPolicyBackend {
    /// Create a local authority for one tenant and principal.
    pub fn new(tenant: TenantId, principal: PrincipalId, role: Role, tier: QuotaTier) -> Self {
        Self {
            tenant,
            principal,
            role,
            quota: tier.defaults(),
            usage: Mutex::new(HashMap::new()),
            rate_bucket: Mutex::new(None),
        }
    }

    /// Reject policy mutations for an unknown tenant before touching local counters.
    fn require_tenant(&self, tenant: TenantId) -> Result<()> {
        if tenant == self.tenant {
            Ok(())
        } else {
            Err(PlutusError::Config(
                "local policy received an unknown tenant".to_string(),
            ))
        }
    }
}

/// Supplies real single-tenant policy decisions without an external database.
#[async_trait]
impl PolicyBackend for LocalPolicyBackend {
    /// Return an active org only for the configured local tenant.
    async fn org_status(&self, tenant: TenantId) -> Result<Option<OrgStatus>> {
        Ok((tenant == self.tenant).then_some(OrgStatus::Active))
    }

    /// Return the configured role only for the exact tenant and principal pair.
    async fn member_role(&self, tenant: TenantId, principal: PrincipalId) -> Result<Option<Role>> {
        Ok((tenant == self.tenant && principal == self.principal).then_some(self.role))
    }

    /// Resolve the configured tenant only for the configured local principal.
    async fn tenant_for_principal(&self, principal: PrincipalId) -> Result<Option<TenantId>> {
        Ok((principal == self.principal).then_some(self.tenant))
    }

    /// Increment one daily quota counter under a mutex and return the enforced limit.
    async fn check_and_increment(
        &self,
        tenant: TenantId,
        dim: QuotaDimension,
        amount: i64,
        today: &str,
    ) -> Result<QuotaOutcome> {
        self.require_tenant(tenant)?;
        let limit = dim.limit_from_config(&self.quota);
        let key = (dim.as_str().to_string(), today.to_string());
        let mut usage = self
            .usage
            .lock()
            .map_err(|_| PlutusError::Store("local usage lock poisoned".to_string()))?;
        let used = usage.entry(key).or_insert(0);
        *used = used
            .checked_add(amount)
            .ok_or_else(|| PlutusError::Store("local usage counter overflow".to_string()))?;
        Ok(QuotaOutcome {
            allowed: *used <= limit,
            used: *used,
            limit,
        })
    }

    /// Refill and consume the single local token bucket under a mutex.
    async fn rate_limit_ok(
        &self,
        tenant: TenantId,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Result<bool> {
        self.require_tenant(tenant)?;
        let rpm = self.quota.rate_limit_rpm as f64;
        let mut bucket = self
            .rate_bucket
            .lock()
            .map_err(|_| PlutusError::Store("local rate-limit lock poisoned".to_string()))?;
        let state = bucket.get_or_insert(LocalRateBucket {
            tokens: rpm,
            last_refill: now,
        });
        let elapsed = (now - state.last_refill).num_milliseconds().max(0) as f64 / 1000.0;
        state.tokens = (state.tokens + elapsed * rpm / 60.0).min(rpm);
        state.last_refill = now;
        if state.tokens < 1.0 {
            return Ok(false);
        }
        state.tokens -= 1.0;
        Ok(true)
    }
}

/// A configurable mock implementation of `PolicyBackend` for use in tests.
///
/// Available under `#[cfg(any(test, feature = "test-helpers"))]` so gate tests
/// inside this crate and integration tests in other crates (e.g. syntheos-server)
/// can exercise the full fail-closed matrix without a live Postgres connection.
#[cfg(any(test, feature = "test-helpers"))]
pub struct MockPolicyBackend {
    /// Org status to return. `None` means the tenant has no org (unknown tenant).
    pub org: Option<OrgStatus>,
    /// Member role to return. `None` means the principal is not a member.
    pub role: Option<Role>,
    /// Tenant to return from `tenant_for_principal`. `None` means no membership row
    /// exists for the queried principal (no org membership = deny login).
    pub tenant: Option<TenantId>,
    /// Whether `check_and_increment` reports the quota as allowed.
    pub quota_ok: bool,
    /// Whether `rate_limit_ok` reports the rate limit as not exhausted.
    pub rate_ok: bool,
    /// When `true`, every method returns an error (tests the gate's error path).
    pub error: bool,
}

/// Constructors for the common mock scenarios used in gate tests.
#[cfg(any(test, feature = "test-helpers"))]
impl MockPolicyBackend {
    /// Active org, Member-role principal, a fresh tenant, quota OK, rate limit OK. Produces `Allow`.
    ///
    /// The `tenant` field is set to a fresh `TenantId::new()` so that
    /// `tenant_for_principal` returns a consistent non-None value that callers can
    /// read back via `mock.tenant.unwrap()` to assert the resolved org in login tests.
    pub fn allow_all() -> Self {
        Self {
            org: Some(OrgStatus::Active),
            role: Some(Role::Member),
            tenant: Some(TenantId::new()),
            quota_ok: true,
            rate_ok: true,
            error: false,
        }
    }

    /// No org found for the tenant. The principal has a membership row pointing to a tenant,
    /// but `org_status` returns `None`. Gate step 1 denies.
    pub fn deny_no_org() -> Self {
        Self {
            org: None,
            role: None,
            tenant: Some(TenantId::new()),
            quota_ok: true,
            rate_ok: true,
            error: false,
        }
    }

    /// Org is suspended. Gate step 1 denies.
    pub fn deny_suspended_org() -> Self {
        Self {
            org: Some(OrgStatus::Suspended),
            role: Some(Role::Member),
            tenant: Some(TenantId::new()),
            quota_ok: true,
            rate_ok: true,
            error: false,
        }
    }

    /// Active org but the principal is not a member: no membership row exists so
    /// `tenant_for_principal` returns `None`. Gate step 2 denies; login step 2 denies.
    pub fn deny_no_member() -> Self {
        Self {
            org: Some(OrgStatus::Active),
            role: None,
            tenant: None,
            quota_ok: true,
            rate_ok: true,
            error: false,
        }
    }

    /// Active org, specified role (e.g. `Viewer` to test RBAC denial). Gate step 2 may deny.
    pub fn with_role(role: Role) -> Self {
        Self {
            org: Some(OrgStatus::Active),
            role: Some(role),
            tenant: Some(TenantId::new()),
            quota_ok: true,
            rate_ok: true,
            error: false,
        }
    }

    /// Quota exhausted for the action's dimension. Gate step 3 denies.
    pub fn deny_quota_exhausted() -> Self {
        Self {
            org: Some(OrgStatus::Active),
            role: Some(Role::Member),
            tenant: Some(TenantId::new()),
            quota_ok: false,
            rate_ok: true,
            error: false,
        }
    }

    /// Rate limit exhausted. Gate step 4 denies.
    pub fn deny_rate_limited() -> Self {
        Self {
            org: Some(OrgStatus::Active),
            role: Some(Role::Member),
            tenant: Some(TenantId::new()),
            quota_ok: true,
            rate_ok: false,
            error: false,
        }
    }

    /// All methods return an error. The gate must return `Err(GateError)`, never `Allow`.
    pub fn always_error() -> Self {
        Self {
            org: None,
            role: None,
            tenant: None,
            quota_ok: false,
            rate_ok: false,
            error: true,
        }
    }
}

/// `PolicyBackend` implementation for the mock: returns the configured responses.
#[cfg(any(test, feature = "test-helpers"))]
#[async_trait]
impl PolicyBackend for MockPolicyBackend {
    /// Return the configured org status, or an error when `self.error` is set.
    async fn org_status(&self, _tenant: TenantId) -> Result<Option<OrgStatus>> {
        if self.error {
            return Err(crate::PlutusError::Store("mock error".into()));
        }
        Ok(self.org)
    }

    /// Return the configured member role, or an error when `self.error` is set.
    async fn member_role(
        &self,
        _tenant: TenantId,
        _principal: PrincipalId,
    ) -> Result<Option<Role>> {
        if self.error {
            return Err(crate::PlutusError::Store("mock error".into()));
        }
        Ok(self.role)
    }

    /// Return the configured tenant for the principal, or an error when `self.error` is set.
    ///
    /// Returns `self.tenant`, which is `None` for `deny_no_member()` (no membership row)
    /// and `Some(tenant)` for all allow-path presets.
    async fn tenant_for_principal(&self, _principal: PrincipalId) -> Result<Option<TenantId>> {
        if self.error {
            return Err(crate::PlutusError::Store("mock error".into()));
        }
        Ok(self.tenant)
    }

    /// Return a `QuotaOutcome` reflecting `self.quota_ok`, or an error when `self.error` is set.
    async fn check_and_increment(
        &self,
        _tenant: TenantId,
        _dim: QuotaDimension,
        _amount: i64,
        _today: &str,
    ) -> Result<QuotaOutcome> {
        if self.error {
            return Err(crate::PlutusError::Store("mock error".into()));
        }
        Ok(QuotaOutcome {
            allowed: self.quota_ok,
            used: if self.quota_ok { 1 } else { 100 },
            limit: if self.quota_ok { 1_000 } else { 50 },
        })
    }

    /// Return `self.rate_ok`, or an error when `self.error` is set.
    async fn rate_limit_ok(
        &self,
        _tenant: TenantId,
        _now: chrono::DateTime<chrono::Utc>,
    ) -> Result<bool> {
        if self.error {
            return Err(crate::PlutusError::Store("mock error".into()));
        }
        Ok(self.rate_ok)
    }
}

#[cfg(test)]
/// Unit tests for policy status parsing and in-process backend enforcement.
mod tests {
    use super::*;

    /// `OrgStatus` round-trips through its text form.
    #[test]
    fn org_status_roundtrip() {
        for (s, expected) in [
            ("active", OrgStatus::Active),
            ("suspended", OrgStatus::Suspended),
            ("deleted", OrgStatus::Deleted),
        ] {
            let parsed: OrgStatus = s.parse().expect("valid status");
            assert_eq!(parsed, expected);
            assert_eq!(parsed.to_string(), s);
        }
    }

    /// Unknown status strings are rejected.
    #[test]
    fn org_status_parse_rejects_unknown() {
        assert!("pending".parse::<OrgStatus>().is_err());
    }

    /// Local policy recognizes only its configured tenant and principal.
    #[tokio::test]
    async fn local_policy_scopes_membership_to_one_identity() {
        let tenant = TenantId::new();
        let principal = PrincipalId::new();
        let backend = LocalPolicyBackend::new(tenant, principal, Role::Owner, QuotaTier::Free);

        assert_eq!(
            backend.org_status(tenant).await.unwrap(),
            Some(OrgStatus::Active)
        );
        assert_eq!(
            backend.member_role(tenant, principal).await.unwrap(),
            Some(Role::Owner)
        );
        assert_eq!(
            backend.tenant_for_principal(principal).await.unwrap(),
            Some(tenant)
        );
        assert_eq!(backend.org_status(TenantId::new()).await.unwrap(), None);
        assert_eq!(
            backend
                .member_role(tenant, PrincipalId::new())
                .await
                .unwrap(),
            None
        );
        let unknown_tenant = TenantId::new();
        assert!(backend
            .check_and_increment(unknown_tenant, QuotaDimension::Tasks, 1, "2026-07-22")
            .await
            .is_err());
        assert!(backend
            .rate_limit_ok(unknown_tenant, chrono::Utc::now())
            .await
            .is_err());
    }

    /// Local policy applies the selected tier's daily hard quota.
    #[tokio::test]
    async fn local_policy_enforces_daily_quota() {
        let tenant = TenantId::new();
        let backend =
            LocalPolicyBackend::new(tenant, PrincipalId::new(), Role::Owner, QuotaTier::Free);

        for expected in 1..=10 {
            let outcome = backend
                .check_and_increment(tenant, QuotaDimension::Tasks, 1, "2026-07-22")
                .await
                .unwrap();
            assert!(outcome.allowed);
            assert_eq!(outcome.used, expected);
        }
        let denied = backend
            .check_and_increment(tenant, QuotaDimension::Tasks, 1, "2026-07-22")
            .await
            .unwrap();
        assert!(!denied.allowed);
        assert_eq!(denied.used, 11);
        assert_eq!(denied.limit, 10);
    }

    /// Local policy applies the selected tier's token-bucket rate limit.
    #[tokio::test]
    async fn local_policy_enforces_rate_limit() {
        let tenant = TenantId::new();
        let backend =
            LocalPolicyBackend::new(tenant, PrincipalId::new(), Role::Owner, QuotaTier::Free);
        let now = chrono::DateTime::parse_from_rfc3339("2026-07-22T00:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);

        for _ in 0..10 {
            assert!(backend.rate_limit_ok(tenant, now).await.unwrap());
        }
        assert!(!backend.rate_limit_ok(tenant, now).await.unwrap());
    }
}
