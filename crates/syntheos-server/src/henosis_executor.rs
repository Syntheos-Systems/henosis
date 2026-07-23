//! Production dispatcher executor for in-process Hermes tools and phylaxd operations.

use std::sync::Arc;

use async_trait::async_trait;
use henosis_hermes::phylaxd_client::{PhylaxdClient, PhylaxdError};
use henosis_hermes::{invoke_controlled, AppState as HermesState, InvokeRequest};
use serde_json::Value;
use syntheos_contracts::{RequestContext, ToolInvocation};
use syntheos_dispatch::{Executor, ExecutorError};

/// Maximum accepted credential slot or algorithm token length.
const MAX_TOKEN_LEN: usize = 128;
/// Maximum accepted base64 payload or signature length.
const MAX_B64_LEN: usize = 2 * 1024 * 1024;
/// Maximum number of arguments in a broker-mediated command.
const MAX_ARGV_ITEMS: usize = 64;
/// Maximum length of one broker-mediated command argument.
const MAX_ARG_LEN: usize = 4096;

/// The live executor behind the canonical dispatcher.
///
/// Credential operations are delegated to phylaxd over its authenticated API.
/// Every other invocation resolves to a Hermes adapter by joining the contract's
/// `tool` and `action` fields with a dot.
pub struct HenosisExecutor {
    /// Complete in-process Hermes runtime, including policy and observability controls.
    hermes: HermesState,
    /// Credential broker used only for non-plaintext mediated operations.
    phylaxd: Arc<PhylaxdClient>,
}

/// Construction and routing helpers for [`HenosisExecutor`].
impl HenosisExecutor {
    /// Build the production executor from process environment configuration.
    pub fn from_env() -> Self {
        Self::new(HermesState::from_env())
    }

    /// Build an executor from explicit Hermes state for tests and alternate runtimes.
    pub fn new(hermes: HermesState) -> Self {
        let phylaxd = hermes.phylaxd.clone();
        Self { hermes, phylaxd }
    }

    /// Invoke a Hermes adapter through the full shared control and observability path.
    async fn execute_hermes(
        &self,
        context: &RequestContext,
        invocation: &ToolInvocation,
    ) -> Result<Value, ExecutorError> {
        let tool_id = format!("{}.{}", invocation.tool, invocation.action);
        let outcome = invoke_controlled(
            &self.hermes,
            &tool_id,
            InvokeRequest {
                tenant_id: Some(context.tenant.to_string()),
                args: invocation.args.clone(),
            },
        )
        .await;
        let response = outcome.response;
        if response.success {
            return Ok(response.result.unwrap_or(Value::Null));
        }
        Err(ExecutorError::new(
            response
                .error
                .as_ref()
                .and_then(|error| error.get("message"))
                .and_then(Value::as_str)
                .map(str::to_string)
                .or_else(|| response.error.map(|error| error.to_string()))
                .unwrap_or_else(|| format!("Hermes tool failed: {tool_id}")),
        ))
    }

    /// Execute one authorized use-without-holding operation through phylaxd.
    async fn execute_phylaxd(
        &self,
        context: &RequestContext,
        invocation: &ToolInvocation,
    ) -> Result<Value, ExecutorError> {
        let category = required_bounded_string(&invocation.args, "category", MAX_TOKEN_LEN)?;
        let name = required_bounded_string(&invocation.args, "name", MAX_TOKEN_LEN)?;
        let tenant_slot = context.tenant.to_string();
        if name != tenant_slot.as_str() {
            return Err(ExecutorError::new(
                "credential slot does not belong to the authenticated tenant",
            ));
        }
        match invocation.action.as_str() {
            "sign" => {
                let payload_b64 =
                    required_bounded_string(&invocation.args, "payload_b64", MAX_B64_LEN)?;
                let algorithm = signing_algorithm(&invocation.args)?;
                let result = self
                    .phylaxd
                    .sign(category, name, payload_b64, algorithm)
                    .await
                    .map_err(phylaxd_error)?;
                Ok(serde_json::json!({
                    "signature_b64": result.signature_b64,
                    "algorithm": algorithm,
                }))
            }
            "verify" => {
                let payload_b64 =
                    required_bounded_string(&invocation.args, "payload_b64", MAX_B64_LEN)?;
                let signature_b64 =
                    required_bounded_string(&invocation.args, "signature_b64", MAX_B64_LEN)?;
                let algorithm = signing_algorithm(&invocation.args)?;
                let result = self
                    .phylaxd
                    .verify(category, name, payload_b64, signature_b64, algorithm)
                    .await
                    .map_err(phylaxd_error)?;
                Ok(serde_json::json!({"valid": result.valid}))
            }
            "derive" => {
                let purpose = required_bounded_string(&invocation.args, "purpose", MAX_TOKEN_LEN)?;
                let length = invocation
                    .args
                    .get("length")
                    .and_then(Value::as_u64)
                    .ok_or_else(|| {
                        ExecutorError::new("phylaxd argument 'length' must be an unsigned integer")
                    })?;
                let length = usize::try_from(length)
                    .map_err(|_| ExecutorError::new("phylaxd argument 'length' is too large"))?;
                if !(1..=64).contains(&length) {
                    return Err(ExecutorError::new(
                        "phylaxd argument 'length' must be between 1 and 64",
                    ));
                }
                let result = self
                    .phylaxd
                    .derive(category, name, purpose, length)
                    .await
                    .map_err(phylaxd_error)?;
                Ok(serde_json::json!({"derived_b64": result.derived_b64}))
            }
            "exec" => {
                let argv = command_arguments(&invocation.args)?;
                let env_var = required_bounded_string(&invocation.args, "env_var", MAX_TOKEN_LEN)?;
                if !valid_env_var(env_var) {
                    return Err(ExecutorError::new(
                        "phylaxd argument 'env_var' is not a valid environment variable name",
                    ));
                }
                let result = self
                    .phylaxd
                    .exec(category, name, &argv, env_var)
                    .await
                    .map_err(phylaxd_error)?;
                Ok(serde_json::json!({
                    "timed_out": result.timed_out,
                    "exit_code": result.exit_code,
                    "stdout_b64": result.stdout_b64,
                    "stderr_b64": result.stderr_b64,
                }))
            }
            action => Err(ExecutorError::new(format!(
                "unknown phylaxd action: {action}"
            ))),
        }
    }
}

#[async_trait]
/// Execute authorized contract invocations against the in-process tool gateway.
impl Executor for HenosisExecutor {
    /// Route the invocation to phylaxd or Hermes after every authority has allowed it.
    async fn execute(
        &self,
        context: &RequestContext,
        invocation: &ToolInvocation,
    ) -> Result<Value, ExecutorError> {
        if invocation.tool == "phylaxd" {
            self.execute_phylaxd(context, invocation).await
        } else {
            self.execute_hermes(context, invocation).await
        }
    }
}

/// Read a required non-empty bounded string argument from an invocation payload.
fn required_bounded_string<'a>(
    args: &'a Value,
    key: &str,
    maximum: usize,
) -> Result<&'a str, ExecutorError> {
    let value = args
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| ExecutorError::new(format!("phylaxd argument '{key}' must be a string")))?;
    if value.is_empty() || value.len() > maximum {
        return Err(ExecutorError::new(format!(
            "phylaxd argument '{key}' must contain 1 to {maximum} bytes"
        )));
    }
    Ok(value)
}

/// Parse and validate the signing algorithm token.
fn signing_algorithm(args: &Value) -> Result<&str, ExecutorError> {
    let algorithm = args
        .get("algorithm")
        .and_then(Value::as_str)
        .unwrap_or("hmac-sha256");
    match algorithm {
        "hmac-sha256" | "ed25519" => Ok(algorithm),
        _ => Err(ExecutorError::new(
            "phylaxd argument 'algorithm' must be hmac-sha256 or ed25519",
        )),
    }
}

/// Parse a bounded direct-exec argument vector.
fn command_arguments(args: &Value) -> Result<Vec<String>, ExecutorError> {
    let values = args
        .get("argv")
        .and_then(Value::as_array)
        .ok_or_else(|| ExecutorError::new("phylaxd argument 'argv' must be an array"))?;
    if values.is_empty() || values.len() > MAX_ARGV_ITEMS {
        return Err(ExecutorError::new(format!(
            "phylaxd argument 'argv' must contain 1 to {MAX_ARGV_ITEMS} items"
        )));
    }
    let mut argv = Vec::with_capacity(values.len());
    for value in values {
        let item = value
            .as_str()
            .ok_or_else(|| ExecutorError::new("every phylaxd 'argv' item must be a string"))?;
        if item.is_empty() || item.len() > MAX_ARG_LEN {
            return Err(ExecutorError::new(format!(
                "every phylaxd 'argv' item must contain 1 to {MAX_ARG_LEN} bytes"
            )));
        }
        argv.push(item.to_string());
    }
    if !argv[0].starts_with('/') {
        return Err(ExecutorError::new(
            "phylaxd argument 'argv[0]' must be an absolute path",
        ));
    }
    Ok(argv)
}

/// Validate a POSIX-shaped environment variable name.
fn valid_env_var(name: &str) -> bool {
    let mut characters = name.chars();
    matches!(
        characters.next(),
        Some(character) if character.is_ascii_alphabetic() || character == '_'
    ) && characters.all(|character| character.is_ascii_alphanumeric() || character == '_')
}

/// Convert a broker failure into a non-secret dispatcher error.
fn phylaxd_error(error: PhylaxdError) -> ExecutorError {
    let message = match error {
        PhylaxdError::AuthMissing => "phylaxd authentication is not configured".to_string(),
        PhylaxdError::TenantNotAuthorized { .. } => {
            "phylaxd rejected the credential operation".to_string()
        }
        PhylaxdError::Unreachable { .. } => "phylaxd is unavailable".to_string(),
        PhylaxdError::Upstream { status, .. } => {
            format!("phylaxd rejected the credential operation with status {status}")
        }
        PhylaxdError::MalformedResponse => {
            "phylaxd returned an invalid operation response".to_string()
        }
    };
    ExecutorError::new(message)
}

#[cfg(test)]
/// Tests for the production executor boundary.
mod tests {
    use super::*;
    use henosis_hermes::{
        audit::AuditTrail,
        axon::AxonPublisher,
        circuit::CircuitRegistry,
        metrics::MetricsRegistry,
        rate_limit::{RateLimitConfig, RateLimiter},
        tenant_config::TenantConfigStore,
        InvokeContext, InvokeResponse, Tool, ToolRegistry, ToolSchema,
    };

    /// A deterministic Hermes tool used to prove in-process adapter execution.
    struct EchoTool;

    #[async_trait]
    /// Hermes tool implementation for [`EchoTool`].
    impl Tool for EchoTool {
        /// Describe the test echo adapter.
        fn schema(&self) -> ToolSchema {
            ToolSchema {
                tool_id: "test.echo".to_string(),
                name: "Echo".to_string(),
                description: "Return the supplied arguments".to_string(),
                input_schema: serde_json::json!({"type": "object"}),
                output_schema: serde_json::json!({"type": "object"}),
                category: "test".to_string(),
                requires_auth: false,
            }
        }

        /// Return the request arguments without reaching a provider.
        async fn invoke(&self, _context: &InvokeContext, request: InvokeRequest) -> InvokeResponse {
            InvokeResponse {
                tool_id: "test.echo".to_string(),
                success: true,
                result: Some(request.args),
                error: None,
                duration_ms: 0,
            }
        }

        /// Keep the echo adapter on an isolated circuit.
        fn provider(&self) -> &'static str {
            "test"
        }
    }

    /// Construct an executor and request identity over deterministic in-memory state.
    fn executor() -> (HenosisExecutor, RequestContext) {
        let mut registry = ToolRegistry::new();
        registry.register(EchoTool);
        let axon = AxonPublisher::from_env();
        let hermes = HermesState {
            registry: Arc::new(registry),
            phylaxd: Arc::new(PhylaxdClient::new("http://127.0.0.1:1".to_string(), None)),
            rate_limiter: Arc::new(RateLimiter::new(RateLimitConfig {
                capacity: 60,
                refill_per_sec: 1.0,
            })),
            circuits: Arc::new(CircuitRegistry::new()),
            metrics: Arc::new(MetricsRegistry::new()),
            audit: Arc::new(AuditTrail::new(axon.clone())),
            axon,
            tenant_config: Arc::new(TenantConfigStore::new()),
            public_url: None,
        };
        let executor = HenosisExecutor::new(hermes);
        let context = RequestContext {
            tenant: syntheos_contracts::TenantId::new(),
            principal: syntheos_contracts::PrincipalId::new(),
            persona: None,
            session: None,
            room: None,
            task: None,
            workflow: None,
            authority: None,
        };
        (executor, context)
    }

    /// A contract invocation resolves to and executes its dotted Hermes adapter ID.
    #[tokio::test]
    async fn dotted_hermes_tool_executes() {
        let (executor, context) = executor();
        let result = executor
            .execute(
                &context,
                &ToolInvocation {
                    tool: "test".to_string(),
                    action: "echo".to_string(),
                    args: serde_json::json!({"value": 7}),
                },
            )
            .await
            .expect("execute");
        assert_eq!(result, serde_json::json!({"value": 7}));
    }

    /// Unknown Hermes IDs fail closed at the executor boundary.
    #[tokio::test]
    async fn unknown_hermes_tool_fails_closed() {
        let (executor, context) = executor();
        let error = executor
            .execute(
                &context,
                &ToolInvocation {
                    tool: "missing".to_string(),
                    action: "tool".to_string(),
                    args: serde_json::json!({}),
                },
            )
            .await
            .expect_err("unknown tool");
        assert!(error.to_string().contains("missing.tool"));
    }

    /// Invalid derive bounds fail before a request can reach phylaxd.
    #[tokio::test]
    async fn phylaxd_derive_bounds_fail_closed() {
        let (executor, context) = executor();
        let error = executor
            .execute(
                &context,
                &ToolInvocation {
                    tool: "phylaxd".to_string(),
                    action: "derive".to_string(),
                    args: serde_json::json!({
                        "category": "test",
                        "name": context.tenant.to_string(),
                        "purpose": "session",
                        "length": 65
                    }),
                },
            )
            .await
            .expect_err("oversized derive");
        assert!(error.to_string().contains("between 1 and 64"));
    }

    /// The executor rejects another tenant's slot before contacting phylaxd.
    #[tokio::test]
    async fn phylaxd_cross_tenant_slot_fails_closed() {
        let (executor, context) = executor();
        let error = executor
            .execute(
                &context,
                &ToolInvocation {
                    tool: "phylaxd".to_string(),
                    action: "derive".to_string(),
                    args: serde_json::json!({
                        "category": "test",
                        "name": syntheos_contracts::TenantId::new().to_string(),
                        "purpose": "session",
                        "length": 32
                    }),
                },
            )
            .await
            .expect_err("cross-tenant slot");
        assert!(error
            .to_string()
            .contains("does not belong to the authenticated tenant"));
    }
}
