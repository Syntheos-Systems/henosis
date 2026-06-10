//! The pluggable step-executor seam and the built-in pure-JSON transform executor.
//!
//! Kleos hardcoded transform/webhook/LLM execution inside `advance_run`. Here execution is a
//! trait: the engine asks the attached [`StepExecutor`] whether it handles a ready step's type
//! and runs it inline when it does; everything else stays `running` for external completion
//! via `complete_step` (the Kleos action/decision/wait semantics). Hephaestus provides the
//! real executor in Phase 5 (roadmap story 5.5 swaps it in); until then the built-in
//! [`TransformExecutor`] covers pure-JSON steps so a step graph runs end-to-end today.

use async_trait::async_trait;
use syntheos_contracts::RunId;

use crate::model::StepType;

/// Everything an executor sees about the step it is asked to run.
#[derive(Debug)]
pub struct StepContext<'a> {
    /// The run the step belongs to.
    pub run_id: RunId,
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

/// The built-in executor for [`StepType::Transform`]: pure JSON manipulation, ported from the
/// Kleos transform step. Config selects one mode:
///
/// - `{"mapping": {"target.path": "source.path", ...}}` -- dot-path remapping.
/// - `{"template": "text with {{var.path}}"}` (or an object of such strings) -- interpolation.
/// - neither -- pass the input through unchanged.
pub struct TransformExecutor;

#[async_trait]
impl StepExecutor for TransformExecutor {
    fn handles(&self, step_type: StepType) -> bool {
        step_type == StepType::Transform
    }

    async fn execute(&self, ctx: StepContext<'_>) -> Result<serde_json::Value, String> {
        if let Some(mapping) = ctx.config.get("mapping") {
            let mapping = mapping
                .as_object()
                .ok_or_else(|| "mapping must be an object".to_string())?;
            let mut output = serde_json::Map::new();
            for (target_path, source_path) in mapping {
                let source_path = source_path.as_str().ok_or_else(|| {
                    format!("mapping value for {target_path:?} must be a string")
                })?;
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

/// Replace `{{path}}` placeholders in `template` with values resolved from `vars` (strings
/// verbatim, `Null` as empty, anything else via its JSON text).
pub fn interpolate(template: &str, vars: &serde_json::Value) -> String {
    let mut result = template.to_string();
    while let Some(start) = result.find("{{") {
        let Some(end_offset) = result[start..].find("}}") else {
            break;
        };
        let path = result[start + 2..start + end_offset].trim().to_string();
        let val = resolve_dot_path(vars, &path);
        let replacement = match &val {
            serde_json::Value::String(s) => s.clone(),
            serde_json::Value::Null => String::new(),
            other => other.to_string(),
        };
        result.replace_range(start..start + end_offset + 2, &replacement);
    }
    result
}

#[cfg(test)]
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
        assert_eq!(serde_json::Value::Object(out), serde_json::json!({"x": {"y": 1}}));
    }

    /// Interpolation substitutes resolved paths; unknown paths become empty.
    #[test]
    fn interpolate_substitutes() {
        let vars = serde_json::json!({"who": "loom", "n": 3});
        assert_eq!(interpolate("hi {{who}} x{{n}} ({{gone}})", &vars), "hi loom x3 ()");
    }

    /// The transform executor's three modes: mapping, template, pass-through.
    #[tokio::test]
    async fn transform_modes() {
        let exec = TransformExecutor;
        let input = serde_json::json!({"src": {"v": 42}, "name": "x"});
        let ctx = |config: &'static str| StepContext {
            run_id: RunId::new(),
            step_id: 1,
            name: "t",
            step_type: StepType::Transform,
            config: Box::leak(Box::new(serde_json::from_str(config).unwrap())),
            input: &input,
            timeout_ms: 1000,
        };
        let mapped = exec.execute(ctx(r#"{"mapping": {"out.v": "src.v"}}"#)).await.unwrap();
        assert_eq!(mapped, serde_json::json!({"out": {"v": 42}}));
        let rendered = exec.execute(ctx(r#"{"template": "v={{src.v}}"}"#)).await.unwrap();
        assert_eq!(rendered, serde_json::json!("v=42"));
        let passthrough = exec.execute(ctx("{}")).await.unwrap();
        assert_eq!(passthrough, input);
    }
}
