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
    /// Profile-allowed tool names for the current thread execution.
    /// Set per-tool-call alongside `current_thread_id` so the
    /// `list_tool_details` introspection tool knows which tools are
    /// actually available to the current profile. Empty = no restriction
    /// (all tools allowed).
    pub current_allowed_tools: Vec<String>,
    /// Pre-serialized catalog of ALL registered tool definitions in OpenAI
    /// function format. Used by the `list_tool_details` built-in tool so the
    /// LLM can introspect tool parameters at runtime without relying solely on
    /// error messages. Populated by `default_registry()`.
    pub tool_catalog: Vec<Value>,
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
            current_allowed_tools: Vec::new(),
            tool_catalog: Vec::new(),
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

    /// Priority ranking for tool ordering — all tools have equal priority.
    fn tool_priority(_name: &str) -> u8 {
        0
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
        // Try exact match first
        if let Some(tool) = self.get(&call.name) {
            let tool = tool.clone();
            let args = call.arguments.clone();
            let result = (tool.handler)(args.clone(), ctx).await;
            return match result {
                Ok(r) => {
                    if r.is_error {
                        // External MCP servers return errors
                        // as Ok(result) with is_error=true. Enrich the error with
                        // the tool's input schema so the LLM can self-correct.
                        let schema_str = serde_json::to_string_pretty(&tool.input_schema)
                            .unwrap_or_else(|_| "(unavailable)".to_string());
                        Ok(McpToolResult {
                            content: format!(
                                "{}\n\nExpected parameters:\n{}",
                                r.content, schema_str
                            ),
                            is_error: true,
                            ..r
                        })
                    } else {
                        Ok(r)
                    }
                }
                Err(e) => {
                    // Enrich error with tool's input_schema so the LLM can
                    // self-correct invalid parameter names or missing fields.
                    let schema_str = serde_json::to_string_pretty(&tool.input_schema)
                        .unwrap_or_else(|_| "(unavailable)".to_string());
                    Err(Error::Message(format!(
                        "Tool '{}' failed: {}\n\nExpected parameters:\n{}",
                        tool.name, e, schema_str
                    )))
                }
            };
        }
        // Fuzzy match: find closest tool name by Levenshtein distance
        let mut candidates: Vec<(&str, usize)> = self
            .tools
            .keys()
            .map(|n| (n.as_str(), levenshtein_distance(&call.name, n)))
            .collect();
        candidates.sort_by_key(|&(_, dist)| dist);
        let suggestion = candidates
            .first()
            .filter(|(_, dist)| *dist <= 3 && *dist < call.name.len())
            .map(|(name, _)| *name);
        if let Some(suggested) = suggestion {
            // Execute the suggested tool instead
            tracing::info!(
                "Fuzzy-matched tool '{}' -> '{}'",
                call.name,
                suggested
            );
            if let Some(tool) = self.get(suggested) {
                let tool = tool.clone();
                let args = call.arguments.clone();
                return (tool.handler)(args, ctx).await;
            }
        }
        // No match found
        let suggestion_msg = if let Some(s) = suggestion {
            format!(". Did you mean '{}'?", s)
        } else {
            String::new()
        };
        Err(Error::Message(format!(
            "Unknown tool: {}{}",
            call.name, suggestion_msg
        )))
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

/// Build the `list_tool_details` introspection tool.
///
/// This tool allows the LLM to request the full definition (description, input
/// schema) of any registered tool at runtime. It reads from a pre-populated
/// catalog on AppContext, avoiding the cost of serializing the registry each call.
fn list_tool_details_tool() -> McpTool {
    McpTool {
        name: "list_tool_details".to_string(),
        description: "Get the full definition (description, input schema / expected parameters) for a specific tool by name. Use this when a tool call returns an error about missing or invalid parameters — call this first to see the correct parameter names and types before retrying.".to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "tool_name": {
                    "type": "string",
                    "description": "The exact name of the tool to inspect (e.g. 'filesystem_read', 'kanban_create_task'). Pass a single tool name. Returns the tool's description and complete parameter schema."
                }
            },
            "required": ["tool_name"]
        }),
        server_name: None,
        handler: Arc::new(|args: Value, ctx: AppContext| {
            Box::pin(async move {
                let tool_name = args
                    .get("tool_name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .trim();
                if tool_name.is_empty() {
                    return Ok(McpToolResult {
                        call_id: String::new(),
                        content: "Error: 'tool_name' parameter is required.".to_string(),
                        is_error: true,
                    });
                }

                // Search the catalog for a tool with matching name
                let allowed = &ctx.current_allowed_tools;
                let unrestricted = allowed.is_empty();

                for tool_def in &ctx.tool_catalog {
                    if let Some(name) = tool_def
                        .pointer("/function/name")
                        .and_then(|v| v.as_str())
                    {
                        if name == tool_name {
                            // Check if the tool is allowed by the current profile
                            let status = if unrestricted || allowed.contains(&name.to_string()) {
                                "AVAILABLE".to_string()
                            } else {
                                format!(
                                    "RESTRICTED — not in profile allowed_tools ({}/{} tools allowed)",
                                    allowed.len(),
                                    ctx.tool_catalog.len()
                                )
                            };
                            let pretty = serde_json::to_string_pretty(tool_def)
                                .unwrap_or_else(|_| "(serialization error)".to_string());
                            return Ok(McpToolResult {
                                call_id: String::new(),
                                content: format!(
                                    "Tool '{}': {}\n\n{}",
                                    tool_name, status, pretty
                                ),
                                is_error: false,
                            });
                        }
                    }
                }

                // Tool not found — list available tools (restricted by profile if applicable)
                let allowed = &ctx.current_allowed_tools;
                let is_restricted = !allowed.is_empty();
                let catalog_tools: Vec<&str> = ctx
                    .tool_catalog
                    .iter()
                    .filter_map(|t| {
                        t.pointer("/function/name")
                            .and_then(|v| v.as_str())
                    })
                    .collect();

                // Show only allowed tools if restricted, otherwise all
                let visible: Vec<&str> = if is_restricted {
                    catalog_tools
                        .into_iter()
                        .filter(|name| allowed.contains(&name.to_string()))
                        .collect()
                } else {
                    catalog_tools
                };

                let header = if is_restricted {
                    format!(
                        "Unknown tool '{}'. Tools available to this profile ({}):",
                        tool_name,
                        visible.len()
                    )
                } else {
                    format!(
                        "Unknown tool '{}'. Available tools ({}):",
                        tool_name,
                        visible.len()
                    )
                };

                Ok(McpToolResult {
                    call_id: String::new(),
                    content: format!("{}\n{}", header, visible.join(", ")),
                    is_error: true,
                })
            })
        }),
    }
}

/// Initialize the default MCP registry with all built-in and external tools.
pub async fn default_registry(ctx: &mut AppContext) -> McpRegistry {
    let mut registry = McpRegistry::new();

    // ── External MCP servers are loaded from config + plugins/mcp/ ──
    // All tools are loaded from external subprocess MCP servers:
    //   fetch, filesystem, skills (Python stdio)
    //   cron, kanban, search, memory, git, query, metrics, subtasks, plugin-manager, actions (Rust stdio)
    // External servers are auto-discovered via load_servers_config() below.

    // External MCP servers (load from config + plugins/mcp/, best-effort)
    let external_tools = external::client::initialize_external_tools(&ctx.data_dir).await;
    for tool in external_tools {
        registry.register(tool);
    }

    // Populate tool catalog (all registered tool definitions in OpenAI function format)
    // so the list_tool_details introspection tool can serve them to the LLM.
    ctx.tool_catalog = registry.to_openai_tools_all();

    // ── list_tool_details: always-available introspection tool ──
    // Registered last so the catalog excludes itself (it reads from AppContext.tool_catalog
    // which was populated just above).
    registry.register(list_tool_details_tool());

    tracing::info!(
        "MCP registry initialized with {} tools (external + built-in)",
        registry.all().len()
    );

    registry
}

/// Compute Levenshtein distance between two strings (case-insensitive).
/// Used for fuzzy-matching unknown tool names to registered tool names.
fn levenshtein_distance(a: &str, b: &str) -> usize {
    let a = a.to_lowercase();
    let b = b.to_lowercase();
    let a_chars: Vec<char> = a.chars().collect();
    let b_chars: Vec<char> = b.chars().collect();
    let a_len = a_chars.len();
    let b_len = b_chars.len();
    // Early exit for empty strings
    if a_len == 0 {
        return b_len;
    }
    if b_len == 0 {
        return a_len;
    }
    // Use two-row DP (optimized)
    let mut prev: Vec<usize> = (0..=b_len).collect();
    let mut curr: Vec<usize> = vec![0; b_len + 1];
    for i in 1..=a_len {
        curr[0] = i;
        for j in 1..=b_len {
            let cost = if a_chars[i - 1] == b_chars[j - 1] { 0 } else { 1 };
            curr[j] = std::cmp::min(
                std::cmp::min(curr[j - 1] + 1, prev[j] + 1),
                prev[j - 1] + cost,
            );
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[b_len]
}
