//! Maintained in-tree as an owned Henosis component.
//!
//! `henosis-hermes` -- external tool gateway library surface.
//!
//! Exposes the tool registry, the in-process invoke path, and all adapter
//! modules so callers (Synapse, Hephaestus) can invoke external tools without
//! going through HTTP. The HTTP/axum server lives in `src/main.rs` (the binary)
//! and wires these same modules onto TCP.
//!
//! # In-process entry points
//!
//! ```ignore
//! use henosis_hermes::{AppState, registry::build_registry};
//! use henosis_hermes::circuit::invoke_with_circuit;
//! use henosis_hermes::tool::{InvokeRequest, InvokeContext};
//! ```

use std::sync::Arc;

// -- module declarations -------------------------------------------------------

/// Adapter implementations for each upstream provider (Gmail, Drive, Calendar,
/// GitHub, Slack, Linear, Notion).
pub mod adapters;

/// Structured audit trail: SHA-256-hashed argument records published to Axon
/// in batches, queryable via `GET /audit`.
pub mod audit;

/// Tenant identity established by standalone HTTP Bearer authentication.
pub mod auth;

/// Best-effort Axon event publisher used by the audit trail, circuit breaker,
/// rate limiter, and tool dispatch to emit observability events.
pub mod axon;

/// Per-provider circuit breakers: fail-fast on open circuits, half-open probing
/// after the recovery window, and the `invoke_with_circuit` dispatch entry point.
pub mod circuit;

/// Runtime configuration loaded from environment variables.
pub mod config;

/// Credential daemon (phylaxd) HTTP client used by adapters to fetch OAuth tokens
/// and raw secrets.
pub mod phylaxd_client;

/// MCP (Model Context Protocol) JSON-RPC bridge at `POST /mcp`. Gated by
/// `HERMES_MCP_ENABLED=true`.
pub mod mcp_bridge;

/// Per-provider invocation metrics (counters, latency percentiles) snapshotted
/// at `GET /metrics`.
pub mod metrics;

/// Background OAuth token refresh daemon for providers whose tokens expire
/// (currently Google OAuth).
pub mod oauth_refresh;

/// Per-(tenant, tool) token-bucket rate limiter enforced on every invocation.
pub mod rate_limit;

/// Tool registry: maps tool IDs to `Arc<dyn Tool>` implementations and
/// provides the `build_registry` factory that registers all known adapters.
pub mod registry;

/// Axum route handlers for the HTTP API (`/tools`, `/tools/{id}/invoke`,
/// `/health/adapters`, `/audit`, `/metrics`, and admin endpoints).
pub mod routes;

/// Per-(tenant, provider) adapter configuration: enabled/disabled flag,
/// per-minute rate-limit override, and default argument injection.
pub mod tenant_config;

/// Core tool types: `Tool` trait, `ToolSchema`, `InvokeRequest`,
/// `InvokeResponse`, `InvokeContext`, `RetryPolicy`, and helper functions.
pub mod tool;

/// Lightweight JSON Schema validator for tool input arguments. Validates the
/// subset of JSON Schema used by adapter input schemas before invocation.
pub mod validation;

/// Inbound webhook ingestion: signature verification (HMAC-SHA256 for GitHub,
/// Slack, Linear) and normalized `WebhookEvent` publishing to Axon.
pub mod webhooks;

// -- re-exports of the primary in-process surface -------------------------------

/// Re-export the registry factory so callers can build a populated registry
/// without traversing into the registry module.
pub use registry::{build_registry, ToolRegistry};

/// Re-export the core tool types so in-process callers can invoke tools without
/// walking into the tool module.
pub use tool::{InvokeContext, InvokeRequest, InvokeResponse, Tool, ToolSchema};

/// Re-export the full controlled invocation path used by HTTP and in-process callers.
pub use routes::{invoke_controlled, InvocationOutcome};

// -- AppState ------------------------------------------------------------------

/// Shared application state threaded through all axum handlers and the
/// in-process invoke surface. Holds the fully-initialized registry, credential
/// client, rate limiter, circuit registry, metrics, audit trail, Axon publisher,
/// and tenant configuration store.
#[derive(Clone)]
pub struct AppState {
    /// The populated tool registry (all adapter implementations).
    pub registry: Arc<registry::ToolRegistry>,
    /// Credential daemon client for OAuth token and secret resolution.
    pub phylaxd: Arc<phylaxd_client::PhylaxdClient>,
    /// Token-bucket rate limiter (per tenant+tool).
    pub rate_limiter: Arc<rate_limit::RateLimiter>,
    /// Per-provider circuit breaker registry.
    pub circuits: Arc<circuit::CircuitRegistry>,
    /// Per-provider invocation metrics registry.
    pub metrics: Arc<metrics::MetricsRegistry>,
    /// Structured audit trail (SHA-256-hashed arg records, Axon-published).
    pub audit: Arc<audit::AuditTrail>,
    /// Best-effort Axon event publisher.
    pub axon: axon::AxonPublisher,
    /// Per-tenant adapter configuration store (enabled flag, rate limits,
    /// default args).
    pub tenant_config: Arc<tenant_config::TenantConfigStore>,
    /// Hermes's external base URL (`HERMES_PUBLIC_URL`), threaded into
    /// `InvokeContext` for webhook-registration adapters.
    pub public_url: Option<String>,
}

/// Production construction helpers for the complete Hermes runtime state.
impl AppState {
    /// Build the full Hermes state from explicit runtime configuration.
    ///
    /// This starts the OAuth refresh and audit-publishing background tasks, so
    /// it must be called from within a Tokio runtime.
    pub fn from_config(mut config: config::Config) -> Self {
        let registry = Arc::new(build_registry());
        let refresh_registry = oauth_refresh::RefreshRegistry::default();
        let phylaxd_url = std::mem::take(&mut config.phylaxd_url);
        let phylaxd_token = config
            .phylaxd_token
            .take()
            .map(|mut token| std::mem::take(&mut *token));
        let phylaxd = Arc::new(
            phylaxd_client::PhylaxdClient::new(phylaxd_url, phylaxd_token)
                .with_refresh_registry(refresh_registry.clone()),
        );
        oauth_refresh::OAuthRefreshDaemon::new(refresh_registry, phylaxd.clone()).spawn();

        let axon = axon::AxonPublisher::from_env();
        let audit = Arc::new(audit::AuditTrail::new(axon.clone()));
        audit.clone().spawn_publisher();

        Self {
            registry,
            phylaxd,
            rate_limiter: Arc::new(rate_limit::RateLimiter::new(
                rate_limit::RateLimitConfig::default(),
            )),
            circuits: Arc::new(circuit::CircuitRegistry::new()),
            metrics: Arc::new(metrics::MetricsRegistry::new()),
            audit,
            axon,
            tenant_config: Arc::new(tenant_config::TenantConfigStore::with_path(
                std::env::var("HERMES_TENANT_CONFIG_PATH")
                    .unwrap_or_else(|_| "data/tenant_config.json".to_string())
                    .into(),
            )),
            public_url: config.public_url.take(),
        }
    }

    /// Build the full Hermes state from process environment variables.
    pub fn from_env() -> Self {
        Self::from_config(config::Config::from_env())
    }
}
