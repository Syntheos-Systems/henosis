//! MCP (Model Context Protocol) bridge.
//!
//! Exposes the same `ToolRegistry` over JSON-RPC 2.0 at `POST /mcp` so
//! third-party MCP clients (Claude Desktop, Cursor) can invoke Hermes
//! tools. Outbound-only: nothing in the rest of Hermes calls MCP.
//!
//! Methods implemented (subset of the MCP 2024-11-05 spec):
//!   - initialize             -- handshake
//!   - tools/list             -- enumerate the registry
//!   - tools/call             -- invoke a tool by id
//!
//! Tenant binding: MCP clients have no concept of multi-tenancy, so the
//! bridge looks for `params.arguments._tenant_id` (or `_tenant`) on
//! tools/call. If absent, requests are routed under `_anon` -- the same
//! tenant the rate limiter uses for unauthenticated invokes.
//!
//! Gated by HERMES_MCP_ENABLED=true; off by default.

use std::time::Instant;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use serde_json::{json, Value};

use crate::circuit::invoke_with_circuit;
use crate::rate_limit::CheckOutcome;
use crate::tool::{InvokeContext, InvokeRequest};
use crate::AppState;

/// MCP protocol version string advertised in the `initialize` response.
const PROTOCOL_VERSION: &str = "2024-11-05";

/// Service name embedded in the `initialize` server info.
const SERVER_NAME: &str = "hermes";

/// Return `true` when `HERMES_MCP_ENABLED` is set to a truthy value. Off by
/// default so the bridge must be explicitly opted into.
pub fn is_enabled() -> bool {
    std::env::var("HERMES_MCP_ENABLED")
        .map(|v| v != "0" && !v.eq_ignore_ascii_case("false"))
        .unwrap_or(false)
}

/// `POST /mcp`: JSON-RPC 2.0 dispatcher. Routes `initialize`, `tools/list`,
/// `tools/call`, and `ping`; all other methods return a `-32601` error.
pub async fn jsonrpc_handler(
    State(state): State<AppState>,
    Json(req): Json<Value>,
) -> impl IntoResponse {
    let id = req.get("id").cloned().unwrap_or(Value::Null);
    let method = req.get("method").and_then(|v| v.as_str()).unwrap_or("");
    let params = req
        .get("params")
        .cloned()
        .unwrap_or_else(|| Value::Object(Default::default()));

    let result = match method {
        "initialize" => handle_initialize(),
        "tools/list" => handle_tools_list(&state),
        "tools/call" => handle_tools_call(&state, &params).await,
        "ping" => Ok(json!({})),
        other => Err(JsonRpcError::method_not_found(other)),
    };

    match result {
        Ok(value) => {
            let resp = json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": value,
            });
            (StatusCode::OK, Json(resp))
        }
        Err(e) => {
            let resp = json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {
                    "code": e.code,
                    "message": e.message,
                    "data": e.data,
                }
            });
            (StatusCode::OK, Json(resp))
        }
    }
}

/// Handle `initialize`: return the protocol version, server info, and
/// capabilities.
fn handle_initialize() -> Result<Value, JsonRpcError> {
    Ok(json!({
        "protocolVersion": PROTOCOL_VERSION,
        "serverInfo": {
            "name": SERVER_NAME,
            "version": env!("CARGO_PKG_VERSION"),
        },
        "capabilities": {
            "tools": { "listChanged": false }
        }
    }))
}

/// Handle `tools/list`: return all registered tools with their schemas and
/// a `_hermes` extension block carrying provider + circuit state.
fn handle_tools_list(state: &AppState) -> Result<Value, JsonRpcError> {
    let tools: Vec<Value> = state
        .registry
        .list()
        .into_iter()
        .map(|s| {
            // `_hermes` carries adapter availability for clients that understand
            // it; standard MCP clients ignore the extra field.
            let provider = state.registry.provider_of(&s.tool_id).unwrap_or("unknown");
            let circuit_state = state.circuits.health(provider).circuit_state;
            json!({
                "name": s.tool_id,
                "description": s.description,
                "inputSchema": s.input_schema,
                "_hermes": {
                    "provider": provider,
                    "circuit_state": circuit_state,
                    "available": circuit_state != "open",
                },
            })
        })
        .collect();
    Ok(json!({ "tools": tools }))
}

/// Handle `tools/call`: look up the tool by name, apply tenant config and
/// rate limiting, dispatch through the circuit breaker, and map the result
/// to MCP's `content` shape.
async fn handle_tools_call(state: &AppState, params: &Value) -> Result<Value, JsonRpcError> {
    let name = params
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| JsonRpcError::invalid_params("missing required field: name"))?;
    let arguments = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| Value::Object(Default::default()));

    let tool = state
        .registry
        .get(name)
        .ok_or_else(|| JsonRpcError::invalid_params(&format!("tool '{name}' not found")))?;

    // MCP has no native tenant concept; allow callers to inject one via
    // either `_tenant_id` or `_tenant` and strip it before dispatch.
    let mut args_obj = match arguments {
        Value::Object(m) => m,
        _ => {
            return Err(JsonRpcError::invalid_params("arguments must be an object"));
        }
    };
    let tenant_id = args_obj
        .remove("_tenant_id")
        .or_else(|| args_obj.remove("_tenant"))
        .and_then(|v| v.as_str().map(String::from))
        .filter(|s| !s.is_empty());

    let provider = state.registry.provider_of(name).unwrap_or("unknown");

    // Per-tenant adapter config: reject a disabled provider, then merge tenant
    // default args (request wins) before dispatch -- parity with the HTTP path.
    let cfg = state
        .tenant_config
        .get(tenant_id.as_deref().unwrap_or("_anon"), provider);
    if !cfg.enabled {
        return Err(JsonRpcError {
            code: -32001,
            message: format!("provider '{provider}' is disabled for this tenant"),
            data: Some(json!({ "code": "adapter_disabled", "provider": provider })),
        });
    }
    let args_obj = match crate::tenant_config::merge_default_args(
        cfg.default_args.as_ref(),
        Value::Object(args_obj),
    ) {
        Value::Object(m) => m,
        other => {
            // merge_default_args preserves object inputs; this is unreachable in
            // practice but keeps the types total.
            return Err(JsonRpcError::invalid_params(&format!(
                "merged arguments are not an object: {other}"
            )));
        }
    };

    let tenant_for_limit = tenant_id.as_deref().unwrap_or("_anon");
    if let CheckOutcome::Throttled { retry_after_secs } = state
        .rate_limiter
        .check_with_capacity(tenant_for_limit, name, cfg.rate_limit_override)
        .await
    {
        state
            .metrics
            .record(provider, crate::metrics::Outcome::RateLimited, 0, 0);
        return Err(JsonRpcError {
            code: -32099,
            message: format!("rate limited; retry in ~{retry_after_secs}s"),
            data: Some(json!({"retry_after_secs": retry_after_secs})),
        });
    }

    let invoke = InvokeRequest {
        tenant_id: tenant_id.clone(),
        args: Value::Object(args_obj),
    };
    // Hash args before dispatch consumes the request (never store the args).
    let args_digest = crate::audit::args_hash(&invoke.args);
    let ctx = InvokeContext {
        credd: state.credd.clone(),
        bases: crate::tool::ProviderBases::default(),
        hermes_public_url: state.public_url.clone(),
    };
    let start = Instant::now();
    let (mut resp, retries) =
        invoke_with_circuit(&state.circuits, &tool, name, &ctx, invoke).await;
    resp.duration_ms = start.elapsed().as_millis() as u64;

    let outcome = crate::metrics::Outcome::classify(&resp);
    state
        .metrics
        .record(provider, outcome, resp.duration_ms, retries);
    let error_code = resp
        .error
        .as_ref()
        .and_then(|e| e.get("code"))
        .and_then(|c| c.as_str())
        .map(String::from);
    state.audit.record(
        tenant_id.clone(),
        name,
        provider,
        resp.duration_ms,
        outcome,
        error_code.clone(),
        retries,
        args_digest,
    );
    state
        .axon
        .tool_invoked(name, tenant_id.as_deref(), outcome.label(), resp.duration_ms);
    if outcome == crate::metrics::Outcome::Error {
        state.axon.tool_failed(name, error_code.as_deref(), retries);
    }

    // Map our InvokeResponse onto MCP's `content` shape. On success we
    // return a single text block with the JSON-encoded result; on error we
    // mark `isError: true` and surface the structured envelope.
    let payload = if resp.success {
        let body = resp
            .result
            .as_ref()
            .map(|v| serde_json::to_string(v).unwrap_or_default())
            .unwrap_or_default();
        json!({
            "content": [{ "type": "text", "text": body }],
            "isError": false,
        })
    } else {
        let err_body = resp
            .error
            .as_ref()
            .map(|v| serde_json::to_string(v).unwrap_or_default())
            .unwrap_or_default();
        json!({
            "content": [{ "type": "text", "text": err_body }],
            "isError": true,
        })
    };

    Ok(payload)
}

/// A JSON-RPC 2.0 error envelope.
#[derive(Debug)]
struct JsonRpcError {
    /// Numeric error code per the JSON-RPC spec.
    code: i64,
    /// Human-readable error message.
    message: String,
    /// Optional structured data attached to the error.
    data: Option<Value>,
}

impl JsonRpcError {
    /// `-32601 Method not found` error for an unknown method name.
    fn method_not_found(method: &str) -> Self {
        Self {
            code: -32601,
            message: format!("method not found: {method}"),
            data: None,
        }
    }

    /// `-32602 Invalid params` error with a descriptive message.
    fn invalid_params(msg: &str) -> Self {
        Self {
            code: -32602,
            message: msg.to_string(),
            data: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enabled_default_off() {
        std::env::remove_var("HERMES_MCP_ENABLED");
        assert!(!is_enabled());
    }

    #[test]
    fn enabled_when_true() {
        std::env::set_var("HERMES_MCP_ENABLED", "true");
        assert!(is_enabled());
        std::env::set_var("HERMES_MCP_ENABLED", "1");
        assert!(is_enabled());
        std::env::set_var("HERMES_MCP_ENABLED", "0");
        assert!(!is_enabled());
        std::env::set_var("HERMES_MCP_ENABLED", "false");
        assert!(!is_enabled());
        std::env::remove_var("HERMES_MCP_ENABLED");
    }
}
