use serde_json::Value;
use sql_forge::sql_forge;
use sqlx::FromRow;
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
pub mod memory_tools;

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

// sql_forge row structs for MCP lookups
#[derive(FromRow)]
struct CauseMetadataRow {
    metadata: Value,
}

#[derive(FromRow)]
struct ChannelPlatformRow {
    platform: Option<String>,
}

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
    /// Per-platform outbound delivery senders.  Each platform gets its own
    /// mpsc channel so that a slow/failing platform never blocks others.
    pub platform_senders: HashMap<String, OutboundSender>,
    /// Current thread ID being executed (set by `process_thread` before the
    /// tool-calling loop so MCP tools can auto-detect context without the LLM
    /// having to pass `thread_id` explicitly).
    pub current_thread_id: Option<i64>,
    /// Current channel ID being executed (set per-tool-call so the MCP
    /// client layer can route to the per-channel connection pool).
    pub current_channel_id: Option<i64>,
    /// Profile-allowed tool names for the current thread execution.
    /// Set per-tool-call alongside `current_thread_id` so the
    /// `list_tool_details` introspection tool knows which tools are
    /// actually available to the current profile. Empty = no restriction
    /// (all tools allowed).
    pub current_allowed_tools: Vec<String>,
    /// Current channel name being executed (e.g. "Home", "Engineering").
    /// Set alongside current_channel_id so MCP tools know the channel identity.
    pub current_channel_name: Option<String>,
    /// Current platform identifier (e.g. "telegram", "mattermost").
    /// Set alongside current_channel_id so MCP tools know the platform.
    pub current_platform: Option<String>,
    /// Current profile name being executed.
    /// Set at thread processing time so MCP tools know the active profile.
    pub current_profile_name: Option<String>,
    /// Pre-serialized catalog of ALL registered tool definitions in OpenAI
    /// function format. Used by the `list_tool_details` built-in tool so the
    /// LLM can introspect tool parameters at runtime without relying solely on
    /// error messages. Populated by `default_registry()`.
    pub tool_catalog: Vec<Value>,
    /// Per-platform file readers for the `read_attached_file` MCP tool.
    /// Keyed by platform name (e.g. "mattermost"). Each reader knows how to
    /// fetch file content from that platform's API using file_id + server_url.
    pub platform_file_readers:
        HashMap<String, Arc<dyn crate::platform::external::FileReader + Send + Sync>>,
}

impl AppContext {
    /// Create a new application context.
    pub fn new(
        pool: PgPool,
        readonly_pool: PgPool,
        data_dir: &str,
        workspace_dir: &str,
        qdrant_url: Option<String>,
        platform_senders: HashMap<String, OutboundSender>,
    ) -> Self {
        Self {
            pool,
            readonly_pool,
            data_dir: data_dir.to_string(),
            workspace_dir: workspace_dir.to_string(),
            qdrant_url,
            platform_senders,
            platform_file_readers: HashMap::new(),
            current_thread_id: None,
            current_channel_id: None,
            current_allowed_tools: Vec::new(),
            current_channel_name: None,
            current_platform: None,
            current_profile_name: None,
            tool_catalog: Vec::new(),
        }
    }
}

/// Async handler type for MCP tool execution.
pub type McpToolHandler = Arc<
    dyn Fn(Value, AppContext) -> Pin<Box<dyn Future<Output = AppResult<McpToolResult>> + Send>>
        + Send
        + Sync,
>;

/// Build a fully-qualified tool name using the unified format:
/// `{server}_{tool-name-with-dashes}`
/// Strips redundant server prefix from the tool name when present
/// (e.g. `filesystem` + `filesystem_read` → `filesystem_read`,
/// not `filesystem_filesystem-read`).
/// If stripping the prefix leaves an empty string (server == tool_name),
/// the original tool name is kept so `fetch` + `fetch` → `fetch_fetch`.
pub fn tool_qualify(server: &str, tool_name: &str) -> String {
    // Strip redundant server prefix from tool name if present
    let tool = if let Some(rest) = tool_name.strip_prefix(server) {
        // Remove any leading separator character after the prefix
        let trimmed = rest.trim_start_matches(['-', '_', '.']);
        if trimmed.is_empty() {
            tool_name
        } else {
            trimmed
        }
    } else {
        tool_name
    };
    let dasherized = tool.replace('_', "-");
    format!("{}_{}", server, dasherized)
}

/// A registered MCP tool.
#[derive(Clone)]
pub struct McpTool {
    pub name: String,
    /// Fully-qualified tool name for display/API purposes.
    /// Same as `name` for built-in tools; for external tools this is
    /// the `{server}_{tool}` formatted name from `tool_qualify()`.
    pub full_name: String,
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

impl Default for McpRegistry {
    fn default() -> Self {
        Self::new()
    }
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

    /// Register multiple tools at once (for batch loading from a server).
    pub fn register_all(&mut self, tools: Vec<McpTool>) {
        for tool in tools {
            self.tools.insert(tool.name.clone(), tool);
        }
    }

    /// Remove all tools belonging to a given server.
    /// Returns the names of removed tools.
    pub fn remove_by_server(&mut self, server_name: &str) -> Vec<String> {
        let mut removed = Vec::new();
        self.tools.retain(|name, tool| {
            if tool.server_name.as_deref() == Some(server_name) {
                removed.push(name.clone());
                false
            } else {
                true
            }
        });
        removed
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

    /// Get the qualified name for a tool.
    /// External tools already have `server_name.name` as their registry key,
    /// so it's returned as-is. Built-in tools have no prefix.
    pub fn qualified_name(&self, name: &str) -> String {
        name.to_string()
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
            tracing::info!("Fuzzy-matched tool '{}' -> '{}'", call.name, suggested);
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

/// Build the `read_attached_file` tool: fetch file content from a platform
/// on demand, avoiding inlining large files in the prompt or DB.
fn read_attached_file_tool() -> McpTool {
    use base64::{engine::general_purpose, Engine};

    McpTool {
        name: "read_attached_file".to_string(),
        full_name: tool_qualify("builtin", "read_attached_file"),
        description: "Read the content of an attached file from a platform channel (e.g. Mattermost). \
                      Use this when a file is mentioned in a message but its content was not inlined \
                      (because it exceeds the inline size limit). Provide the `file_id` and optionally \
                      the `server_url` to fetch the file. Returns file content as text or base64."
            .to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "file_id": {
                    "type": "string",
                    "description": "The file identifier from the platform (e.g. Mattermost file_id)."
                },
                "server_url": {
                    "type": "string",
                    "description": "Optional server URL (e.g. http://mattermost:8065). Auto-detected from message metadata if omitted."
                }
            },
            "required": ["file_id"]
        }),
        server_name: None,
        handler: Arc::new(|args: Value, ctx: AppContext| {
            Box::pin(async move {
                let file_id = args
                    .get("file_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .trim()
                    .to_string();

                if file_id.is_empty() {
                    return Ok(McpToolResult {
                        call_id: String::new(),
                        content: "Error: 'file_id' parameter is required.".to_string(),
                        is_error: true,
                    });
                }

                // Determine server_url from args or from cause message metadata
                let server_url = match args.get("server_url").and_then(|v| v.as_str()) {
                    Some(url) if !url.trim().is_empty() => url.trim().to_string(),
                    _ => {
                        // Try to look up from the thread's cause message
                        if let Some(tid) = ctx.current_thread_id {
                            match sql_forge!(
                                CauseMetadataRow,
                                r#"SELECT metadata FROM messages WHERE thread_id = :tid AND role = 'cause' ORDER BY thread_sequence ASC, id ASC LIMIT 1"#,
                                ( :tid = tid )
                            )
                            .fetch_optional(&ctx.pool)
                            .await
                            {
                                Ok(Some(row)) => {
                                    match row.metadata.get("server_url").and_then(|v| v.as_str()) {
                                        Some(url) if !url.is_empty() => url.to_string(),
                                        _ => return Ok(McpToolResult {
                                            call_id: String::new(),
                                            content: "Error: server_url not found in message metadata. Provide it explicitly.".to_string(),
                                            is_error: true,
                                        }),
                                    }
                                }
                                Ok(None) => return Ok(McpToolResult {
                                    call_id: String::new(),
                                    content: "Error: No cause message found for current thread.".to_string(),
                                    is_error: true,
                                }),
                                Err(e) => return Ok(McpToolResult {
                                    call_id: String::new(),
                                    content: format!("Error querying cause message: {}", e),
                                    is_error: true,
                                }),
                            }
                        } else {
                            return Ok(McpToolResult {
                                call_id: String::new(),
                                content: "Error: No current thread and no server_url provided. Pass 'server_url' explicitly.".to_string(),
                                is_error: true,
                            });
                        }
                    }
                };

                // Determine platform from channel
                let platform = if let Some(cid) = ctx.current_channel_id {
                    match sql_forge!(
                        ChannelPlatformRow,
                        r#"SELECT COALESCE(platform, 'mattermost') AS platform FROM channels WHERE id = :cid"#,
                        ( :cid = cid )
                    )
                    .fetch_optional(&ctx.pool)
                    .await
                    {
                        Ok(Some(row)) => row.platform.unwrap_or_else(|| "mattermost".to_string()),
                        _ => "mattermost".to_string(),
                    }
                } else {
                    "mattermost".to_string()
                };

                let reader = match ctx.platform_file_readers.get(&platform) {
                    Some(r) => r.clone(),
                    None => {
                        let available: Vec<&String> = ctx.platform_file_readers.keys().collect();
                        let available_str: Vec<&str> = available.iter().map(|s| s.as_str()).collect();
                        return Ok(McpToolResult {
                            call_id: String::new(),
                            content: format!(
                                "Error: No file reader for platform '{}'. Available: {}",
                                platform,
                                available_str.join(", ")
                            ),
                            is_error: true,
                        });
                    }
                };

                match reader.read_file(&file_id, &server_url).await {
                    Ok(bytes) => {
                        if let Ok(text) = String::from_utf8(bytes.clone()) {
                            Ok(McpToolResult {
                                call_id: String::new(),
                                content: format!(
                                    "📄 File content ({} bytes):\n\n{}",
                                    bytes.len(),
                                    text
                                ),
                                is_error: false,
                            })
                        } else {
                            let b64 = general_purpose::STANDARD.encode(&bytes);
                            Ok(McpToolResult {
                                call_id: String::new(),
                                content: format!(
                                    "📄 Binary file ({} bytes, base64-encoded):\n{}",
                                    bytes.len(),
                                    b64
                                ),
                                is_error: false,
                            })
                        }
                    }
                    Err(e) => Ok(McpToolResult {
                        call_id: String::new(),
                        content: format!("Error reading file '{}': {}", file_id, e),
                        is_error: true,
                    }),
                }
            })
        }),
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
        full_name: tool_qualify("builtin", "list_tool_details"),
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
    let external_tools =
        external::client::initialize_external_tools(&ctx.data_dir, &ctx.workspace_dir).await;
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

    // ── read_attached_file: platform-generic file reading ──
    // Allows the agent to read file attachments that exceed the inline
    // size limit by delegating to the appropriate platform's FileReader.
    registry.register(read_attached_file_tool());

    // ── Built-in memory tools (replace external mcp-server-memory) ──
    // manage_memory, promote_to_memory, list_memories, review_memories
    for tool in memory_tools::all_memory_tools() {
        registry.register(tool);
    }

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
            let cost = if a_chars[i - 1] == b_chars[j - 1] {
                0
            } else {
                1
            };
            curr[j] = std::cmp::min(
                std::cmp::min(curr[j - 1] + 1, prev[j] + 1),
                prev[j - 1] + cost,
            );
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[b_len]
}
