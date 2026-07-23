//! Henosis-owned, side-effect-free adapters used to verify the governed path.

use async_trait::async_trait;
use serde_json::json;

use crate::tool::{InvokeContext, InvokeRequest, InvokeResponse, Tool, ToolSchema};

/// Stable identifier for the deterministic Henosis readiness probe.
const TOOL_ID: &str = "henosis.probe";
/// Provider key used to group the probe's health and audit records.
const PROVIDER: &str = "henosis";

/// Return a deterministic readiness response without network or credential access.
pub struct HenosisProbeTool;

#[async_trait]
/// Implements the Hermes tool contract for the local Henosis readiness probe.
impl Tool for HenosisProbeTool {
    /// Return the public, no-argument schema for the readiness probe.
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            tool_id: TOOL_ID.to_string(),
            name: "Henosis Readiness Probe".to_string(),
            description: "Return the deterministic readiness state of the Henosis runtime."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {}
            }),
            output_schema: json!({
                "type": "object",
                "required": ["status", "runtime"],
                "properties": {
                    "status": { "type": "string", "enum": ["ready"] },
                    "runtime": { "type": "string", "enum": ["henosis"] }
                }
            }),
            category: "system".to_string(),
            requires_auth: false,
        }
    }

    /// Return the local Henosis provider key for health, audit, and policy grouping.
    fn provider(&self) -> &'static str {
        PROVIDER
    }

    /// Return the stable readiness result without inspecting credentials or performing I/O.
    async fn invoke(&self, _context: &InvokeContext, _request: InvokeRequest) -> InvokeResponse {
        InvokeResponse {
            tool_id: TOOL_ID.to_string(),
            success: true,
            result: Some(json!({ "status": "ready", "runtime": "henosis" })),
            error: None,
            duration_ms: 0,
        }
    }
}

#[cfg(test)]
/// Contains focused tests for the deterministic Henosis readiness probe.
mod tests {
    use std::sync::Arc;

    use serde_json::json;

    use super::*;
    use crate::circuit::{invoke_with_circuit, CircuitRegistry};
    use crate::phylaxd_client::PhylaxdClient;
    use crate::registry::build_registry;
    use crate::tool::{ProviderBases, Tool};

    /// Build the minimal context needed to prove this local adapter does not call phylaxd.
    fn context() -> InvokeContext {
        InvokeContext {
            phylaxd: Arc::new(PhylaxdClient::new("http://127.0.0.1:1".to_string(), None)),
            bases: ProviderBases::default(),
            hermes_public_url: None,
        }
    }

    #[test]
    /// Verifies the registry-visible schema is side-effect-free and closed to unknown arguments.
    fn schema_is_closed_and_does_not_require_authentication() {
        let schema = HenosisProbeTool.schema();

        assert_eq!(schema.tool_id, TOOL_ID);
        assert_eq!(schema.category, "system");
        assert!(!schema.requires_auth);
        assert_eq!(schema.input_schema["type"], "object");
        assert_eq!(schema.input_schema["additionalProperties"], false);
        assert_eq!(schema.input_schema["properties"], json!({}));
    }

    #[test]
    /// Verifies the bundled registry exposes the probe under its stable identifier.
    fn build_registry_contains_probe() {
        let registry = build_registry();
        let schema = registry
            .get(TOOL_ID)
            .expect("the bundled registry must contain the Henosis probe")
            .schema();

        assert_eq!(schema.tool_id, TOOL_ID);
        assert_eq!(registry.provider_of(TOOL_ID), Some(PROVIDER));
    }

    #[tokio::test]
    /// Verifies direct invocation returns the stable local readiness result without I/O.
    async fn invoke_returns_deterministic_readiness() {
        let response = HenosisProbeTool
            .invoke(
                &context(),
                InvokeRequest {
                    tenant_id: None,
                    args: json!({}),
                },
            )
            .await;

        assert!(response.success);
        assert_eq!(response.tool_id, TOOL_ID);
        assert_eq!(
            response.result,
            Some(json!({ "status": "ready", "runtime": "henosis" }))
        );
        assert_eq!(response.error, None);
    }

    #[tokio::test]
    /// Verifies the shared controlled path rejects non-object and unknown probe arguments.
    async fn controlled_path_rejects_non_object_and_unknown_arguments() {
        let tool: Arc<dyn Tool> = Arc::new(HenosisProbeTool);
        let circuits = CircuitRegistry::new();

        for args in [json!("not-an-object"), json!({ "unexpected": true })] {
            let (response, retries) = invoke_with_circuit(
                &circuits,
                &tool,
                TOOL_ID,
                &context(),
                InvokeRequest {
                    tenant_id: None,
                    args,
                },
            )
            .await;

            assert!(!response.success);
            assert_eq!(retries, 0);
            assert_eq!(
                response
                    .error
                    .as_ref()
                    .and_then(|error| error["code"].as_str()),
                Some("validation_failed")
            );
        }
    }
}
