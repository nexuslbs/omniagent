use anyhow::Result;
use serde_json::Value;
use sqlx::PgPool;
use std::collections::HashMap;
use std::sync::Arc;

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
    #[expect(dead_code)]
    pub id: String,
    pub name: String,
    pub arguments: Value,
}

/// A tool execution result to send back to the LLM.
#[derive(Debug, Clone)]
pub struct McpToolResult {
    #[expect(dead_code)]
    pub call_id: String,
    pub content: String,
    #[expect(dead_code)]
    pub is_error: bool,
}

use crate::prompt_builder::MemoryStore;

/// Shared application context, available to all MCP tool handlers.
#[derive(Debug, Clone)]
pub struct AppContext {
    pub pool: PgPool,
    pub readonly_pool: PgPool,
    pub data_dir: String,
    pub workspace_dir: String,
    pub qdrant_url: Option<String>,
    /// Read-only memory store (MEMORY.md + USER.md) for system prompt injection.
    pub memory_store: Arc<MemoryStore>,
    /// Per-platform outbound delivery senders.  Each platform gets its own
    /// mpsc channel so that a slow/failing platform never blocks others.
    pub platform_senders: HashMap<String, OutboundSender>,
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
    #[expect(dead_code)]
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

    // Skill creation tool
    registry.register(tools::skills::create_skill_tool());

    // Kanban tools
    registry.register(tools::kanban::create_kanban_task_tool());
    registry.register(tools::kanban::list_kanban_tasks_tool());
    registry.register(tools::kanban::update_kanban_task_tool());
    registry.register(tools::kanban::delete_kanban_task_tool());
    registry.register(tools::kanban::add_kanban_dependency_tool());
    registry.register(tools::kanban::remove_kanban_dependency_tool());

    // Cron tools
    registry.register(tools::cron::create_cron_job_tool());
    registry.register(tools::cron::list_cron_jobs_tool());
    registry.register(tools::cron::delete_cron_job_tool());
    registry.register(tools::cron::update_cron_job_tool());

    // Memory tools
    registry.register(tools::memory::promote_to_memory_tool());
    registry.register(tools::memory::list_memories_tool());
    registry.register(tools::memory::review_memories_tool());
    registry.register(tools::memory::manage_memory_tool());

    // Metrics tool
    registry.register(tools::metrics::get_metrics_tool());

    // Database query tool (read-only)
    registry.register(tools::query::query_database_tool(ctx));

    // Docker compose tool
    registry.register(tools::docker::compose_tool());

    // Git/GitHub tools
    registry.register(tools::git::create_github_repo_tool());
    registry.register(tools::git::clone_repo_tool());
    registry.register(tools::git::commit_and_push_tool());
    registry.register(tools::git::status_tool());

    // Plugin management tool
    registry.register(tools::plugin_manager::plugin_manager_tool());

    // External MCP servers (load from config, best-effort)
    let external_tools = external::client::initialize_external_tools(&ctx.data_dir);
    for tool in external_tools {
        registry.register(tool);
    }

    registry
}
