//! Production dispatcher executor for in-process Hermes tools and Phylax operations.

use std::sync::Arc;

use async_trait::async_trait;
use base64::Engine;
use henosis_hermes::{invoke_controlled, AppState as HermesState, InvokeRequest};
use henosis_phylax::{PhylaxStore, SignAlgo};
use serde_json::Value;
use syntheos_contracts::{RequestContext, ToolInvocation};
use syntheos_dispatch::{Executor, ExecutorError};

/// The live executor behind the canonical dispatcher.
///
/// Credential-authority operations stay inside Phylax. Every other invocation resolves to a
/// Hermes adapter by joining the contract's `tool` and `action` fields with a dot.
pub struct HenosisExecutor {
    /// Complete in-process Hermes runtime, including policy and observability controls.
    hermes: HermesState,
    /// Credential authority used for use-without-holding operations.
    phylax: Arc<PhylaxStore>,
}

/// Construction and routing helpers for [`HenosisExecutor`].
impl HenosisExecutor {
    /// Build the production executor from the environment and the live Phylax store.
    pub fn from_env(phylax: Arc<PhylaxStore>) -> Self {
        Self::new(HermesState::from_env(), phylax)
    }

    /// Build an executor from explicit components for integration tests and alternate runtimes.
    pub fn new(hermes: HermesState, phylax: Arc<PhylaxStore>) -> Self {
        Self { hermes, phylax }
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

    /// Execute one authorized Phylax use-without-holding operation.
    async fn execute_phylax(
        &self,
        context: &RequestContext,
        invocation: &ToolInvocation,
    ) -> Result<Value, ExecutorError> {
        let category = required_string(&invocation.args, "category")?;
        let name = required_string(&invocation.args, "name")?;
        match invocation.action.as_str() {
            "sign" => {
                let payload = required_string(&invocation.args, "payload")?;
                let algorithm = parse_algorithm(&invocation.args)?;
                let signature = self
                    .phylax
                    .resolve_sign(
                        &context.tenant,
                        &context.principal,
                        category,
                        name,
                        payload.as_bytes(),
                        algorithm,
                    )
                    .map_err(phylax_error)?;
                Ok(serde_json::json!({
                    "signature": base64::engine::general_purpose::STANDARD.encode(signature),
                    "algorithm": algorithm_token(algorithm),
                }))
            }
            "verify" => {
                let payload = required_string(&invocation.args, "payload")?;
                let signature = required_string(&invocation.args, "signature")?;
                let signature = base64::engine::general_purpose::STANDARD
                    .decode(signature)
                    .map_err(|error| {
                        ExecutorError::new(format!("invalid base64 signature: {error}"))
                    })?;
                let algorithm = parse_algorithm(&invocation.args)?;
                let valid = self
                    .phylax
                    .resolve_verify(
                        &context.tenant,
                        &context.principal,
                        category,
                        name,
                        payload.as_bytes(),
                        &signature,
                        algorithm,
                    )
                    .map_err(phylax_error)?;
                Ok(serde_json::json!({"valid": valid}))
            }
            "derive" => {
                let purpose = required_string(&invocation.args, "purpose")?;
                let length = invocation
                    .args
                    .get("length")
                    .and_then(Value::as_u64)
                    .ok_or_else(|| {
                        ExecutorError::new("Phylax argument 'length' must be an unsigned integer")
                    })?;
                let length = usize::try_from(length)
                    .map_err(|_| ExecutorError::new("Phylax argument 'length' is too large"))?;
                let derived = self
                    .phylax
                    .resolve_derive(
                        &context.tenant,
                        &context.principal,
                        category,
                        name,
                        purpose,
                        length,
                    )
                    .map_err(phylax_error)?;
                Ok(serde_json::json!({
                    "derived": base64::engine::general_purpose::STANDARD.encode(derived),
                }))
            }
            "exec" => {
                let argv = invocation
                    .args
                    .get("argv")
                    .and_then(Value::as_array)
                    .ok_or_else(|| ExecutorError::new("Phylax argument 'argv' must be an array"))?
                    .iter()
                    .map(|value| {
                        value.as_str().map(str::to_string).ok_or_else(|| {
                            ExecutorError::new("every Phylax 'argv' item must be a string")
                        })
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                let env_var = required_string(&invocation.args, "env_var")?;
                let outcome = self
                    .phylax
                    .resolve_exec(
                        &context.tenant,
                        &context.principal,
                        category,
                        name,
                        &argv,
                        env_var,
                    )
                    .await
                    .map_err(phylax_error)?;
                Ok(serde_json::json!({
                    "timed_out": outcome.timed_out,
                    "exit_code": outcome.exit_code,
                    "stdout": base64::engine::general_purpose::STANDARD.encode(outcome.stdout),
                    "stderr": base64::engine::general_purpose::STANDARD.encode(outcome.stderr),
                }))
            }
            action => Err(ExecutorError::new(format!(
                "unknown Phylax action: {action}"
            ))),
        }
    }
}

#[async_trait]
/// Execute authorized contract invocations against the in-process authorities.
impl Executor for HenosisExecutor {
    /// Route the invocation to Phylax or Hermes after the full gate chain has authorized it.
    async fn execute(
        &self,
        context: &RequestContext,
        invocation: &ToolInvocation,
    ) -> Result<Value, ExecutorError> {
        if invocation.tool == "phylax" {
            self.execute_phylax(context, invocation).await
        } else {
            self.execute_hermes(context, invocation).await
        }
    }
}

/// Read a required string argument from an invocation payload.
fn required_string<'a>(args: &'a Value, key: &str) -> Result<&'a str, ExecutorError> {
    args.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| ExecutorError::new(format!("Phylax argument '{key}' must be a string")))
}

/// Parse the optional signing algorithm, defaulting to HMAC-SHA256.
fn parse_algorithm(args: &Value) -> Result<SignAlgo, ExecutorError> {
    let token = args
        .get("algorithm")
        .and_then(Value::as_str)
        .unwrap_or("hmac-sha256");
    SignAlgo::parse(token)
        .ok_or_else(|| ExecutorError::new(format!("unsupported Phylax algorithm: {token}")))
}

/// Return the wire token for a Phylax signing algorithm.
fn algorithm_token(algorithm: SignAlgo) -> &'static str {
    match algorithm {
        SignAlgo::HmacSha256 => "hmac-sha256",
        SignAlgo::Ed25519 => "ed25519",
    }
}

/// Convert a Phylax failure into the dispatcher executor boundary.
fn phylax_error(error: henosis_phylax::PhylaxError) -> ExecutorError {
    ExecutorError::new(format!("Phylax execution failed: {error}"))
}

#[cfg(test)]
/// Tests for the production executor boundary.
mod tests {
    use super::*;
    use henosis_hermes::{
        audit::AuditTrail,
        axon::AxonPublisher,
        circuit::CircuitRegistry,
        credd_client::CreddClient,
        metrics::MetricsRegistry,
        rate_limit::{RateLimitConfig, RateLimiter},
        tenant_config::TenantConfigStore,
        InvokeContext, InvokeResponse, Tool, ToolRegistry, ToolSchema,
    };
    use henosis_phylax::{ResolveMode, SecretData};
    use syntheos_axon::AxonBus;

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

    /// Construct an executor and identities over an in-memory Phylax store.
    fn executor() -> (HenosisExecutor, Arc<PhylaxStore>, RequestContext) {
        let bus = Arc::new(AxonBus::new());
        let phylax = Arc::new(
            PhylaxStore::open_in_memory(bus, *henosis_phylax::crypto::generate_key())
                .expect("phylax"),
        );
        let mut registry = ToolRegistry::new();
        registry.register(EchoTool);
        let axon = AxonPublisher::from_env();
        let hermes = HermesState {
            registry: Arc::new(registry),
            credd: Arc::new(CreddClient::new("http://127.0.0.1:1".to_string(), None)),
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
        let executor = HenosisExecutor::new(hermes, phylax.clone());
        let context = RequestContext {
            tenant: syntheos_contracts::TenantId::new(),
            principal: syntheos_contracts::PrincipalId::new(),
            persona: None,
            session: None,
            room: None,
            task: None,
            workflow: None,
        };
        (executor, phylax, context)
    }

    /// A contract invocation resolves to and executes its dotted Hermes adapter ID.
    #[tokio::test]
    async fn dotted_hermes_tool_executes() {
        let (executor, _phylax, context) = executor();
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
        let (executor, _phylax, context) = executor();
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

    /// An authorized sign operation executes without returning the stored secret.
    #[tokio::test]
    async fn phylax_sign_executes_without_secret_material() {
        let (executor, phylax, context) = executor();
        phylax
            .store_secret(
                &context.tenant,
                &context.principal,
                "test",
                "signing",
                &SecretData::Note {
                    content: "never-return-this".to_string(),
                },
            )
            .expect("secret");
        phylax
            .create_policy(
                &context.tenant,
                Some(&context.principal),
                Some("test"),
                Some("signing"),
                &[ResolveMode::Sign],
                None,
            )
            .expect("policy");

        let result = executor
            .execute(
                &context,
                &ToolInvocation {
                    tool: "phylax".to_string(),
                    action: "sign".to_string(),
                    args: serde_json::json!({
                        "category": "test",
                        "name": "signing",
                        "payload": "hello"
                    }),
                },
            )
            .await
            .expect("sign");
        assert!(result["signature"].as_str().is_some());
        assert!(!result.to_string().contains("never-return-this"));
    }
}
