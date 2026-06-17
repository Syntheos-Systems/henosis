//! In-process MCP JSONRPC bridge for tool registration and execution.
//!
//! # Tool Gating
//!
//! This bridge delegates directly to the `ToolExecutor` without per-call
//! permission checks. Tool gating (allow/deny decisions) is the responsibility
//! of the `ToolExecutor` implementation, not this bridge. The bridge is a
//! transparent protocol adapter.

use std::sync::Arc;

use serde_json::{Value, json};

use crate::claude_max::ToolExecutor;

/// In-process MCP JSONRPC handler backed by a ToolExecutor.
pub(crate) struct McpBridge {
    /// Tool executor providing schemas and execution.
    tools: Arc<dyn ToolExecutor>,
}

/// Adds inherent behavior for `McpBridge`.
impl McpBridge {
    /// Create a new MCP bridge wrapping the given tool executor.
    pub(crate) fn new(tools: Arc<dyn ToolExecutor>) -> Self {
        Self { tools }
    }

    /// Handle an MCP JSONRPC request and return the JSONRPC response.
    pub(crate) async fn handle_jsonrpc(&self, request: Value) -> Value {
        let id = request.get("id").cloned().unwrap_or(Value::Null);
        let method = request.get("method").and_then(|m| m.as_str()).unwrap_or("");
        let params = request.get("params").cloned().unwrap_or(json!({}));

        match method {
            "initialize" => self.handle_initialize(id),
            "tools/list" => self.handle_tools_list(id),
            "tools/call" => self.handle_tools_call(id, params).await,
            _ => json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {
                    "code": -32601,
                    "message": format!("method not found: {}", method)
                }
            }),
        }
    }

    /// Respond to initialize with server capabilities.
    fn handle_initialize(&self, id: Value) -> Value {
        json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "protocolVersion": "2024-11-05",
                "capabilities": {
                    "tools": {
                        "listChanged": true
                    }
                },
                "serverInfo": {
                    "name": "synapse-tools",
                    "version": "0.1.0"
                }
            }
        })
    }

    /// Convert ToolExecutor schemas to MCP tool format and return them.
    fn handle_tools_list(&self, id: Value) -> Value {
        let schemas = self.tools.tool_schemas();
        let mcp_tools: Vec<Value> = schemas
            .into_iter()
            .map(|schema| self.to_mcp_tool(schema))
            .collect();

        json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "tools": mcp_tools
            }
        })
    }

    /// Execute a tool call and return the MCP result.
    async fn handle_tools_call(&self, id: Value, params: Value) -> Value {
        let name = params.get("name").and_then(|n| n.as_str()).unwrap_or("");
        let arguments = params.get("arguments").cloned().unwrap_or(json!({}));

        let result = self.tools.execute_tool(name, arguments).await;

        match result {
            Ok(exec_result) => json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "content": [
                        {"type": "text", "text": exec_result.output}
                    ],
                    "isError": exec_result.is_error
                }
            }),
            Err(e) => json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "content": [
                        {"type": "text", "text": format!("tool execution error: {}", e)}
                    ],
                    "isError": true
                }
            }),
        }
    }

    /// Convert an Anthropic-format tool schema to MCP format.
    /// Renames `input_schema` (snake_case) to `inputSchema` (camelCase).
    fn to_mcp_tool(&self, mut schema: Value) -> Value {
        if let Some(obj) = schema.as_object_mut()
            && let Some(input_schema) = obj.remove("input_schema")
        {
            obj.insert("inputSchema".to_string(), input_schema);
        }
        schema
    }
}

/// Groups `{` functionality.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::claude_max::{ToolExecutionResult, ToolExecutor};
    use async_trait::async_trait;
    use serde_json::json;

    /// Mock tool executor for testing.
    struct MockExecutor;

    /// Implements `ToolExecutor` behavior for `MockExecutor`.
    #[async_trait]
    impl ToolExecutor for MockExecutor {
        /// Handles `tool_schemas` behavior.
        fn tool_schemas(&self) -> Vec<serde_json::Value> {
            vec![json!({
                "name": "echo",
                "description": "Echoes input back",
                "input_schema": {
                    "type": "object",
                    "properties": {
                        "text": {"type": "string"}
                    },
                    "required": ["text"]
                }
            })]
        }

        /// Handles `execute_tool` behavior.
        async fn execute_tool(
            &self,
            name: &str,
            input: serde_json::Value,
        ) -> anyhow::Result<ToolExecutionResult> {
            match name {
                "echo" => Ok(ToolExecutionResult {
                    output: input["text"].as_str().unwrap_or("").to_string(),
                    is_error: false,
                }),
                _ => Ok(ToolExecutionResult {
                    output: format!("unknown tool: {}", name),
                    is_error: true,
                }),
            }
        }
    }

    /// Handles `tools_list_converts_input_schema_to_camel_case` behavior.
    #[tokio::test]
    async fn tools_list_converts_input_schema_to_camel_case() {
        let bridge = McpBridge::new(std::sync::Arc::new(MockExecutor));
        let request = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/list",
            "params": {}
        });
        let response = bridge.handle_jsonrpc(request).await;

        assert_eq!(response["jsonrpc"], "2.0");
        assert_eq!(response["id"], 1);
        let tools = &response["result"]["tools"];
        assert_eq!(tools[0]["name"], "echo");
        // MCP uses camelCase inputSchema, not snake_case input_schema
        assert!(tools[0].get("inputSchema").is_some());
        assert!(tools[0].get("input_schema").is_none());
    }

    /// Handles `tools_call_executes_and_returns_result` behavior.
    #[tokio::test]
    async fn tools_call_executes_and_returns_result() {
        let bridge = McpBridge::new(std::sync::Arc::new(MockExecutor));
        let request = json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {
                "name": "echo",
                "arguments": {"text": "hello"}
            }
        });
        let response = bridge.handle_jsonrpc(request).await;

        assert_eq!(response["id"], 2);
        assert_eq!(response["result"]["content"][0]["type"], "text");
        assert_eq!(response["result"]["content"][0]["text"], "hello");
        assert_eq!(response["result"]["isError"], false);
    }

    /// Handles `tools_call_unknown_tool_returns_error` behavior.
    #[tokio::test]
    async fn tools_call_unknown_tool_returns_error() {
        let bridge = McpBridge::new(std::sync::Arc::new(MockExecutor));
        let request = json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {
                "name": "nonexistent",
                "arguments": {}
            }
        });
        let response = bridge.handle_jsonrpc(request).await;

        assert_eq!(response["result"]["isError"], true);
    }

    /// Handles `initialize_returns_capabilities` behavior.
    #[tokio::test]
    async fn initialize_returns_capabilities() {
        let bridge = McpBridge::new(std::sync::Arc::new(MockExecutor));
        let request = json!({
            "jsonrpc": "2.0",
            "id": 0,
            "method": "initialize",
            "params": {}
        });
        let response = bridge.handle_jsonrpc(request).await;

        assert_eq!(
            response["result"]["capabilities"]["tools"]["listChanged"],
            true
        );
    }

    /// Handles `unknown_method_returns_jsonrpc_error` behavior.
    #[tokio::test]
    async fn unknown_method_returns_jsonrpc_error() {
        let bridge = McpBridge::new(std::sync::Arc::new(MockExecutor));
        let request = json!({
            "jsonrpc": "2.0",
            "id": 99,
            "method": "unknown/method",
            "params": {}
        });
        let response = bridge.handle_jsonrpc(request).await;

        assert!(response.get("error").is_some());
        assert_eq!(response["error"]["code"], -32601);
    }
}
