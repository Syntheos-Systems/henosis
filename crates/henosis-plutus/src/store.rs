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
use crate::billing::{Entitlement, EntitlementSource, EntitlementStatus};
use crate::quota::{QuotaConfig, QuotaDimension, QuotaOutcome, QuotaTier};
use crate::rbac::Role;
use crate::{PlutusError, Result};

/// One `entitlement` row as sqlx decodes it, before it is parsed into an [`Entitlement`].
///
/// Named because the raw tuple (id, tenant_id, tier, source, subscription_id, status,
/// period_end) is wide enough that clippy rightly refuses to read it inline.
type EntitlementRow = (
    i64,
    Uuid,
    String,
    String,
    Option<String>,
    String,
    Option<String>,
);

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

    /// Insert or update the Stripe customer id mapped to `tenant`.
    ///
    /// Idempotent: a repeated call for the same `tenant` overwrites `stripe_customer_id`
    /// rather than erroring, so replaying a `customer.created`/`customer.updated` webhook
    /// converges to the latest value.
    pub async fn upsert_billing_customer(
        &self,
        tenant: TenantId,
        stripe_customer_id: &str,
    ) -> Result<()> {
        sqlx::query(
            r#"INSERT INTO billing_customer (tenant_id, stripe_customer_id)
               VALUES ($1, $2)
               ON CONFLICT (tenant_id) DO UPDATE
                   SET stripe_customer_id = EXCLUDED.stripe_customer_id"#,
        )
        .bind(tenant.as_uuid())
        .bind(stripe_customer_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Insert or update a Stripe-sourced entitlement keyed by `subscription_id`.
    ///
    /// Idempotent on `stripe_subscription_id`: replaying the same subscription event, or
    /// receiving a later update for the same subscription, updates `tier`, `status`,
    /// `current_period_end`, and `updated_at` in place rather than inserting a duplicate row.
    pub async fn upsert_stripe_entitlement(
        &self,
        tenant: TenantId,
        subscription_id: &str,
        tier: QuotaTier,
        status: EntitlementStatus,
        current_period_end: Option<&str>,
    ) -> Result<()> {
        sqlx::query(
            r#"INSERT INTO entitlement
               (tenant_id, tier, source, stripe_subscription_id, status, current_period_end)
               VALUES ($1, $2, 'stripe', $3, $4, $5)
               ON CONFLICT (stripe_subscription_id) DO UPDATE
                   SET tier = EXCLUDED.tier,
                       status = EXCLUDED.status,
                       current_period_end = EXCLUDED.current_period_end,
                       updated_at = to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS"Z"')"#,
        )
        .bind(tenant.as_uuid())
        .bind(tier.as_str())
        .bind(subscription_id)
        .bind(status.as_str())
        .bind(current_period_end)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Grant `tier` to `tenant` with no backing Stripe subscription (`source = 'manual'`).
    ///
    /// Always inserts a new row: `stripe_subscription_id` is left NULL, and Postgres UNIQUE
    /// constraints permit any number of NULLs, so repeated manual grants -- for the same or
    /// different tenants -- never collide with each other or with Stripe-sourced rows.
    pub async fn grant_manual_entitlement(&self, tenant: TenantId, tier: QuotaTier) -> Result<()> {
        sqlx::query(
            r#"INSERT INTO entitlement (tenant_id, tier, source, stripe_subscription_id, status)
               VALUES ($1, $2, 'manual', NULL, 'active')"#,
        )
        .bind(tenant.as_uuid())
        .bind(tier.as_str())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Set the status of the entitlement identified by `subscription_id`.
    ///
    /// Returns `Ok(true)` when a row was updated and `Ok(false)` when no entitlement carries
    /// that subscription id. The caller must not treat "no such subscription" as a successful
    /// transition: an event naming an unknown subscription has to be recorded and ignored, not
    /// silently accepted as if a tier had changed.
    ///
    /// Private helper shared by `cancel_entitlement` and `mark_entitlement_past_due`; each
    /// public method is a thin wrapper naming the specific transition it performs.
    async fn set_entitlement_status(
        &self,
        subscription_id: &str,
        status: EntitlementStatus,
    ) -> Result<bool> {
        let updated = sqlx::query(
            r#"UPDATE entitlement
               SET status = $1,
                   updated_at = to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS"Z"')
               WHERE stripe_subscription_id = $2"#,
        )
        .bind(status.as_str())
        .bind(subscription_id)
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok(updated > 0)
    }

    /// Mark the entitlement for `subscription_id` as canceled (the Stripe subscription ended).
    ///
    /// Returns `Ok(false)` when no entitlement exists for that subscription id.
    pub async fn cancel_entitlement(&self, subscription_id: &str) -> Result<bool> {
        self.set_entitlement_status(subscription_id, EntitlementStatus::Canceled)
            .await
    }

    /// Mark the entitlement for `subscription_id` as past-due (a Stripe payment failed).
    ///
    /// Returns `Ok(false)` when no entitlement exists for that subscription id.
    pub async fn mark_entitlement_past_due(&self, subscription_id: &str) -> Result<bool> {
        self.set_entitlement_status(subscription_id, EntitlementStatus::PastDue)
            .await
    }

    /// Resolve which tenant owns a Stripe customer id.
    ///
    /// Stripe subscription events identify the payer by customer id, never by our tenant id,
    /// so this mapping is how a webhook is bound to an org. Returns `Ok(None)` for an unknown
    /// customer: the pipeline must record and ignore such an event rather than guess a tenant.
    pub async fn tenant_for_stripe_customer(
        &self,
        stripe_customer_id: &str,
    ) -> Result<Option<TenantId>> {
        let row: Option<(Uuid,)> =
            sqlx::query_as("SELECT tenant_id FROM billing_customer WHERE stripe_customer_id = $1")
                .bind(stripe_customer_id)
                .fetch_optional(&self.pool)
                .await?;
        match row {
            None => Ok(None),
            Some((uuid,)) => TenantId::from_uuid(uuid)
                .map(Some)
                .map_err(|e| PlutusError::Store(e.to_string())),
        }
    }

    /// Look up the quota tier a Stripe price id maps to.
    ///
    /// Returns `Ok(None)` when the price id has no mapping; the webhook handler should treat
    /// that as "cannot determine tier" and fail the event rather than guessing a tier.
    pub async fn price_tier(&self, stripe_price_id: &str) -> Result<Option<QuotaTier>> {
        let row: Option<(String,)> =
            sqlx::query_as("SELECT tier FROM billing_price_map WHERE stripe_price_id = $1")
                .bind(stripe_price_id)
                .fetch_optional(&self.pool)
                .await?;
        match row {
            None => Ok(None),
            Some((s,)) => {
                let tier =
                    QuotaTier::from_str(&s).map_err(|e| PlutusError::Store(e.to_string()))?;
                Ok(Some(tier))
            }
        }
    }

    /// Insert or update the tier that `stripe_price_id` maps to.
    pub async fn insert_price_mapping(
        &self,
        stripe_price_id: &str,
        tier: QuotaTier,
    ) -> Result<()> {
        sqlx::query(
            r#"INSERT INTO billing_price_map (stripe_price_id, tier)
               VALUES ($1, $2)
               ON CONFLICT (stripe_price_id) DO UPDATE
                   SET tier = EXCLUDED.tier"#,
        )
        .bind(stripe_price_id)
        .bind(tier.as_str())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Return whether `event_id` has already been recorded in the `billing_event` log.
    ///
    /// Used by the webhook handler to skip a redelivered event before doing any processing.
    pub async fn billing_event_seen(&self, event_id: &str) -> Result<bool> {
        let row: Option<(String,)> =
            sqlx::query_as("SELECT event_id FROM billing_event WHERE event_id = $1")
                .bind(event_id)
                .fetch_optional(&self.pool)
                .await?;
        Ok(row.is_some())
    }

    /// Record a processed webhook event for idempotency, storing its raw JSON payload and the
    /// outcome of processing it.
    ///
    /// `payload_json` must be a valid JSON string; it is cast to `jsonb` in the query text
    /// rather than bound as a typed value, since the workspace `sqlx` has no `json` feature.
    /// `ON CONFLICT (event_id) DO NOTHING` makes a repeated call for an already-recorded event
    /// a no-op rather than an error, so a redelivered webhook is safe to record twice.
    pub async fn record_billing_event(
        &self,
        event_id: &str,
        event_type: &str,
        payload_json: &str,
        outcome: &str,
    ) -> Result<()> {
        sqlx::query(
            r#"INSERT INTO billing_event (event_id, event_type, payload, outcome, processed_at)
               VALUES ($1, $2, $3::jsonb, $4,
                       to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS"Z"'))
               ON CONFLICT (event_id) DO NOTHING"#,
        )
        .bind(event_id)
        .bind(event_type)
        .bind(payload_json)
        .bind(outcome)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Apply `tier` to `tenant`: update `org.plan_tier` and reset `quota_config` to the
    /// tier's defaults, in a single transaction.
    ///
    /// This is the single "entitlement -> quota" primitive. Every path that changes a
    /// tenant's effective tier -- a Stripe webhook, a manual grant, an admin override --
    /// should call this rather than touching `org` or `quota_config` directly, so the tier
    /// and its quota configuration always move together.
    pub async fn apply_tier(&self, tenant: TenantId, tier: QuotaTier) -> Result<()> {
        let tenant_uuid = tenant.as_uuid();
        let defaults = tier.defaults();

        let mut tx = self.pool.begin().await?;

        let org_updated = sqlx::query("UPDATE org SET plan_tier = $1 WHERE tenant_id = $2")
            .bind(tier.as_str())
            .bind(tenant_uuid)
            .execute(&mut *tx)
            .await?
            .rows_affected();

        // Fail closed on an unknown tenant. Without this guard the UPDATE would match zero
        // rows and report success, and the quota_config upsert below would then manufacture
        // an orphan quota row for an org that does not exist -- a webhook naming a stale or
        // deleted tenant would silently "apply" a paid tier to nothing. Dropping `tx`
        // without committing rolls back the UPDATE.
        if org_updated == 0 {
            return Err(PlutusError::Config(format!(
                "apply_tier: no org for tenant {tenant}"
            )));
        }

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

    /// Look up the current `plan_tier` for `tenant`.
    ///
    /// Returns `Ok(None)` when no `org` row exists for the tenant.
    pub async fn org_tier(&self, tenant: TenantId) -> Result<Option<QuotaTier>> {
        let row: Option<(String,)> = sqlx::query_as("SELECT plan_tier FROM org WHERE tenant_id = $1")
            .bind(tenant.as_uuid())
            .fetch_optional(&self.pool)
            .await?;
        match row {
            None => Ok(None),
            Some((s,)) => {
                let tier =
                    QuotaTier::from_str(&s).map_err(|e| PlutusError::Store(e.to_string()))?;
                Ok(Some(tier))
            }
        }
    }

    /// Look up the full `quota_config` row for `tenant`.
    ///
    /// Returns `Ok(None)` when no `quota_config` row exists for the tenant. Mirrors the row
    /// shape read by the private `quota_config_limit` helper, but returns the whole
    /// configuration rather than a single dimension's limit.
    pub async fn quota_config(&self, tenant: TenantId) -> Result<Option<QuotaConfig>> {
        let row: Option<(i64, i64, i64, i64, i64)> = sqlx::query_as(
            r#"SELECT max_tasks_per_day, max_tokens_per_day,
                      max_tool_calls_per_day, max_memory_stores_per_day, rate_limit_rpm
               FROM quota_config
               WHERE tenant_id = $1"#,
        )
        .bind(tenant.as_uuid())
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(|(tasks, tokens, tool_calls, memory_stores, rpm)| QuotaConfig {
            max_tasks_per_day: tasks,
            max_tokens_per_day: tokens,
            max_tool_calls_per_day: tool_calls,
            max_memory_stores_per_day: memory_stores,
            rate_limit_rpm: rpm,
        }))
    }

    /// Look up the entitlement row for a given Stripe `subscription_id`.
    ///
    /// Returns `Ok(None)` when no entitlement has been recorded for that subscription id
    /// (never true for a manual grant, since those have no subscription id to look up by).
    pub async fn entitlement_for_subscription(
        &self,
        subscription_id: &str,
    ) -> Result<Option<Entitlement>> {
        let row: Option<EntitlementRow> =
            sqlx::query_as(
                r#"SELECT id, tenant_id, tier, source, stripe_subscription_id, status,
                          current_period_end
                   FROM entitlement
                   WHERE stripe_subscription_id = $1"#,
            )
            .bind(subscription_id)
            .fetch_optional(&self.pool)
            .await?;

        match row {
            None => Ok(None),
            Some((id, tenant_uuid, tier_s, source_s, sub_id, status_s, period_end)) => {
                let tenant_id = TenantId::from_uuid(tenant_uuid)
                    .map_err(|e| PlutusError::Store(e.to_string()))?;
                let tier =
                    QuotaTier::from_str(&tier_s).map_err(|e| PlutusError::Store(e.to_string()))?;
                let source = EntitlementSource::from_str(&source_s)
                    .map_err(|e| PlutusError::Store(e.to_string()))?;
                let status = EntitlementStatus::from_str(&status_s)
                    .map_err(|e| PlutusError::Store(e.to_string()))?;
                Ok(Some(Entitlement {
                    id,
                    tenant_id,
                    tier,
                    source,
                    stripe_subscription_id: sub_id,
                    status,
                    current_period_end: period_end,
                }))
            }
        }
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
                let status = OrgStatus::from_str(&s).map_err(PlutusError::Store)?;
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

    /// Resolve which tenant (org) `principal` belongs to via their membership row.
    ///
    /// Executes `SELECT tenant_id FROM org_member WHERE principal_id = $1 LIMIT 1`.
    /// Returns `Ok(Some(tenant))` when a membership row exists, or `Ok(None)` when
    /// the principal has no org membership. Used by the operator login flow to map
    /// a verified principal to its org before checking org status and role.
    async fn tenant_for_principal(&self, principal: PrincipalId) -> Result<Option<TenantId>> {
        let row: Option<(Uuid,)> = sqlx::query_as(
            "SELECT tenant_id FROM org_member WHERE principal_id = $1 LIMIT 1",
        )
        .bind(principal.as_uuid())
        .fetch_optional(&self.pool)
        .await?;
        match row {
            None => Ok(None),
            Some((uuid,)) => TenantId::from_uuid(uuid)
                .map(Some)
                .map_err(|e| PlutusError::Store(e.to_string())),
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
            2,
            "expected two Plutus migrations (0001_plutus_core.sql, 0002_plutus_billing.sql)"
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

    /// DB-free: `EntitlementStatus` and `EntitlementSource` round-trip through their text
    /// forms and reject unknown strings. No live Postgres required (D1 rule 6).
    #[test]
    fn entitlement_status_and_source_text_roundtrip() {
        for (s, expected) in [
            ("active", EntitlementStatus::Active),
            ("past_due", EntitlementStatus::PastDue),
            ("canceled", EntitlementStatus::Canceled),
        ] {
            let parsed: EntitlementStatus = s.parse().expect("valid status");
            assert_eq!(parsed, expected);
            assert_eq!(parsed.to_string(), s);
        }
        assert!("pending".parse::<EntitlementStatus>().is_err());

        for (s, expected) in [
            ("stripe", EntitlementSource::Stripe),
            ("manual", EntitlementSource::Manual),
        ] {
            let parsed: EntitlementSource = s.parse().expect("valid source");
            assert_eq!(parsed, expected);
            assert_eq!(parsed.to_string(), s);
        }
        assert!("imported".parse::<EntitlementSource>().is_err());
    }

    /// Live: upserting the same Stripe subscription id twice with different tiers results in
    /// exactly one entitlement row (enforced by the `stripe_subscription_id` UNIQUE
    /// constraint), and the row reflects the second (latest) upsert.
    #[tokio::test]
    async fn live_stripe_entitlement_upsert_is_idempotent() {
        let Some(store) = live_store().await else {
            return;
        };
        let tenant = TenantId::new();
        let owner = PrincipalId::new();
        store
            .create_org(tenant, "stripe-upsert-test", owner, QuotaTier::Free)
            .await
            .expect("create_org");

        let subscription_id = format!("sub_test_{}", tenant.as_uuid());

        store
            .upsert_stripe_entitlement(
                tenant,
                &subscription_id,
                QuotaTier::Pro,
                EntitlementStatus::Active,
                Some("2026-08-01T00:00:00Z"),
            )
            .await
            .expect("first upsert");

        store
            .upsert_stripe_entitlement(
                tenant,
                &subscription_id,
                QuotaTier::Team,
                EntitlementStatus::Active,
                Some("2026-09-01T00:00:00Z"),
            )
            .await
            .expect("second upsert");

        let entitlement = store
            .entitlement_for_subscription(&subscription_id)
            .await
            .expect("query entitlement")
            .expect("entitlement exists");
        assert_eq!(entitlement.tier, QuotaTier::Team, "second upsert's tier wins");
        assert_eq!(
            entitlement.current_period_end.as_deref(),
            Some("2026-09-01T00:00:00Z"),
            "second upsert's period end wins"
        );
    }

    /// Live: a manual grant has source = Manual and no Stripe subscription id; two manual
    /// grants for two different tenants both succeed, proving the NULL-unique behavior on
    /// `stripe_subscription_id` (Postgres UNIQUE permits any number of NULLs).
    #[tokio::test]
    async fn live_grant_manual_entitlement() {
        let Some(store) = live_store().await else {
            return;
        };
        let tenant_a = TenantId::new();
        let tenant_b = TenantId::new();
        let owner_a = PrincipalId::new();
        let owner_b = PrincipalId::new();
        store
            .create_org(tenant_a, "manual-grant-a", owner_a, QuotaTier::Free)
            .await
            .expect("create_org a");
        store
            .create_org(tenant_b, "manual-grant-b", owner_b, QuotaTier::Free)
            .await
            .expect("create_org b");

        store
            .grant_manual_entitlement(tenant_a, QuotaTier::Pro)
            .await
            .expect("manual grant a");
        store
            .grant_manual_entitlement(tenant_b, QuotaTier::Team)
            .await
            .expect("manual grant b -- must not collide with grant a's NULL subscription id");

        let row: (String, Option<String>) = sqlx::query_as(
            "SELECT source, stripe_subscription_id FROM entitlement WHERE tenant_id = $1",
        )
        .bind(tenant_a.as_uuid())
        .fetch_one(&store.pool)
        .await
        .expect("fetch manual entitlement a");
        assert_eq!(row.0, EntitlementSource::Manual.as_str());
        assert!(row.1.is_none(), "manual grant has no subscription id");
    }

    /// Live: insert_price_mapping then price_tier returns the tier; an unrecognized price id
    /// returns None.
    #[tokio::test]
    async fn live_price_map_round_trip() {
        let Some(store) = live_store().await else {
            return;
        };
        let price_id = format!("price_test_{}", TenantId::new().as_uuid());

        store
            .insert_price_mapping(&price_id, QuotaTier::Pro)
            .await
            .expect("insert_price_mapping");

        let tier = store
            .price_tier(&price_id)
            .await
            .expect("price_tier query")
            .expect("mapping exists");
        assert_eq!(tier, QuotaTier::Pro);

        let unknown_price_id = format!("price_unknown_{}", TenantId::new().as_uuid());
        let missing = store
            .price_tier(&unknown_price_id)
            .await
            .expect("price_tier query for unknown id");
        assert!(missing.is_none());
    }

    /// Live: billing_event_seen is false before recording, true after; recording the same
    /// event id twice (a webhook redelivery) does not error.
    #[tokio::test]
    async fn live_billing_event_idempotency() {
        let Some(store) = live_store().await else {
            return;
        };
        let event_id = format!("evt_test_{}", TenantId::new().as_uuid());

        let seen_before = store
            .billing_event_seen(&event_id)
            .await
            .expect("billing_event_seen before recording");
        assert!(!seen_before, "unseen event id reports false before recording");

        store
            .record_billing_event(
                &event_id,
                "customer.subscription.updated",
                r#"{"id":"evt_test"}"#,
                "applied pro tier",
            )
            .await
            .expect("record_billing_event first delivery");

        store
            .record_billing_event(
                &event_id,
                "customer.subscription.updated",
                r#"{"id":"evt_test"}"#,
                "applied pro tier",
            )
            .await
            .expect("record_billing_event redelivery must not error");

        let seen_after = store
            .billing_event_seen(&event_id)
            .await
            .expect("billing_event_seen after recording");
        assert!(seen_after, "recorded event id reports true after recording");
    }

    /// Live: apply_tier updates org.plan_tier and resets quota_config to the tier's defaults,
    /// both inside one transaction.
    #[tokio::test]
    async fn live_apply_tier_sets_org_tier_and_quota() {
        let Some(store) = live_store().await else {
            return;
        };
        let tenant = TenantId::new();
        let owner = PrincipalId::new();
        store
            .create_org(tenant, "apply-tier-test", owner, QuotaTier::Free)
            .await
            .expect("create_org");

        store
            .apply_tier(tenant, QuotaTier::Pro)
            .await
            .expect("apply_tier");

        let tier = store
            .org_tier(tenant)
            .await
            .expect("org_tier query")
            .expect("org exists");
        assert_eq!(tier, QuotaTier::Pro);

        let cfg = store
            .quota_config(tenant)
            .await
            .expect("quota_config query")
            .expect("quota_config exists");
        assert_eq!(cfg, QuotaTier::Pro.defaults());
    }

    /// Live: upsert active -> mark_entitlement_past_due -> status PastDue ->
    /// cancel_entitlement -> status Canceled.
    #[tokio::test]
    async fn live_cancel_and_past_due_transitions() {
        let Some(store) = live_store().await else {
            return;
        };
        let tenant = TenantId::new();
        let owner = PrincipalId::new();
        store
            .create_org(tenant, "transitions-test", owner, QuotaTier::Free)
            .await
            .expect("create_org");

        let subscription_id = format!("sub_transitions_{}", tenant.as_uuid());
        store
            .upsert_stripe_entitlement(
                tenant,
                &subscription_id,
                QuotaTier::Pro,
                EntitlementStatus::Active,
                None,
            )
            .await
            .expect("initial upsert active");

        let hit = store
            .mark_entitlement_past_due(&subscription_id)
            .await
            .expect("mark_entitlement_past_due");
        assert!(hit, "a known subscription id must report a row was updated");
        let past_due = store
            .entitlement_for_subscription(&subscription_id)
            .await
            .expect("query after past_due")
            .expect("entitlement exists");
        assert_eq!(past_due.status, EntitlementStatus::PastDue);

        let hit = store
            .cancel_entitlement(&subscription_id)
            .await
            .expect("cancel_entitlement");
        assert!(hit, "a known subscription id must report a row was updated");
        let canceled = store
            .entitlement_for_subscription(&subscription_id)
            .await
            .expect("query after cancel")
            .expect("entitlement exists");
        assert_eq!(canceled.status, EntitlementStatus::Canceled);
    }

    /// Live: a status transition naming an unknown subscription id reports `false` rather than
    /// silently succeeding. The webhook pipeline relies on this to distinguish "canceled a real
    /// entitlement" from "this event names a subscription we have never seen".
    #[tokio::test]
    async fn live_status_transition_on_unknown_subscription_reports_no_row() {
        let Some(store) = live_store().await else {
            return;
        };
        let unknown = format!("sub_never_seen_{}", TenantId::new().as_uuid());
        assert!(
            !store.cancel_entitlement(&unknown).await.expect("cancel query succeeds"),
            "canceling an unknown subscription must report no row updated"
        );
        assert!(
            !store
                .mark_entitlement_past_due(&unknown)
                .await
                .expect("past_due query succeeds"),
            "past-due on an unknown subscription must report no row updated"
        );
    }

    /// Live: `apply_tier` against a tenant with no org fails closed and leaves no orphan
    /// `quota_config` row behind. Without the rows-affected guard the org UPDATE would match
    /// nothing, report success, and the quota upsert would manufacture a paid quota row for an
    /// org that does not exist.
    #[tokio::test]
    async fn live_apply_tier_on_unknown_org_fails_closed() {
        let Some(store) = live_store().await else {
            return;
        };
        let ghost = TenantId::new();
        let err = store
            .apply_tier(ghost, QuotaTier::Enterprise)
            .await
            .expect_err("apply_tier on an org-less tenant must fail");
        assert!(
            matches!(err, PlutusError::Config(_)),
            "expected a config error, got {err:?}"
        );
        assert!(
            store
                .quota_config(ghost)
                .await
                .expect("quota_config query succeeds")
                .is_none(),
            "the rolled-back transaction must leave no orphan quota_config row"
        );
    }

    /// Live: a Stripe customer id round-trips back to the tenant that owns it, and an unknown
    /// customer id resolves to `None` rather than a guessed tenant.
    #[tokio::test]
    async fn live_tenant_for_stripe_customer_round_trip() {
        let Some(store) = live_store().await else {
            return;
        };
        let tenant = TenantId::new();
        let owner = PrincipalId::new();
        store
            .create_org(tenant, "customer-lookup-test", owner, QuotaTier::Free)
            .await
            .expect("create_org");
        let customer_id = format!("cus_{}", tenant.as_uuid());
        store
            .upsert_billing_customer(tenant, &customer_id)
            .await
            .expect("upsert_billing_customer");

        let found = store
            .tenant_for_stripe_customer(&customer_id)
            .await
            .expect("lookup succeeds")
            .expect("customer maps to a tenant");
        assert_eq!(found, tenant);

        assert!(
            store
                .tenant_for_stripe_customer("cus_does_not_exist")
                .await
                .expect("lookup succeeds")
                .is_none(),
            "an unknown customer id must not resolve to a tenant"
        );
    }
}
