use serde_json::Value;
use sqlx::PgPool;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use crate::error::{AppResult, Error};
use crate::platform::OutboundSender;

/// Truncate content to `max_chars` bytes (safe UTF-8 boundary).
/// Appends a truncation note when content exceeds the limit.
pub fn truncate_content(content: &str, max_chars: usize) -> String {
    if content.len() <= max_chars {
        return content.to_string();
    }
    let truncate_at = content
        .char_indices()
        .nth(max_chars)
        .map(|(i, _)| i)
        .unwrap_or(content.len());
    format!(
        "{}...\n\n[... truncated from {} to ~{} chars]",
        &content[..truncate_at],
        content.len(),
        max_chars
    )
}

/// Default maximum output size for tool results (50K chars).
pub const DEFAULT_MAX_TOOL_OUTPUT_CHARS: usize = 50_000;

pub mod external;
pub mod tools;

/// A tool call requested by the LLM.
#[derive(Debug, Clone)]
pub struct McpToolCall {
    #[allow(dead_code)]
    pub id: String,
    pub name: String,
    pub arguments: Value,
}

/// A tool execution result to send back to the LLM.
#[derive(Debug, Clone)]
pub struct McpToolResult {
    #[allow(dead_code)]
    pub call_id: String,
    pub content: String,
    pub is_error: bool,
}

use crate::prompt_builder::MemoryStore;

/// Shared application context, available to all MCP tool handlers.
#[derive(Debug, Clone)]
pub struct AppContext {
    pub pool: PgPool,
    pub readonly_pool: PgPool,
    pub data_dir: String,
    /// Workspace directory for path validation (used by external MCP servers).
    #[allow(dead_code)]
    pub workspace_dir: String,
    pub qdrant_url: Option<String>,
    /// Read-only memory store (MEMORY.md + USER.md) for system prompt injection.
    pub memory_store: Arc<MemoryStore>,
    /// Per-platform outbound delivery senders.  Each platform gets its own
    /// mpsc channel so that a slow/failing platform never blocks others.
    pub platform_senders: HashMap<String, OutboundSender>,
    /// Current thread ID being executed (set by `process_thread` before the
    /// tool-calling loop so MCP tools can auto-detect context without the LLM
    /// having to pass `thread_id` explicitly).
    pub current_thread_id: Option<i64>,
}

impl AppContext {
    /// Create a new application context with a loaded memory store.
    pub fn new(
        pool: PgPool,
        readonly_pool: PgPool,
        data_dir: &str,
        workspace_dir: &str,
        qdrant_url: Option<String>,
        platform_senders: HashMap<String, OutboundSender>,
    ) -> Self {
        // Load memory store from the default profile's memories directory
        let profile_path = format!("{}/profiles/default", data_dir);
        let mut memory_store = MemoryStore::new(&profile_path);
        memory_store.load_from_disk();

        Self {
            pool,
            readonly_pool,
            data_dir: data_dir.to_string(),
            workspace_dir: workspace_dir.to_string(),
            qdrant_url,
            memory_store: Arc::new(memory_store),
            platform_senders,
            current_thread_id: None,
        }
    }
}

/// Async handler type for MCP tool execution.
pub type McpToolHandler =
    Arc<dyn Fn(Value, AppContext) -> Pin<Box<dyn Future<Output = AppResult<McpToolResult>> + Send>> + Send + Sync>;

/// A registered MCP tool.
#[derive(Clone)]
pub struct McpTool {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    pub server_name: Option<String>,
    pub handler: McpToolHandler,
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

    /// Priority ranking for tool ordering — execution tools first, management tools last.
    fn tool_priority(name: &str) -> u8 {
        if name.starts_with("filesystem") {
            0
        } else if name.starts_with("docker") {
            1
        } else if name.starts_with("query_") || name == "fetch" {
            2
        } else if name.starts_with("search_") || name.starts_with("manage_subtask") {
            3
        } else if name == "plugin_manager" || name == "list_plugins" || name == "get_plugin" {
            5
        } else if name.starts_with("kanban") || name.starts_with("cron") {
            10
        } else if name.starts_with("commit_and_push")
            || name.starts_with("create_github")
            || name.starts_with("clone_repo")
        {
            0
        } else {
            4
        }
    }

    /// Get tools allowed for a given profile, sorted by execution priority.
    pub fn allowed(&self, allowed_names: &[String]) -> Vec<&McpTool> {
        let mut tools: Vec<&McpTool> = self
            .tools
            .values()
            .filter(|t| allowed_names.contains(&t.name))
            .collect();
        tools.sort_by_key(|t| Self::tool_priority(&t.name));
        tools
    }

    /// Get the qualified name for a tool: `server_name:name` if it has a server,
    /// or just `name` for built-in tools and unknown tools.
    pub fn qualified_name(&self, name: &str) -> String {
        if let Some(tool) = self.tools.get(name) {
            if let Some(ref sn) = tool.server_name {
                format!("{}:{}", sn, name)
            } else {
                name.to_string()
            }
        } else {
            name.to_string()
        }
    }

    /// Execute a tool call — directly awaits the async handler (no spawn_blocking).
    pub async fn execute(&self, call: &McpToolCall, ctx: AppContext) -> AppResult<McpToolResult> {
        let tool = self
            .get(&call.name)
            .ok_or_else(|| Error::Message(format!("Unknown tool: {}", call.name)))?
            .clone();
        let args = call.arguments.clone();
        (tool.handler)(args, ctx).await
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
    #[allow(dead_code)]
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
pub async fn default_registry(ctx: &AppContext) -> McpRegistry {
    let mut registry = McpRegistry::new();

    // Load enabled/disabled state from tools.yml
    let tool_entries =
        crate::plugins_yaml::load_raw(&ctx.data_dir, &crate::plugins_yaml::PluginYamlType::Tool)
            .unwrap_or_default();

    // ── Built-in tools are loaded from external MCP servers via plugins/mcp/ ──
    // All DB-dependent tools have been externalized to subprocess MCP servers:
    //   fetch, filesystem, docker-compose, skills (Python stdio)
    //   cron, kanban, search, memory, git, query, metrics, subtasks, plugin-manager (Rust stdio)
    // External servers are auto-discovered via load_servers_config() below.
    // The only remaining built-in tools are system actions that call internal Rust functions.
    // Actions tools (built-in system actions) — gated by tools.yml
    for action_tool in crate::mcp::tools::actions::tools() {
        let enabled = tool_entries
            .get(&action_tool.name)
            .map(|e| e.enabled)
            // Built-in tools not in tools.yml are disabled by default
            .unwrap_or(false);
        if enabled {
            registry.register(action_tool);
        } else {
            tracing::info!(
                "Skipping disabled built-in tool '{}' — enable in tools.yml",
                action_tool.name
            );
        }
    }

    // External MCP servers (load from config + plugins/mcp/, best-effort)
    let external_tools = external::client::initialize_external_tools(&ctx.data_dir).await;
    for tool in external_tools {
        registry.register(tool);
    }

    registry
}
