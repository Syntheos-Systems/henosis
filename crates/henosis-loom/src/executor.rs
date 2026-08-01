//! The pluggable step-executor seam and the built-in pure-JSON transform executor.
//!
//! Kleos hardcoded transform/webhook/LLM execution inside `advance_run`. Here execution is a
//! trait: the engine asks the attached [`StepExecutor`] whether it handles a ready step's type
//! and runs it inline when it does; everything else stays `running` for external completion
//! via `complete_step`. The built-in [`TransformExecutor`] covers pure-JSON steps so a step
//! graph can run end-to-end.

use async_trait::async_trait;
use syntheos_contracts::{PrincipalId, RunId, TenantId};

use crate::model::StepType;

/// Everything an executor sees about the step it is asked to run.
#[derive(Debug)]
pub struct StepContext<'a> {
    /// The run the step belongs to.
    pub run_id: RunId,
    /// The tenant that owns the run, resolved by Loom rather than step input.
    pub tenant: TenantId,
    /// The principal that owns the run, resolved by Loom rather than step input.
    pub principal: PrincipalId,
    /// The step's run-internal id.
    pub step_id: i64,
    /// The step name.
    pub name: &'a str,
    /// What kind of work the step performs.
    pub step_type: StepType,
    /// Executor-specific configuration from the definition.
    pub config: &'a serde_json::Value,
    /// The merged input (run input overlaid with completed dependency outputs).
    pub input: &'a serde_json::Value,
    /// Per-attempt timeout in milliseconds (advisory to the executor; enforced by the sweep).
    pub timeout_ms: i64,
}

/// Executes steps inline during an advance pass.
///
/// `Ok(output)` completes the step; `Err(message)` fails the attempt, with the step's normal
/// retry budget applying. An executor must only claim types it can actually run -- a claimed
/// type that errors burns the step's retries.
#[async_trait]
pub trait StepExecutor: Send + Sync {
    /// Whether this executor runs steps of `step_type` inline.
    fn handles(&self, step_type: StepType) -> bool;

    /// Run the step to completion, returning its output object.
    async fn execute(&self, ctx: StepContext<'_>) -> Result<serde_json::Value, String>;
}

/// The built-in executor for [`StepType::Transform`]: pure JSON manipulation. Config selects
/// one mode:
///
/// - `{"mapping": {"target.path": "source.path", ...}}` -- dot-path remapping.
/// - `{"template": "text with {{var.path}}"}` (or an object of such strings) -- interpolation.
/// - neither -- pass the input through unchanged.
pub struct TransformExecutor;

#[async_trait]
/// Implements transform-step execution.
impl StepExecutor for TransformExecutor {
    /// Reports whether the requested type is a transform step.
    fn handles(&self, step_type: StepType) -> bool {
        step_type == StepType::Transform
    }

    /// Executes the configured transformation against the step input.
    async fn execute(&self, ctx: StepContext<'_>) -> Result<serde_json::Value, String> {
        if let Some(mapping) = ctx.config.get("mapping") {
            let mapping = mapping
                .as_object()
                .ok_or_else(|| "mapping must be an object".to_string())?;
            let mut output = serde_json::Map::new();
            for (target_path, source_path) in mapping {
                let source_path = source_path
                    .as_str()
                    .ok_or_else(|| format!("mapping value for {target_path:?} must be a string"))?;
                let value = resolve_dot_path(ctx.input, source_path);
                set_dot_path(&mut output, target_path, value);
            }
            Ok(serde_json::Value::Object(output))
        } else if let Some(template) = ctx.config.get("template") {
            match template {
                serde_json::Value::String(tmpl) => {
                    Ok(serde_json::Value::String(interpolate(tmpl, ctx.input)))
                }
                serde_json::Value::Object(tmpl_obj) => {
                    let mut output = serde_json::Map::new();
                    for (k, v) in tmpl_obj {
                        let rendered = if let serde_json::Value::String(s) = v {
                            serde_json::Value::String(interpolate(s, ctx.input))
                        } else {
                            v.clone()
                        };
                        output.insert(k.clone(), rendered);
                    }
                    Ok(serde_json::Value::Object(output))
                }
                _ => Ok(ctx.input.clone()),
            }
        } else {
            Ok(ctx.input.clone())
        }
    }
}

/// Resolve a dot-path like `foo.bar.baz` into a nested JSON value (`Null` when absent).
pub fn resolve_dot_path(obj: &serde_json::Value, path: &str) -> serde_json::Value {
    let mut current = obj;
    for key in path.split('.') {
        match current {
            serde_json::Value::Object(map) => match map.get(key) {
                Some(val) => current = val,
                None => return serde_json::Value::Null,
            },
            _ => return serde_json::Value::Null,
        }
    }
    current.clone()
}

/// Set a dot-path like `foo.bar` on a JSON object map, creating (or overwriting non-object)
/// intermediate objects.
pub fn set_dot_path(
    obj: &mut serde_json::Map<String, serde_json::Value>,
    path: &str,
    value: serde_json::Value,
) {
    match path.split_once('.') {
        None => {
            obj.insert(path.to_string(), value);
        }
        Some((head, rest)) => {
            let entry = obj
                .entry(head.to_string())
                .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
            if let serde_json::Value::Object(inner) = entry {
                set_dot_path(inner, rest, value);
            } else {
                let mut inner = serde_json::Map::new();
                set_dot_path(&mut inner, rest, value);
                obj.insert(head.to_string(), serde_json::Value::Object(inner));
            }
        }
    }
}

/// The dispatch seam that the composition layer (syntheos-server) implements to connect a
/// Hephaestus step to the in-process Hephaestus executor.
///
/// Defined here in the kernel crate so [`HephaestusStepExecutor`] can live in henosis-loom
/// without a compile-time dependency on henosis-hephaestus. The real implementation
/// (`HephaestusRuntimeDispatch` in syntheos-server) holds the hephaestus `AppState` and
/// calls `run_task_to_completion` from henosis-hephaestus. Tests use a fake implementation.
#[async_trait]
pub trait HephaestusDispatch: Send + Sync {
    /// Submit `input` to the Hephaestus executor and await a terminal result.
    ///
    /// Returns the step output JSON on success, or an error message on failure.
    /// The `input` is the merged step payload: `StepContext::input` overlaid with
    /// `StepContext::config` (config keys win). The dispatch implementation extracts
    /// the `"input"` string key as the agent task prompt; other keys map to
    /// `CreateTaskBody` fields (`agent`, `project`, `system`, etc.).
    async fn run(
        &self,
        tenant: TenantId,
        principal: PrincipalId,
        input: serde_json::Value,
    ) -> Result<serde_json::Value, String>;
}

/// Inline executor for [`StepType::Hephaestus`] steps.
///
/// Delegates to the [`HephaestusDispatch`] implementation supplied at construction time.
/// In production the dispatch is `HephaestusRuntimeDispatch` (syntheos-server); in tests
/// it is a fake implementation that avoids the LLM call entirely.
///
/// `execute` merges the step config over the step input (config keys win) and forwards the
/// combined payload to the dispatch. The config can pin the agent prompt with
/// `{"input": "..."}` or supply `agent`, `project`, `system` overrides.
pub struct HephaestusStepExecutor<D: HephaestusDispatch> {
    /// The Hephaestus dispatch implementation (real or fake).
    dispatch: D,
}

/// Methods for `HephaestusStepExecutor`.
impl<D: HephaestusDispatch> HephaestusStepExecutor<D> {
    /// Wrap a dispatch implementation.
    pub fn new(dispatch: D) -> Self {
        Self { dispatch }
    }
}

#[async_trait]
/// Implements Hephaestus step dispatch.
impl<D: HephaestusDispatch + 'static> StepExecutor for HephaestusStepExecutor<D> {
    /// Claims only [`StepType::Hephaestus`] steps.
    fn handles(&self, step_type: StepType) -> bool {
        step_type == StepType::Hephaestus
    }

    /// Merge config over input and forward to the dispatch; the result becomes the step output.
    async fn execute(&self, ctx: StepContext<'_>) -> Result<serde_json::Value, String> {
        // Start from the step input (run input overlaid with dependency outputs).
        let mut payload = match ctx.input {
            serde_json::Value::Object(map) => map.clone(),
            other => {
                // Non-object input: wrap it under "input" so the dispatch can find it.
                let mut m = serde_json::Map::new();
                m.insert("input".to_string(), other.clone());
                m
            }
        };
        // Overlay config keys (config wins on overlap, so the step definition can
        // pin specific fields such as the agent prompt or the target project).
        if let Some(cfg_obj) = ctx.config.as_object() {
            for (k, v) in cfg_obj {
                payload.insert(k.clone(), v.clone());
            }
        }
        // Scope is supplied out of band from the owning run. Remove payload aliases so a
        // workflow definition cannot create two conflicting notions of task authority.
        payload.remove("tenant_id");
        payload.remove("principal_id");
        self.dispatch
            .run(
                ctx.tenant,
                ctx.principal,
                serde_json::Value::Object(payload),
            )
            .await
    }
}

/// A composite executor that delegates to the first member whose [`StepExecutor::handles`]
/// returns true.
///
/// Used in the composition layer (syntheos-server) to bundle the built-in
/// [`TransformExecutor`] with a [`HephaestusStepExecutor`] without replacing either.
/// Ordering matters: the first executor that claims a type wins. An unclaimed type is
/// left for external completion (the existing Loom behavior).
pub struct CompositeStepExecutor {
    /// Ordered executors; first match wins. Must be non-empty for useful behavior.
    executors: Vec<Box<dyn StepExecutor>>,
}

/// Methods for `CompositeStepExecutor`.
impl CompositeStepExecutor {
    /// Build from an ordered list of executors.
    pub fn new(executors: Vec<Box<dyn StepExecutor>>) -> Self {
        Self { executors }
    }
}

#[async_trait]
/// Implements ordered composite step dispatch.
impl StepExecutor for CompositeStepExecutor {
    /// Returns true if any member executor handles `step_type`.
    fn handles(&self, step_type: StepType) -> bool {
        self.executors.iter().any(|e| e.handles(step_type))
    }

    /// Delegates to the first member that handles the step type.
    ///
    /// Returns an error if no member claims the type (caller should check `handles` first,
    /// but this is a safe fallback).
    async fn execute(&self, ctx: StepContext<'_>) -> Result<serde_json::Value, String> {
        // Copy step_type before the loop so it is available after ctx is potentially moved.
        let step_type = ctx.step_type;
        for executor in &self.executors {
            if executor.handles(step_type) {
                return executor.execute(ctx).await;
            }
        }
        Err(format!(
            "no executor handles step type {:?}",
            step_type.as_str()
        ))
    }
}

/// Replace `{{path}}` placeholders in `template` with values resolved from `vars` (strings
/// verbatim, `Null` as empty, anything else via its JSON text).
///
/// The scan runs once over the original template. Replacement text is appended literally and
/// never scanned again, so self-referential values terminate. An unclosed placeholder and its
/// remainder are preserved verbatim.
pub fn interpolate(template: &str, vars: &serde_json::Value) -> String {
    let mut result = String::with_capacity(template.len());
    let mut rest = template;
    loop {
        let Some(start) = rest.find("{{") else {
            result.push_str(rest);
            break;
        };
        result.push_str(&rest[..start]);
        let after_open = &rest[start + 2..];
        let Some(end) = after_open.find("}}") else {
            result.push_str(&rest[start..]);
            break;
        };
        let path = after_open[..end].trim();
        let replacement = match resolve_dot_path(vars, path) {
            serde_json::Value::String(s) => s,
            serde_json::Value::Null => String::new(),
            other => other.to_string(),
        };
        result.push_str(&replacement);
        rest = &after_open[end + 2..];
    }
    result
}

#[cfg(test)]
/// Tests the built-in and composed step executors.
mod tests {
    use super::*;

    /// Dot paths resolve nested values and miss to Null.
    #[test]
    fn dot_path_resolves_and_misses() {
        let obj = serde_json::json!({"a": {"b": {"c": 7}}});
        assert_eq!(resolve_dot_path(&obj, "a.b.c"), serde_json::json!(7));
        assert_eq!(resolve_dot_path(&obj, "a.x"), serde_json::Value::Null);
    }

    /// set_dot_path creates intermediate objects.
    #[test]
    fn set_dot_path_creates_intermediates() {
        let mut out = serde_json::Map::new();
        set_dot_path(&mut out, "x.y", serde_json::json!(1));
        assert_eq!(
            serde_json::Value::Object(out),
            serde_json::json!({"x": {"y": 1}})
        );
    }

    /// Interpolation substitutes resolved paths; unknown paths become empty.
    #[test]
    fn interpolate_substitutes() {
        let vars = serde_json::json!({"who": "loom", "n": 3});
        assert_eq!(
            interpolate("hi {{who}} x{{n}} ({{gone}})", &vars),
            "hi loom x3 ()"
        );
    }

    /// Self-referential replacement text is emitted literally instead of being rescanned.
    #[test]
    fn interpolate_self_reference_is_literal() {
        let vars = serde_json::json!({"x": "{{x}}"});
        assert_eq!(interpolate("{{x}}", &vars), "{{x}}");
    }

    /// Replacement text containing another placeholder is not interpreted a second time.
    #[test]
    fn interpolate_does_not_rescan_replacement() {
        let vars = serde_json::json!({"a": "{{b}}", "b": "B"});
        assert_eq!(interpolate("{{a}}", &vars), "{{b}}");
    }

    /// Unclosed placeholder text stays literal and an empty path resolves to empty.
    #[test]
    fn interpolate_handles_unclosed_and_empty_placeholders() {
        let vars = serde_json::json!({"who": "loom"});
        assert_eq!(interpolate("hi {{who", &vars), "hi {{who");
        assert_eq!(interpolate("[{{}}]", &vars), "[]");
        assert_eq!(
            interpolate("a {{who}} b {{missing", &vars),
            "a loom b {{missing"
        );
    }

    /// Fake dispatch that echoes the input back as the output with a marker field added.
    struct FakeDispatch;

    #[async_trait]
    /// Implements a fixture Hephaestus dispatch for executor tests.
    impl HephaestusDispatch for FakeDispatch {
        /// Echo the input JSON, adding `"dispatched": true` to confirm this ran.
        async fn run(
            &self,
            tenant: TenantId,
            principal: PrincipalId,
            input: serde_json::Value,
        ) -> Result<serde_json::Value, String> {
            let mut out = match input {
                serde_json::Value::Object(m) => m,
                other => {
                    let mut m = serde_json::Map::new();
                    m.insert("value".to_string(), other);
                    m
                }
            };
            out.insert("tenant_id".to_string(), serde_json::json!(tenant));
            out.insert("principal_id".to_string(), serde_json::json!(principal));
            out.insert("dispatched".to_string(), serde_json::Value::Bool(true));
            Ok(serde_json::Value::Object(out))
        }
    }

    /// A HephaestusStepExecutor with FakeDispatch runs a Hephaestus step and records output.
    #[tokio::test]
    async fn hephaestus_step_executor_runs_and_records_output() {
        let exec = HephaestusStepExecutor::new(FakeDispatch);
        let input = serde_json::json!({"task": "summarise"});
        let config = serde_json::json!({
            "input": "please summarise",
            "agent": "test-agent",
            "tenant_id": TenantId::new(),
            "principal_id": PrincipalId::new()
        });
        let tenant = TenantId::new();
        let principal = PrincipalId::new();
        let ctx = StepContext {
            run_id: RunId::new(),
            tenant,
            principal,
            step_id: 1,
            name: "call-hephaestus",
            step_type: StepType::Hephaestus,
            config: Box::leak(Box::new(config)),
            input: &input,
            timeout_ms: 5000,
        };
        // handles() should claim Hephaestus steps.
        assert!(exec.handles(StepType::Hephaestus));
        assert!(!exec.handles(StepType::Transform));

        let result = exec.execute(ctx).await.unwrap();
        // Config keys overlay input; "input" and "agent" come from config, "task" from input.
        assert_eq!(result["input"], "please summarise");
        assert_eq!(result["agent"], "test-agent");
        assert_eq!(result["task"], "summarise");
        assert_eq!(result["tenant_id"], serde_json::json!(tenant));
        assert_eq!(result["principal_id"], serde_json::json!(principal));
        assert_eq!(result["dispatched"], true);
    }

    /// CompositeStepExecutor delegates to the right member and handles both Transform and Hephaestus.
    #[tokio::test]
    async fn composite_executor_delegates_correctly() {
        let composite = CompositeStepExecutor::new(vec![
            Box::new(TransformExecutor),
            Box::new(HephaestusStepExecutor::new(FakeDispatch)),
        ]);

        assert!(composite.handles(StepType::Transform));
        assert!(composite.handles(StepType::Hephaestus));
        assert!(!composite.handles(StepType::Action));

        let input = serde_json::json!({"src": {"v": 7}});
        let transform_config = serde_json::from_str(r#"{"mapping": {"out.v": "src.v"}}"#).unwrap();
        let transform_ctx = StepContext {
            run_id: RunId::new(),
            tenant: TenantId::new(),
            principal: PrincipalId::new(),
            step_id: 1,
            name: "t",
            step_type: StepType::Transform,
            config: Box::leak(Box::new(transform_config)),
            input: &input,
            timeout_ms: 1000,
        };
        let transform_out = composite.execute(transform_ctx).await.unwrap();
        assert_eq!(transform_out, serde_json::json!({"out": {"v": 7}}));

        let heph_input = serde_json::json!({"prompt": "hello"});
        let heph_config = serde_json::json!({});
        let heph_ctx = StepContext {
            run_id: RunId::new(),
            tenant: TenantId::new(),
            principal: PrincipalId::new(),
            step_id: 2,
            name: "h",
            step_type: StepType::Hephaestus,
            config: Box::leak(Box::new(heph_config)),
            input: &heph_input,
            timeout_ms: 1000,
        };
        let heph_out = composite.execute(heph_ctx).await.unwrap();
        assert_eq!(heph_out["dispatched"], true);
        assert_eq!(heph_out["prompt"], "hello");
    }

    /// The transform executor's three modes: mapping, template, pass-through.
    #[tokio::test]
    async fn transform_modes() {
        let exec = TransformExecutor;
        let input = serde_json::json!({"src": {"v": 42}, "name": "x"});
        let ctx = |config: &'static str| StepContext {
            run_id: RunId::new(),
            tenant: TenantId::new(),
            principal: PrincipalId::new(),
            step_id: 1,
            name: "t",
            step_type: StepType::Transform,
            config: Box::leak(Box::new(serde_json::from_str(config).unwrap())),
            input: &input,
            timeout_ms: 1000,
        };
        let mapped = exec
            .execute(ctx(r#"{"mapping": {"out.v": "src.v"}}"#))
            .await
            .unwrap();
        assert_eq!(mapped, serde_json::json!({"out": {"v": 42}}));
        let rendered = exec
            .execute(ctx(r#"{"template": "v={{src.v}}"}"#))
            .await
            .unwrap();
        assert_eq!(rendered, serde_json::json!("v=42"));
        let passthrough = exec.execute(ctx("{}")).await.unwrap();
        assert_eq!(passthrough, input);
    }
}
