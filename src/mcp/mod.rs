use serde_json::Value;
use sql_forge::sql_forge;
use sqlx::FromRow;
use sqlx::PgPool;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use tokio::sync::RwLock;

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
/// Default maximum chars of a tool result kept inline in the `tool-result`
/// message (settings `max_inline_chars`). Larger results are spilled to
/// `{OMNI_DIR}/data/spill` (full content on disk) and the message carries a
/// bounded preview + locator instead. Same default as
/// `DEFAULT_MAX_TOOL_OUTPUT_CHARS`.
pub const DEFAULT_MAX_INLINE_CHARS: usize = 50_000;

/// Result of spilling an oversized tool result.
#[derive(Debug, Clone, PartialEq)]
pub struct SpilledOutput {
    /// Content to inline in the tool-result message: the original text when
    /// under the threshold, otherwise a bounded head/tail preview + locator.
    pub inline: String,
    /// Path of the spill file when the content was spilled, else `None`.
    pub spill_path: Option<std::path::PathBuf>,
}

/// Head chars kept in a spill preview for a given inline budget (3/5).
pub fn spill_preview_head_chars(max_inline_chars: usize) -> usize {
    max_inline_chars * 3 / 5
}

/// Tail chars kept in a spill preview for a given inline budget (2/5).
pub fn spill_preview_tail_chars(max_inline_chars: usize) -> usize {
    max_inline_chars * 2 / 5
}

/// Longest UTF-8-safe prefix of `s` that fits in `max_bytes`.
fn utf8_safe_prefix(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Sanitize a filename segment for spill files: keep `[A-Za-z0-9._-]`,
/// replace everything else with `_`, collapse `..`, strip leading/trailing
/// dots/underscores, cap at 80 chars. Never returns an empty string and never
/// produces a path separator or `..` traversal.
pub fn sanitize_spill_segment(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut last_underscore = false;
    for c in raw.chars() {
        if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
            out.push(c);
            last_underscore = false;
        } else if !last_underscore {
            out.push('_');
            last_underscore = true;
        }
    }
    let out = out.replace("..", "_");
    let out = out.trim_matches('.').trim_matches('_').to_string();
    let out = if out.is_empty() {
        "result".to_string()
    } else {
        out
    };
    if out.len() > 80 {
        out[..80].to_string()
    } else {
        out
    }
}

/// Compose the bounded preview that replaces an oversized tool result inline:
/// head + tail (UTF-8 safe) plus an explicit locator line the model can feed
/// to `filesystem_read` to recover the full output.
pub fn compose_spill_preview(
    content: &str,
    max_inline_chars: usize,
    spill_path: &std::path::Path,
) -> String {
    let head_chars = spill_preview_head_chars(max_inline_chars);
    let tail_chars = spill_preview_tail_chars(max_inline_chars);
    let total = content.len();
    let head = utf8_safe_prefix(content, head_chars);
    let mut tail_start = total.saturating_sub(tail_chars);
    while tail_start < total && !content.is_char_boundary(tail_start) {
        tail_start += 1;
    }
    let tail = &content[tail_start..];
    // Guard: if head+tail already covers everything, don't duplicate it.
    if head.len() + tail.len() >= total {
        return content.to_string();
    }
    let omitted = total - head.len() - tail.len();
    format!(
        "{head}\n\n[... {omitted} chars omitted — see full output below ...]\n\n{tail}\n\n[full output: {}]",
        spill_path.display()
    )
}

/// Write `content` to `path` with exclusive creation (`wx`): fails if the file
/// already exists and never follows symlinks. Permissions 0600 on unix.
fn write_spill_file(path: &std::path::Path, content: &str) -> std::io::Result<()> {
    use std::io::Write;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(path)?;
    f.write_all(content.as_bytes())?;
    f.sync_all().ok();
    Ok(())
}

/// Spill an oversized tool result to a session-scoped file.
///
/// When `content` exceeds `max_inline_chars` chars the FULL text is persisted
/// to `{spill_root}/{thread_id}/{call_id}-{tool}.txt` (exclusive create, 0600)
/// and the returned inline content is a bounded head/tail preview + locator.
/// Under the threshold the content is returned unchanged. Any write failure
/// degrades to the classic inline truncation so the message is never lost.
pub fn spill_tool_result(
    content: &str,
    thread_id: i64,
    call_id: &str,
    tool_name: &str,
    spill_root: &std::path::Path,
    max_inline_chars: usize,
) -> SpilledOutput {
    if content.len() <= max_inline_chars {
        return SpilledOutput {
            inline: content.to_string(),
            spill_path: None,
        };
    }
    let thread_dir = spill_root.join(thread_id.to_string());
    if let Err(e) = std::fs::create_dir_all(&thread_dir) {
        tracing::warn!(
            "spill: cannot create spill dir {} ({e}); falling back to inline truncation",
            thread_dir.display()
        );
        return SpilledOutput {
            inline: truncate_content(content, max_inline_chars),
            spill_path: None,
        };
    }
    let safe_call = sanitize_spill_segment(call_id);
    let safe_tool = sanitize_spill_segment(tool_name);
    let base_name = format!("{safe_call}-{safe_tool}");
    let mut path = thread_dir.join(format!("{base_name}.txt"));
    let mut written = false;
    for attempt in 0..8 {
        match write_spill_file(&path, content) {
            Ok(()) => {
                written = true;
                break;
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                path = thread_dir.join(format!("{base_name}-{}.txt", attempt + 1));
            }
            Err(e) => {
                tracing::warn!(
                    "spill: cannot write {} ({e}); falling back to inline truncation",
                    path.display()
                );
                return SpilledOutput {
                    inline: truncate_content(content, max_inline_chars),
                    spill_path: None,
                };
            }
        }
    }
    if !written {
        tracing::warn!("spill: could not allocate a unique spill file for {base_name}");
        return SpilledOutput {
            inline: truncate_content(content, max_inline_chars),
            spill_path: None,
        };
    }
    SpilledOutput {
        inline: compose_spill_preview(content, max_inline_chars, &path),
        spill_path: Some(path),
    }
}

pub mod external;
pub mod task_tools;

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

/// Shared application context, available to all MCP tool handlers.
#[derive(Debug, Clone)]
pub struct AppContext {
    pub pool: PgPool,
    pub readonly_pool: PgPool,
    pub data_dir: String,
    /// Per-platform outbound delivery senders.  Each platform gets its own
    /// mpsc channel so that a slow/failing platform never blocks others.
    /// Wrapped in Arc<RwLock> so new platforms can be dynamically added at
    /// runtime via the API.
    pub platform_senders: Arc<RwLock<HashMap<String, OutboundSender>>>,
    /// Current thread ID being executed (set by `process_thread` before the
    /// tool-calling loop so MCP tools can auto-detect context without the LLM
    /// having to pass `thread_id` explicitly).
    pub current_thread_id: Option<i64>,
    /// Current channel ID (== channel NAME, the channels.yml key) being
    /// executed (set per-tool-call so MCP tools know the channel identity).
    pub current_channel_id: Option<String>,
    /// Profile-allowed tool names for the current thread execution.
    /// Set per-tool-call alongside `current_thread_id` so the
    /// `list_tool_details` introspection tool knows which tools are
    /// actually available to the current profile. Empty = no restriction
    /// (all tools allowed).
    pub current_allowed_tools: Vec<String>,
    /// Current channel name being executed (e.g. "Home", "Engineering").
    /// Set alongside current_channel_id so MCP tools know the channel identity.
    pub current_channel_name: Option<String>,
    /// Current platform identifier (e.g. &quot;telegram&quot;, &quot;slack&quot;).
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
    /// Per-platform references for the `read_attached_file` MCP tool.
    /// Keyed by platform name. Each platform plugin implements `read_file`
    /// internally, so the core stays plugin-agnostic — no knowledge of
    /// plugin-specific config fields like `access_token`.
    /// Wrapped in Arc<RwLock> so platforms can be dynamically added/removed.
    pub platforms: Arc<RwLock<HashMap<String, Arc<dyn crate::platform::Platform>>>>,
    /// External MCP client registry. One client per server, shared across
    /// all channels. Replaces the former per-channel PoolManager.
    pub external_clients: Arc<crate::mcp::external::client::ExternalMcpClients>,
}

impl AppContext {
    pub fn new(
        pool: PgPool,
        readonly_pool: PgPool,
        data_dir: &str,
        platform_senders: HashMap<String, OutboundSender>,
        external_clients: Arc<crate::mcp::external::client::ExternalMcpClients>,
    ) -> Self {
        Self {
            pool,
            readonly_pool,
            data_dir: data_dir.to_string(),
            platform_senders: Arc::new(RwLock::new(platform_senders)),
            platforms: Arc::new(RwLock::new(HashMap::new())),
            current_thread_id: None,
            current_channel_id: None,
            current_allowed_tools: Vec::new(),
            current_channel_name: None,
            current_platform: None,
            current_profile_name: None,
            tool_catalog: Vec::new(),
            external_clients,
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
    /// Maximum time in seconds to wait for this tool to complete.
    /// `None` = NO timeout (tool runs until it finishes, errors, or the agent
    /// cancels it). A timeout exists ONLY when explicitly set — either by the
    /// tool's own declaration (e.g. builtin wait-task = 310s) or by an agent
    /// config that opts in. There is deliberately NO default fallback: fixed
    /// tool timeouts were removed (Aug 2026) because background tasks now give
    /// the agent full tracking/cancel/log control — a tool must never be
    /// killed by an invisible clock the agent didn't set.
    pub timeout_secs: Option<u64>,
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
        self.tools.insert(tool.full_name.clone(), tool);
    }

    /// Register multiple tools at once (for batch loading from a server).
    pub fn register_all(&mut self, tools: Vec<McpTool>) {
        for tool in tools {
            self.tools.insert(tool.full_name.clone(), tool);
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

    /// Priority ranking for tool ordering: all tools have equal priority.
    fn tool_priority(_name: &str) -> u8 {
        0
    }

    /// Get tools allowed for a given profile, sorted by execution priority.
    pub fn allowed(&self, allowed_names: &[String]) -> Vec<&McpTool> {
        let mut tools: Vec<&McpTool> = self
            .tools
            .values()
            .filter(|t| allowed_names.contains(&t.full_name))
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

    /// Get the timeout in seconds for a tool by name.
    /// Returns `None` when the tool has no explicit timeout (run until done).
    pub fn get_timeout_secs(&self, name: &str) -> Option<u64> {
        self.get(name).and_then(|t| t.timeout_secs)
    }

    /// Execute a tool call: directly awaits the async handler (no spawn_blocking).
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
                        "name": tool.full_name,
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
                        "name": tool.full_name,
                        "description": tool.description,
                        "parameters": tool.input_schema,
                    }
                })
            })
            .collect()
    }
}

/// Build the `poll-task` tool: check the status of a background task.
fn poll_task_tool() -> McpTool {
    McpTool {
        name: "builtin_poll-task".to_string(),
        full_name: tool_qualify("builtin", "poll_task"),
        description: "Check the status of a previously started background tool task. Returns the task's current status (running/completed/failed/cancelled), elapsed time, and result if done.".to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "task_id": {
                    "type": "string",
                    "description": "The task ID returned from a previous tool call that returned status=processing"
                }
            },
            "required": ["task_id"]
        }),
        server_name: None,
        timeout_secs: Some(10),
        handler: std::sync::Arc::new(|args: Value, ctx: crate::mcp::AppContext| {
            Box::pin(crate::mcp::task_tools::handle_poll_task(args, ctx))
        }),
    }
}

/// Build the `wait-task` tool: wait for a background task to complete.
fn wait_task_tool() -> McpTool {
    McpTool {
        name: "builtin_wait-task".to_string(),
        full_name: tool_qualify("builtin", "wait_task"),
        description: "Wait for a background tool task to complete, with a configurable timeout. Polls every 500ms and returns the result when done, or a timeout status if the task doesn't finish in time.".to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "task_id": {
                    "type": "string",
                    "description": "The task ID returned from a previous tool call that returned status=processing"
                },
                "timeout_secs": {
                    "type": "integer",
                    "description": "Maximum seconds to wait (default: 900). The tool polls every 500ms and returns as soon as the task finishes, so a long value costs nothing for fast tasks and avoids burning an iteration per 30s on long ones. Use 900-1800 for a Rust cargo build or full dev-stack setup. There is NO hard cap; the handler self-bounds by this argument.",
                    "default": 900
                },
                "tail": {
                    "type": "integer",
                    "description": "Maximum characters of logs to return, truncated from the end (default: 1000, 0 for no limit)",
                    "default": 1000
                }
            },
            "required": ["task_id"]
        }),
        server_name: None,
        // No declared timeout: the handler bounds itself by its own
        // `timeout_secs` argument and returns a timeout STATUS (not an error)
        // when exceeded — an external kill clock would cut a legitimately
        // long wait short and force the agent into extra wait calls.
        timeout_secs: None,
        handler: std::sync::Arc::new(|args: Value, ctx: crate::mcp::AppContext| {
            Box::pin(crate::mcp::task_tools::handle_wait_task(args, ctx))
        }),
    }
}

/// Build the `cancel-task` tool: cancel a running background task.
fn cancel_task_tool() -> McpTool {
    McpTool {
        name: "builtin_cancel-task".to_string(),
        full_name: tool_qualify("builtin", "cancel_task"),
        description: "Cancel a running background task. The task's abort signal is sent and it will stop as soon as possible. Use when the task is no longer needed.".to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "task_id": {
                    "type": "string",
                    "description": "The task ID to cancel"
                }
            },
            "required": ["task_id"]
        }),
        server_name: None,
        timeout_secs: Some(10),
        handler: std::sync::Arc::new(|args: Value, ctx: crate::mcp::AppContext| {
            Box::pin(crate::mcp::task_tools::handle_cancel_task(args, ctx))
        }),
    }
}

/// Build the `read-task-logs` tool: stream log output from a background task.
fn read_task_logs_tool() -> McpTool {
    McpTool {
        name: "builtin_read-task-logs".to_string(),
        full_name: tool_qualify("builtin", "read_task_logs"),
        description: "Read intermediate log output from a running or completed background task. Supports cursor-based pagination for long logs.".to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "task_id": {
                    "type": "string",
                    "description": "The task ID to read logs from"
                },
                "cursor": {
                    "type": "integer",
                    "description": "Line offset to start reading from (default: 0)"
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum number of lines to return (default: 100, max: 1000)"
                }
            },
            "required": ["task_id"]
        }),
        server_name: None,
        timeout_secs: Some(10),
        handler: std::sync::Arc::new(|args: Value, ctx: crate::mcp::AppContext| {
            Box::pin(crate::mcp::task_tools::handle_read_task_logs(args, ctx))
        }),
    }
}

/// Build the `read-attached-file` tool: fetch file content from a platform
/// on demand, avoiding inlining large files in the prompt or DB.
fn read_attached_file_tool() -> McpTool {
    use base64::{engine::general_purpose, Engine};

    McpTool {
        name: "builtin_read-attached-file".to_string(),
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
                    "description": "Optional server URL. Auto-detected from message metadata if omitted."
                }
            },
            "required": ["file_id"]
        }),
        server_name: None,
        timeout_secs: None,
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

                // Determine platform from channel (channels.yml; id == name)
                let platform = if let Some(cid) = ctx.current_channel_id {
                    crate::db::channels::get_channel_by_name(&ctx.pool, &cid)
                        .await
                        .ok()
                        .flatten()
                        .and_then(|c| c.platform)
                        .unwrap_or_default()
                } else {
                    String::new()
                };

                let platforms_guard = ctx.platforms.read().await;
                let platform_client = match platforms_guard.get(&platform) {
                    Some(p) => p.clone(),
                    None => {
                        let available: Vec<String> = platforms_guard.keys().cloned().collect();
                        return Ok(McpToolResult {
                            call_id: String::new(),
                            content: format!(
                                "Error: No platform client for '{}'. Available: {}",
                                platform,
                                available.join(", ")
                            ),
                            is_error: true,
                        });
                    }
                };
                drop(platforms_guard);

                match platform_client.read_file(&file_id, &server_url).await {
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
        description: "Get the full definition (description, input schema / expected parameters) for a specific tool by name. Use this when a tool call returns an error about missing or invalid parameters: call this first to see the correct parameter names and types before retrying.".to_string(),
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
        timeout_secs: None,
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
                                    "RESTRICTED: not in profile allowed_tools ({}/{} tools allowed)",
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

                // Tool not found: list available tools (restricted by profile if applicable)
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
    // Pass the DB pool so $secret:NAME refs in plugin configs resolve to real
    // secret values (e.g. git plugin's GITHUB_APP_KEY) instead of passing the
    // literal "$secret:..." string to the subprocess / configure message.
    let external_tools = external::client::initialize_external_tools(
        &ctx.data_dir,
        Some(&ctx.pool),
        &ctx.external_clients,
    )
    .await;
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
    // size limit by delegating to the platform's read_file implementation.
    registry.register(read_attached_file_tool());

    // ── Task management tools for non-blocking tool execution ──
    registry.register(poll_task_tool());
    registry.register(wait_task_tool());
    registry.register(cancel_task_tool());
    registry.register(read_task_logs_tool());
    registry.register(omniagent_api_tool());
    registry.register(fail_thread_tool());

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

/// Build the `omniagent-api` tool: generic fetch-like HTTP client for the
/// core omniagent API (localhost:8080). Replaces the cron/kanban plugin MCP
/// tools with ONE generic tool: method + path + optional JSON body. Covers
/// kanban task CRUD (/kanban/tasks...), schedule CRUD (/schedule... including
/// DELETE /schedule/{id}), run-cron (/schedule/{id}/run), review
/// (/kanban/tasks/{id}/review), plugins and actions endpoints.
fn omniagent_api_tool() -> McpTool {
    McpTool {
        name: "builtin_omniagent-api".to_string(),
        full_name: tool_qualify("builtin", "omniagent_api"),
        description: "Call the core omniagent HTTP API (localhost:8080). Specify an HTTP method, an API path and an optional JSON body; returns the response body as text. Covers kanban task CRUD (/kanban/tasks...), schedule CRUD (/schedule, /schedule/{id} incl. DELETE), run-cron (/schedule/{id}/run), review (/kanban/tasks/{id}/review), plugins and actions endpoints. This replaces the old kanban_*/cron_* plugin tools.".to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "method": {
                    "type": "string",
                    "enum": ["GET", "POST", "PATCH", "DELETE"],
                    "description": "HTTP method"
                },
                "path": {
                    "type": "string",
                    "description": "API path, e.g. /kanban/tasks, /schedule, /schedule/{id}, /schedule/{id}/run"
                },
                "body": {
                    "type": "object",
                    "description": "Optional JSON body for POST/PATCH requests"
                }
            },
            "required": ["method", "path"]
        }),
        server_name: None,
        timeout_secs: Some(30),
        handler: std::sync::Arc::new(|args: Value, _ctx: crate::mcp::AppContext| {
            Box::pin(async move {
                let method = args
                    .get("method")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_uppercase();
                let path = args
                    .get("path")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .trim()
                    .to_string();
                if method.is_empty() || path.is_empty() {
                    return Ok(McpToolResult {
                        call_id: String::new(),
                        content: "Error: both 'method' and 'path' are required.".to_string(),
                        is_error: true,
                    });
                }
                let url = format!("http://localhost:8080{}", path);
                let client = reqwest::Client::builder()
                    .timeout(std::time::Duration::from_secs(30))
                    .build()
                    .unwrap_or_else(|_| reqwest::Client::new());
                let method_parsed = match method.as_str() {
                    "GET" => reqwest::Method::GET,
                    "POST" => reqwest::Method::POST,
                    "PATCH" => reqwest::Method::PATCH,
                    "DELETE" => reqwest::Method::DELETE,
                    m => {
                        return Ok(McpToolResult {
                            call_id: String::new(),
                            content: format!("Error: unsupported method '{}'", m),
                            is_error: true,
                        })
                    }
                };
                let mut req = client.request(method_parsed, &url);
                if let Some(body) = args.get("body") {
                    if !body.is_null() {
                        req = req.json(body);
                    }
                }
                match req.send().await {
                    Ok(resp) => {
                        let status = resp.status().as_u16();
                        let text = resp.text().await.unwrap_or_default();
                        Ok(McpToolResult {
                            call_id: String::new(),
                            content: format!("HTTP {}\n{}", status, text),
                            is_error: status >= 400,
                        })
                    }
                    Err(e) => Ok(McpToolResult {
                        call_id: String::new(),
                        content: format!("Error calling omniagent API: {}", e),
                        is_error: true,
                    }),
                }
            })
        }),
    }
}

/// Builder for the builtin `fail-thread` tool (Phase 2): ends the current
/// thread as FAILED with an Error-type last message and applies the
/// metadata.workflow_step kanban transition (spec §8 N1, §3 F0-F4).
fn fail_thread_tool() -> McpTool {
    McpTool {
        name: "builtin_fail-thread".to_string(),
        full_name: tool_qualify("builtin", "fail_thread"),
        description: "End the current thread as FAILED with an Error-type last message and apply the metadata.workflow_step kanban transition. workflow_step accepts STEP keys only: \"running\", \"testing\", \"blocked\" (empty string = executor default). Any other value (e.g. \"review\" or role names) is invalid and blocks the task.".to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "workflow_step": {
                    "type": "string",
                    "enum": ["", "running", "testing", "blocked"],
                    "description": "Target workflow step for the failing thread: empty = executor default (F0); running = executor rework (F1); testing = re-test (F2); blocked = block the task (F3). Invalid values (incl. review / role names) block the task (F4)."
                },
                "reason": {
                    "type": "string",
                    "description": "Optional reason text stored in the Error-type final message."
                }
            },
            "required": ["workflow_step"]
        }),
        server_name: None,
        timeout_secs: None,
        handler: Arc::new(|args: Value, ctx: AppContext| {
            Box::pin(crate::mcp::task_tools::handle_fail_thread(args, ctx))
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ─── truncate_content tests ───

    #[test]
    fn test_truncate_content_short_enough() {
        assert_eq!(truncate_content("hello", 10), "hello");
    }

    #[test]
    fn test_truncate_content_exact_boundary() {
        assert_eq!(truncate_content("hello", 5), "hello");
    }

    #[test]
    fn test_truncate_content_truncated() {
        let result = truncate_content("hello world this is long", 5);
        assert!(result.starts_with("hello"));
        assert!(result.contains("[... truncated from"));
    }

    #[test]
    fn test_truncate_content_empty() {
        assert_eq!(truncate_content("", 10), "");
    }

    #[test]
    fn test_truncate_content_multi_byte_utf8() {
        // Use content with multi-byte characters and truncate
        let result = truncate_content("héllo wörld", 5);
        // Should not panic, should give some truncated string
        // Note: truncation suffix can make result longer than original
        assert!(result.starts_with("héllo") || result.starts_with("héll"));
        assert!(result.contains("[... truncated from"));
    }

    #[test]
    fn test_truncate_content_shows_correct_stats() {
        let content = "hello world this is a long message";
        let result = truncate_content(content, 10);
        // Extract the actual length from the truncation note
        assert!(result.contains("[... truncated from "));
        assert!(result.contains(&format!("{}", content.len())));
    }

    // ─── tool_qualify tests ───

    #[test]
    fn test_tool_qualify_normal() {
        assert_eq!(tool_qualify("filesystem", "read"), "filesystem_read");
    }

    #[test]
    fn test_tool_qualify_redundant_prefix() {
        assert_eq!(
            tool_qualify("filesystem", "filesystem_read"),
            "filesystem_read"
        );
    }

    #[test]
    fn test_tool_qualify_redundant_prefix_with_dash() {
        assert_eq!(tool_qualify("my-srv", "my-srv-read"), "my-srv_read");
    }

    #[test]
    fn test_tool_qualify_empty_after_stripping() {
        assert_eq!(tool_qualify("fetch", "fetch"), "fetch_fetch");
    }

    #[test]
    fn test_tool_qualify_underscore_to_dash_in_tool_name() {
        assert_eq!(tool_qualify("server", "my_tool"), "server_my-tool");
    }

    #[test]
    fn test_tool_qualify_redundant_with_dash_separator() {
        assert_eq!(tool_qualify("server", "server.my_tool"), "server_my-tool");
    }

    #[test]
    fn test_tool_qualify_redundant_with_underscore_separator() {
        assert_eq!(tool_qualify("server", "server_my_tool"), "server_my-tool");
    }

    // ─── levenshtein_distance tests ───

    #[test]
    fn test_levenshtein_equal() {
        assert_eq!(levenshtein_distance("hello", "hello"), 0);
    }

    #[test]
    fn test_levenshtein_case_insensitive() {
        assert_eq!(levenshtein_distance("Hello", "hello"), 0);
    }

    #[test]
    fn test_levenshtein_completely_different() {
        assert_eq!(levenshtein_distance("abc", "xyz"), 3);
    }

    #[test]
    fn test_levenshtein_one_empty() {
        assert_eq!(levenshtein_distance("", "hello"), 5);
        assert_eq!(levenshtein_distance("hello", ""), 5);
    }

    #[test]
    fn test_levenshtein_single_diff() {
        assert_eq!(
            levenshtein_distance("filesystem_read", "filesystem_reax"),
            1
        );
    }

    #[test]
    fn test_levenshtein_insertion() {
        assert_eq!(levenshtein_distance("cat", "cats"), 1);
    }

    #[test]
    fn test_levenshtein_deletion() {
        assert_eq!(levenshtein_distance("cats", "cat"), 1);
    }

    #[test]
    fn test_levenshtein_substitution() {
        assert_eq!(levenshtein_distance("kitten", "sitten"), 1);
    }

    #[test]
    fn test_levenshtein_case_insensitive_mixed() {
        assert_eq!(levenshtein_distance("ABC", "abc"), 0);
        assert_eq!(levenshtein_distance("AbC", "aBc"), 0);
    }

    // ─── McpRegistry tests ───

    fn make_test_handler() -> McpToolHandler {
        Arc::new(|_args: Value, _ctx: AppContext| {
            Box::pin(async {
                Ok(McpToolResult {
                    call_id: String::new(),
                    content: "ok".to_string(),
                    is_error: false,
                })
            })
        })
    }

    fn make_tool(name: &str, server: Option<&str>, timeout: Option<u64>) -> McpTool {
        let full_name = if let Some(srv) = server {
            tool_qualify(srv, name)
        } else {
            name.to_string()
        };
        McpTool {
            name: name.to_string(),
            full_name,
            description: format!("Tool: {}", name),
            input_schema: json!({"type": "object", "properties": {}}),
            server_name: server.map(|s| s.to_string()),
            timeout_secs: timeout,
            handler: make_test_handler(),
        }
    }

    #[test]
    fn test_registry_new_is_empty() {
        let registry = McpRegistry::new();
        assert!(registry.all().is_empty());
    }

    #[test]
    fn test_registry_register_and_get() {
        let mut registry = McpRegistry::new();
        let tool = make_tool("read", None, Some(30));
        let name = tool.full_name.clone();
        registry.register(tool);
        assert!(registry.get(&name).is_some());
        assert_eq!(registry.get(&name).unwrap().name, "read");
    }

    #[test]
    fn test_registry_get_non_existent() {
        let registry = McpRegistry::new();
        assert!(registry.get("nonexistent").is_none());
    }

    #[test]
    fn test_registry_register_all() {
        let mut registry = McpRegistry::new();
        let tools = vec![
            make_tool("read", None, Some(30)),
            make_tool("write", None, Some(30)),
        ];
        registry.register_all(tools);
        assert_eq!(registry.all().len(), 2);
    }

    #[test]
    fn test_registry_remove_by_server() {
        let mut registry = McpRegistry::new();
        registry.register(make_tool("read", Some("fs"), Some(30)));
        registry.register(make_tool("write", Some("fs"), Some(30)));
        registry.register(make_tool("other", Some("another"), Some(30)));

        let removed = registry.remove_by_server("fs");
        assert_eq!(removed.len(), 2);
        assert!(removed.iter().any(|n| n.contains("read")));
        assert!(removed.iter().any(|n| n.contains("write")));
        assert_eq!(registry.all().len(), 1);
    }

    #[test]
    fn test_registry_remove_by_server_no_match() {
        let mut registry = McpRegistry::new();
        registry.register(make_tool("read", Some("fs"), Some(30)));
        let removed = registry.remove_by_server("nonexistent");
        assert!(removed.is_empty());
        assert_eq!(registry.all().len(), 1);
    }

    #[test]
    fn test_registry_all() {
        let mut registry = McpRegistry::new();
        registry.register(make_tool("read", None, Some(30)));
        registry.register(make_tool("write", None, Some(60)));
        assert_eq!(registry.all().len(), 2);
    }

    #[test]
    fn test_registry_allowed() {
        let mut registry = McpRegistry::new();
        registry.register(make_tool("read", None, Some(30)));
        registry.register(make_tool("write", None, Some(30)));

        let allowed_names = vec!["read".to_string()];
        let allowed = registry.allowed(&allowed_names);
        assert_eq!(allowed.len(), 1);
        assert_eq!(allowed[0].name, "read");
    }

    #[test]
    fn test_registry_allowed_empty_list() {
        let mut registry = McpRegistry::new();
        registry.register(make_tool("read", None, Some(30)));
        let allowed = registry.allowed(&[]);
        assert!(allowed.is_empty());
    }

    #[test]
    fn test_get_timeout_secs_found() {
        let mut registry = McpRegistry::new();
        registry.register(make_tool("read", None, Some(42)));
        assert_eq!(registry.get_timeout_secs("read"), Some(42));
    }

    #[test]
    fn test_get_timeout_secs_not_found_is_none() {
        let registry = McpRegistry::new();
        assert_eq!(registry.get_timeout_secs("nonexistent"), None);
    }

    #[test]
    fn test_get_timeout_secs_none_means_no_timeout() {
        let mut registry = McpRegistry::new();
        registry.register(make_tool("long", None, None));
        assert_eq!(registry.get_timeout_secs("long"), None);
    }

    #[test]
    fn test_to_openai_tools() {
        let mut registry = McpRegistry::new();
        registry.register(make_tool("my_tool", None, Some(30)));
        let allowed = vec!["my_tool".to_string()];
        let openai_tools = registry.to_openai_tools(&allowed);
        assert_eq!(openai_tools.len(), 1);

        let tool_def = &openai_tools[0];
        assert_eq!(tool_def["type"], "function");
        assert_eq!(tool_def["function"]["name"], "my_tool");
        assert_eq!(tool_def["function"]["description"], "Tool: my_tool");
    }

    #[test]
    fn test_to_openai_tools_all() {
        let mut registry = McpRegistry::new();
        registry.register(make_tool("tool_a", None, Some(30)));
        registry.register(make_tool("tool_b", None, Some(30)));
        let openai_tools = registry.to_openai_tools_all();
        assert_eq!(openai_tools.len(), 2);
    }

    #[test]
    fn test_to_openai_tools_all_empty() {
        let registry = McpRegistry::new();
        let openai_tools = registry.to_openai_tools_all();
        assert!(openai_tools.is_empty());
    }

    #[test]
    fn test_registry_allowed_filters_by_full_name() {
        let mut registry = McpRegistry::new();
        // Register a tool with a specific full_name, but test allowed with that full_name
        let tool = McpTool {
            name: "read".to_string(),
            full_name: "fs_read".to_string(),
            description: "Read tool".to_string(),
            input_schema: json!({"type": "object", "properties": {}}),
            server_name: Some("fs".to_string()),
            timeout_secs: Some(30),
            handler: make_test_handler(),
        };
        registry.register(tool);

        // Allowed with matching full_name
        let allowed = registry.allowed(&["fs_read".to_string()]);
        assert_eq!(allowed.len(), 1);

        // Allowed with non-matching name
        let allowed = registry.allowed(&["read".to_string()]);
        assert!(allowed.is_empty());
    }

    // ─── tool-result spill tests ───

    fn spill_test_root(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "omniagent-spill-test-{}-{}",
            std::process::id(),
            sanitize_spill_segment(name)
        ))
    }

    #[test]
    fn test_sanitize_spill_segment() {
        assert_eq!(sanitize_spill_segment("filesystem_read"), "filesystem_read");
        assert_eq!(sanitize_spill_segment("call_abc-123"), "call_abc-123");
        // Path traversal / separators / spaces are neutralized
        assert_eq!(sanitize_spill_segment("../../etc/passwd"), "etc_passwd");
        assert_eq!(sanitize_spill_segment("a b:c"), "a_b_c");
        assert_eq!(sanitize_spill_segment("..."), "result");
        assert_eq!(sanitize_spill_segment(""), "result");
        // Long names are capped to a single safe segment
        let long = "x".repeat(200);
        assert_eq!(sanitize_spill_segment(&long).len(), 80);
    }

    #[test]
    fn test_spill_under_threshold_unchanged() {
        let root = spill_test_root("under_threshold");
        let _ = std::fs::remove_dir_all(&root);
        let content = "y".repeat(100);
        let out = spill_tool_result(&content, 7, "call_1", "filesystem_read", &root, 500);
        assert_eq!(out.inline, content);
        assert!(out.spill_path.is_none());
        assert!(
            !root.exists(),
            "no spill dir should be created under threshold"
        );
    }

    #[test]
    fn test_spill_over_threshold_writes_full_file() {
        let root = spill_test_root("over_threshold");
        let _ = std::fs::remove_dir_all(&root);
        let content: String = (0..5000)
            .map(|i| format!("line {i}: {:08x}\n", i * 31))
            .collect();
        assert!(content.len() > 1000);
        let out = spill_tool_result(&content, 7, "call_abc", "search_database", &root, 1000);
        let path = out.spill_path.expect("spill path expected");
        assert!(path.starts_with(&root));
        let path_str = path.to_string_lossy();
        assert!(
            path_str.contains("/7/"),
            "session-scoped per thread id: {path_str}"
        );
        assert!(path_str.ends_with(".txt"));
        assert!(path_str.contains("call_abc"));
        assert!(path_str.contains("search_database"));
        // Full content on disk, unchanged
        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert_eq!(on_disk, content);
        // Preview: bounded, contains head, tail and the locator
        let inline = &out.inline;
        assert!(inline.len() < content.len());
        assert!(inline.contains(&content[..200]), "head present");
        assert!(
            inline.contains(&content[content.len() - 200..]),
            "tail present"
        );
        assert!(inline.contains(&format!("[full output: {}]", path.display())));
        assert!(inline.contains("omitted"));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn test_spill_preview_composition_and_bounds() {
        let max_inline = 10_000;
        assert_eq!(
            spill_preview_head_chars(max_inline) + spill_preview_tail_chars(max_inline),
            max_inline
        );
        let content = "a".repeat(100_000);
        let path = std::path::Path::new("/tmp/x/1/call-foo.txt");
        let preview = compose_spill_preview(&content, max_inline, path);
        assert!(preview.contains("[full output: /tmp/x/1/call-foo.txt]"));
        // head + tail + overhead stays bounded
        assert!(preview.len() < max_inline + 200);
        assert!(preview.contains("aaaaa"));
        assert!(preview.contains("omitted"));
        // Under threshold → content returned verbatim (no duplication)
        let small = "b".repeat(5_000);
        assert_eq!(compose_spill_preview(&small, max_inline, path), small);
    }

    #[test]
    fn test_spill_filename_has_no_secret_or_traversal() {
        let root = spill_test_root("filename_safe");
        let _ = std::fs::remove_dir_all(&root);
        let content = "z".repeat(2000);
        let evil_tool = "../../ghp_ABCDEF_secret_token";
        let out = spill_tool_result(&content, 42, "call:weird/../id", evil_tool, &root, 500);
        let path = out.spill_path.expect("spilled");
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        assert!(!name.contains('/'), "no separators in filename: {name}");
        assert!(!name.contains(".."), "no traversal in filename: {name}");
        assert!(!name.contains(':'), "no colons in filename: {name}");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn test_spill_collision_gets_unique_suffix() {
        let root = spill_test_root("collision");
        let _ = std::fs::remove_dir_all(&root);
        let content = "c".repeat(2000);
        let first = spill_tool_result(&content, 1, "call_x", "tool", &root, 500);
        let second = spill_tool_result(&content, 1, "call_x", "tool", &root, 500);
        let p1 = first.spill_path.expect("first spilled");
        let p2 = second.spill_path.expect("second spilled");
        assert_ne!(p1, p2, "collision must produce a unique file");
        assert!(p1.exists());
        assert!(p2.exists());
        assert_eq!(std::fs::read_to_string(&p1).unwrap(), content);
        assert_eq!(std::fs::read_to_string(&p2).unwrap(), content);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn test_spill_multibyte_utf8_preview() {
        let content = "héllo wörld — ".repeat(5000);
        let max_inline = 1000;
        let path = std::path::Path::new("/tmp/x/1/call.txt");
        let preview = compose_spill_preview(&content, max_inline, path);
        assert!(preview.contains("[full output: /tmp/x/1/call.txt]"));
        assert!(preview.len() < content.len());
        // Round-trip via spill keeps full fidelity even with multi-byte content
        let root = spill_test_root("multibyte");
        let _ = std::fs::remove_dir_all(&root);
        let out = spill_tool_result(&content, 3, "call_ü", "tool", &root, max_inline);
        let p = out.spill_path.expect("spilled");
        assert_eq!(std::fs::read_to_string(&p).unwrap(), content);
        std::fs::remove_dir_all(&root).ok();
    }
}
