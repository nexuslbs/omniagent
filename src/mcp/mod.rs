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
        "{head}\n\n[... {omitted} chars omitted - see full output below ...]\n\n{tail}\n\n[full output: {}]",
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

pub mod behavior;
pub mod external;
pub mod task_tools;
pub use behavior::ToolBehavior;

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
    /// Effective tool allow-list for the current thread execution: the
    /// profile's tools intersected with the running workflow ROLE's tools.
    /// Set per-tool-call alongside `current_thread_id` so the
    /// `list_tool_details` introspection tool knows which tools are actually
    /// available. `None` = no restriction (all tools allowed); `Some([])` =
    /// NO tool allowed.
    pub current_allowed_tools: Option<Vec<String>>,
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
    /// internally, so the core stays plugin-agnostic - no knowledge of
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
            current_allowed_tools: None,
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

/// Maximum length of an EXPOSED tool name: `{plugin}__{tool}`.
///
/// The intersection of the constraints of the mainstream providers
/// (Anthropic / OpenAI / Gemini / OpenAI-compatible) is exactly
/// `[A-Za-z0-9_-]{1,64}`; core enforces that shared guard and nothing
/// provider-specific.
pub const MAX_EXPOSED_TOOL_NAME_LEN: usize = 64;

/// The plugin component used for tools that core implements itself.
pub const BUILTIN_PLUGIN_NAME: &str = "builtin";

/// The RESERVED separator between the plugin component and the in-plugin tool
/// component of an exposed tool name. It may never appear inside a component,
/// which is what makes the decode unambiguous.
pub const TOOL_NAME_SEPARATOR: &str = "__";

/// Validate ONE component (plugin name or in-plugin tool name) of an exposed
/// tool name.
///
/// VALIDATE, DON'T MANGLE: a declared component is either accepted as-is or
/// rejected with an actionable error naming the failed rule - it is never
/// rewritten into a valid-looking variant.
pub fn validate_component(component: &str) -> Result<(), String> {
    if component.is_empty() {
        return Err("is empty".to_string());
    }
    if component.contains(TOOL_NAME_SEPARATOR) {
        return Err(format!(
            "contains the reserved separator \"{}\"",
            TOOL_NAME_SEPARATOR
        ));
    }
    if component.starts_with('_') {
        return Err("starts with '_', the leading character of the separator".to_string());
    }
    if component.ends_with('_') {
        return Err(
            "ends with '_', which makes the separator ambiguous at the boundary".to_string(),
        );
    }
    if let Some(c) = component
        .chars()
        .find(|c| !(c.is_ascii_alphanumeric() || *c == '-' || *c == '_'))
    {
        return Err(format!(
            "contains {:?}, outside the allowed charset [A-Za-z0-9_-]",
            c
        ));
    }
    if let Some(first) = component.chars().next() {
        if !(first.is_ascii_alphanumeric() || first == '-') {
            return Err(format!(
                "starts with {:?}, must start with [A-Za-z0-9-]",
                first
            ));
        }
    }
    Ok(())
}

/// Validate an exposed tool name built from its two components.
///
/// Returns the failed rule as an actionable message (naming the offending
/// component and the offending exposed name) so every caller can reject the
/// tool loudly instead of exposing a name a provider or a consumer would
/// mis-read.
pub fn validate_exposed_name(plugin: &str, tool: &str) -> Result<(), String> {
    if let Err(e) = validate_component(plugin) {
        return Err(format!("plugin name '{}' {}", plugin, e));
    }
    if let Err(e) = validate_component(tool) {
        return Err(format!("tool name '{}' {}", tool, e));
    }
    let exposed = format!("{}{}{}", plugin, TOOL_NAME_SEPARATOR, tool);
    if exposed.len() > MAX_EXPOSED_TOOL_NAME_LEN {
        return Err(format!(
            "exposed name '{}' is {} chars, over the {} char limit",
            exposed,
            exposed.len(),
            MAX_EXPOSED_TOOL_NAME_LEN
        ));
    }
    Ok(())
}

/// Build a fully-qualified tool name using the unified grammar:
/// `{plugin}__{tool}` (double underscore).
///
/// The declared tool name is used VERBATIM except for one authoring
/// convenience: a redundant plugin prefix is dropped (`filesystem` +
/// `filesystem_read` -> `filesystem__read`, never
/// `filesystem__filesystem_read`). Nothing is lowercased or hyphenated
/// (VALIDATE, DON'T MANGLE): a name that violates the grammar is rejected by
/// `validate_exposed_name` at registration time (see `McpRegistry::register`),
/// never fixed up here.
///
/// `__` is reserved and may not appear inside a component, which is what makes
/// the name round-trip through `tool_dequalify` (`split_once("__")`).
pub fn tool_qualify(server: &str, tool_name: &str) -> String {
    format!(
        "{}{}{}",
        server,
        TOOL_NAME_SEPARATOR,
        tool_component(server, tool_name)
    )
}

/// The in-plugin tool component of a DECLARED tool name: the declared name
/// with a redundant plugin prefix removed (`filesystem` + `filesystem_read` ->
/// `read`). A pure, idempotent function of the declared pair
/// (`tool_component(server, tool_component(server, t)) ==
/// tool_component(server, t)`), so the exposed name is deterministic. This is
/// not a mangle: the remainder is used verbatim and must still satisfy
/// `validate_component`.
fn tool_component<'a>(server: &str, tool_name: &'a str) -> &'a str {
    if let Some(rest) = tool_name.strip_prefix(server) {
        let trimmed = rest.trim_start_matches(['-', '_', '.']);
        if !trimmed.is_empty() {
            return trimmed;
        }
    }
    tool_name
}

/// Decode an exposed tool name back into its `(plugin, tool)` components.
///
/// The single canonical decode path: `split_once("__")` is unambiguous
/// because a component may never contain the separator. Callers that need the
/// in-plugin tool name (e.g. invoking the tool over MCP) MUST use this.
pub fn tool_dequalify(exposed: &str) -> Option<(&str, &str)> {
    exposed.split_once(TOOL_NAME_SEPARATOR)
}

/// The pre-flip exposed name of a tool: `{plugin}_{tool-with-dashes}`.
///
/// LEGACY ALIAS WINDOW (one release): names produced by the old grammar keep
/// resolving so `allowed_tools` entries, skills, kanban bodies and docs
/// written before the separator flip do not silently drop tools. Removed in
/// the P2 cleanup phase.
pub fn tool_legacy_alias(server: &str, tool_name: &str) -> String {
    // Reproduces the old algorithm EXACTLY (redundant-prefix strip + `_`->`-`).
    let tool = if let Some(rest) = tool_name.strip_prefix(server) {
        let trimmed = rest.trim_start_matches(['-', '_', '.']);
        if trimmed.is_empty() {
            tool_name
        } else {
            trimmed
        }
    } else {
        tool_name
    };
    format!("{}_{}", server, tool.replace('_', "-"))
}

/// A registered MCP tool.
#[derive(Clone)]
pub struct McpTool {
    /// The canonical tool name - ALWAYS the fully-qualified name:
    /// `builtin_{tool}` for built-ins, `{server}_{tool}` for external MCP
    /// tools (see `tool_qualify`). There is deliberately NO separate short
    /// name: every surface (prompt, schema, registry, API) uses this single
    /// name. The only place that may know a plugin-internal short form is
    /// the tool's own handler (which by construction knows its plugin).
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    pub server_name: Option<String>,
    /// Maximum time in seconds to wait for this tool to complete.
    /// `None` = NO timeout (tool runs until it finishes, errors, or the agent
    /// cancels it). A timeout exists ONLY when explicitly set - either by the
    /// tool's own declaration (e.g. builtin wait-task = 310s) or by an agent
    /// config that opts in. There is deliberately NO default fallback: fixed
    /// tool timeouts were removed (Aug 2026) because background tasks now give
    /// the agent full tracking/cancel/log control - a tool must never be
    /// killed by an invisible clock the agent didn't set.
    pub timeout_secs: Option<u64>,
    /// Declared behaviour of this tool (audit V-2), read from its plugin
    /// manifest. Default = nothing declared: not read-only, no coordination
    /// family, does not affect the agent's own stack.
    pub behavior: ToolBehavior,
    pub handler: McpToolHandler,
}

impl McpTool {
    /// Build a built-in tool. `short_name` is the plugin-internal name
    /// (e.g. "poll_task"); the canonical `name` is ALWAYS derived via
    /// `tool_qualify("builtin", short_name)` → `builtin__poll_task`. Only
    /// this constructor knows the builtin prefix - callers pass the short
    /// form and the full name is never hardcoded. This is the ONLY place a
    /// short name is acceptable: it is immediately qualified.
    pub fn builtin(
        short_name: &str,
        description: String,
        input_schema: Value,
        timeout_secs: Option<u64>,
        handler: McpToolHandler,
    ) -> Self {
        Self {
            name: tool_qualify("builtin", short_name),
            description,
            input_schema,
            server_name: None,
            timeout_secs,
            behavior: ToolBehavior::default(),
            handler,
        }
    }
}

/// The plugin component of a registered tool: its server name, or the
/// `builtin` plugin for tools core implements itself.
fn plugin_of(tool: &McpTool) -> String {
    tool.server_name
        .clone()
        .unwrap_or_else(|| BUILTIN_PLUGIN_NAME.to_string())
}

impl McpTool {
    /// The `(plugin, in-plugin tool name)` components of this tool's exposed
    /// name. Derived from `name` + `server_name`: there is deliberately no
    /// second stored copy of the short name.
    pub fn components(&self) -> (String, String) {
        let plugin = plugin_of(self);
        let prefix = format!("{}{}", plugin, TOOL_NAME_SEPARATOR);
        let tool = self
            .name
            .strip_prefix(&prefix)
            .unwrap_or(self.name.as_str())
            .to_string();
        (plugin, tool)
    }

    /// Legacy (pre-separator-flip) exposed names of this tool: the one-release
    /// alias window for callers and `allowed_tools` entries written before the
    /// flip. Empty when the name cannot be represented in the legacy grammar.
    pub fn legacy_names(&self) -> Vec<String> {
        let (plugin, tool) = self.components();
        if validate_component(&plugin).is_err() || validate_component(&tool).is_err() {
            return Vec::new();
        }
        let alias = tool_legacy_alias(&plugin, &tool);
        if alias == self.name {
            Vec::new()
        } else {
            vec![alias]
        }
    }
}

/// A tool REJECTED at registration because its exposed name violates the
/// exposed-name grammar (or exceeds the length limit).
///
/// Such a tool is never registered, never sent to a provider and never listed
/// in the prompt's available tools; the API and the dashboard surface this
/// record with plugin, tool and the failed rule.
#[derive(Debug, Clone, serde::Serialize)]
pub struct InvalidTool {
    pub plugin: String,
    pub tool: String,
    pub reason: String,
    pub exposed_name: String,
}

/// Two registrations under one exposed name: the second one overwrote the
/// first (reported, never silent).
#[derive(Debug, Clone, serde::Serialize)]
pub struct ToolCollision {
    pub name: String,
    pub existing_plugin: String,
    pub incoming_plugin: String,
}

/// Registry of all available MCP tools.
#[derive(Clone)]
pub struct McpRegistry {
    tools: HashMap<String, McpTool>,
    /// Tools rejected by the exposed-name grammar at registration time.
    /// Never registered, never sent to a provider.
    invalid: Vec<InvalidTool>,
    /// Exposed-name collisions observed while registering.
    collisions: Vec<ToolCollision>,
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
            invalid: Vec::new(),
            collisions: Vec::new(),
        }
    }

    /// Register a tool.
    ///
    /// VALIDATE, DON'T MANGLE: a tool whose plugin or in-plugin name violates
    /// the exposed-name grammar, or whose exposed name exceeds
    /// `MAX_EXPOSED_TOOL_NAME_LEN`, is REJECTED. It is never registered, so
    /// it is never sent to a provider, never listed in the prompt's available
    /// tools and never dispatchable. The rejection is recorded in
    /// `McpRegistry::invalid_tools` with plugin, tool and the failed rule.
    ///
    /// A second registration under an identical exposed name is REPORTED
    /// (loud log + `McpRegistry::collisions`), never a silent overwrite.
    pub fn register(&mut self, tool: McpTool) {
        let (plugin, short) = tool.components();
        if let Err(reason) = validate_exposed_name(&plugin, &short) {
            warn_invalid_tool(&plugin, &short, &reason);
            self.invalid.push(InvalidTool {
                plugin,
                tool: short,
                reason,
                exposed_name: tool.name.clone(),
            });
            return;
        }
        if let Some(previous) = self.tools.get(&tool.name) {
            let existing_plugin = plugin_of(previous);
            warn_collision(&tool.name, &existing_plugin, &plugin);
            self.collisions.push(ToolCollision {
                name: tool.name.clone(),
                existing_plugin,
                incoming_plugin: plugin,
            });
        }
        self.tools.insert(tool.name.clone(), tool);
    }

    /// Register multiple tools at once (for batch loading from a server).
    pub fn register_all(&mut self, tools: Vec<McpTool>) {
        for tool in tools {
            self.register(tool);
        }
    }

    // ── Tool behaviour sets (audit V-2) ─────────────────────────────────
    // Every set below is DERIVED FROM TOOL DESCRIPTORS (plugin manifests),
    // never from a hardcoded tool-name allowlist: a tool registered under
    // another id keeps its declared protection.

    /// Tools whose declared behaviour applies the exact-repeat read guard.
    ///
    /// Fail CLOSED BUT LOUD: an undeclared tool is never treated as
    /// read-only; a read-looking name emits one warning per process so the
    /// missing manifest entry stays visible.
    pub fn guarded_read_only_tools(&self) -> std::collections::HashSet<String> {
        for tool in self.tools.values() {
            if tool.behavior.is_empty() {
                crate::mcp::behavior::warn_missing_descriptor(&tool.name);
            }
        }
        self.tools
            .values()
            .filter(|t| t.behavior.repeat_guard_enabled())
            .map(|t| t.name.clone())
            .collect()
    }

    /// Tools that declare themselves read-only. Handed to the prompt plugin
    /// so compaction keeps a generous excerpt of their results.
    pub fn read_only_tools(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .tools
            .values()
            .filter(|t| t.behavior.read_only)
            .map(|t| t.name.clone())
            .collect();
        names.sort();
        names
    }

    /// Tools that can change the stack the agent itself runs in: the
    /// self-restart guard must run before any of them executes.
    pub fn own_stack_tools(&self) -> std::collections::HashSet<String> {
        self.tools
            .values()
            .filter(|t| t.behavior.affects_own_stack)
            .map(|t| t.name.clone())
            .collect()
    }

    /// Tools declaring a coordination family (e.g. "subtasks"): the core
    /// loop treats the whole family as one capability, so a renamed tool is
    /// still recognised.
    pub fn family_tools(&self, family: &str) -> std::collections::HashSet<String> {
        self.tools
            .values()
            .filter(|t| t.behavior.family.as_deref() == Some(family))
            .map(|t| t.name.clone())
            .collect()
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
    ///
    /// Exact (canonical) match first; during the one-release alias window a
    /// LEGACY name (`{plugin}_{tool-with-dashes}`) also resolves, so callers
    /// and configs written before the separator flip keep working.
    pub fn get(&self, name: &str) -> Option<&McpTool> {
        if let Some(tool) = self.tools.get(name) {
            return Some(tool);
        }
        self.tools
            .values()
            .find(|tool| tool.legacy_names().iter().any(|alias| alias == name))
    }

    /// Get all tools.
    pub fn all(&self) -> Vec<&McpTool> {
        self.tools.values().collect()
    }

    /// Tools rejected by the exposed-name grammar at registration time.
    pub fn invalid_tools(&self) -> &[InvalidTool] {
        &self.invalid
    }

    /// Exposed-name collisions observed while registering.
    pub fn collisions(&self) -> &[ToolCollision] {
        &self.collisions
    }

    /// Priority ranking for tool ordering: all tools have equal priority.
    fn tool_priority(_name: &str) -> u8 {
        0
    }

    /// Get tools allowed for a given profile, sorted by execution priority.
    /// Tools permitted by an OPTIONAL allow-list: `None` means "no
    /// restriction" (every registered tool), `Some([])` means "no tool".
    pub fn allowed_opt(&self, allowed_names: Option<&[String]>) -> Vec<&McpTool> {
        match allowed_names {
            None => self.all(),
            Some(names) => self.allowed(names),
        }
    }

    pub fn allowed(&self, allowed_names: &[String]) -> Vec<&McpTool> {
        let mut tools: Vec<&McpTool> = self
            .tools
            .values()
            .filter(|t| {
                allowed_names.iter().any(|name| {
                    name == &t.name || t.legacy_names().iter().any(|alias| alias == name)
                })
            })
            .collect();
        tools.sort_by_key(|t| Self::tool_priority(&t.name));
        tools
    }

    /// Get the qualified name for a tool.
    /// Tool names are ALWAYS fully qualified (builtin_* / {server}_{tool}),
    /// so a name is returned as-is. Kept for callers that expect a
    /// qualification step; there is no short-name form anymore.
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

    /// True when an exposed name may be sent to a provider: non-empty,
    /// inside `[A-Za-z0-9_-]` and at most `MAX_EXPOSED_TOOL_NAME_LEN` chars.
    fn is_exposable(name: &str) -> bool {
        !name.is_empty()
            && name.len() <= MAX_EXPOSED_TOOL_NAME_LEN
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    }

    /// Second line of defence behind `McpRegistry::register`: drop (and
    /// loudly report) any tool whose exposed name must not reach a provider.
    fn exposable(tools: Vec<&McpTool>) -> Vec<&McpTool> {
        tools
            .into_iter()
            .filter(|tool| {
                if Self::is_exposable(&tool.name) {
                    true
                } else {
                    let (plugin, short) = tool.components();
                    warn_invalid_tool(
                        &plugin,
                        &short,
                        &format!(
                            "exposed name '{}' is not provider-safe ([A-Za-z0-9_-]{{1,{}}})",
                            tool.name, MAX_EXPOSED_TOOL_NAME_LEN
                        ),
                    );
                    false
                }
            })
            .collect()
    }

    /// Build the OpenAI-compatible tools array for an OPTIONAL allow-list:
    /// `None` = every registered tool, `Some([])` = no tool at all.
    pub fn to_openai_tools_opt(&self, allowed_names: Option<&[String]>) -> Vec<Value> {
        Self::exposable(self.allowed_opt(allowed_names))
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

    /// Build the OpenAI-compatible tools array for the LLM.
    pub fn to_openai_tools(&self, allowed_names: &[String]) -> Vec<Value> {
        Self::exposable(self.allowed(allowed_names))
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
        Self::exposable(self.all())
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

/// Loudly report a tool rejected by the exposed-name grammar (plugin, tool
/// and failed rule). The tool is not registered and never reaches a provider;
/// `McpRegistry::invalid_tools` and the dashboard surface it.
fn warn_invalid_tool(plugin: &str, tool: &str, reason: &str) {
    tracing::warn!(
        "tool '{}__{}' REJECTED: {} (not registered, not exposed to the agent)",
        plugin,
        tool,
        reason
    );
}

/// Loudly report two registrations under one exposed name.
fn warn_collision(name: &str, existing_plugin: &str, incoming_plugin: &str) {
    tracing::error!(
        "tool-name collision on '{}': plugin '{}' overwrote plugin '{}'",
        name,
        incoming_plugin,
        existing_plugin
    );
}

/// Build the `poll-task` tool: check the status of a background task.
fn poll_task_tool() -> McpTool {
    McpTool {
        name: tool_qualify("builtin", "poll_task"),
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
        behavior: ToolBehavior::default(),
        handler: std::sync::Arc::new(|args: Value, ctx: crate::mcp::AppContext| {
            Box::pin(crate::mcp::task_tools::handle_poll_task(args, ctx))
        }),
    }
}

/// Build the `wait-task` tool: wait for a background task to complete.
fn wait_task_tool() -> McpTool {
    McpTool {
        name: tool_qualify("builtin", "wait_task"),
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
        // when exceeded - an external kill clock would cut a legitimately
        // long wait short and force the agent into extra wait calls.
        timeout_secs: None,
        behavior: ToolBehavior::default(),
        handler: std::sync::Arc::new(|args: Value, ctx: crate::mcp::AppContext| {
            Box::pin(crate::mcp::task_tools::handle_wait_task(args, ctx))
        }),
    }
}

/// Build the `wait-for-status` tool: wait until a kanban task or thread
/// reaches one of the target statuses (threads 1136/1146 incident: the agent
/// had no status-change listener and burned 3x1h blind wait-task calls on a
/// dead background watcher while the target task was already done). Unlike
/// wait-task/poll-task (background TOOL tasks), this listens to the real
/// kanban/thread DB status and returns within ~1-2s of a transition.
fn wait_for_status_tool() -> McpTool {
    McpTool {
        name: tool_qualify("builtin", "wait_for_status"),
        description: "Wait until a KANBAN TASK or THREAD reaches one of the target statuses; returns as soon as it does (checks the real DB status about every second). This is the first-class way to 'listen' to kanban/thread state changes - use it when you must act when a task or thread transitions (incident 1136/1146). It does NOT track background tool tasks: use builtin__wait_task / builtin__poll_task for docker/ssh exec processes that returned status=processing. Pass exactly one of task_id (kanban task, statuses e.g. done/blocked/review/testing/running/todo) or thread_id (conversation thread, statuses e.g. pending/processing/completed/failed/skipped). 'until' is a comma-separated list of target statuses. Bounded by timeout_s (default 900): on timeout it returns a timeout STATUS (not an error) - then re-check the real state (kanban_list-kanban-tasks / GET /kanban/tasks/{id}) and re-wait in bounded chunks only if the wait still makes sense.".to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "task_id": {
                    "type": "string",
                    "description": "Kanban task id to wait on (e.g. task_omnidev_...). Mutually exclusive with thread_id."
                },
                "thread_id": {
                    "type": "integer",
                    "description": "Thread id to wait on (e.g. 1234). Mutually exclusive with task_id."
                },
                "until": {
                    "type": "string",
                    "description": "Comma-separated target statuses, e.g. 'done,blocked' for a kanban task, or 'completed,failed' for a thread. Returns as soon as the entity's status is one of these."
                },
                "timeout_s": {
                    "type": "integer",
                    "description": "Maximum seconds to wait (default: 900). The wait ends as soon as the status matches, so a long value costs nothing for fast transitions. Prefer bounded waits; when it returns 'timeout', re-check the real state before waiting again.",
                    "default": 900
                }
            },
            "required": ["until"]
        }),
        server_name: None,
        // No declared timeout: like wait-task, the handler self-bounds by its
        // own timeout_s argument and returns a timeout STATUS (not an error).
        timeout_secs: None,
        behavior: ToolBehavior::default(),
        handler: std::sync::Arc::new(|args: Value, ctx: crate::mcp::AppContext| {
            Box::pin(crate::mcp::task_tools::handle_wait_for_status(args, ctx))
        }),
    }
}

/// Build the `cancel-task` tool: cancel a running background task.
fn cancel_task_tool() -> McpTool {
    McpTool {
        name: tool_qualify("builtin", "cancel_task"),
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
        behavior: ToolBehavior::default(),
        handler: std::sync::Arc::new(|args: Value, ctx: crate::mcp::AppContext| {
            Box::pin(crate::mcp::task_tools::handle_cancel_task(args, ctx))
        }),
    }
}

/// Build the `read-task-logs` tool: stream log output from a background task.
fn read_task_logs_tool() -> McpTool {
    McpTool {
        name: tool_qualify("builtin", "read_task_logs"),
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
        behavior: ToolBehavior::default(),
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
        name: tool_qualify("builtin", "read_attached_file"),
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
        behavior: ToolBehavior::default(),
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
        name: tool_qualify("builtin", "list_tool_details"),
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
        behavior: ToolBehavior::default(),
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

                for tool_def in &ctx.tool_catalog {
                    if let Some(name) = tool_def
                        .pointer("/function/name")
                        .and_then(|v| v.as_str())
                    {
                        if name == tool_name {
                            // Check if the tool is allowed by the current profile
                            let permitted = allowed
                                .as_ref()
                                .map(|names| names.contains(&name.to_string()))
                                .unwrap_or(true);
                            let status = if permitted {
                                "AVAILABLE".to_string()
                            } else {
                                format!(
                                    "RESTRICTED: not in the effective allowed tools ({}/{} tools allowed)",
                                    allowed.as_ref().map(|names| names.len()).unwrap_or(0),
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
                let is_restricted = allowed.is_some();
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
                        .filter(|name| {
                            allowed
                                .as_ref()
                                .map(|names| names.contains(&name.to_string()))
                                .unwrap_or(true)
                        })
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

    // ── read_attached_file: platform-generic file reading ──
    // Allows the agent to read file attachments that exceed the inline
    // size limit by delegating to the platform's read_file implementation.
    registry.register(read_attached_file_tool());

    // ── Task management tools for non-blocking tool execution ──
    // (wait-for-status listens to kanban/thread status changes - incident
    // 1136/1146 - while poll/wait/cancel/read-task-logs track background
    // TOOL tasks.)
    registry.register(poll_task_tool());
    registry.register(wait_task_tool());
    registry.register(wait_for_status_tool());
    registry.register(cancel_task_tool());
    registry.register(read_task_logs_tool());
    registry.register(omniagent_api_tool());
    registry.register(fail_thread_tool());

    // Populate tool catalog (all registered tool definitions in OpenAI
    // function format) so the list_tool_details introspection tool can serve
    // them to the LLM. Populated AFTER every builtin above so wait/poll/
    // cancel/read-task-logs/read-attached-file/omniagent-api/fail-thread/
    // wait-for-status appear in the catalog (bug fix 2026-09-07: they were
    // registered after catalog population, so builtin_list-tool-details
    // reported them as 'Unknown tool' although they exist and are callable -
    // threads 1136/1146).
    ctx.tool_catalog = registry.to_openai_tools_all();

    // ── list_tool_details: always-available introspection tool ──
    // Registered LAST so the catalog excludes only itself (it reads from
    // AppContext.tool_catalog which was populated just above).
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

/// Default base URL of the core omniagent HTTP API (the historical hardcoded
/// value), used when neither `CORE_API_BASE_URL` nor `HOST`/`PORT` are set.
const DEFAULT_CORE_API_BASE_URL: &str = "http://localhost:8080";

/// Resolve the base URL of the core omniagent HTTP API (audit V-8).
///
/// Resolution order:
/// 1. `CORE_API_BASE_URL` env var when set and non-empty (explicit override:
///    reverse proxy, non-default port, remote API),
/// 2. `HOST` + `PORT` env vars - the same vars `AgentConfig::from_env` reads
///    and the settings page exposes. A wildcard bind address (`0.0.0.0`,
///    `::`) is not dialable, so it is mapped to `127.0.0.1`/`[::1]`; `PORT`
///    defaults to `8080`,
/// 3. [`DEFAULT_CORE_API_BASE_URL`] (`http://localhost:8080`).
///
/// A trailing `/` is trimmed so the result can be concatenated with an API
/// path that already starts with `/`.
pub(crate) fn core_api_base_url() -> String {
    fn env_non_empty(name: &str) -> Option<String> {
        std::env::var(name)
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    }
    core_api_base_url_from(
        env_non_empty("CORE_API_BASE_URL").as_deref(),
        env_non_empty("HOST").as_deref(),
        env_non_empty("PORT").as_deref(),
    )
}

/// Pure resolution logic behind [`core_api_base_url`] (unit-testable without
/// mutating the process environment).
fn core_api_base_url_from(
    explicit: Option<&str>,
    host: Option<&str>,
    port: Option<&str>,
) -> String {
    if let Some(explicit) = explicit.map(str::trim).filter(|v| !v.is_empty()) {
        return explicit.trim_end_matches('/').to_string();
    }
    let host = host
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(|v| match v {
            "0.0.0.0" => "127.0.0.1".to_string(),
            "::" | "[::]" => "[::1]".to_string(),
            other => other.to_string(),
        })
        .unwrap_or_else(|| "localhost".to_string());
    let port = port
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .unwrap_or("8080");
    format!("http://{}:{}", host, port)
}

/// Join a resolved core API base URL with an API path (audit V-8). The
/// request URL of the `omniagent-api` tool is built here so it can be unit
/// tested without spinning up the HTTP handler.
fn core_api_url(base_url: &str, path: &str) -> String {
    if path.starts_with('/') {
        format!("{}{}", base_url, path)
    } else {
        format!("{}/{}", base_url, path)
    }
}

/// Build the `omniagent-api` tool: generic fetch-like HTTP client for the
/// core omniagent API. Replaces the cron/kanban plugin MCP
/// tools with ONE generic tool: method + path + optional JSON body. Covers
/// kanban task CRUD (/kanban/tasks...), schedule CRUD (/schedule... including
/// DELETE /schedule/{id}), run-cron (/schedule/{id}/run), review
/// (/kanban/tasks/{id}/review), plugins and actions endpoints.
///
/// The core API base URL is resolved once by [`core_api_base_url`] and used
/// BOTH in the tool description and in the request URL (audit V-8: the
/// `localhost:8080` literal used to be hardcoded in both places).
fn omniagent_api_tool() -> McpTool {
    let base_url = core_api_base_url();
    McpTool {
        name: tool_qualify("builtin", "omniagent_api"),
        description: format!(
            "Call the core omniagent HTTP API ({}). Specify an HTTP method, an API path and an optional JSON body; returns the response body as text. Covers kanban task CRUD (/kanban/tasks...), schedule CRUD (/schedule, /schedule/{{id}} incl. DELETE), run-cron (/schedule/{{id}}/run), review (/kanban/tasks/{{id}}/review), plugins and actions endpoints. This replaces the old kanban_*/cron_* plugin tools.",
            base_url
        ),
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
        behavior: ToolBehavior::default(),
        handler: std::sync::Arc::new(move |args: Value, _ctx: crate::mcp::AppContext| {
            let base_url = base_url.clone();
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
                let url = core_api_url(&base_url, &path);
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
        name: tool_qualify("builtin", "fail_thread"),
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
        behavior: ToolBehavior::default(),
        handler: Arc::new(|args: Value, ctx: AppContext| {
            Box::pin(crate::mcp::task_tools::handle_fail_thread(args, ctx))
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ─── core_api_base_url tests (audit V-8) ───

    #[test]
    fn test_core_api_base_url_default_is_localhost_8080() {
        assert_eq!(
            core_api_base_url_from(None, None, None),
            DEFAULT_CORE_API_BASE_URL
        );
    }

    #[test]
    fn test_core_api_base_url_uses_host_and_port_env() {
        assert_eq!(
            core_api_base_url_from(None, Some("127.0.0.1"), Some("9999")),
            "http://127.0.0.1:9999"
        );
    }

    #[test]
    fn test_core_api_base_url_port_defaults_when_only_host_set() {
        assert_eq!(
            core_api_base_url_from(None, Some("api.internal"), None),
            "http://api.internal:8080"
        );
    }

    #[test]
    fn test_core_api_base_url_wildcard_bind_address_is_dialable() {
        assert_eq!(
            core_api_base_url_from(None, Some("0.0.0.0"), Some("8080")),
            "http://127.0.0.1:8080"
        );
        assert_eq!(
            core_api_base_url_from(None, Some("::"), None),
            "http://[::1]:8080"
        );
    }

    #[test]
    fn test_core_api_base_url_explicit_override_wins_and_trims_slash() {
        assert_eq!(
            core_api_base_url_from(
                Some("http://api.internal:9000/"),
                Some("127.0.0.1"),
                Some("9999")
            ),
            "http://api.internal:9000"
        );
    }

    #[test]
    fn test_core_api_url_uses_resolved_base_url_and_appends_path() {
        // V-8 verification: HOST=127.0.0.1 PORT=9999 must yield
        // http://127.0.0.1:9999/... - never localhost:8080.
        let base = core_api_base_url_from(None, Some("127.0.0.1"), Some("9999"));
        assert_eq!(
            core_api_url(&base, "/kanban/tasks"),
            "http://127.0.0.1:9999/kanban/tasks"
        );
        assert_eq!(
            core_api_url(&base, "kanban/tasks"),
            "http://127.0.0.1:9999/kanban/tasks"
        );
        assert!(!core_api_url(&base, "/schedule").contains("localhost:8080"));
    }

    #[test]
    fn test_core_api_url_default_base() {
        let base = core_api_base_url_from(None, None, None);
        assert_eq!(
            core_api_url(&base, "/kanban/tasks"),
            "http://localhost:8080/kanban/tasks"
        );
    }

    #[test]
    fn test_omniagent_api_tool_description_uses_resolved_base_url() {
        // Description and request URL must agree on the resolved base URL.
        let base = core_api_base_url();
        let desc = omniagent_api_tool().description;
        assert!(
            desc.contains(&base),
            "description {:?} does not contain base URL {:?}",
            desc,
            base
        );
    }

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

    // --- tool_qualify / validate_component / tool_dequalify tests ---

    #[test]
    fn test_tool_qualify_uses_double_underscore_separator() {
        assert_eq!(tool_qualify("filesystem", "read"), "filesystem__read");
        assert_eq!(
            tool_qualify("search", "channel_prompts"),
            "search__channel_prompts"
        );
        assert_eq!(tool_qualify("builtin", "poll_task"), "builtin__poll_task");
    }

    #[test]
    fn test_tool_qualify_keeps_declared_names_verbatim() {
        // VALIDATE, DON'T MANGLE: underscores stay underscores.
        assert_eq!(tool_qualify("server", "my_tool"), "server__my_tool");
        assert_eq!(
            tool_qualify("builtin", "omniagent_api"),
            "builtin__omniagent_api"
        );
    }

    #[test]
    fn test_tool_qualify_redundant_prefix() {
        assert_eq!(
            tool_qualify("filesystem", "filesystem_read"),
            "filesystem__read"
        );
        assert_eq!(tool_qualify("my-srv", "my-srv-read"), "my-srv__read");
    }

    #[test]
    fn test_tool_qualify_empty_after_stripping() {
        assert_eq!(tool_qualify("fetch", "fetch"), "fetch__fetch");
    }

    #[test]
    fn test_tool_round_trip_is_lossless() {
        for (plugin, tool) in [
            ("builtin", "poll_task"),
            ("search", "channel_prompts"),
            ("filesystem", "read"),
            ("semantic_search", "semantic_search_index"),
        ] {
            let exposed = tool_qualify(plugin, tool);
            let (p, t) = tool_dequalify(&exposed).expect("must decode");
            assert_eq!(p, plugin, "decoded plugin of '{}'", exposed);
            assert!(
                validate_component(t).is_ok(),
                "decoded tool '{}' must be valid",
                t
            );
        }
    }

    #[test]
    fn test_validate_component_rejects_reserved_separator() {
        let err = validate_component("my__plugin").unwrap_err();
        assert!(err.contains("separator"), "unexpected error: {}", err);
    }

    #[test]
    fn test_validate_component_rejects_leading_underscore() {
        assert!(validate_component("_plugin").is_err());
        assert!(validate_component("_").is_err());
    }

    #[test]
    fn test_validate_component_rejects_trailing_underscore() {
        let err = validate_component("my_plugin_").unwrap_err();
        assert!(err.contains("ends with '_'"), "unexpected error: {}", err);
    }

    #[test]
    fn test_validate_component_rejects_charset_and_empty() {
        assert!(validate_component("").is_err());
        assert!(validate_component("my.plugin").is_err());
        assert!(validate_component("my plugin").is_err());
        assert!(validate_component("my/plugin").is_err());
        assert!(validate_component("-ok-name_1").is_ok());
    }

    #[test]
    fn test_exposed_name_length_guard_accepts_64_rejects_65() {
        let plugin = "a".repeat(60);
        assert_eq!(plugin.len() + 2 + 3, 65);
        assert!(validate_exposed_name(&plugin, "ab").is_ok());
        assert!(validate_exposed_name(&plugin, "abc").is_err());
    }

    #[test]
    fn test_invalid_component_is_rejected_and_recorded() {
        let mut reg = McpRegistry::new();
        reg.register(make_tool("my_tool", Some("my_plugin_"), None));
        assert!(reg.all().is_empty(), "invalid tool must not be registered");
        let invalid = reg.invalid_tools();
        assert_eq!(invalid.len(), 1);
        assert_eq!(invalid[0].plugin, "my_plugin_");
        assert_eq!(invalid[0].tool, "my_tool");
        assert!(invalid[0].reason.contains("ends with '_'"));
    }

    #[test]
    fn test_duplicate_registration_is_reported_not_silent() {
        let mut reg = McpRegistry::new();
        reg.register(make_tool("builtin__poll_task", None, None));
        reg.register(make_tool("builtin__poll_task", None, None));
        assert_eq!(reg.all().len(), 1);
        assert_eq!(reg.collisions().len(), 1);
        assert_eq!(reg.collisions()[0].name, "builtin__poll_task");
    }

    #[test]
    fn test_legacy_alias_window_resolves_pre_flip_names() {
        let mut reg = McpRegistry::new();
        reg.register(make_tool("poll_task", Some("builtin"), None));
        assert!(reg.get("builtin__poll_task").is_some());
        assert!(
            reg.get("builtin_poll-task").is_some(),
            "the pre-flip name must still resolve during the alias window"
        );
        assert_eq!(reg.allowed(&["builtin_poll-task".to_string()]).len(), 1);
    }

    #[test]
    fn test_to_openai_tools_excludes_invalid_names() {
        let mut reg = McpRegistry::new();
        reg.register(make_tool("read", Some("filesystem"), None));
        let long = "x".repeat(70);
        reg.register(make_tool(&long, Some("filesystem"), None));
        let names: Vec<String> = reg
            .to_openai_tools_all()
            .iter()
            .map(|t| t["function"]["name"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(names, vec!["filesystem__read".to_string()]);
        assert!(names.iter().all(|n| n.len() <= MAX_EXPOSED_TOOL_NAME_LEN));
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

    #[test]
    fn behavior_sets_are_derived_from_descriptors() {
        let mut reg = McpRegistry::new();
        // (a) A read-only tool under an id nobody hardcoded IS guarded.
        let mut custom = make_tool("zorp_inspect", None, None);
        custom.behavior = ToolBehavior {
            read_only: true,
            ..Default::default()
        };
        reg.register(custom);
        // A name that LOOKS like a legacy read tool but declares no
        // descriptor stays unguarded: fail closed, never allowlist by name.
        reg.register(make_tool("filesystem_read", None, None));

        let guarded = reg.guarded_read_only_tools();
        assert!(
            guarded.contains("zorp_inspect"),
            "descriptor-declared read tool must be guarded"
        );
        assert!(
            !guarded.contains("filesystem_read"),
            "undeclared tool must fail closed"
        );
        assert!(crate::agent::helpers::is_guarded_read_only(
            &guarded,
            "zorp_inspect"
        ));
        assert!(!crate::agent::helpers::is_guarded_read_only(
            &guarded,
            "filesystem_read"
        ));
        // The set handed to the prompt plugin follows descriptors too.
        assert_eq!(reg.read_only_tools(), vec!["zorp_inspect".to_string()]);
    }

    #[test]
    fn own_stack_and_family_sets_follow_descriptors() {
        let mut reg = McpRegistry::new();
        // (b) The docker tool is renamed in its manifest ("compose"); the
        // self-restart guard follows the DESCRIPTOR, so the registry name
        // docker_compose stays protected.
        let mut compose = make_tool("compose", Some("docker"), None);
        compose.behavior = ToolBehavior {
            affects_own_stack: true,
            ..Default::default()
        };
        reg.register(compose);
        // (c) A subtask tool registered under a brand-new id still resets the
        // proactive reminder counter through its declared family.
        let mut zap = make_tool("zap_thread_items", Some("subtasks"), None);
        zap.behavior = ToolBehavior {
            family: Some("subtasks".to_string()),
            ..Default::default()
        };
        reg.register(zap);

        assert!(reg.own_stack_tools().contains("docker__compose"));
        assert!(reg
            .family_tools("subtasks")
            .contains("subtasks__zap_thread_items"));
        assert!(!reg.own_stack_tools().contains("subtasks__zap_thread_items"));
    }

    fn make_tool(name: &str, server: Option<&str>, timeout: Option<u64>) -> McpTool {
        // name IS the full name (the only name): qualify it like real tools.
        let qualified = if let Some(srv) = server {
            tool_qualify(srv, name)
        } else {
            name.to_string()
        };
        McpTool {
            name: qualified.clone(),
            description: format!("Tool: {}", name),
            input_schema: json!({"type": "object", "properties": {}}),
            server_name: server.map(|s| s.to_string()),
            timeout_secs: timeout,
            behavior: ToolBehavior::default(),
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
        let name = tool.name.clone();
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
    fn test_registry_allowed_filters_by_name() {
        let mut registry = McpRegistry::new();
        // name IS the full name (the only name): "fs_read" is the qualified
        // tool name, matching how real tools are registered.
        let tool = McpTool {
            name: "fs_read".to_string(),
            description: "Read tool".to_string(),
            input_schema: json!({"type": "object", "properties": {}}),
            server_name: Some("fs".to_string()),
            timeout_secs: Some(30),
            behavior: ToolBehavior::default(),
            handler: make_test_handler(),
        };
        registry.register(tool);

        // Allowed with matching full name
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
        let content = "héllo wörld - ".repeat(5000);
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
