//! `PlutusStore`: the Postgres-backed policy store for org status, RBAC, quota, and rate limits.
//!
//! All queries use runtime `sqlx::query` / `sqlx::query_as` (no compile-time `query!` macros)
//! so the build is DB-free -- no `DATABASE_URL` needed at build time. Migrations are applied via
//! `sqlx::migrate!()` on `PlutusStore::open`, which embeds the SQL at compile time.
//!
//! D1 rule 2 (runtime queries), D1 rule 3 (Postgres types), D1 rule 4 (atomic upsert),
//! D1 rule 5 (rate-limit transaction with SELECT FOR UPDATE) are all implemented here.

use std::str::FromStr;

use sqlx::PgPool;
use syntheos_contracts::{PrincipalId, TenantId};
use uuid::Uuid;

use crate::backend::{OrgStatus, PolicyBackend};
use crate::quota::{QuotaConfig, QuotaDimension, QuotaOutcome, QuotaTier};
use crate::rbac::Role;
use crate::{PlutusError, Result};

/// The Postgres-backed policy store.
///
/// Wraps a `PgPool` connection pool. Migrations are applied on `open`; subsequent calls
/// are idempotent (sqlx migration tracking). All methods implement `PolicyBackend` --
/// the gate depends on that trait, not on this concrete type, so tests can substitute a mock.
pub struct PlutusStore {
    /// The underlying connection pool. Shared across async tasks via `Arc<PlutusStore>`.
    pool: PgPool,
}

impl PlutusStore {
    /// Open (or connect to) the Postgres database at `url` and apply pending migrations.
    ///
    /// `url` is a standard Postgres connection string (e.g. `postgres://user:pw@host/db`).
    /// The migrations in `migrations/` are embedded at compile time via `sqlx::migrate!()`.
    pub async fn open(url: &str) -> Result<Self> {
        let pool = PgPool::connect(url)
            .await
            .map_err(|e| PlutusError::Store(format!("connect: {e}")))?;
        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .map_err(PlutusError::from)?;
        Ok(Self { pool })
    }

    /// Create a new org for `tenant`, bootstrapping the owner membership and tier quota config.
    ///
    /// Inserts: one `org` row, one `org_member` row (owner role), one `quota_config` row
    /// from `tier.defaults()`. Idempotent at the transaction level -- the caller is responsible
    /// for not calling twice with the same `tenant`; a duplicate key error is surfaced as
    /// `PlutusError::Store`.
    pub async fn create_org(
        &self,
        tenant: TenantId,
        name: &str,
        owner: PrincipalId,
        tier: QuotaTier,
    ) -> Result<()> {
        let tenant_uuid: Uuid = tenant.as_uuid();
        let owner_uuid: Uuid = owner.as_uuid();
        let tier_str = tier.as_str();
        let defaults = tier.defaults();

        let mut tx = self.pool.begin().await?;

        // Insert the org row.
        sqlx::query(
            r#"INSERT INTO org (tenant_id, name, owner_id, status, plan_tier)
               VALUES ($1, $2, $3, 'active', $4)"#,
        )
        .bind(tenant_uuid)
        .bind(name)
        .bind(owner_uuid)
        .bind(tier_str)
        .execute(&mut *tx)
        .await?;

        // Insert the owner as the first member with the owner role.
        sqlx::query(
            r#"INSERT INTO org_member (tenant_id, principal_id, role)
               VALUES ($1, $2, 'owner')"#,
        )
        .bind(tenant_uuid)
        .bind(owner_uuid)
        .execute(&mut *tx)
        .await?;

        // Insert the tier's default quota configuration.
        sqlx::query(
            r#"INSERT INTO quota_config
               (tenant_id, max_tasks_per_day, max_tokens_per_day,
                max_tool_calls_per_day, max_memory_stores_per_day, rate_limit_rpm)
               VALUES ($1, $2, $3, $4, $5, $6)"#,
        )
        .bind(tenant_uuid)
        .bind(defaults.max_tasks_per_day)
        .bind(defaults.max_tokens_per_day)
        .bind(defaults.max_tool_calls_per_day)
        .bind(defaults.max_memory_stores_per_day)
        .bind(defaults.rate_limit_rpm)
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok(())
    }

    /// Update an existing org's status (active / suspended / deleted).
    pub async fn set_org_status(&self, tenant: TenantId, status: OrgStatus) -> Result<()> {
        sqlx::query("UPDATE org SET status = $1 WHERE tenant_id = $2")
            .bind(status.to_string())
            .bind(tenant.as_uuid())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Add `principal` to `tenant`'s org with the given `role`.
    ///
    /// A duplicate (same tenant + principal) is surfaced as `PlutusError::Store`.
    pub async fn add_member(
        &self,
        tenant: TenantId,
        principal: PrincipalId,
        role: Role,
    ) -> Result<()> {
        sqlx::query(
            r#"INSERT INTO org_member (tenant_id, principal_id, role)
               VALUES ($1, $2, $3)"#,
        )
        .bind(tenant.as_uuid())
        .bind(principal.as_uuid())
        .bind(role.as_str())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Overwrite the quota configuration for `tenant`.
    ///
    /// Intended for testing and administrative adjustment; `create_org` inserts the initial row.
    pub async fn set_quota(&self, tenant: TenantId, cfg: &QuotaConfig) -> Result<()> {
        sqlx::query(
            r#"INSERT INTO quota_config
               (tenant_id, max_tasks_per_day, max_tokens_per_day,
                max_tool_calls_per_day, max_memory_stores_per_day, rate_limit_rpm)
               VALUES ($1, $2, $3, $4, $5, $6)
               ON CONFLICT (tenant_id) DO UPDATE
                   SET max_tasks_per_day         = EXCLUDED.max_tasks_per_day,
                       max_tokens_per_day        = EXCLUDED.max_tokens_per_day,
                       max_tool_calls_per_day    = EXCLUDED.max_tool_calls_per_day,
                       max_memory_stores_per_day = EXCLUDED.max_memory_stores_per_day,
                       rate_limit_rpm            = EXCLUDED.rate_limit_rpm"#,
        )
        .bind(tenant.as_uuid())
        .bind(cfg.max_tasks_per_day)
        .bind(cfg.max_tokens_per_day)
        .bind(cfg.max_tool_calls_per_day)
        .bind(cfg.max_memory_stores_per_day)
        .bind(cfg.rate_limit_rpm)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Idempotently bootstrap the operator org on first boot.
    ///
    /// Reads `SYNTHEOS_PLUTUS_OPERATOR_TENANT` and `SYNTHEOS_PLUTUS_OPERATOR_PRINCIPAL`
    /// from the environment. If the org does not already exist, creates it as a `Free` tier
    /// org with the operator principal as owner, then upgrades it to `Enterprise` quota so
    /// the operator is not rate-limited during initial setup.
    ///
    /// Fails loudly (returns `Err`) when either env var is missing, malformed, or the insert
    /// errors unexpectedly -- same fail-loud posture as the phylax/supervisor boot path.
    /// If the org already exists, returns `Ok(())` without touching it.
    pub async fn bootstrap_operator_org_if_absent(&self) -> Result<()> {
        let tenant_str = std::env::var("SYNTHEOS_PLUTUS_OPERATOR_TENANT").map_err(|_| {
            PlutusError::Config(
                "SYNTHEOS_PLUTUS_OPERATOR_TENANT is required when Plutus is enabled".into(),
            )
        })?;
        let principal_str =
            std::env::var("SYNTHEOS_PLUTUS_OPERATOR_PRINCIPAL").map_err(|_| {
                PlutusError::Config(
                    "SYNTHEOS_PLUTUS_OPERATOR_PRINCIPAL is required when Plutus is enabled".into(),
                )
            })?;

        let tenant: TenantId = tenant_str
            .parse()
            .map_err(|e| PlutusError::Config(format!("SYNTHEOS_PLUTUS_OPERATOR_TENANT: {e}")))?;
        let principal: PrincipalId = principal_str.parse().map_err(|e| {
            PlutusError::Config(format!("SYNTHEOS_PLUTUS_OPERATOR_PRINCIPAL: {e}"))
        })?;

        // Check whether the org already exists.
        let existing: Option<(String,)> =
            sqlx::query_as("SELECT status FROM org WHERE tenant_id = $1")
                .bind(tenant.as_uuid())
                .fetch_optional(&self.pool)
                .await?;

        if existing.is_some() {
            tracing::debug!(
                tenant = %tenant,
                "plutus operator org already exists; skipping bootstrap"
            );
            return Ok(());
        }

        // Create the operator org with Enterprise quota.
        self.create_org(tenant, "operator", principal, QuotaTier::Enterprise)
            .await?;

        tracing::info!(
            tenant = %tenant,
            principal = %principal,
            "plutus operator org bootstrapped (enterprise tier)"
        );
        Ok(())
    }

    /// Resolve the limit for a given dimension from `quota_config`.
    ///
    /// Returns `None` when no quota_config row exists for the tenant (org created without one).
    async fn quota_config_limit(
        &self,
        tenant_uuid: Uuid,
        dim: QuotaDimension,
    ) -> Result<Option<i64>> {
        // Select the specific column rather than the full row to avoid sqlx FromRow boilerplate.
        let row: Option<(i64, i64, i64, i64, i64)> = sqlx::query_as(
            r#"SELECT max_tasks_per_day, max_tokens_per_day,
                      max_tool_calls_per_day, max_memory_stores_per_day, rate_limit_rpm
               FROM quota_config
               WHERE tenant_id = $1"#,
        )
        .bind(tenant_uuid)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(|(tasks, tokens, tool_calls, memory_stores, rpm)| {
            let cfg = QuotaConfig {
                max_tasks_per_day: tasks,
                max_tokens_per_day: tokens,
                max_tool_calls_per_day: tool_calls,
                max_memory_stores_per_day: memory_stores,
                rate_limit_rpm: rpm,
            };
            dim.limit_from_config(&cfg)
        }))
    }
}

/// `PolicyBackend` implementation for the Postgres-backed store.
///
/// Each method maps to the corresponding store query. All errors are converted to
/// `PlutusError::Store` and propagated to the gate as `Err(GateError)` (fail-closed).
#[async_trait::async_trait]
impl PolicyBackend for PlutusStore {
    /// Look up the org status for `tenant`.
    ///
    /// Returns `Ok(None)` when no row exists (unknown tenant). An sqlx error is
    /// surfaced as `Err(PlutusError::Store)` -- the gate treats that as a denial.
    async fn org_status(&self, tenant: TenantId) -> Result<Option<OrgStatus>> {
        let row: Option<(String,)> =
            sqlx::query_as("SELECT status FROM org WHERE tenant_id = $1")
                .bind(tenant.as_uuid())
                .fetch_optional(&self.pool)
                .await?;
        match row {
            None => Ok(None),
            Some((s,)) => {
                let status = OrgStatus::from_str(&s).map_err(|e| PlutusError::Store(e))?;
                Ok(Some(status))
            }
        }
    }

    /// Look up the member role of `principal` within `tenant`'s org.
    ///
    /// Returns `Ok(None)` when the principal is not a member.
    async fn member_role(
        &self,
        tenant: TenantId,
        principal: PrincipalId,
    ) -> Result<Option<Role>> {
        let row: Option<(String,)> = sqlx::query_as(
            "SELECT role FROM org_member WHERE tenant_id = $1 AND principal_id = $2",
        )
        .bind(tenant.as_uuid())
        .bind(principal.as_uuid())
        .fetch_optional(&self.pool)
        .await?;
        match row {
            None => Ok(None),
            Some((s,)) => {
                let role = s
                    .parse::<Role>()
                    .map_err(|e| PlutusError::Store(e.to_string()))?;
                Ok(Some(role))
            }
        }
    }

    /// Atomically increment the daily usage counter and return whether the result is within quota.
    ///
    /// Uses a single `INSERT ... ON CONFLICT DO UPDATE ... RETURNING used` (D1 rule 4).
    /// The increment is applied unconditionally; an over-limit request is denied but counted,
    /// which is acceptable for a daily hard cap.
    async fn check_and_increment(
        &self,
        tenant: TenantId,
        dim: QuotaDimension,
        amount: i64,
        today: &str,
    ) -> Result<QuotaOutcome> {
        let tenant_uuid = tenant.as_uuid();

        // Resolve the limit for this dimension.
        let limit = self
            .quota_config_limit(tenant_uuid, dim)
            .await?
            .ok_or_else(|| {
                PlutusError::Config(format!(
                    "no quota_config for tenant {tenant}; create_org must precede check_and_increment"
                ))
            })?;

        // Atomic upsert: add `amount` to today's counter, return the new total.
        let row: (i64,) = sqlx::query_as(
            r#"INSERT INTO usage_counter (tenant_id, dimension, day, used)
               VALUES ($1, $2, $3, $4)
               ON CONFLICT (tenant_id, dimension, day)
               DO UPDATE SET used = usage_counter.used + EXCLUDED.used
               RETURNING used"#,
        )
        .bind(tenant_uuid)
        .bind(dim.as_str())
        .bind(today)
        .bind(amount)
        .fetch_one(&self.pool)
        .await?;

        let used = row.0;
        Ok(QuotaOutcome {
            allowed: used <= limit,
            used,
            limit,
        })
    }

    /// Token-bucket rate-limit check for `tenant` at the given `now` instant.
    ///
    /// Runs inside a transaction with `SELECT ... FOR UPDATE` on the bucket row to serialize
    /// concurrent refill/take (D1 rule 5). Refills `elapsed_seconds * rpm / 60` tokens up to
    /// the `rpm` cap, then takes one token if available.
    async fn rate_limit_ok(
        &self,
        tenant: TenantId,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Result<bool> {
        let tenant_uuid = tenant.as_uuid();

        // Fetch the rate limit from quota_config.
        let qrow: Option<(i64,)> =
            sqlx::query_as("SELECT rate_limit_rpm FROM quota_config WHERE tenant_id = $1")
                .bind(tenant_uuid)
                .fetch_optional(&self.pool)
                .await?;
        let rpm = match qrow {
            Some((r,)) => r as f64,
            None => {
                return Err(PlutusError::Config(format!(
                    "no quota_config for tenant {tenant}; cannot rate-limit"
                )))
            }
        };

        let mut tx = self.pool.begin().await?;

        // Lock the bucket row for this org.
        let bucket: Option<(f64, String)> = sqlx::query_as(
            "SELECT tokens, last_refill FROM rate_limit_bucket WHERE tenant_id = $1 FOR UPDATE",
        )
        .bind(tenant_uuid)
        .fetch_optional(&mut *tx)
        .await?;

        let now_str = now.to_rfc3339();

        let (new_tokens, allowed) = match bucket {
            None => {
                // First request: create the bucket fully loaded minus this one token.
                let initial = (rpm - 1.0).max(0.0);
                (initial, true)
            }
            Some((old_tokens, last_refill_str)) => {
                // Refill based on elapsed time since last refill.
                let last_refill = chrono::DateTime::parse_from_rfc3339(&last_refill_str)
                    .map(|dt| dt.with_timezone(&chrono::Utc))
                    .unwrap_or(now);
                let elapsed = (now - last_refill).num_milliseconds().max(0) as f64 / 1000.0;
                let refill = elapsed * rpm / 60.0;
                let refilled = (old_tokens + refill).min(rpm);
                if refilled >= 1.0 {
                    (refilled - 1.0, true)
                } else {
                    (refilled, false)
                }
            }
        };

        // Upsert the bucket state.
        sqlx::query(
            r#"INSERT INTO rate_limit_bucket (tenant_id, tokens, last_refill)
               VALUES ($1, $2, $3)
               ON CONFLICT (tenant_id) DO UPDATE
                   SET tokens = EXCLUDED.tokens,
                       last_refill = EXCLUDED.last_refill"#,
        )
        .bind(tenant_uuid)
        .bind(new_tokens)
        .bind(&now_str)
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok(allowed)
    }
}

/// Tests for `PlutusStore`.
#[cfg(test)]
mod tests {
    use super::*;

    /// The embedded migrator resolves `./migrations` at compile time and the SQL file
    /// parses without errors. This is a database-free gate: it proves the migration the
    /// binary self-applies on boot is well-formed, without standing up Postgres.
    /// Mirrors `henosis-rift-server`'s `migrations_embed_and_parse` test (D1 rule 6).
    #[test]
    fn migrations_embed_and_parse() {
        let migrator = sqlx::migrate!("./migrations");
        assert_eq!(
            migrator.migrations.len(),
            1,
            "expected exactly one Plutus core migration (0001_plutus_core.sql)"
        );
    }

    /// Helper: connect to the live test database, returning `None` when the env var is unset.
    ///
    /// All live tests call this; when unset they `eprintln!`-skip gracefully so the offline
    /// workspace build stays green. Live tests require `SYNTHEOS_PLUTUS_TEST_PG_URL` to point
    /// at a reachable Postgres instance with a test schema.
    async fn live_store() -> Option<PlutusStore> {
        let url = match std::env::var("SYNTHEOS_PLUTUS_TEST_PG_URL") {
            Ok(u) if !u.is_empty() => u,
            _ => {
                eprintln!(
                    "[plutus live test] SYNTHEOS_PLUTUS_TEST_PG_URL not set -- skipping live store tests"
                );
                return None;
            }
        };
        Some(PlutusStore::open(&url).await.expect("open test store"))
    }

    /// Live: create an org and verify org_status returns Active and member_role returns Owner.
    #[tokio::test]
    async fn live_create_org_and_query_status_and_role() {
        let Some(store) = live_store().await else {
            return;
        };
        let tenant = TenantId::new();
        let owner = PrincipalId::new();
        store
            .create_org(tenant, "test-org", owner, QuotaTier::Free)
            .await
            .expect("create_org");
        let status = store
            .org_status(tenant)
            .await
            .expect("org_status")
            .expect("org exists");
        assert_eq!(status, OrgStatus::Active);
        let role = store
            .member_role(tenant, owner)
            .await
            .expect("member_role")
            .expect("owner is a member");
        assert_eq!(role, Role::Owner);
    }

    /// Live: org_status returns None for an unknown tenant (fail-closed: no org = deny).
    #[tokio::test]
    async fn live_org_status_returns_none_for_unknown_tenant() {
        let Some(store) = live_store().await else {
            return;
        };
        let unknown = TenantId::new();
        let status = store.org_status(unknown).await.expect("query succeeds");
        assert!(status.is_none());
    }

    /// Live: check_and_increment allows while under the limit and denies when exceeded.
    #[tokio::test]
    async fn live_check_and_increment_quota_boundary() {
        let Some(store) = live_store().await else {
            return;
        };
        let tenant = TenantId::new();
        let owner = PrincipalId::new();
        // Create the org first so quota_config exists.
        store
            .create_org(tenant, "quota-test", owner, QuotaTier::Free)
            .await
            .expect("create_org");
        // Override the quota to a tiny limit (2 tasks per day) for boundary testing.
        store
            .set_quota(
                tenant,
                &QuotaConfig {
                    max_tasks_per_day: 2,
                    max_tokens_per_day: 100_000,
                    max_tool_calls_per_day: 50,
                    max_memory_stores_per_day: 100,
                    rate_limit_rpm: 10,
                },
            )
            .await
            .expect("set_quota");
        let today = "2026-06-29"; // fixed test date, no wall-clock dep

        // First two increments should be allowed.
        let out1 = store
            .check_and_increment(tenant, QuotaDimension::Tasks, 1, today)
            .await
            .expect("increment 1");
        assert!(out1.allowed, "first increment within limit");

        let out2 = store
            .check_and_increment(tenant, QuotaDimension::Tasks, 1, today)
            .await
            .expect("increment 2");
        assert!(out2.allowed, "second increment at limit boundary");

        // Third increment exceeds the limit (used = 3 > limit 2).
        let out3 = store
            .check_and_increment(tenant, QuotaDimension::Tasks, 1, today)
            .await
            .expect("increment 3");
        assert!(!out3.allowed, "third increment exceeds limit");
        assert_eq!(out3.limit, 2);
    }

    /// Live: rate_limit_ok allows requests up to rpm, then denies, then allows after advancing now.
    #[tokio::test]
    async fn live_rate_limit_token_bucket() {
        let Some(store) = live_store().await else {
            return;
        };
        let tenant = TenantId::new();
        let owner = PrincipalId::new();
        store
            .create_org(tenant, "rate-test", owner, QuotaTier::Free)
            .await
            .expect("create_org");
        // Override rpm to 2 so we can exhaust it quickly.
        store
            .set_quota(
                tenant,
                &QuotaConfig {
                    max_tasks_per_day: 100,
                    max_tokens_per_day: 100_000,
                    max_tool_calls_per_day: 100,
                    max_memory_stores_per_day: 100,
                    rate_limit_rpm: 2,
                },
            )
            .await
            .expect("set_quota");

        let t0 = chrono::Utc::now();
        // First request fills and takes from a full bucket (rpm=2, so 1 token remains after first).
        let r1 = store.rate_limit_ok(tenant, t0).await.expect("rate 1");
        assert!(r1, "first request allowed");

        // Second request takes the last token.
        let r2 = store.rate_limit_ok(tenant, t0).await.expect("rate 2");
        assert!(r2, "second request allowed");

        // Third request: bucket is empty.
        let r3 = store.rate_limit_ok(tenant, t0).await.expect("rate 3");
        assert!(!r3, "third request denied -- bucket empty");

        // Advance now by 60 seconds: should refill rpm=2 tokens.
        let t1 = t0 + chrono::Duration::seconds(60);
        let r4 = store.rate_limit_ok(tenant, t1).await.expect("rate 4 after 60s");
        assert!(r4, "request allowed after 60s refill");
    }
}
