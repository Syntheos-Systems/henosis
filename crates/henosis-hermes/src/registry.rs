//! Tool registry: maps tool IDs to `Arc<dyn Tool>` implementations and
//! provides the `build_registry` factory that registers all bundled adapters.

use std::collections::HashMap;
use std::sync::Arc;

use crate::adapters;
use crate::tool::{Tool, ToolSchema};

/// The central registry mapping tool IDs to their adapter implementations.
/// Populated once at startup by [`build_registry`] and shared read-only across
/// all in-flight invocations.
pub struct ToolRegistry {
    /// Map from tool ID to the adapter implementation.
    tools: HashMap<String, Arc<dyn Tool>>,
}

/// Provides tool registration, lookup, and provider inventory operations.
impl ToolRegistry {
    /// Construct an empty registry with no registered tools.
    pub fn new() -> Self {
        Self {
            tools: HashMap::new(),
        }
    }

    /// Register a tool implementation. The tool's ID is read from its schema.
    /// A second registration under the same ID replaces the first.
    pub fn register(&mut self, tool: impl Tool + 'static) {
        let id = tool.schema().tool_id.clone();
        self.tools.insert(id, Arc::new(tool));
    }

    /// Look up a tool by its ID, returning a cloned reference-counted pointer.
    /// Returns `None` when no tool with that ID is registered.
    pub fn get(&self, tool_id: &str) -> Option<Arc<dyn Tool>> {
        self.tools.get(tool_id).cloned()
    }

    /// Return all registered tool schemas, sorted by `tool_id`. Used by
    /// `GET /tools` and the MCP `tools/list` handler.
    pub fn list(&self) -> Vec<ToolSchema> {
        let mut schemas: Vec<ToolSchema> = self.tools.values().map(|t| t.schema()).collect();
        schemas.sort_by(|a, b| a.tool_id.cmp(&b.tool_id));
        schemas
    }

    /// The upstream provider a tool talks to, if the tool exists.
    pub fn provider_of(&self, tool_id: &str) -> Option<&'static str> {
        self.tools.get(tool_id).map(|t| t.provider())
    }

    /// All (tool_id, provider) pairs, sorted by tool_id. Used to assemble the
    /// adapter health report grouped by provider.
    pub fn tool_providers(&self) -> Vec<(String, &'static str)> {
        let mut pairs: Vec<(String, &'static str)> = self
            .tools
            .iter()
            .map(|(id, t)| (id.clone(), t.provider()))
            .collect();
        pairs.sort_by(|a, b| a.0.cmp(&b.0));
        pairs
    }
}

/// Creates an empty tool registry by default.
impl Default for ToolRegistry {
    /// Delegate to [`ToolRegistry::new`] so the registry can be
    /// default-constructed in tests.
    fn default() -> Self {
        Self::new()
    }
}

/// Build a fully-populated `ToolRegistry` with every bundled adapter
/// registered. Called once at startup by the binary and also available to
/// in-process callers.
pub fn build_registry() -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    adapters::register_all(&mut registry);
    registry
}
