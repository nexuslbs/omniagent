use anyhow::Result;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use sqlx::PgPool;

pub mod tools;

/// A tool call requested by the LLM.
#[derive(Debug, Clone)]
pub struct McpToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
}

/// A tool execution result to send back to the LLM.
#[derive(Debug, Clone)]
pub struct McpToolResult {
    pub call_id: String,
    pub content: String,
    pub is_error: bool,
}

/// Shared context available to all MCP tool handlers.
#[derive(Clone)]
pub struct AppContext {
    pub pool: PgPool,
    pub data_dir: String,
    pub qdrant_url: Option<String>,
}

impl AppContext {
    pub fn new(pool: PgPool, data_dir: &str, qdrant_url: Option<String>) -> Self {
        Self {
            pool,
            data_dir: data_dir.to_string(),
            qdrant_url,
        }
    }
}

/// A registered MCP tool.
#[derive(Clone)]
pub struct McpTool {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    pub handler: Arc<dyn Fn(Value, AppContext) -> Result<McpToolResult> + Send + Sync>,
}

/// Registry of all available MCP tools.
#[derive(Clone)]
pub struct McpRegistry {
    tools: HashMap<String, McpTool>,
}

impl McpRegistry {
    pub fn new() -> Self {
        Self {
            tools: HashMap::new(),
        }
    }

    /// Register a tool.
    pub fn register(&mut self, tool: McpTool) {
        self.tools.insert(tool.name.clone(), tool);
    }

    /// Get a tool by name.
    pub fn get(&self, name: &str) -> Option<&McpTool> {
        self.tools.get(name)
    }

    /// Get all tools.
    pub fn all(&self) -> Vec<&McpTool> {
        self.tools.values().collect()
    }

    /// Get tools allowed for a given profile.
    pub fn allowed(&self, allowed_names: &[String]) -> Vec<&McpTool> {
        self.tools
            .values()
            .filter(|t| allowed_names.contains(&t.name))
            .collect()
    }

    /// Execute a tool call.
    pub fn execute(&self, call: &McpToolCall, ctx: AppContext) -> Result<McpToolResult> {
        let tool = self
            .get(&call.name)
            .ok_or_else(|| anyhow::anyhow!("Unknown tool: {}", call.name))?;
        (tool.handler)(call.arguments.clone(), ctx)
    }

    /// Build the OpenAI-compatible tools array for the LLM.
    pub fn to_openai_tools(&self, allowed_names: &[String]) -> Vec<Value> {
        self.allowed(allowed_names)
            .iter()
            .map(|tool| {
                serde_json::json!({
                    "type": "function",
                    "function": {
                        "name": tool.name,
                        "description": tool.description,
                        "parameters": tool.input_schema,
                    }
                })
            })
            .collect()
    }

    /// Build all tools for OpenAI format.
    pub fn to_openai_tools_all(&self) -> Vec<Value> {
        self.all()
            .iter()
            .map(|tool| {
                serde_json::json!({
                    "type": "function",
                    "function": {
                        "name": tool.name,
                        "description": tool.description,
                        "parameters": tool.input_schema,
                    }
                })
            })
            .collect()
    }
}

/// Initialize the default MCP registry with all built-in tools.
pub fn default_registry(ctx: &AppContext) -> McpRegistry {
    let mut registry = McpRegistry::new();

    // Filesystem tools
    registry.register(tools::filesystem::read_tool());
    registry.register(tools::filesystem::write_tool());
    registry.register(tools::filesystem::list_tool());
    registry.register(tools::filesystem::search_tool());
    registry.register(tools::filesystem::info_tool());

    // HTTP fetch tool
    registry.register(tools::fetch::fetch_tool());

    // Search tools
    registry.register(tools::search::search_messages_tool(ctx));
    registry.register(tools::search::search_wiki_tool(ctx));

    registry
}
