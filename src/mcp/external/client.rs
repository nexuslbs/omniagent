//! MCP client implementations for stdio and HTTP transports.
//!
//! Each external MCP server is represented by an `McpServerClient` that
//! manages the connection lifecycle: initialize → tools/list → tools/call → shutdown.
//!
//! The `StdioMcpClient` spawns a subprocess and communicates via stdin/stdout
//! using **non-blocking async I/O** (`tokio::process::Command`).
//! The `HttpMcpClient` connects to an HTTP server endpoint using `reqwest` (async).

use crate::err_str;
use crate::error::{AppResult, ErrorContext};
use crate::mcp::external::config::McpServerConfig;
use crate::mcp::external::protocol::*;
use crate::mcp::external::supervisor;
use crate::mcp::{McpTool, McpToolResult};
use async_trait::async_trait;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio::task::JoinHandle;
use tokio::time::timeout;

// ---------------------------------------------------------------------------
// Circuit breaker state
// ---------------------------------------------------------------------------

/// Circuit breaker states for external MCP servers.
#[derive(Debug, Clone, PartialEq)]
pub enum CircuitState {
    /// Normal operation: requests are allowed.
    Closed,
    /// Too many failures: requests are blocked.
    Open,
    /// Healing period: one test request is allowed.
    #[allow(dead_code)]
    HalfOpen,
}

/// Per-server circuit breaker.
#[derive(Debug, Clone)]
pub struct CircuitBreaker {
    state: Arc<parking_lot::Mutex<CircuitStateInner>>,
}

#[derive(Debug)]
struct CircuitStateInner {
    state: CircuitState,
    consecutive_failures: u32,
    max_retries: u32,
    /// When the circuit was opened (std::time::Instant ticks). None when closed.
    opened_at: Option<std::time::Instant>,
}

impl CircuitBreaker {
    pub fn new(max_retries: u32) -> Self {
        Self {
            state: Arc::new(parking_lot::Mutex::new(CircuitStateInner {
                state: CircuitState::Closed,
                consecutive_failures: 0,
                max_retries,
                opened_at: None,
            })),
        }
    }

    /// Check if a request is allowed.
    ///
    /// Aug 2026: ALWAYS returns true. A circuit breaker must never make a
    /// plugin "stop working": a counted failure (timeout, tool error, or even
    /// a transport hiccup) does not mean the plugin is broken - it can be a
    /// long build, a slow network, or a busy server. The agent decides what is
    /// wrong: it sees the error, and if a task takes too long it cancels it
    /// with cancel-task. On a genuine transport failure the client fails the
    /// call loudly AND respawns the plugin process (see call_tool), so the
    /// next call works - the agent picks it up later. Blocking tool calls is
    /// never the right behavior.
    pub fn is_allowed(&self) -> bool {
        true
    }

    /// Record a successful request: resets failure count.
    pub fn record_success(&self) {
        let mut inner = self.state.lock();
        inner.consecutive_failures = 0;
        inner.state = CircuitState::Closed;
        inner.opened_at = None;
    }

    /// Record a failed request. Opens the circuit if max retries exceeded.
    pub fn record_failure(&self) {
        let mut inner = self.state.lock();
        inner.consecutive_failures += 1;
        if inner.consecutive_failures >= inner.max_retries {
            inner.state = CircuitState::Open;
            inner.opened_at = Some(std::time::Instant::now());
            tracing::warn!(
                "Circuit breaker opened after {} consecutive failures (will recover after 30s cooldown)",
                inner.consecutive_failures
            );
        }
    }

    /// Get the current state (for diagnostics).
    pub fn state(&self) -> CircuitState {
        self.state.lock().state.clone()
    }
}

// ---------------------------------------------------------------------------
// Server health status
// ---------------------------------------------------------------------------

/// Health status of an external MCP server.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct ServerHealth {
    pub connected: bool,
    pub tool_count: usize,
    pub circuit_state: CircuitState,
    pub last_error: Option<String>,
}

// ---------------------------------------------------------------------------
// MCP Server Client trait (async)
// ---------------------------------------------------------------------------

/// A client for an external MCP server.
#[async_trait]
pub trait McpServerClient: Send + Sync {
    /// Initialize the connection and discover available tools.
    async fn initialize(&self) -> AppResult<Vec<McpExternalTool>>;

    /// Call a tool on the server. `meta` carries runtime context (channel_id, etc.)
    /// and is sent as the `_meta` field in the JSON-RPC params.
    async fn call_tool(
        &self,
        name: &str,
        arguments: &Value,
        meta: Option<Value>,
    ) -> AppResult<McpToolResult>;

    /// Shutdown the connection.
    async fn shutdown(&self) -> AppResult<()>;

    /// Get the server's display name.
    fn name(&self) -> &str;

    /// Check server health.
    #[allow(dead_code)]
    fn health(&self) -> ServerHealth;

    /// Get the server's per-tool timeout in seconds.
    /// `None` = no timeout (the default): tools run until done; the agent
    /// tracks/cancels them via background tasks. Only Some() when explicitly
    /// configured.
    fn timeout_secs(&self) -> Option<u64> {
        None
    }

    /// Stop supervising the child process of this client.
    ///
    /// Called when a client is REPLACED or REMOVED (plugin disable, reload, a
    /// fresh client for the same server): a superseded client must never
    /// restart a child behind the registry's back. Default: no-op (HTTP clients
    /// and test doubles have no child).
    fn stop_supervision(&self) {}

    /// Restart this client's child after a crash, with bounded backoff.
    /// `Ok(true)` = serving again, `Ok(false)` = intentionally stopped,
    /// `Err(reason)` = gave up (loud). Default: this client is not supervised.
    async fn restart_after_crash(&self) -> Result<bool, String> {
        Err("this MCP client is not supervised for restarts".to_string())
    }

    /// Per-tool behaviour declared by the plugin manifest (audit V-2).
    /// The default is EMPTY: an undeclared tool stays behaviour-neutral
    /// (fail closed) - core never guesses read-only-ness from a name.
    fn tool_behaviors(&self) -> &crate::mcp::behavior::ToolBehaviorMap {
        static EMPTY: once_cell::sync::Lazy<crate::mcp::behavior::ToolBehaviorMap> =
            once_cell::sync::Lazy::new(crate::mcp::behavior::ToolBehaviorMap::new);
        &EMPTY
    }

    /// Convert external tools to McpTool instances with a circuit-breaking wrapper.
    async fn to_mcp_tools(&self) -> Vec<McpTool> {
        let tools = match self.initialize().await {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(
                    "Failed to initialize external MCP server '{}': {:?}",
                    self.name(),
                    e
                );
                return vec![];
            }
        };

        let server_name = self.name().to_string();
        // Descriptors declared by the plugin manifest travel with the server
        // config; they are matched against the raw tool name from the server.
        let behaviors = self.tool_behaviors().clone();
        let mut result = Vec::with_capacity(tools.len());

        for t in tools {
            // Prefix tool names with server name to avoid collisions
            // in the registry HashMap (e.g., "test-python-tool_echo").
            // Uses the unified tool_qualify() function which handles both
            // already-prefixed names (strips redundant prefix) and bare names.
            // The output is always {hyphenated-server}_{hyphenated-tool}
            // tool_qualify is the single source of truth for tool naming.
            let prefixed_name = crate::mcp::tool_qualify(&server_name, &t.name);
            let schema = convert_input_schema(&t.input_schema);
            // Use a direct, unambiguous description that tells the LLM this is
            // a callable function, not something requiring filesystem discovery.
            let description = format!("{} (callable via function-calling API)", t.description);
            let sn = server_name.clone();
            let tn = t.name.clone();

            result.push(McpTool {
                name: prefixed_name.clone(),
                description,
                input_schema: schema,
                server_name: Some(server_name.clone()),
                timeout_secs: self.timeout_secs(),
                behavior: crate::mcp::behavior::for_tool(&behaviors, &server_name, &prefixed_name),
                handler: Arc::new(move |args: Value, ctx: crate::mcp::AppContext| {
                    let sn = sn.clone();
                    let tn = tn.clone();
                    Box::pin(async move {
                        // Build _meta context from AppContext (channel_id always, profile_name ALWAYS non-empty)
                        let mut meta_map = serde_json::Map::new();
                        if let Some(ref cid) = ctx.current_channel_id {
                            meta_map.insert("channel_id".to_string(), serde_json::json!(cid));
                        }
                        if let Some(tid) = ctx.current_thread_id {
                            meta_map.insert("thread_id".to_string(), serde_json::json!(tid));
                        }
                        // NEVER omit: an absent _meta.profile_name made the
                        // remote memory plugin invent `profiles/default`
                        // (telegram thread 2719). Fall back to the resolved
                        // default profile, which is a DECLARED profile.
                        meta_map.insert(
                            "profile_name".to_string(),
                            serde_json::json!(crate::mcp::meta_profile_name(&ctx)),
                        );
                        if let Some(ref plat) = ctx.current_platform {
                            meta_map.insert("platform".to_string(), serde_json::json!(plat));
                        }
                        if let Some(ref cn) = ctx.current_channel_name {
                            meta_map.insert("channel_name".to_string(), serde_json::json!(cn));
                        }
                        let meta = if meta_map.is_empty() {
                            None
                        } else {
                            Some(Value::Object(meta_map))
                        };

                        match ctx.external_clients.call_tool(&sn, &tn, &args, meta).await {
                            Ok(res) => Ok(res),
                            Err(e) => Ok(McpToolResult {
                                call_id: String::new(),
                                content: format!(
                                    "External MCP server '{}' tool '{}' failed: {}",
                                    sn, tn, e
                                ),
                                is_error: true,
                            }),
                        }
                    })
                }),
            });
        }

        result
    }
}

/// Per-server MCP client registry.
///
/// Owns all active MCP client instances (stdio and HTTP), one per server,
/// shared across all channels. Replaces the former per-channel `PoolManager`.
/// Populated during startup initialization and on hot-reload.
pub struct ExternalMcpClients {
    clients: parking_lot::RwLock<HashMap<String, Arc<dyn McpServerClient>>>,
}

impl std::fmt::Debug for ExternalMcpClients {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let names: Vec<String> = self.clients.read().keys().cloned().collect();
        f.debug_struct("ExternalMcpClients")
            .field("clients", &names)
            .finish()
    }
}

impl Default for ExternalMcpClients {
    fn default() -> Self {
        Self::new()
    }
}

impl ExternalMcpClients {
    /// Create a new empty client registry.
    pub fn new() -> Self {
        Self {
            clients: parking_lot::RwLock::new(HashMap::new()),
        }
    }

    /// Register an MCP client for a server.
    ///
    /// A replaced client stops supervising its child first, so one plugin can
    /// never end up with two live MCP children (the old client must not restart
    /// a child behind the registry's back).
    pub fn register(&self, name: &str, client: Arc<dyn McpServerClient>) {
        let replaced = {
            let mut registry = self.clients.write();
            registry.insert(name.to_string(), client)
        };
        if let Some(old) = replaced {
            old.stop_supervision();
        }
    }

    /// Remove an MCP client (e.g. on disable).
    pub fn remove(&self, name: &str) {
        let removed = {
            let mut registry = self.clients.write();
            registry.remove(name)
        };
        if let Some(old) = removed {
            old.stop_supervision();
        }
    }

    /// Names of the servers that currently hold a client in this process.
    pub fn server_names(&self) -> Vec<String> {
        self.clients.read().keys().cloned().collect()
    }

    /// Get a client by server name.
    pub fn get(&self, name: &str) -> Option<Arc<dyn McpServerClient>> {
        self.clients.read().get(name).cloned()
    }

    /// Get the tool timeout for a server.
    pub fn get_timeout_secs(&self, server_name: &str) -> Option<u64> {
        self.get(server_name).and_then(|c| c.timeout_secs())
    }

    /// Call a tool on the specified MCP server.
    pub async fn call_tool(
        &self,
        server_name: &str,
        tool_name: &str,
        args: &Value,
        meta: Option<Value>,
    ) -> AppResult<McpToolResult> {
        let client = self.get(server_name).ok_or_else(|| {
            err_str!(
                "MCP server '{}' not found in client registry (not initialized)",
                server_name
            )
        })?;
        match client.call_tool(tool_name, args, meta).await {
            Ok(r) => Ok(r),
            Err(e) => Err(err_str!(
                "{} [liveness: {}]",
                e,
                supervisor::summary_line(server_name)
            )),
        }
    }
}

/// Convert MCP inputSchema to the JSON Schema format the LLM expects.
fn convert_input_schema(schema: &Value) -> Value {
    // MCP inputSchema is already JSON Schema-compatible.
    // We just ensure the required fields exist.
    if schema.is_object() {
        let mut s = schema.clone();
        if s.get("type").is_none() {
            s["type"] = Value::String("object".to_string());
        }
        s
    } else {
        serde_json::json!({
            "type": "object",
            "properties": {}
        })
    }
}

// ---------------------------------------------------------------------------
// Multiplexed MCP Client (async, stdio)
// ---------------------------------------------------------------------------

/// An MCP client that communicates with a subprocess via stdin/stdout
/// using a **multiplexed** design: all JSON-RPC requests share one subprocess
/// but concurrent callers are dispatched via request IDs instead of a Mutex.
///
/// A background reader task reads stdout lines, parses JSON-RPC responses,
/// and sends the result to the waiting caller via a oneshot channel keyed
/// by request ID. The `call_tool` method writes to stdin via an mpsc channel
/// (non-blocking) and awaits the oneshot - zero locks in the hot path.
///
/// This replaces the old `Mutex<Option<AsyncChildProcess>>` which serialized
/// ALL tool calls to the same server, even when the server could handle
/// concurrent requests.
pub struct StdioMcpClient {
    config: McpServerConfig,
    /// Non-blocking sender for writing JSON-RPC requests to stdin.
    stdin_tx: Mutex<Option<mpsc::UnboundedSender<String>>>,
    /// Pending requests keyed by JSON-RPC request ID.
    pending: Arc<parking_lot::Mutex<HashMap<u64, oneshot::Sender<String>>>>,
    /// Background task: reads stdout and dispatches responses by ID.
    read_task: Mutex<Option<JoinHandle<()>>>,
    /// Background task: drains the request channel and writes stdin.
    /// Kept separate from read_task so a burst of responses can never starve
    /// request writes (single select! loop dropped requests under load -
    /// G17b, Aug 2026).
    write_task: Mutex<Option<JoinHandle<()>>>,
    /// Child process handle (for lifecycle/cleanup).
    child: Mutex<Option<tokio::process::Child>>,
    next_id: AtomicU64,
    tools: Mutex<Vec<McpExternalTool>>,
    circuit: CircuitBreaker,
    /// Consecutive tools/call timeouts with no success in between. Used to
    /// detect a wedged (alive but unresponsive) plugin so the client can
    /// auto-restart it instead of timing out for a whole thread (filesystem
    /// MCP outage, Sep 2026: one unbounded walk left the server busy-looping
    /// forever and every call timed out at the configured limit).
    consecutive_timeouts: AtomicU64,
    connected: Mutex<bool>,
    last_error: Mutex<Option<String>>,
    /// Weak self handle for the child-liveness watchdog. Armed right after
    /// construction (while the client is still an `Arc`); unset means no
    /// watchdog (directly constructed clients in unit tests).
    self_weak: parking_lot::RwLock<Option<Weak<StdioMcpClient>>>,
    /// Child generation: bumped whenever the current child is superseded
    /// (respawn / shutdown / replaced client), so the watchdog of a child that
    /// was killed ON PURPOSE exits silently instead of reporting a crash.
    generation: AtomicU64,
    /// Set when this client is torn down on purpose.
    shutting_down: AtomicBool,
    /// Serializes spawn/respawn/supervised restart: one plugin can never end
    /// up with two live MCP children.
    lifecycle_gate: Mutex<()>,
    /// Background task: captures the child's stderr into the bounded tail.
    stderr_task: Mutex<Option<JoinHandle<()>>>,
    /// Background task: child-liveness watchdog (exit detection + restart).
    watchdog: Mutex<Option<JoinHandle<()>>>,
}

/// How often the child-liveness watchdog checks the child's exit status.
const WATCHDOG_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Outcome of a supervised restart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RestartOutcome {
    Restarted,
    GaveUp,
    Stopped,
}

/// Human text for an exit status: exit code or terminating signal.
fn exit_status_text(status: std::process::ExitStatus) -> String {
    match status.code() {
        Some(code) => format!("exit code {code}"),
        None => format!("{status}"),
    }
}

/// Last few captured stderr lines as a single line, for embedding in a message.
fn stderr_tail_one_line(server: &str) -> String {
    let tail = supervisor::stderr_tail(server);
    if tail.is_empty() {
        return "no stderr output captured".to_string();
    }
    let start = tail.len().saturating_sub(3);
    tail[start..].join(" | ")
}

/// Drop guard for an in-flight `tools/call` request (stdio transport).
///
/// When the future awaiting the response is dropped BEFORE the response
/// arrives (thread ended, /stop-thread, channel close, client-side timeout,
/// executor cancellation), the guard sends an MCP `notifications/cancelled`
/// for the request id and drops the pending entry. The shared server framework
/// (mcp-server-util) then aborts the handler task, and plugins that wrap
/// subprocesses in kill-on-drop guards (docker compose) kill the underlying
/// OS process - so no tool-spawned subprocess survives the thread that issued
/// it (thread 73, Aug 2026: `docker compose exec … cargo` chain still alive
/// 6+ minutes after the thread ended).
///
/// On a NORMAL completion the guard is disarmed and Drop is a no-op.
struct InFlightCallGuard {
    id: u64,
    stdin: Option<mpsc::UnboundedSender<String>>,
    pending: Arc<parking_lot::Mutex<HashMap<u64, oneshot::Sender<String>>>>,
    armed: bool,
}

impl InFlightCallGuard {
    fn new(
        id: u64,
        stdin: Option<mpsc::UnboundedSender<String>>,
        pending: Arc<parking_lot::Mutex<HashMap<u64, oneshot::Sender<String>>>>,
    ) -> Self {
        Self {
            id,
            stdin,
            pending,
            armed: true,
        }
    }

    /// Disarm the guard: the response arrived, do not cancel.
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for InFlightCallGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        // Drop the pending entry so a late response can't dispatch to a dead
        // oneshot (and so the map doesn't accumulate entries if the server is
        // wedged and never answers).
        self.pending.lock().remove(&self.id);
        // Best-effort notify: the server aborts the handler and kills the
        // underlying subprocess (docker KillOnDrop). The unbounded channel
        // send never blocks; a closed channel (server gone) is a silent no-op.
        if let Some(sender) = &self.stdin {
            let notif = build_cancel_notification(self.id);
            let _ = sender.send(notif);
        }
    }
}

/// Fail ALL pending requests with a JSON-RPC error so every waiting caller
/// receives a concrete error instead of hanging until a timeout. Called when
/// the connection dies (write error, read error, EOF) - the agent must KNOW
/// an error happened and which one, never believe a call is still running.
fn fail_all_pending(
    pending: &Arc<parking_lot::Mutex<HashMap<u64, oneshot::Sender<String>>>>,
    server_name: &str,
    reason: &str,
) {
    let mut map = pending.lock();
    let ids: Vec<u64> = map.keys().copied().collect();
    if ids.is_empty() {
        return;
    }
    tracing::error!(
        "MCP server '{}' connection failed ({}): failing {} pending request(s) loudly",
        server_name,
        reason,
        ids.len()
    );
    for id in ids {
        if let Some(tx) = map.remove(&id) {
            let err = serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {
                    "code": -32000,
                    "message": format!("MCP server '{}' connection failed: {}", server_name, reason),
                },
            });
            let _ = tx.send(err.to_string());
        }
    }
}

impl StdioMcpClient {
    pub fn new(config: McpServerConfig) -> Self {
        Self {
            circuit: CircuitBreaker::new(config.max_retries),
            consecutive_timeouts: AtomicU64::new(0),
            config,
            stdin_tx: Mutex::new(None),
            pending: Arc::new(parking_lot::Mutex::new(HashMap::new())),
            read_task: Mutex::new(None),
            write_task: Mutex::new(None),
            child: Mutex::new(None),
            next_id: AtomicU64::new(1),
            tools: Mutex::new(Vec::new()),
            connected: Mutex::new(false),
            last_error: Mutex::new(None),
            self_weak: parking_lot::RwLock::new(None),
            generation: AtomicU64::new(0),
            shutting_down: AtomicBool::new(false),
            lifecycle_gate: Mutex::new(()),
            stderr_task: Mutex::new(None),
            watchdog: Mutex::new(None),
        }
    }

    /// Arm liveness supervision. Must be called ONCE, right after construction,
    /// while the client is still an `Arc` (the watchdog needs a weak handle).
    pub fn arm_supervision(self: &Arc<Self>) {
        *self.self_weak.write() = Some(Arc::downgrade(self));
        supervisor::clear(&self.config.name);
    }

    /// Supersede the current child's watchdog. Call BEFORE killing a child on
    /// purpose: the watchdog then exits without reporting a crash.
    fn supersede(&self) -> u64 {
        self.generation.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// Current pid of the live child (None when there is no child).
    async fn child_pid(&self) -> Option<u32> {
        self.child.lock().await.as_ref().and_then(|c| c.id())
    }

    /// True when a live child exists AND both transport tasks are running.
    async fn connection_healthy(&self) -> bool {
        if !*self.connected.lock().await {
            return false;
        }
        {
            let wt = self.write_task.lock().await;
            if wt.as_ref().map(|h| h.is_finished()).unwrap_or(true) {
                return false;
            }
        }
        {
            let rt = self.read_task.lock().await;
            if rt.as_ref().map(|h| h.is_finished()).unwrap_or(true) {
                return false;
            }
        }
        let mut guard = self.child.lock().await;
        match guard.as_mut() {
            None => false,
            Some(child) => matches!(child.try_wait(), Ok(None)),
        }
    }

    /// Spawn the subprocess and start the background reader task.
    /// Returns the stdin sender for writing requests.
    async fn spawn_process(&self) -> AppResult<mpsc::UnboundedSender<String>> {
        let cmd = self.config.command.as_ref().ok_or_else(|| {
            err_str!(
                "stdio MCP server '{}' has no command configured",
                self.config.name
            )
        })?;

        tracing::info!(
            "Spawning external MCP server '{}': {} {}",
            self.config.name,
            cmd,
            self.config.args.join(" ")
        );

        let mut command = Command::new(cmd);
        command
            .args(&self.config.args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            // CAPTURED, not inherited: the child's stderr feeds a bounded tail
            // buffer so a crash can be reported WITH the child's own error
            // output (every line is mirrored to the process log as
            // `[mcp:<plugin>] ...`, so nothing becomes invisible).
            .stderr(std::process::Stdio::piped());
        // Platform-level env isolation (2026-09-01): never inherit the agent's
        // ambient environment. Empty env, then only the explicitly configured
        // env below plus an explicit minimal PATH for the child's own spawns.
        command.env_clear();
        command.env("PATH", crate::process_env::child_path());
        // HOME_FOR_PLUGIN_CHILD: the child env is isolated and never inherits
        // the server's HOME. Several MCP servers (Go binaries with an embedded
        // SQLite store, node tools using a cache dir) resolve their state
        // directory from $HOME and abort at startup when it is undefined
        // ("determine home directory: $HOME is not defined"). Default it to
        // the server working directory (the plugin dir) so state persists
        // across restarts, falling back to the temp dir.
        if !self.config.env.contains_key("HOME") {
            let home = self
                .config
                .current_dir
                .clone()
                .unwrap_or_else(|| std::env::temp_dir().to_string_lossy().to_string());
            command.env("HOME", home);
        }

        if let Some(dir) = &self.config.current_dir {
            command.current_dir(dir);
        }
        for (key, value) in &self.config.env {
            command.env(key, value);
        }

        let mut child = command
            .spawn()
            .ctx(format!("Failed to spawn MCP server '{}'", self.config.name))?;

        // The child exists: the recorded liveness becomes "starting".
        supervisor::record_starting(&self.config.name, child.id());

        let child_stdin = child.stdin.take().ok_or_else(|| {
            err_str!("Failed to open stdin for MCP server '{}'", self.config.name)
        })?;
        let child_stdout = child.stdout.take().ok_or_else(|| {
            err_str!(
                "Failed to open stdout for MCP server '{}'",
                self.config.name
            )
        })?;
        let child_stderr = child.stderr.take().ok_or_else(|| {
            err_str!(
                "Failed to open stderr for MCP server '{}'",
                self.config.name
            )
        })?;

        // Stderr capture task: mirrors every child stderr line to the process
        // log (`[mcp:<plugin>] ...`) and keeps the last N in a bounded ring
        // buffer, so a later crash or a failed handshake can report the child's
        // OWN error text instead of a generic "did not initialize".
        let stderr_name = self.config.name.clone();
        let stderr_handle = tokio::spawn(async move {
            let mut lines = BufReader::new(child_stderr);
            let mut buf = String::new();
            loop {
                buf.clear();
                match lines.read_line(&mut buf).await {
                    Ok(0) => break,
                    Ok(_) => supervisor::push_stderr_line(&stderr_name, &buf),
                    Err(_) => break,
                }
            }
        });
        {
            let mut st = self.stderr_task.lock().await;
            if let Some(old) = st.take() {
                old.abort();
            }
            *st = Some(stderr_handle);
        }

        // Create mpsc channel for writing requests
        let (stdin_tx, mut stdin_rx) = mpsc::unbounded_channel::<String>();
        let pending = self.pending.clone();
        let server_name = self.config.name.clone();
        let reader = BufReader::new(child_stdout);

        // ─────────────────────────────────────────────────────────────────
        // TWO DEDICATED TASKS (writer + reader), never one select! loop.
        //
        // Before Aug 2026 a single `tokio::select!` loop handled BOTH stdin
        // writes AND stdout reads. Under a burst of concurrent calls (50
        // parallel docker_compose exec), the read branch was constantly
        // awakened by streaming responses and STARVED the write branch: the
        // loop stopped dequeuing from `stdin_rx` after ~46 of 50 requests,
        // the rest sat in the unbounded channel forever (unbounded sends
        // never fail - silent loss by construction), and those callers hung
        // until their timeout. Splitting into a dedicated writer task makes
        // starvation impossible: each task does exactly one job.
        //
        // On ANY connection failure (write error, read error, EOF), ALL
        // pending requests are failed LOUDLY with a JSON-RPC error so every
        // caller gets a concrete error - the agent KNOWS an error happened
        // and which one, instead of thinking the call is still running.
        // ─────────────────────────────────────────────────────────────────

        // Writer task: drains the request channel → stdin (atomic line writes).
        let writer_pending = pending.clone();
        let writer_name = server_name.clone();
        let writer_handle = tokio::spawn(async move {
            let mut writer = child_stdin;
            while let Some(request) = stdin_rx.recv().await {
                // ATOMIC LINE WRITE: `request + "\n"` in ONE buffer, one
                // write_all (one await, one OS write). Two separate write_all
                // calls (request, then "\n") let the runtime interleave a
                // second request between them, producing `req1req2\n` - the
                // server's serde parse fails and the request is dropped.
                let mut buf = String::with_capacity(request.len() + 1);
                buf.push_str(&request);
                buf.push('\n');
                if let Err(e) = writer.write_all(buf.as_bytes()).await {
                    tracing::error!("MCP server '{}' stdin write error: {}", writer_name, e);
                    fail_all_pending(
                        &writer_pending,
                        &writer_name,
                        &format!("stdin write error: {e}"),
                    );
                    break;
                }
                if let Err(e) = writer.flush().await {
                    tracing::error!("MCP server '{}' stdin flush error: {}", writer_name, e);
                    fail_all_pending(
                        &writer_pending,
                        &writer_name,
                        &format!("stdin flush error: {e}"),
                    );
                    break;
                }
            }
            tracing::info!("MCP server '{}' background writer stopped", writer_name);
        });

        // Reader task: reads stdout lines and routes responses by ID.
        let reader_pending = pending.clone();
        let reader_name = server_name.clone();
        let reader_handle = tokio::spawn(async move {
            let mut reader = reader;
            let mut line_buf = String::new();
            loop {
                match reader.read_line(&mut line_buf).await {
                    Ok(0) => {
                        // EOF: the server process is gone. Fail every pending
                        // caller loudly - the agent must KNOW the connection
                        // died, not wait forever.
                        tracing::info!("MCP server '{}' closed stdout", reader_name);
                        fail_all_pending(
                            &reader_pending,
                            &reader_name,
                            "connection closed (server exited)",
                        );
                        break;
                    }
                    Ok(_) => {
                        let line = std::mem::take(&mut line_buf);
                        let trimmed = line.trim().to_string();
                        if trimmed.is_empty() {
                            continue;
                        }
                        // Parse JSON-RPC response to extract ID
                        if let Ok(val) = serde_json::from_str::<serde_json::Value>(&trimmed) {
                            if let Some(id_val) = val.get("id") {
                                if let Some(id) = id_val.as_u64() {
                                    let mut map = reader_pending.lock();
                                    if let Some(tx) = map.remove(&id) {
                                        let _ = tx.send(trimmed);
                                    } else {
                                        tracing::warn!(
                                            "MCP server '{}' response id={} NOT in pending map",
                                            reader_name,
                                            id
                                        );
                                    }
                                }
                            }
                        }
                    }
                    Err(e) => {
                        tracing::error!("MCP server '{}' stdout read error: {}", reader_name, e);
                        fail_all_pending(
                            &reader_pending,
                            &reader_name,
                            &format!("stdout read error: {e}"),
                        );
                        break;
                    }
                }
            }
            tracing::info!("MCP server '{}' background reader stopped", reader_name);
        });

        *self.read_task.lock().await = Some(reader_handle);
        *self.write_task.lock().await = Some(writer_handle);
        *self.child.lock().await = Some(child);
        *self.stdin_tx.lock().await = Some(stdin_tx.clone());

        // Child-liveness watchdog: the ONLY place that notices a child died,
        // with a real exit status. Before this a dead child was invisible (its
        // tools silently stopped working and every thread saw a bare
        // `Unknown tool`).
        self.start_watchdog().await;

        Ok(stdin_tx)
    }

    /// Start the liveness watchdog for the CURRENT child generation.
    ///
    /// The watchdog polls the child's exit status every
    /// [`WATCHDOG_POLL_INTERVAL`]; on an UNEXPECTED exit it
    ///  1. logs ERROR with plugin, pid, exit code/signal and the stderr tail,
    ///  2. records the crash in the supervision registry, and
    ///  3. performs the supervised restart (bounded backoff, loud give-up).
    ///
    /// A child killed ON PURPOSE bumps the generation first, so its watchdog
    /// exits silently and a deliberate kill is never reported as a crash.
    async fn start_watchdog(&self) {
        let Some(weak) = self.self_weak.read().clone() else {
            tracing::debug!(
                "MCP server '{}' has no supervision handle armed - child liveness is not watched",
                self.config.name
            );
            return;
        };
        let generation = self.generation.load(Ordering::SeqCst);
        let name = self.config.name.clone();
        let handle = tokio::spawn(async move {
            loop {
                tokio::time::sleep(WATCHDOG_POLL_INTERVAL).await;
                let Some(client) = weak.upgrade() else {
                    return; // client dropped: nobody left to serve
                };
                if client.generation.load(Ordering::SeqCst) != generation {
                    return; // superseded on purpose
                }
                let exit = {
                    let mut guard = client.child.lock().await;
                    match guard.as_mut() {
                        None => None,
                        Some(child) => {
                            let pid = child.id();
                            match child.try_wait() {
                                Ok(Some(status)) => Some((pid, exit_status_text(status))),
                                Ok(None) => None,
                                Err(e) => Some((pid, format!("wait error: {e}"))),
                            }
                        }
                    }
                };
                let Some((pid, status)) = exit else { continue };
                // Reap the handle: this child is gone for good.
                {
                    let mut guard = client.child.lock().await;
                    guard.take();
                }
                if client.shutting_down.load(Ordering::SeqCst) {
                    return; // deliberate teardown, not a crash
                }
                let tail = supervisor::stderr_tail(&name);
                let msg = supervisor::record_crashed(&name, pid, &status, &tail);
                tracing::error!("{}", msg);
                // Hand the restart to the installed hook: a `dyn` boundary, so
                // this future never awaits the restart chain that spawns the
                // NEXT watchdog (such a recursive future cannot be proven
                // `Send`). The restarted child gets its own watchdog.
                supervisor::request_restart(&name);
                return;
            }
        });
        {
            let mut wd = self.watchdog.lock().await;
            if let Some(old) = wd.take() {
                old.abort();
            }
            *wd = Some(handle);
        }
    }

    /// Restart a crashed child with bounded backoff. Never spawns a second
    /// child (the lifecycle gate + `respawn_if_needed` reuse a healthy child).
    async fn supervise_restart(&self, name: &str) -> RestartOutcome {
        let mut last_error = "unknown failure".to_string();
        for attempt in 1..=supervisor::MAX_RESTART_ATTEMPTS {
            let delay = supervisor::backoff_for_attempt(attempt);
            supervisor::record_restarting(name, attempt, delay);
            tracing::warn!(
                "MCP server '{}' crashed - supervised restart attempt {}/{} in {}s",
                name,
                attempt,
                supervisor::MAX_RESTART_ATTEMPTS,
                delay.as_secs()
            );
            tokio::time::sleep(delay).await;
            if self.shutting_down.load(Ordering::SeqCst) {
                return RestartOutcome::Stopped;
            }
            match self.respawn_if_needed().await {
                Ok(true) => {
                    let pid = self.child_pid().await;
                    let tools = self.tools.lock().await.len();
                    supervisor::record_restarted(name, pid, tools);
                    tracing::info!(
                        "MCP server '{}' auto-restarted after a crash (attempt {}, {} tool(s))",
                        name,
                        attempt,
                        tools
                    );
                    // A reload that ran while the child was dead may have
                    // dropped its tools from the registry: ask the server layer
                    // to put them back (no-op when they are still registered).
                    supervisor::notify_tools_revived(name);
                    return RestartOutcome::Restarted;
                }
                Ok(false) => {
                    // Another caller already revived a healthy child.
                    let pid = self.child_pid().await;
                    let tools = self.tools.lock().await.len();
                    supervisor::record_running(name, pid, tools);
                    return RestartOutcome::Restarted;
                }
                Err(e) => {
                    last_error = e.to_string();
                    tracing::error!(
                        "MCP server '{}' supervised restart attempt {}/{} failed: {}",
                        name,
                        attempt,
                        supervisor::MAX_RESTART_ATTEMPTS,
                        last_error
                    );
                }
            }
        }
        let msg = supervisor::record_give_up(name, supervisor::MAX_RESTART_ATTEMPTS, &last_error);
        tracing::error!("{}", msg);
        RestartOutcome::GaveUp
    }

    /// Send a JSON-RPC request via the multiplexed channel and await the response.
    /// Returns the response string on success.
    async fn send_and_await(&self, request: &str, id: u64, timeout_secs: u64) -> AppResult<String> {
        let (tx, rx) = oneshot::channel();

        // Insert sender into pending map BEFORE sending (avoid race)
        {
            let mut pending = self.pending.lock();
            pending.insert(id, tx);
        }

        // Send the request via mpsc (non-blocking)
        let tx_guard = self.stdin_tx.lock().await;
        let sender = tx_guard
            .as_ref()
            .ok_or_else(|| err_str!("MCP server '{}' not initialized", self.config.name))?;
        sender
            .send(request.to_string())
            .map_err(|_| err_str!("MCP server '{}' stdin channel closed", self.config.name))?;
        drop(tx_guard); // release lock before awaiting

        // Await the response via oneshot with timeout
        let response = timeout(Duration::from_secs(timeout_secs), rx)
            .await
            .map_err(|_| {
                // Clean up pending entry on timeout
                let mut pending = self.pending.lock();
                pending.remove(&id);
                err_str!(
                    "MCP server '{}' did not respond within {}s (id={})",
                    self.config.name,
                    timeout_secs,
                    id
                )
            })?
            .map_err(|_| {
                err_str!(
                    "MCP server '{}' response channel cancelled (id={})",
                    self.config.name,
                    id
                )
            })?;

        Ok(response)
    }

    /// Run a full MCP handshake: configure → initialize → initialized notification → tools/list.
    /// Uses the multiplexed channel (background reader must be started first).
    async fn initialize_handshake(
        &self,
        config_env: &HashMap<String, String>,
    ) -> AppResult<ListToolsResult> {
        let server_name = &self.config.name;

        // Step 0: Send plugin configuration before initialize
        if !config_env.is_empty() {
            let cfg_req = build_configure_request(config_env);
            // configure always uses id=0 (hardcoded in build_configure_request)
            let ack = self.send_and_await(&cfg_req, 0, 5).await?;
            tracing::debug!(
                "MCP server '{}' configure response: {}",
                server_name,
                ack.trim()
            );
        }

        // Step 1: Initialize
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let req = build_initialize_request(id);
        let response = self.send_and_await(&req, id, 30).await?;
        let init_result =
            match parse_response(&response).ctx("Failed to parse MCP initialize response")? {
                JsonRpcResponse::Success { result, .. } => result,
                JsonRpcResponse::Error { error, .. } => {
                    return Err(err_str!(
                        "MCP initialize error ({}): {}",
                        error.code,
                        error.message
                    ));
                }
            };

        if let Some(server_info) = init_result.get("serverInfo") {
            tracing::info!(
                "MCP server '{}' connected: {} v{}",
                server_name,
                server_info
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown"),
                server_info
                    .get("version")
                    .and_then(|v| v.as_str())
                    .unwrap_or("0"),
            );
        }

        // Step 2: Send initialized notification (no response expected)
        let notif = build_initialized_notification();
        let tx_guard = self.stdin_tx.lock().await;
        if let Some(sender) = tx_guard.as_ref() {
            let _ = sender.send(notif);
        }
        drop(tx_guard);

        // Step 3: List tools
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let req = build_list_tools_request(id);
        let response = self.send_and_await(&req, id, 30).await?;
        let list_result =
            match parse_response(&response).ctx("Failed to parse MCP tools/list response")? {
                JsonRpcResponse::Success { result, .. } => result,
                JsonRpcResponse::Error { error, .. } => {
                    return Err(err_str!(
                        "MCP tools/list error ({}): {}",
                        error.code,
                        error.message
                    ));
                }
            };

        let tools: ListToolsResult =
            serde_json::from_value(list_result).ctx("Failed to parse tools/list result")?;

        tracing::info!(
            "MCP server '{}' exposes {} tool(s)",
            server_name,
            tools.tools.len()
        );

        Ok(tools)
    }

    /// Respawn the plugin subprocess after a connection loss.
    ///
    /// Kills the old child (if any), aborts the dead writer/reader tasks,
    /// clears transport state, re-spawns the process, and re-runs the full MCP
    /// handshake (configure → initialize → tools/list). On success the client
    /// is ready for the next tool call. The current (failed) call still
    /// returns an error to the agent - the respawn benefits the NEXT call.
    async fn respawn(&self) -> AppResult<bool> {
        self.respawn_if_needed().await
    }

    /// Gated respawn: holds the lifecycle gate and REUSES a healthy child, so
    /// the watchdog's supervised restart and a concurrently failing `call_tool`
    /// can never spawn two children for one plugin.
    async fn respawn_if_needed(&self) -> AppResult<bool> {
        let _gate = self.lifecycle_gate.lock().await;
        if self.connection_healthy().await {
            return Ok(false);
        }
        self.respawn_locked().await?;
        Ok(true)
    }

    /// The respawn body. Callers must hold `lifecycle_gate`.
    async fn respawn_locked(&self) -> AppResult<()> {
        let server_name = self.config.name.clone();

        // This child is replaced ON PURPOSE: supersede its watchdog BEFORE the
        // kill, so a deliberate kill is never reported as a crash.
        self.supersede();

        // Kill the old child process if still around.
        {
            let mut guard = self.child.lock().await;
            if let Some(mut child) = guard.take() {
                child.kill().await.ok();
                let _ = child.wait().await;
            }
        }
        // Abort any dead writer/reader tasks and clear the stdin sender.
        {
            let mut wt = self.write_task.lock().await;
            if let Some(h) = wt.take() {
                h.abort();
            }
        }
        {
            let mut rt = self.read_task.lock().await;
            if let Some(h) = rt.take() {
                h.abort();
            }
        }
        *self.stdin_tx.lock().await = None;
        *self.connected.lock().await = false;
        // Clear cached tools so initialize() re-lists them after respawn.
        *self.tools.lock().await = Vec::new();
        // Any leftover pending requests are already failed by fail_all_pending.

        // Re-spawn and re-handshake.
        let _stdin_tx = self.spawn_process().await?;
        let config_env: HashMap<String, String> = self.config.env.clone();
        let result = self.initialize_handshake(&config_env).await?;
        *self.tools.lock().await = result.tools.clone();
        *self.connected.lock().await = true;

        supervisor::record_running(&server_name, self.child_pid().await, result.tools.len());
        tracing::info!(
            "MCP server '{}' respawned successfully ({} tools)",
            server_name,
            result.tools.len()
        );
        Ok(())
    }
}

#[async_trait]
impl McpServerClient for StdioMcpClient {
    fn tool_behaviors(&self) -> &crate::mcp::behavior::ToolBehaviorMap {
        &self.config.tool_behavior
    }

    async fn initialize(&self) -> AppResult<Vec<McpExternalTool>> {
        {
            let tools = self.tools.lock().await;
            if !tools.is_empty() {
                return Ok(tools.clone());
            }
        }

        let _stdin_tx = self.spawn_process().await?;

        // Build config_env from the server config's env map
        let config_env: HashMap<String, String> = self.config.env.clone();
        let result = match self.initialize_handshake(&config_env).await {
            Ok(r) => r,
            Err(e) => {
                // A handshake that never completes must be recorded TRUTHFULLY,
                // with the child's own stderr - never reduced to a generic
                // "did not initialize" (which hid the cause for hours).
                let reason = format!(
                    "{} (stderr: {})",
                    e,
                    stderr_tail_one_line(&self.config.name)
                );
                supervisor::record_start_failed(&self.config.name, &reason);
                return Err(err_str!(
                    "MCP server '{}' failed to initialize: {}",
                    self.config.name,
                    reason
                ));
            }
        };

        *self.tools.lock().await = result.tools.clone();
        *self.connected.lock().await = true;
        supervisor::record_running(
            &self.config.name,
            self.child_pid().await,
            result.tools.len(),
        );
        Ok(result.tools)
    }

    async fn call_tool(
        &self,
        name: &str,
        arguments: &Value,
        meta: Option<Value>,
    ) -> AppResult<McpToolResult> {
        if !self.circuit.is_allowed() {
            return Err(err_str!(
                "Circuit breaker is OPEN for external MCP server '{}'. \
                 Tool calls are temporarily blocked due to repeated failures. \
                 Try again later or check server status.",
                self.config.name
            ));
        }

        // Check if background writer + reader tasks are still alive
        {
            let wt = self.write_task.lock().await;
            let rt = self.read_task.lock().await;
            let writer_dead = wt.as_ref().map(|h| h.is_finished()).unwrap_or(true); // None = not initialized
            let reader_dead = rt.as_ref().map(|h| h.is_finished()).unwrap_or(true); // None = not initialized
            if writer_dead || reader_dead {
                // The connection is gone. Fail the call loudly AND respawn the
                // plugin process so the NEXT call can succeed - the agent sees
                // this error and retries (it picks the plugin back up later).
                // A dead plugin must restart, never stay blocked.
                drop(wt);
                drop(rt);
                match self.respawn().await {
                    Ok(_) => {
                        return Err(err_str!(
                            "MCP server '{}' connection lost (background {} stopped); \
                             plugin respawned - retry the call",
                            self.config.name,
                            if writer_dead && reader_dead {
                                "writer and reader tasks"
                            } else if writer_dead {
                                "writer task"
                            } else {
                                "reader task"
                            }
                        ));
                    }
                    Err(e) => {
                        return Err(err_str!(
                            "MCP server '{}' connection lost (background {} stopped); \
                             respawn failed: {}",
                            self.config.name,
                            if writer_dead && reader_dead {
                                "writer and reader tasks"
                            } else if writer_dead {
                                "writer task"
                            } else {
                                "reader task"
                            },
                            e
                        ));
                    }
                }
            }
        }

        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let req = build_call_tool_request(id, name, arguments, meta);

        // Explicit timeout only if configured (`None` = wait indefinitely -
        // the agent controls lifetime via background tasks, and a connection
        // failure fails ALL pending calls loudly, so nothing hangs silently).
        let timeout_dur = self.config.timeout_secs.map(Duration::from_secs);

        let (tx, rx) = oneshot::channel();
        {
            let mut pending = self.pending.lock();
            pending.insert(id, tx);
        }

        let stdin_clone = {
            let tx_guard = self.stdin_tx.lock().await;
            let sender = tx_guard
                .as_ref()
                .ok_or_else(|| err_str!("MCP server '{}' not initialized", self.config.name))?;
            sender
                .send(req)
                .map_err(|_| err_str!("MCP server '{}' stdin channel closed", self.config.name))?;
            sender.clone()
        };

        // If this future is dropped before the response arrives (thread ended,
        // /stop-thread, channel close, client-side timeout), the guard sends
        // `notifications/cancelled` to the server so it aborts the handler and
        // kills the underlying subprocess (docker KillOnDrop) - no stale
        // tool-spawned process survives its thread (thread 73, Aug 2026).
        let mut cancel_guard = InFlightCallGuard::new(id, Some(stdin_clone), self.pending.clone());

        let response = match timeout_dur {
            Some(dur) => match tokio::time::timeout(dur, rx).await {
                Ok(Ok(resp)) => resp,
                Ok(Err(_)) => {
                    // Response channel cancelled = the connection died while
                    // awaiting. fail_all_pending already errored every caller
                    // loudly; the next call will detect the dead reader/writer
                    // and respawn the plugin. No failure counting.
                    return Err(err_str!(
                        "MCP server '{}' response channel cancelled (connection died)",
                        self.config.name,
                    ));
                }
                Err(_elapsed) => {
                    // A single timeout is usually NOT a plugin failure (long
                    // build, slow network, busy server), so it is never
                    // counted toward the circuit breaker. But REPEATED
                    // consecutive timeouts with no success in between mean
                    // the plugin is wedged: alive yet unresponsive (Sep 2026
                    // filesystem MCP outage - one unbounded directory walk
                    // left the server busy-looping and EVERY call timed out
                    // for whole threads). Auto-restart on the 2nd
                    // consecutive timeout so the next call hits a fresh
                    // server, and surface the restart loudly in the error.
                    let n = self.consecutive_timeouts.fetch_add(1, Ordering::SeqCst) + 1;
                    if n == 2 {
                        tracing::error!(
                            "MCP server '{}' tool '{}' timed out {}x in a row - plugin unresponsive, restarting it",
                            self.config.name, name, n
                        );
                        if let Err(re) = self.respawn().await {
                            tracing::error!(
                                "MCP server '{}' auto-restart after repeated timeouts failed: {}",
                                self.config.name,
                                re
                            );
                        } else {
                            tracing::info!(
                                "MCP server '{}' auto-restarted after repeated timeouts (tool '{}')",
                                self.config.name, name
                            );
                        }
                        self.consecutive_timeouts.store(0, Ordering::SeqCst);
                    }
                    return Err(err_str!(
                        "MCP server '{}' tool '{}' timed out after {} seconds{}",
                        self.config.name,
                        name,
                        dur.as_secs(),
                        if n >= 2 {
                            " (plugin was unresponsive - auto-restarted; please retry)"
                        } else {
                            ""
                        }
                    ));
                }
            },
            None => match rx.await {
                Ok(resp) => resp,
                Err(_) => {
                    // Same as above: channel cancelled = connection died.
                    return Err(err_str!(
                        "MCP server '{}' response channel cancelled (connection died)",
                        self.config.name,
                    ));
                }
            },
        };
        cancel_guard.disarm();

        let result_value =
            match parse_response(&response).ctx("Failed to parse MCP tool call response")? {
                JsonRpcResponse::Success { result, .. } => result,
                JsonRpcResponse::Error { error, .. } => {
                    // A JSON-RPC error response means the plugin is ALIVE and
                    // answered - the tool itself failed (e.g. a compose
                    // command returned non-zero). This is a normal tool
                    // outcome the agent must see, not a transport failure.
                    // Do NOT count it toward the circuit breaker.
                    return Err(err_str!(
                        "MCP tool call error ({}): {}",
                        error.code,
                        error.message
                    ));
                }
            };

        let result: CallToolResult =
            serde_json::from_value(result_value).ctx("Failed to parse tools/call result")?;

        self.circuit.record_success();
        self.consecutive_timeouts.store(0, Ordering::SeqCst);
        let text = extract_tool_result_text(&result);
        Ok(McpToolResult {
            call_id: String::new(),
            content: text,
            is_error: result.is_error,
        })
    }

    async fn shutdown(&self) -> AppResult<()> {
        // Deliberate teardown: the watchdog must never report this as a crash.
        self.shutting_down.store(true, Ordering::SeqCst);
        self.supersede();

        // Close stdin by dropping the sender (the writer task will stop on rx closed)
        *self.stdin_tx.lock().await = None;

        // Abort the background writer + reader tasks
        {
            let mut wt = self.write_task.lock().await;
            if let Some(handle) = wt.take() {
                handle.abort();
            }
        }
        {
            let mut rt = self.read_task.lock().await;
            if let Some(handle) = rt.take() {
                handle.abort();
            }
        }

        // Kill the child process
        let mut guard = self.child.lock().await;
        if let Some(mut child) = guard.take() {
            child.kill().await.ok();
            child.wait().await.ok();
        }

        // Stop the stderr capture + watchdog tasks of this child.
        {
            let mut st = self.stderr_task.lock().await;
            if let Some(h) = st.take() {
                h.abort();
            }
        }
        {
            let mut wd = self.watchdog.lock().await;
            if let Some(h) = wd.take() {
                h.abort();
            }
        }

        // Cancel all pending requests
        {
            let mut pending = self.pending.lock();
            pending.clear();
        }

        *self.connected.lock().await = false;
        supervisor::record_stopped(&self.config.name, "shutdown requested");
        Ok(())
    }

    fn name(&self) -> &str {
        &self.config.name
    }

    fn health(&self) -> ServerHealth {
        ServerHealth {
            connected: *self.connected.blocking_lock(),
            tool_count: self.tools.blocking_lock().len(),
            circuit_state: self.circuit.state(),
            last_error: self.last_error.blocking_lock().clone(),
        }
    }

    async fn restart_after_crash(&self) -> Result<bool, String> {
        match self.supervise_restart(&self.config.name).await {
            RestartOutcome::Restarted => Ok(true),
            RestartOutcome::Stopped => Ok(false),
            RestartOutcome::GaveUp => Err(format!(
                "MCP server '{}' stayed down after {} supervised restart attempt(s)",
                self.config.name,
                supervisor::MAX_RESTART_ATTEMPTS
            )),
        }
    }

    fn timeout_secs(&self) -> Option<u64> {
        self.config.timeout_secs
    }
}

impl Drop for StdioMcpClient {
    fn drop(&mut self) {
        // The client is gone: no watchdog may restart its child afterwards.
        self.shutting_down.store(true, Ordering::SeqCst);
        self.supersede();

        // Best-effort: shut down the background writer + reader and kill the child.
        if let Ok(mut wt) = self.write_task.try_lock() {
            if let Some(handle) = wt.take() {
                handle.abort();
            }
        }
        if let Ok(mut rt) = self.read_task.try_lock() {
            if let Some(handle) = rt.take() {
                handle.abort();
            }
        }
        if let Ok(mut st) = self.stderr_task.try_lock() {
            if let Some(handle) = st.take() {
                handle.abort();
            }
        }
        if let Ok(mut wd) = self.watchdog.try_lock() {
            if let Some(handle) = wd.take() {
                handle.abort();
            }
        }
        if let Ok(mut guard) = self.child.try_lock() {
            if let Some(mut child) = guard.take() {
                // Kill it: a dropped client owns no child any more, and an MCP
                // child must never outlive the registry entry that spawned it.
                let _ = child.start_kill();
                let _ = child.try_wait();
            }
        }
    }
}

// ---------------------------------------------------------------------------
// HTTP MCP Client (async)
// ---------------------------------------------------------------------------

/// An MCP client that connects to an HTTP server.
/// Uses a simple request-response pattern via POST.
pub struct HttpMcpClient {
    config: McpServerConfig,
    client: reqwest::Client,
    next_id: AtomicU64,
    tools: Mutex<Vec<McpExternalTool>>,
    circuit: CircuitBreaker,
    connected: Mutex<bool>,
    last_error: Mutex<Option<String>>,
}

impl HttpMcpClient {
    pub fn new(config: McpServerConfig) -> Self {
        // No fixed HTTP client timeout: `None` means the request runs until
        // the server answers (agent controls lifetime via bg tasks). An
        // explicitly configured timeout still applies.
        let client = match config.timeout_secs {
            Some(secs) => reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(secs))
                .build()
                .unwrap_or_default(),
            None => reqwest::Client::builder().build().unwrap_or_default(),
        };

        Self {
            circuit: CircuitBreaker::new(config.max_retries),
            config,
            client,
            next_id: AtomicU64::new(1),
            tools: Mutex::new(Vec::new()),
            connected: Mutex::new(false),
            last_error: Mutex::new(None),
        }
    }

    fn base_url(&self) -> &str {
        self.config
            .url
            .as_deref()
            .unwrap_or("http://localhost:3000/mcp")
    }

    async fn post(&self, body: &str) -> AppResult<String> {
        let url = self.base_url();
        let response = self
            .client
            .post(url)
            .header("Content-Type", "application/json")
            .body(body.to_string())
            .send()
            .await
            .ctx(format!(
                "HTTP request to MCP server '{}' at {} failed",
                self.config.name, url
            ))?;

        let text = response.text().await.ctx(format!(
            "Failed to read HTTP response from MCP server '{}'",
            self.config.name
        ))?;

        Ok(text)
    }
}

#[async_trait]
impl McpServerClient for HttpMcpClient {
    fn tool_behaviors(&self) -> &crate::mcp::behavior::ToolBehaviorMap {
        &self.config.tool_behavior
    }

    async fn initialize(&self) -> AppResult<Vec<McpExternalTool>> {
        {
            let tools = self.tools.lock().await;
            if !tools.is_empty() {
                return Ok(tools.clone());
            }
        }

        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let req = build_initialize_request(id);
        let response = self.post(&req).await?;

        let result_value =
            match parse_response(response.trim()).ctx("Failed to parse MCP response")? {
                JsonRpcResponse::Success { result, .. } => result,
                JsonRpcResponse::Error { error, .. } => {
                    return Err(err_str!(
                        "MCP initialize error ({}): {}",
                        error.code,
                        error.message
                    ));
                }
            };

        if let Some(server_info) = result_value.get("serverInfo") {
            tracing::info!(
                "HTTP MCP server '{}' connected: {} v{}",
                self.config.name,
                server_info
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown"),
                server_info
                    .get("version")
                    .and_then(|v| v.as_str())
                    .unwrap_or("0"),
            );
        }

        // List tools
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let req = build_list_tools_request(id);
        let response = self.post(&req).await?;

        let list_value =
            match parse_response(response.trim()).ctx("Failed to parse MCP tools/list response")? {
                JsonRpcResponse::Success { result, .. } => result,
                JsonRpcResponse::Error { error, .. } => {
                    return Err(err_str!(
                        "MCP tools/list error ({}): {}",
                        error.code,
                        error.message
                    ));
                }
            };

        let tools: ListToolsResult =
            serde_json::from_value(list_value).ctx("Failed to parse tools/list result")?;

        tracing::info!(
            "HTTP MCP server '{}' exposes {} tool(s)",
            self.config.name,
            tools.tools.len()
        );

        *self.connected.lock().await = true;
        *self.tools.lock().await = tools.tools.clone();
        Ok(tools.tools)
    }

    async fn call_tool(
        &self,
        name: &str,
        arguments: &Value,
        meta: Option<Value>,
    ) -> AppResult<McpToolResult> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let req = build_call_tool_request(id, name, arguments, meta);
        let response = self.post(&req).await?;

        let result_value =
            match parse_response(response.trim()).ctx("Failed to parse MCP response")? {
                JsonRpcResponse::Success { result, .. } => result,
                JsonRpcResponse::Error { error, .. } => {
                    return Err(err_str!(
                        "MCP tool call error ({}): {}",
                        error.code,
                        error.message
                    ));
                }
            };

        let result: CallToolResult =
            serde_json::from_value(result_value).ctx("Failed to parse tools/call result")?;

        let text = extract_tool_result_text(&result);
        Ok(McpToolResult {
            call_id: String::new(),
            content: text,
            is_error: result.is_error,
        })
    }

    async fn shutdown(&self) -> AppResult<()> {
        *self.connected.lock().await = false;
        Ok(())
    }

    fn name(&self) -> &str {
        &self.config.name
    }

    fn health(&self) -> ServerHealth {
        ServerHealth {
            connected: *self.connected.blocking_lock(),
            tool_count: self.tools.blocking_lock().len(),
            circuit_state: self.circuit.state(),
            last_error: self.last_error.blocking_lock().clone(),
        }
    }

    fn timeout_secs(&self) -> Option<u64> {
        self.config.timeout_secs
    }
}

// ---------------------------------------------------------------------------
// Factory: create the right client type from config
// ---------------------------------------------------------------------------

/// Create an MCP client from a server configuration.
pub fn create_client(config: McpServerConfig) -> Box<dyn McpServerClient> {
    match config.transport {
        crate::mcp::external::config::McpTransport::Stdio => Box::new(StdioMcpClient::new(config)),
        crate::mcp::external::config::McpTransport::Http => Box::new(HttpMcpClient::new(config)),
    }
}

/// Initialize all external MCP servers and register their tools.
/// Returns a list of McpTool instances merged from all servers.
/// Each initialized client is registered in `clients` for runtime tool dispatch.
///
/// `pool` is used to resolve `$secret:NAME` references in each server's env
/// map (the sync config loader passes them through verbatim because it has no
/// DB access). Resolving here means the subprocess env AND the `configure`
/// message both carry real secret values - e.g. the git plugin receives the
/// actual GITHUB_APP_KEY instead of the literal "$secret:GITHUB_APP_KEY".
/// Pass `None` when no DB pool is available (resolution is skipped).
pub async fn initialize_external_tools(
    data_dir: &str,
    pool: Option<&sqlx::PgPool>,
    clients: &ExternalMcpClients,
) -> Vec<McpTool> {
    let mut configs = crate::mcp::external::config::load_servers_config(data_dir);
    // Resolve $env:/$secret: refs now that we have a DB pool (the sync loader
    // in config.rs passes $secret: through verbatim).
    if let Some(pool) = pool {
        for cfg in &mut configs {
            crate::plugins_yaml::resolve_config_refs(&mut cfg.env, pool).await;
        }
    }

    let mut all_tools = Vec::new();

    // Load enabled/disabled state from tools.yml
    let tool_entries =
        crate::plugins_yaml::load_raw(data_dir, &crate::plugins_yaml::PluginYamlType::Tool)
            .unwrap_or_default();

    for cfg in configs {
        let server_name = cfg.name.clone();

        // Check if this server is disabled in tools.yml
        if let Some(entry) = tool_entries.get(&server_name) {
            if !entry.enabled {
                tracing::info!(
                    "Skipping disabled MCP server '{}' (set enabled: true in tools.yml to enable)",
                    server_name
                );
                continue;
            }
        }

        let client: Arc<dyn McpServerClient> = match cfg.transport {
            crate::mcp::external::config::McpTransport::Stdio => {
                let c = Arc::new(StdioMcpClient::new(cfg));
                c.arm_supervision();
                c as Arc<dyn McpServerClient>
            }
            crate::mcp::external::config::McpTransport::Http => Arc::new(HttpMcpClient::new(cfg)),
        };
        let tools = client.to_mcp_tools().await;
        let count = tools.len();
        if count == 0 && supervisor::runtime_status(&server_name).is_none() {
            // LOUD: an enabled server that exposed nothing must say why. A
            // start failure that the client already recorded is kept (it
            // carries the child's real stderr).
            let reason =
                "the MCP handshake produced no tools (check the omniagent log for the child's own output)";
            supervisor::record_never_started(&server_name, reason);
            tracing::error!(
                "external MCP server '{}' is enabled but exposed 0 tool(s): {}",
                server_name,
                reason
            );
        }
        clients.register(&server_name, client);
        all_tools.extend(tools);

        tracing::info!(
            "Initialized {} external tool(s) from '{}'",
            count,
            server_name
        );
    }

    all_tools
}

/// Initialize a single external MCP server by name and return its tools.
/// Used for hot-reloading when a plugin is enabled via the dashboard.
/// Returns an error if the server config is not found or initialization fails.
/// Creates and registers an MCP client in `clients` for runtime tool dispatch.
///
/// `pool` resolves `$secret:NAME` references in the server's env map (see
/// `initialize_external_tools`). Pass `None` when no DB pool is available.
pub async fn initialize_single_server_tools(
    data_dir: &str,
    pool: Option<&sqlx::PgPool>,
    server_name: &str,
    clients: &ExternalMcpClients,
) -> Result<Vec<McpTool>, String> {
    // Load all configs to find this server
    let mut configs = crate::mcp::external::config::load_servers_config(data_dir);
    let mut cfg = configs
        .iter_mut()
        .find(|c| c.name == server_name)
        .cloned()
        .ok_or_else(|| format!("MCP server '{}' not found in config", server_name))?;

    // Resolve $env:/$secret: refs now that we have a DB pool (the sync loader
    // in config.rs passes $secret: through verbatim).
    if let Some(pool) = pool {
        crate::plugins_yaml::resolve_config_refs(&mut cfg.env, pool).await;
    }

    let client: Arc<dyn McpServerClient> = match cfg.transport {
        crate::mcp::external::config::McpTransport::Stdio => {
            let c = Arc::new(StdioMcpClient::new(cfg));
            c.arm_supervision();
            c as Arc<dyn McpServerClient>
        }
        crate::mcp::external::config::McpTransport::Http => Arc::new(HttpMcpClient::new(cfg)),
    };
    let tools = client.to_mcp_tools().await;

    if tools.is_empty() {
        // Say WHY truthfully: the child's own stderr when it died or failed the
        // handshake, never a generic "did not initialize".
        let why = match supervisor::runtime_status(server_name) {
            Some(s) => s.message,
            None => "the MCP handshake produced no tools (check the omniagent log for the child's own output)".to_string(),
        };
        return Err(format!(
            "MCP server '{}' initialized but returned no tools: {}",
            server_name, why
        ));
    }

    tracing::info!(
        "Hot-reloaded {} external tool(s) from '{}'",
        tools.len(),
        server_name
    );

    clients.register(server_name, client);
    Ok(tools)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_circuit_breaker_initial_state() {
        let cb = CircuitBreaker::new(3);
        assert!(cb.is_allowed());
        assert_eq!(cb.state(), CircuitState::Closed);
    }

    #[test]
    fn test_circuit_breaker_never_blocks_tool_calls() {
        // Aug 2026 design: a circuit breaker must NEVER make a plugin "stop
        // working". Even after many recorded failures (state = Open), tool
        // calls remain allowed - the agent sees errors and decides, and the
        // client respawns the plugin on genuine transport failure. The breaker
        // state is retained purely as a diagnostic.
        let cb = CircuitBreaker::new(3);
        cb.record_failure();
        assert!(cb.is_allowed());
        cb.record_failure();
        assert!(cb.is_allowed());
        cb.record_failure();
        assert!(cb.is_allowed());
        assert_eq!(cb.state(), CircuitState::Open);
    }

    #[test]
    fn test_circuit_breaker_resets_on_success() {
        let cb = CircuitBreaker::new(3);
        cb.record_failure();
        cb.record_failure();
        cb.record_success();
        assert!(cb.is_allowed());
        assert_eq!(cb.state(), CircuitState::Closed);
    }

    #[test]
    fn test_convert_input_schema() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "name": {"type": "string"}
            }
        });
        let converted = convert_input_schema(&schema);
        assert_eq!(converted["type"], "object");
    }

    #[test]
    fn test_convert_input_schema_missing_type() {
        let schema = serde_json::json!({
            "properties": {"x": {"type": "number"}}
        });
        let converted = convert_input_schema(&schema);
        assert_eq!(converted["type"], "object");
    }

    #[tokio::test]
    async fn inflight_guard_cancels_on_drop() {
        // Dropping the guard before completion must (a) remove the pending
        // entry and (b) emit a `notifications/cancelled` frame on the wire.
        let pending = Arc::new(parking_lot::Mutex::new(HashMap::new()));
        let (stdin_tx, mut stdin_rx) = mpsc::unbounded_channel::<String>();

        {
            let guard = InFlightCallGuard::new(42, Some(stdin_tx.clone()), pending.clone());
            assert_eq!(pending.lock().len(), 0);
            drop(guard);
        }

        let frame = stdin_rx.recv().await.expect("cancel frame emitted");
        assert!(
            frame.contains("notifications/cancelled"),
            "frame should be a cancel notification: {frame}"
        );
        assert!(
            frame.contains("\"requestId\":42"),
            "frame should name id 42: {frame}"
        );

        // Disarmed guard must NOT emit anything.
        {
            let mut guard = InFlightCallGuard::new(43, Some(stdin_tx.clone()), pending.clone());
            guard.disarm();
            drop(guard);
        }
        assert!(
            tokio::time::timeout(Duration::from_millis(100), stdin_rx.recv())
                .await
                .is_err(),
            "no frame should be emitted after disarm"
        );
    }

    #[tokio::test]
    async fn inflight_guard_drops_pending_entry() {
        let pending = Arc::new(parking_lot::Mutex::new(HashMap::new()));
        let (tx, _rx) = oneshot::channel::<String>();
        pending.lock().insert(7u64, tx);
        let (_stdin_tx, stdin_rx) = mpsc::unbounded_channel::<String>();
        drop(stdin_rx); // closed channel: send is a no-op

        let guard = InFlightCallGuard::new(7, None, pending.clone());
        drop(guard);

        assert!(
            pending.lock().get(&7).is_none(),
            "pending entry must be removed on drop"
        );
    }

    /// A minimal stdio MCP server (POSIX sh) used by the liveness regression
    /// test: it answers initialize / tools/list / tools/call, prints one line
    /// to stderr at startup (the stderr tail the crash report must carry) and
    /// can be killed at any moment.
    const FAKE_MCP_SERVER_SH: &str = r#"#!/bin/sh
# Fake MCP stdio server for the liveness-supervision regression test.
echo "fake-mcp starting (pid $$)" >&2
while IFS= read -r line; do
  id="${line#*\"id\":}"
  id="${id%%,*}"
  id="${id%%\}*}"
  id=$(printf '%s' "$id" | tr -cd '0-9')
  [ -n "$id" ] || id=0
  case "$line" in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":"2024-11-05","capabilities":{},"serverInfo":{"name":"fake-mcp","version":"1"}}}\n' "$id"
      ;;
    *'"method":"tools/list"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[{"name":"echo","description":"echo","inputSchema":{"type":"object","properties":{}}}]}}\n' "$id"
      ;;
    *'"notifications/'*)
      ;;
    *'"method":"tools/call"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"pong"}]}}\n' "$id"
      ;;
    *'"method":'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{}}\n' "$id"
      ;;
  esac
done
echo "fake-mcp exiting" >&2
"#;

    /// Regression (2026-09-27, production incident thread 3356): an external
    /// MCP child that DIES at runtime used to be invisible - no exit log, no
    /// status, no restart, and every thread got a bare `Unknown tool`. This
    /// test kills a real stdio MCP child mid-run and asserts the whole chain:
    /// the death is recorded with pid + exit status + stderr tail, the child is
    /// auto-restarted, and the tool is callable again afterwards.
    #[tokio::test]
    async fn dead_mcp_child_is_recorded_and_auto_restarted() {
        use crate::mcp::external::supervisor;

        let dir = tempfile::tempdir().expect("tempdir");
        let script = dir.path().join("fake_mcp.sh");
        std::fs::write(&script, FAKE_MCP_SERVER_SH).expect("write fake server");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&script).expect("stat").permissions();
            perms.set_mode(0o700);
            std::fs::set_permissions(&script, perms).expect("chmod");
        }

        let server = "liveness-fake-server";
        supervisor::clear(server);

        let cfg = McpServerConfig {
            name: server.to_string(),
            tool_behavior: Default::default(),
            transport: crate::mcp::external::config::McpTransport::Stdio,
            command: Some("/bin/sh".to_string()),
            args: vec![script.to_string_lossy().to_string()],
            url: None,
            env: HashMap::new(),
            current_dir: Some(dir.path().to_string_lossy().to_string()),
            timeout_secs: Some(20),
            max_retries: 3,
            allowed_tools: vec!["*".to_string()],
            pool_size: 1,
        };
        let client = Arc::new(StdioMcpClient::new(cfg));
        client.arm_supervision();

        let clients = ExternalMcpClients::new();
        clients.register(server, client.clone() as Arc<dyn McpServerClient>);

        let tools = client.to_mcp_tools().await;
        assert_eq!(tools.len(), 1, "the fake server exposes exactly one tool");
        assert!(tools[0].name.contains("echo"), "got: {}", tools[0].name);
        assert_eq!(
            supervisor::runtime_status(server).map(|s| s.state),
            Some(supervisor::LivenessState::Running),
            "a healthy handshake must be recorded as running"
        );

        // Production installs the restart hook in main.rs; this test installs its
        // own (name-filtered, so parallel tests cannot interfere).
        {
            let hook_client = client.clone();
            let hook_name = server.to_string();
            supervisor::set_restart_hook(Arc::new(move |n: String| {
                if n != hook_name {
                    return;
                }
                let hook_client = hook_client.clone();
                tokio::spawn(async move {
                    let _ = hook_client.restart_after_crash().await;
                });
            }));
        }

        // Kill the child mid-run, exactly like a crashing plugin process.
        let killed_pid = {
            let mut guard = client.child.lock().await;
            let child = guard.as_mut().expect("child spawned");
            let pid = child.id();
            child.start_kill().expect("kill signal sent");
            pid
        };

        // (a) The death is recorded with pid + exit status + stderr tail (the
        // same values the watchdog logs at ERROR through crash_message).
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        let exit = loop {
            if let Some(s) = supervisor::runtime_status(server) {
                if let Some(exit) = s.exit.clone() {
                    break exit;
                }
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the child died but no crash was recorded"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        };
        assert_eq!(exit.pid, killed_pid, "the crash must name the dead child");
        assert!(
            !exit.status.is_empty(),
            "the crash must carry an exit status"
        );
        #[cfg(unix)]
        assert!(
            exit.status.contains("signal"),
            "a SIGKILLed child must be reported as a signal, got: {}",
            exit.status
        );
        assert!(
            exit.stderr_tail
                .iter()
                .any(|l| l.contains("fake-mcp starting")),
            "the stderr tail must be captured, got: {:?}",
            exit.stderr_tail
        );
        let logged = supervisor::crash_message(server, exit.pid, &exit.status, &exit.stderr_tail);
        assert!(logged.contains(server), "got: {logged}");
        assert!(logged.contains(&exit.status), "got: {logged}");

        // (b) The supervisor restarts the child (bounded backoff, attempt 1 = 1s).
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(s) = supervisor::runtime_status(server) {
                if s.state == supervisor::LivenessState::Running && s.restarts >= 1 {
                    break;
                }
                if s.state == supervisor::LivenessState::Failed {
                    panic!("the supervisor gave up: {}", s.message);
                }
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the crashed MCP child was never restarted"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        // (c) The tool is callable again through the registry path the tool
        // handler uses, and the plugin still exposes its tool.
        let result = clients
            .call_tool(server, "echo", &serde_json::json!({}), None)
            .await
            .expect("the restarted child must answer a tool call");
        assert!(result.content.contains("pong"), "got: {}", result.content);
        assert!(!result.is_error, "the call after the restart must succeed");
        assert_eq!(
            client.to_mcp_tools().await.len(),
            1,
            "the restarted plugin must still expose its tool"
        );

        supervisor::clear(server);
    }
}
