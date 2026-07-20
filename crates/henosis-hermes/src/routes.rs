//! Axum route handlers for the Hermes HTTP API.
//!
//! All handlers receive `AppState` via axum's `State` extractor and return
//! `impl IntoResponse`. The invoke path applies rate limiting, tenant config,
//! circuit checking, validation, and audit in the same order as the MCP path
//! so both surfaces produce identical outcomes.

use std::collections::BTreeMap;
use std::time::Instant;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use serde_json::json;

use crate::circuit::invoke_with_circuit;
use crate::metrics::Outcome;
use crate::rate_limit::CheckOutcome;
use crate::tool::{err, error_response, InvokeContext, InvokeRequest, InvokeResponse};
use crate::AppState;

/// A controlled Hermes invocation plus the HTTP transport metadata needed by
/// the gateway wrapper.
#[derive(Debug, Clone)]
pub struct InvocationOutcome {
    /// The adapter response envelope returned to every caller surface.
    pub response: InvokeResponse,
    /// HTTP-equivalent status for callers that expose an HTTP transport.
    pub status: StatusCode,
    /// Retry delay emitted only when the rate limiter throttles the call.
    pub retry_after_secs: Option<u64>,
}

/// `GET /tools`: list all registered tools, sorted by tool ID.
pub async fn list_tools(State(state): State<AppState>) -> impl IntoResponse {
    Json(state.registry.list())
}

/// `POST /tools/{tool_id}/invoke`: invoke a tool with the given JSON request
/// body. Applies tenant config, rate limiting, circuit checking, validation,
/// and audit before dispatching to the adapter.
pub async fn invoke_tool(
    State(state): State<AppState>,
    Path(tool_id): Path<String>,
    Json(req): Json<InvokeRequest>,
) -> impl IntoResponse {
    let outcome = invoke_controlled(&state, &tool_id, req).await;
    if let Some(retry_after_secs) = outcome.retry_after_secs {
        return (
            outcome.status,
            [(
                axum::http::header::RETRY_AFTER,
                retry_after_secs.to_string(),
            )],
            Json(outcome.response),
        )
            .into_response();
    }
    (outcome.status, Json(outcome.response)).into_response()
}

/// Invoke one Hermes tool through the complete shared control path.
///
/// Both the HTTP gateway and absorbed in-process callers use this function so
/// tenant policy, rate limits, circuits, metrics, audit, and Axon events cannot
/// diverge between transports.
pub async fn invoke_controlled(
    state: &AppState,
    tool_id: &str,
    mut req: InvokeRequest,
) -> InvocationOutcome {
    let tool = match state.registry.get(tool_id) {
        Some(t) => t,
        None => {
            return InvocationOutcome {
                response: error_response(
                    tool_id,
                    "tool_not_found",
                    format!("tool '{tool_id}' not found"),
                    None,
                ),
                status: StatusCode::NOT_FOUND,
                retry_after_secs: None,
            };
        }
    };

    let provider = state.registry.provider_of(tool_id).unwrap_or("unknown");
    let tenant_owned = req.tenant_id.clone();
    let tenant_key = req.tenant_id.as_deref().unwrap_or("_anon");

    // Per-tenant adapter config: a disabled provider fails closed with a
    // structured 403 the LLM can reason about ("not enabled for your tenant"),
    // distinct from a circuit-open 503 ("temporarily unavailable").
    let cfg = state.tenant_config.get(tenant_key, provider);
    if !cfg.enabled {
        state.audit.record(
            tenant_owned,
            tool_id,
            provider,
            0,
            Outcome::Error,
            Some("adapter_disabled".into()),
            0,
            crate::audit::args_hash(&req.args),
        );
        let response = InvokeResponse {
            tool_id: tool_id.to_string(),
            success: false,
            result: None,
            error: Some(err(
                "adapter_disabled",
                format!("provider '{provider}' is disabled for this tenant"),
                None,
            )),
            duration_ms: 0,
        };
        return InvocationOutcome {
            response,
            status: StatusCode::FORBIDDEN,
            retry_after_secs: None,
        };
    }

    // Merge tenant default args (as defaults, request wins) before dispatch so
    // the audit hash reflects the args the adapter actually received.
    req.args = crate::tenant_config::merge_default_args(cfg.default_args.as_ref(), req.args);

    // Capture the audit-relevant facts before `req` is consumed by dispatch:
    // the args are hashed (never stored) and the tenant is cloned.
    let args_digest = crate::audit::args_hash(&req.args);

    // Rate-limit per (tenant, tool). Anonymous calls (no tenant_id) share a
    // single bucket keyed on "_anon".
    let tenant_for_limit = req.tenant_id.as_deref().unwrap_or("_anon");
    if let CheckOutcome::Throttled { retry_after_secs } = state
        .rate_limiter
        .check_with_capacity(tenant_for_limit, tool_id, cfg.rate_limit_override)
        .await
    {
        // A throttled call never reaches the adapter, but it is still an
        // invocation outcome the metrics + audit surfaces should reflect.
        state.metrics.record(provider, Outcome::RateLimited, 0, 0);
        state.audit.record(
            tenant_owned,
            tool_id,
            provider,
            0,
            Outcome::RateLimited,
            Some("rate_limited".into()),
            0,
            args_digest,
        );
        let response = InvokeResponse {
            tool_id: tool_id.to_string(),
            success: false,
            result: None,
            error: Some(err(
                "rate_limited",
                format!(
                    "tenant '{}' over rate limit for tool '{}'",
                    tenant_for_limit, tool_id
                ),
                Some(&format!("retry in ~{retry_after_secs}s")),
            )),
            duration_ms: 0,
        };
        return InvocationOutcome {
            response,
            status: StatusCode::TOO_MANY_REQUESTS,
            retry_after_secs: Some(retry_after_secs),
        };
    }

    let ctx = InvokeContext {
        credd: state.credd.clone(),
        bases: crate::tool::ProviderBases::default(),
        hermes_public_url: state.public_url.clone(),
    };

    let start = Instant::now();
    let (mut resp, retries) = invoke_with_circuit(&state.circuits, &tool, tool_id, &ctx, req).await;
    resp.duration_ms = start.elapsed().as_millis() as u64;

    // Fold the outcome (and the upstream retry count) into metrics + audit.
    let outcome = Outcome::classify(&resp);
    let error_code = error_code_of(&resp);
    state
        .metrics
        .record(provider, outcome, resp.duration_ms, retries);
    state.audit.record(
        tenant_owned.clone(),
        tool_id,
        provider,
        resp.duration_ms,
        outcome,
        error_code.clone(),
        retries,
        args_digest,
    );
    state.axon.tool_invoked(
        tool_id,
        tenant_owned.as_deref(),
        outcome.label(),
        resp.duration_ms,
    );
    if outcome == Outcome::Error {
        state
            .axon
            .tool_failed(tool_id, error_code.as_deref(), retries);
    }

    // A tripped circuit fails fast with HTTP 503 so callers can distinguish
    // "upstream temporarily unavailable" from an ordinary adapter error.
    let status = if is_circuit_open(&resp) {
        StatusCode::SERVICE_UNAVAILABLE
    } else {
        StatusCode::OK
    };
    InvocationOutcome {
        response: resp,
        status,
        retry_after_secs: None,
    }
}

/// Return `true` when the response error code is `circuit_open`.
fn is_circuit_open(resp: &InvokeResponse) -> bool {
    resp.error
        .as_ref()
        .and_then(|e| e.get("code"))
        .and_then(|c| c.as_str())
        == Some("circuit_open")
}

/// Extract the structured error code from a failed response, if present.
fn error_code_of(resp: &InvokeResponse) -> Option<String> {
    resp.error
        .as_ref()
        .and_then(|e| e.get("code"))
        .and_then(|c| c.as_str())
        .map(String::from)
}

/// `GET /metrics`: per-provider and global invocation metrics in JSON.
pub async fn metrics(State(state): State<AppState>) -> impl IntoResponse {
    Json(state.metrics.snapshot(state.circuits.open_count()))
}

/// `GET /admin/tenants/{tenant_id}/adapters`: configured adapter overrides for
/// a tenant.
pub async fn list_tenant_adapters(
    State(state): State<AppState>,
    Path(tenant_id): Path<String>,
) -> impl IntoResponse {
    Json(json!({
        "tenant_id": tenant_id,
        "adapters": state.tenant_config.list(&tenant_id),
    }))
}

/// `PUT /admin/tenants/{tenant_id}/adapters/{provider}`: set a tenant's adapter
/// config.
pub async fn set_tenant_adapter(
    State(state): State<AppState>,
    Path((tenant_id, provider)): Path<(String, String)>,
    Json(config): Json<crate::tenant_config::TenantAdapterConfig>,
) -> impl IntoResponse {
    state.tenant_config.set(&tenant_id, &provider, config);
    (
        StatusCode::OK,
        Json(json!({
            "tenant_id": tenant_id,
            "provider": provider,
            "config": state.tenant_config.get(&tenant_id, &provider),
        })),
    )
}

/// `PUT /admin/tenants/{tenant_id}/adapters/{provider}/disable`: shortcut to
/// disable a provider for a tenant.
pub async fn disable_tenant_adapter(
    State(state): State<AppState>,
    Path((tenant_id, provider)): Path<(String, String)>,
) -> impl IntoResponse {
    state.tenant_config.disable(&tenant_id, &provider);
    (
        StatusCode::OK,
        Json(json!({ "tenant_id": tenant_id, "provider": provider, "enabled": false })),
    )
}

/// `GET /audit`: recent audit records, newest first, filterable by
/// `tenant_id`, `tool_id`, `outcome`, `since`/`until` (RFC3339), and `limit`.
pub async fn audit(
    State(state): State<AppState>,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    let parse_ts = |k: &str| {
        params
            .get(k)
            .and_then(|v| chrono::DateTime::parse_from_rfc3339(v).ok())
            .map(|d| d.with_timezone(&chrono::Utc))
    };
    let query = crate::audit::AuditQuery {
        tenant_id: params.get("tenant_id").cloned(),
        tool_id: params.get("tool_id").cloned(),
        outcome: params.get("outcome").cloned(),
        since: parse_ts("since"),
        until: parse_ts("until"),
        limit: params.get("limit").and_then(|v| v.parse().ok()),
    };
    let records = state.audit.query(&query);
    Json(json!({ "count": records.len(), "records": records }))
}

/// `GET /tools/{tool_id}/health`: passive health for a single tool -- provider
/// name, circuit state, last success/failure timestamps, and the configured
/// rate limit.
pub async fn tool_health(
    State(state): State<AppState>,
    Path(tool_id): Path<String>,
) -> impl IntoResponse {
    let Some(provider) = state.registry.provider_of(&tool_id) else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("tool '{tool_id}' not found") })),
        )
            .into_response();
    };
    let health = state.circuits.health(provider);
    (
        StatusCode::OK,
        Json(json!({
            "tool_id": tool_id,
            "provider": provider,
            "circuit_state": health.circuit_state,
            "last_success_at": health.last_success_at,
            "last_failure_at": health.last_failure_at,
            "consecutive_failures": health.consecutive_failures,
            "rate_limit_per_min": state.rate_limiter.capacity(),
        })),
    )
        .into_response()
}

/// `GET /health/adapters`: passive health for every adapter, grouped by
/// provider. Circuit state is per-provider, so each provider group carries one
/// shared circuit snapshot plus the list of tool ids it covers.
pub async fn adapters_health(State(state): State<AppState>) -> impl IntoResponse {
    let mut by_provider: BTreeMap<&'static str, Vec<String>> = BTreeMap::new();
    for (tool_id, provider) in state.registry.tool_providers() {
        by_provider.entry(provider).or_default().push(tool_id);
    }

    let providers: Vec<_> = by_provider
        .into_iter()
        .map(|(provider, tools)| {
            let health = state.circuits.health(provider);
            json!({
                "provider": provider,
                "circuit_state": health.circuit_state,
                "last_success_at": health.last_success_at,
                "last_failure_at": health.last_failure_at,
                "consecutive_failures": health.consecutive_failures,
                "tools": tools,
            })
        })
        .collect();

    Json(json!({
        "rate_limit_per_min": state.rate_limiter.capacity(),
        "providers": providers,
    }))
    .into_response()
}

#[cfg(test)]
/// Tests for the transport-independent controlled invocation path.
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    use async_trait::async_trait;
    use serde_json::json;

    use crate::audit::{AuditQuery, AuditTrail};
    use crate::axon::AxonPublisher;
    use crate::circuit::CircuitRegistry;
    use crate::credd_client::CreddClient;
    use crate::metrics::MetricsRegistry;
    use crate::rate_limit::{RateLimitConfig, RateLimiter};
    use crate::tenant_config::{TenantAdapterConfig, TenantConfigStore};
    use crate::tool::{Tool, ToolSchema};
    use crate::ToolRegistry;

    /// A deterministic adapter that records how often Hermes reaches it.
    struct EchoTool {
        /// Shared invocation counter used to prove rejected calls never dispatch.
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    /// Tool implementation for the controlled-path tests.
    impl Tool for EchoTool {
        /// Describe the test adapter.
        fn schema(&self) -> ToolSchema {
            ToolSchema {
                tool_id: "test.echo".to_string(),
                name: "Echo".to_string(),
                description: "Return the supplied arguments".to_string(),
                input_schema: json!({"type": "object"}),
                output_schema: json!({"type": "object"}),
                category: "test".to_string(),
                requires_auth: false,
            }
        }

        /// Count and return one invocation.
        async fn invoke(&self, _context: &InvokeContext, request: InvokeRequest) -> InvokeResponse {
            self.calls.fetch_add(1, Ordering::SeqCst);
            InvokeResponse {
                tool_id: "test.echo".to_string(),
                success: true,
                result: Some(request.args),
                error: None,
                duration_ms: 0,
            }
        }

        /// Isolate the adapter under a stable provider key.
        fn provider(&self) -> &'static str {
            "test"
        }
    }

    /// Construct complete in-memory Hermes state with a configurable rate limit.
    fn state(capacity: u32) -> (AppState, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let mut registry = ToolRegistry::new();
        registry.register(EchoTool {
            calls: calls.clone(),
        });
        let axon = AxonPublisher::from_env();
        let state = AppState {
            registry: Arc::new(registry),
            credd: Arc::new(CreddClient::new("http://127.0.0.1:1".to_string(), None)),
            rate_limiter: Arc::new(RateLimiter::new(RateLimitConfig {
                capacity,
                refill_per_sec: 0.0001,
            })),
            circuits: Arc::new(CircuitRegistry::new()),
            metrics: Arc::new(MetricsRegistry::new()),
            audit: Arc::new(AuditTrail::new(axon.clone())),
            axon,
            tenant_config: Arc::new(TenantConfigStore::new()),
            public_url: None,
        };
        (state, calls)
    }

    /// A successful in-process call records metrics and a hashed audit entry.
    #[tokio::test]
    async fn controlled_success_records_observability() {
        let (state, calls) = state(10);
        let outcome = invoke_controlled(
            &state,
            "test.echo",
            InvokeRequest {
                tenant_id: Some("tenant-a".to_string()),
                args: json!({"value": 7}),
            },
        )
        .await;

        assert_eq!(outcome.status, StatusCode::OK);
        assert!(outcome.response.success);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let metrics = state.metrics.snapshot(state.circuits.open_count());
        assert_eq!(metrics["global"]["total_invocations"], 1);
        let records = state.audit.query(&AuditQuery::default());
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].tool_id, "test.echo");
        assert_eq!(records[0].tenant_id.as_deref(), Some("tenant-a"));
    }

    /// Tenant disablement fails closed before the adapter is reached.
    #[tokio::test]
    async fn controlled_invocation_enforces_tenant_disablement() {
        let (state, calls) = state(10);
        state.tenant_config.set(
            "tenant-a",
            "test",
            TenantAdapterConfig {
                enabled: false,
                rate_limit_override: None,
                default_args: None,
            },
        );

        let outcome = invoke_controlled(
            &state,
            "test.echo",
            InvokeRequest {
                tenant_id: Some("tenant-a".to_string()),
                args: json!({}),
            },
        )
        .await;

        assert_eq!(outcome.status, StatusCode::FORBIDDEN);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(
            outcome
                .response
                .error
                .as_ref()
                .and_then(|error| error["code"].as_str()),
            Some("adapter_disabled")
        );
    }

    /// A depleted tenant/tool bucket returns retry metadata without dispatching twice.
    #[tokio::test]
    async fn controlled_invocation_enforces_rate_limit() {
        let (state, calls) = state(1);
        let request = InvokeRequest {
            tenant_id: Some("tenant-a".to_string()),
            args: json!({}),
        };
        let first = invoke_controlled(&state, "test.echo", request.clone()).await;
        let second = invoke_controlled(&state, "test.echo", request).await;

        assert_eq!(first.status, StatusCode::OK);
        assert_eq!(second.status, StatusCode::TOO_MANY_REQUESTS);
        assert!(second.retry_after_secs.is_some());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let metrics = state.metrics.snapshot(state.circuits.open_count());
        assert_eq!(metrics["providers"]["test"]["rate_limited_count"], 1);
    }
}
