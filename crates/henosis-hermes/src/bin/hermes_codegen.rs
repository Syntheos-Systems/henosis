//! `hermes-codegen` -- OpenAPI 3.0 to adapter-skeleton emitter.
//!
//! Usage:
//!   hermes-codegen <openapi.yaml>                      # list operations
//!   hermes-codegen <openapi.yaml> <operationId>        # emit to stdout
//!   hermes-codegen <openapi.yaml> <operationId> --out src/adapters/foo.rs
//!
//! The skeleton implements the `Tool` trait with a populated `ToolSchema`
//! drawn from the OpenAPI operation, and a placeholder `invoke` that
//! validates `tenant_id`, fetches a credd token under a TODO provider tag,
//! and calls the upstream URL. The user fills in argument parsing, response
//! shaping, and provider-specific auth headers.

use std::path::PathBuf;
use std::process::ExitCode;

use serde_yaml::Value as Yaml;

/// Entry point. Parses CLI args, reads the OpenAPI spec, and either lists
/// all operations or emits a Rust adapter skeleton for the requested operationId.
fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("usage: hermes-codegen <openapi.yaml> [operationId] [--out <path>]");
        return ExitCode::from(2);
    }
    let yaml_path = PathBuf::from(&args[1]);
    let operation_id = args.get(2).cloned().filter(|s| !s.starts_with("--"));
    let out_path = parse_out(&args);

    let raw = match std::fs::read_to_string(&yaml_path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("read {} failed: {e}", yaml_path.display());
            return ExitCode::from(2);
        }
    };
    let spec: Yaml = match serde_yaml::from_str(&raw) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("openapi parse failed: {e}");
            return ExitCode::from(2);
        }
    };

    let ops = collect_operations(&spec);
    if ops.is_empty() {
        eprintln!("no operations found in {}", yaml_path.display());
        return ExitCode::from(2);
    }

    let target = match operation_id {
        Some(id) => match ops.iter().find(|o| o.operation_id == id) {
            Some(op) => op,
            None => {
                eprintln!("operationId '{id}' not found. available:");
                for op in ops.iter().take(40) {
                    eprintln!("  {} ({} {})", op.operation_id, op.method, op.path);
                }
                if ops.len() > 40 {
                    eprintln!("  ... and {} more", ops.len() - 40);
                }
                return ExitCode::from(2);
            }
        },
        None => {
            println!("# {} operations in {}", ops.len(), yaml_path.display());
            for op in &ops {
                println!("{}\t{} {}", op.operation_id, op.method, op.path);
            }
            return ExitCode::SUCCESS;
        }
    };

    let skeleton = emit_skeleton(target);
    match out_path {
        Some(p) => match std::fs::write(&p, &skeleton) {
            Ok(()) => eprintln!("wrote {} ({} bytes)", p.display(), skeleton.len()),
            Err(e) => {
                eprintln!("write {} failed: {e}", p.display());
                return ExitCode::from(2);
            }
        },
        None => print!("{skeleton}"),
    }
    ExitCode::SUCCESS
}

/// Scan CLI args for `--out <path>` and return the output path if provided.
fn parse_out(args: &[String]) -> Option<PathBuf> {
    let i = args.iter().position(|a| a == "--out")?;
    args.get(i + 1).map(PathBuf::from)
}

/// A single OpenAPI operation extracted from a path item.
#[derive(Debug)]
struct Operation {
    /// The `operationId` from the spec, or a synthetic id derived from the method + path.
    operation_id: String,
    /// HTTP method in uppercase (e.g. "GET").
    method: String,
    /// URL path template (e.g. "/v2/users/{id}").
    path: String,
    /// Short human-readable summary from the spec.
    summary: Option<String>,
    /// Longer description from the spec.
    description: Option<String>,
    /// Query, path, and header parameters declared on the operation.
    parameters: Vec<Parameter>,
    /// True when a `requestBody` is marked `required: true`.
    request_body_required: bool,
}

/// A single parameter from an OpenAPI operation's `parameters` list.
#[derive(Debug)]
struct Parameter {
    /// Parameter name as declared in the spec.
    name: String,
    /// Location of the parameter: "path", "query", or "header".
    location: String,
    /// Whether the parameter is marked `required` in the spec.
    required: bool,
    /// Optional description from the spec.
    description: Option<String>,
}

/// Walk every path item in the spec and collect one `Operation` per HTTP method.
fn collect_operations(spec: &Yaml) -> Vec<Operation> {
    let Some(paths) = spec.get("paths").and_then(|p| p.as_mapping()) else {
        return vec![];
    };
    const METHODS: [&str; 7] = ["get", "post", "put", "patch", "delete", "options", "head"];
    let mut out = Vec::new();
    for (path_v, item_v) in paths {
        let Some(path) = path_v.as_str() else {
            continue;
        };
        let Some(item) = item_v.as_mapping() else {
            continue;
        };
        for (method_v, op_v) in item {
            let Some(method) = method_v.as_str() else {
                continue;
            };
            if !METHODS.contains(&method) {
                continue;
            }
            let Some(op_map) = op_v.as_mapping() else {
                continue;
            };
            let operation_id = op_map
                .get(Yaml::String("operationId".into()))
                .and_then(|v| v.as_str())
                .map(String::from)
                .unwrap_or_else(|| format!("{}_{}", method, sanitize_path(path)));
            let summary = pull_str(op_map, "summary");
            let description = pull_str(op_map, "description");
            let parameters = collect_parameters(op_map);
            let request_body_required = op_map
                .get(Yaml::String("requestBody".into()))
                .and_then(|v| v.as_mapping())
                .and_then(|m| m.get(Yaml::String("required".into())))
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            out.push(Operation {
                operation_id,
                method: method.to_uppercase(),
                path: path.to_string(),
                summary,
                description,
                parameters,
                request_body_required,
            });
        }
    }
    out
}

/// Extract the `parameters` array from an operation mapping and convert each
/// entry into a `Parameter`.
fn collect_parameters(op_map: &serde_yaml::Mapping) -> Vec<Parameter> {
    let Some(seq) = op_map
        .get(Yaml::String("parameters".into()))
        .and_then(|v| v.as_sequence())
    else {
        return vec![];
    };
    seq.iter()
        .filter_map(|p| {
            let m = p.as_mapping()?;
            let name = pull_str(m, "name")?;
            let location = pull_str(m, "in").unwrap_or_else(|| "query".into());
            let required = m
                .get(Yaml::String("required".into()))
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let description = pull_str(m, "description");
            Some(Parameter {
                name,
                location,
                required,
                description,
            })
        })
        .collect()
}

/// Look up a string key in a YAML mapping and return it if non-empty.
fn pull_str(m: &serde_yaml::Mapping, key: &str) -> Option<String> {
    m.get(Yaml::String(key.into()))
        .and_then(|v| v.as_str())
        .map(String::from)
        .filter(|s| !s.is_empty())
}

/// Convert a URL path template into a safe Rust identifier fragment by
/// replacing slashes, braces, and hyphens with underscores and dropping
/// any non-alphanumeric, non-underscore characters.
fn sanitize_path(p: &str) -> String {
    p.trim_start_matches('/')
        .replace(['/', '{', '}', '-'], "_")
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect()
}

/// Generate a complete Rust adapter skeleton for the given operation.
///
/// The skeleton compiles cleanly but contains `TODO` placeholders for the
/// argument parsing, request construction, and response shaping steps.
fn emit_skeleton(op: &Operation) -> String {
    let tool_id = op.operation_id.to_lowercase();
    let struct_name = camel_case(&op.operation_id) + "Tool";
    let module_doc = op
        .description
        .clone()
        .or_else(|| op.summary.clone())
        .unwrap_or_else(|| format!("{} {}", op.method, op.path));
    let summary = op
        .summary
        .clone()
        .unwrap_or_else(|| op.operation_id.clone());

    let mut params_doc = String::new();
    for p in &op.parameters {
        let req = if p.required { "required" } else { "optional" };
        let desc = p.description.as_deref().unwrap_or("");
        params_doc.push_str(&format!(
            "//!   - {} ({}, {}): {}\n",
            p.name, p.location, req, desc
        ));
    }
    if op.request_body_required {
        params_doc.push_str("//!   - body (required JSON object)\n");
    }

    // Build the skeleton via String::push_str to avoid format! brace gymnastics.
    let mut s = String::new();
    s.push_str("//! Adapter skeleton generated by `hermes-codegen` for operation `");
    s.push_str(&op.operation_id);
    s.push_str("`.\n//!\n//! ");
    s.push_str(&module_doc);
    s.push_str("\n//!\n//! Endpoint: ");
    s.push_str(&op.method);
    s.push(' ');
    s.push_str(&op.path);
    s.push_str("\n//! Parameters:\n");
    s.push_str(&params_doc);
    s.push_str(
        "//!\n\
         //! TODO before merging:\n\
         //!  1. Replace PROVIDER with the credd provider tag (e.g. \"stripe\").\n\
         //!  2. Implement parse_args to validate the JSON args from the caller.\n\
         //!  3. Build the upstream request -- query params, headers, body.\n\
         //!  4. Shape the success response into the adapter's result envelope.\n\
         //!  5. Override retry_policy() with RetryPolicy::non_idempotent() if the\n\
         //!     operation must not be replayed (creates/sends/charges).\n\
         //!  6. Fill in the test_adapter! stub at the bottom of this file.\n\n",
    );
    s.push_str(
        "use async_trait::async_trait;\n\
         use serde_json::{json, Value};\n\
         use tracing::warn;\n\n\
         use crate::adapters::common::{build_http, credd_error_to_response, send_with_retry, truncate};\n\
         use crate::tool::{\n    \
            err, error_response, InvokeContext, InvokeRequest, InvokeResponse, Tool, ToolSchema,\n\
         };\n\n",
    );
    s.push_str(&format!("const TOOL_ID: &str = \"{tool_id}\";\n"));
    s.push_str("const PROVIDER: &str = \"TODO_PROVIDER\";\n");
    s.push_str(&format!("const ENDPOINT: &str = \"{}\";\n\n", op.path));
    s.push_str(&format!("/// Adapter for the `{tool_id}` tool (generated skeleton).\n"));
    s.push_str(&format!("pub struct {struct_name};\n\n"));

    s.push_str("#[async_trait]\n");
    s.push_str(&format!("impl Tool for {struct_name} {{\n"));
    s.push_str("    fn schema(&self) -> ToolSchema {\n        ToolSchema {\n");
    s.push_str("            tool_id: TOOL_ID.to_string(),\n");
    s.push_str(&format!("            name: \"{summary}\".to_string(),\n"));
    let escaped_doc = module_doc.replace('"', "\\\"");
    s.push_str(&format!(
        "            description: \"{escaped_doc}\".to_string(),\n"
    ));
    s.push_str(
        "            input_schema: json!({\n                \"type\": \"object\",\n                \"properties\": {\n                    // TODO: enumerate from the OpenAPI parameters above\n                }\n            }),\n",
    );
    s.push_str(
        "            output_schema: json!({ \"type\": \"object\" }),\n            category: \"todo\".to_string(),\n            requires_auth: true,\n        }\n    }\n\n    fn provider(&self) -> &'static str {\n        PROVIDER\n    }\n\n",
    );

    let method_lower = op.method.to_lowercase();
    s.push_str("    async fn invoke(&self, ctx: &InvokeContext, req: InvokeRequest) -> InvokeResponse {\n");
    s.push_str(
        "        let tenant_id = match req.tenant_id.as_deref().filter(|s| !s.is_empty()) {\n            Some(t) => t.to_string(),\n            None => return error_response(TOOL_ID, \"bad_request\", \"tenant_id is required\", None),\n        };\n\n",
    );
    s.push_str(
        "        let token = match ctx.credd.fetch_token(&tenant_id, PROVIDER).await {\n            Ok(t) => t,\n            Err(e) => return credd_error_to_response(TOOL_ID, &e),\n        };\n\n",
    );
    s.push_str(
        "        let http = match build_http() {\n            Ok(c) => c,\n            Err(e) => return error_response(TOOL_ID, \"internal_error\", e.to_string(), None),\n        };\n\n",
    );
    s.push_str("        let _ = req.args; // TODO: parse args.\n\n");
    s.push_str(&format!(
        "        let request = http.{method_lower}(ENDPOINT).bearer_auth(&token);\n"
    ));
    s.push_str(
        "        let outcome = match send_with_retry(request, &self.retry_policy()).await {\n            Ok(o) => o,\n            Err(e) => {\n                return InvokeResponse {\n                    tool_id: TOOL_ID.into(),\n                    success: false,\n                    result: None,\n                    error: Some(err(\"upstream_unreachable\", format!(\"upstream request failed: {e}\"), None)),\n                    duration_ms: 0,\n                };\n            }\n        };\n\n",
    );
    s.push_str(
        "        let status = outcome.status;\n        let body_text = outcome.body;\n\n",
    );
    s.push_str(
        "        if !status.is_success() {\n            warn!(status = %status, body = %truncate(&body_text, 256), \"upstream api error\");\n            return InvokeResponse {\n                tool_id: TOOL_ID.into(),\n                success: false,\n                result: None,\n                error: Some(json!({\n                    \"code\": \"upstream_api_error\",\n                    \"message\": format!(\"upstream returned HTTP {}\", status.as_u16()),\n                    \"status\": status.as_u16(),\n                    \"body\": truncate(&body_text, 512),\n                })),\n                duration_ms: 0,\n            };\n        }\n\n",
    );
    s.push_str(
        "        let parsed: Value = serde_json::from_str(&body_text).unwrap_or(Value::Null);\n        InvokeResponse {\n            tool_id: TOOL_ID.into(),\n            success: true,\n            result: Some(parsed),\n            error: None,\n            duration_ms: 0,\n        }\n    }\n}\n\n",
    );

    // Emit a test_adapter! stub. It is commented out because the macro
    // redirects the invoke context's provider base URL at a mock server; this
    // skeleton calls ENDPOINT directly, so wire the request through
    // `ctx.bases.<provider>` first, then uncomment.
    s.push_str("#[cfg(test)]\nmod tests {\n");
    s.push_str("    // After routing the request through ctx.bases, uncomment:\n");
    s.push_str("    //\n");
    s.push_str("    // test_adapter!(\n");
    s.push_str("    //     happy_path,\n");
    s.push_str(&format!("    //     tool: {struct_name},\n"));
    s.push_str(&format!("    //     method: \"{}\",\n", op.method));
    s.push_str(&format!("    //     path: \"{}\",\n", op.path));
    s.push_str("    //     respond: serde_json::json!({ \"ok\": true }),\n");
    s.push_str("    //     args: serde_json::json!({ }),\n");
    s.push_str("    //     expect: { \"ok\" => true }\n");
    s.push_str("    // );\n");
    s.push_str("}\n");

    s
}

/// Convert a snake_case or hyphenated identifier to UpperCamelCase.
fn camel_case(s: &str) -> String {
    let mut out = String::new();
    let mut upper = true;
    for c in s.chars() {
        if c == '_' || c == '-' {
            upper = true;
        } else if upper {
            out.extend(c.to_uppercase());
            upper = false;
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn camel_case_basic() {
        assert_eq!(camel_case("foo_bar"), "FooBar");
        assert_eq!(camel_case("get_user_by_id"), "GetUserById");
        assert_eq!(camel_case("FooBar"), "FooBar");
    }

    #[test]
    fn camel_case_hyphens() {
        assert_eq!(camel_case("create-issue"), "CreateIssue");
    }

    #[test]
    fn sanitize_path_strips_slashes_and_braces() {
        assert_eq!(sanitize_path("/users/{id}/posts"), "users__id__posts");
        assert_eq!(sanitize_path("/v2/api-spec"), "v2_api_spec");
    }

    #[test]
    fn parse_out_extracts_path() {
        let args: Vec<String> = vec!["cmd".into(), "spec.yaml".into(), "--out".into(), "foo.rs".into()];
        let p = parse_out(&args).unwrap();
        assert_eq!(p, PathBuf::from("foo.rs"));
    }

    #[test]
    fn parse_out_missing_returns_none() {
        let args: Vec<String> = vec!["cmd".into(), "spec.yaml".into()];
        assert!(parse_out(&args).is_none());
    }

    #[test]
    fn emit_skeleton_contains_struct_and_impl() {
        let op = Operation {
            operation_id: "create_issue".into(),
            method: "POST".into(),
            path: "/issues".into(),
            summary: Some("Create issue".into()),
            description: None,
            parameters: vec![],
            request_body_required: true,
        };
        let s = emit_skeleton(&op);
        assert!(s.contains("pub struct CreateIssueTool"));
        assert!(s.contains("impl Tool for CreateIssueTool"));
        assert!(s.contains("TOOL_ID"));
        assert!(s.contains("// TODO: enumerate from the OpenAPI parameters"));
    }

    #[test]
    fn emit_skeleton_includes_test_stub() {
        let op = Operation {
            operation_id: "list_users".into(),
            method: "GET".into(),
            path: "/users".into(),
            summary: None,
            description: None,
            parameters: vec![],
            request_body_required: false,
        };
        let s = emit_skeleton(&op);
        assert!(s.contains("test_adapter!"));
        assert!(s.contains("ListUsersTool"));
    }
}
