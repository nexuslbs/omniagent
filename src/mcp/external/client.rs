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
use crate::mcp::{McpTool, McpToolResult};
use async_trait::async_trait;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
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
                handler: Arc::new(move |args: Value, ctx: crate::mcp::AppContext| {
                    let sn = sn.clone();
                    let tn = tn.clone();
                    Box::pin(async move {
                        // Build _meta context from AppContext (channel_id always, optional thread/profile/platform)
                        let mut meta_map = serde_json::Map::new();
                        if let Some(cid) = ctx.current_channel_id {
                            meta_map.insert("channel_id".to_string(), serde_json::json!(cid));
                        }
                        if let Some(tid) = ctx.current_thread_id {
                            meta_map.insert("thread_id".to_string(), serde_json::json!(tid));
                        }
                        if let Some(ref pn) = ctx.current_profile_name {
                            meta_map.insert("profile_name".to_string(), serde_json::json!(pn));
                        }
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
    pub fn register(&self, name: &str, client: Arc<dyn McpServerClient>) {
        let mut registry = self.clients.write();
        registry.insert(name.to_string(), client);
    }

    /// Remove an MCP client (e.g. on disable).
    pub fn remove(&self, name: &str) {
        let mut registry = self.clients.write();
        registry.remove(name);
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
        client.call_tool(tool_name, args, meta).await
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
    connected: Mutex<bool>,
    last_error: Mutex<Option<String>>,
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
            .stderr(std::process::Stdio::inherit());

        if let Some(dir) = &self.config.current_dir {
            command.current_dir(dir);
        }
        for (key, value) in &self.config.env {
            command.env(key, value);
        }

        let mut child = command
            .spawn()
            .ctx(format!("Failed to spawn MCP server '{}'", self.config.name))?;

        let child_stdin = child.stdin.take().ok_or_else(|| {
            err_str!("Failed to open stdin for MCP server '{}'", self.config.name)
        })?;
        let child_stdout = child.stdout.take().ok_or_else(|| {
            err_str!(
                "Failed to open stdout for MCP server '{}'",
                self.config.name
            )
        })?;

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

        Ok(stdin_tx)
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
    async fn respawn(&self) -> AppResult<()> {
        let server_name = self.config.name.clone();

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
        let result = self.initialize_handshake(&config_env).await?;

        *self.tools.lock().await = result.tools.clone();
        *self.connected.lock().await = true;
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
                    Ok(()) => {
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
                    // A timeout is NOT a plugin failure - it can be caused by
                    // many legitimate reasons (long build, slow network, busy
                    // server). Never count it toward the circuit breaker; the
                    // agent gets the timeout error and can retry or wait
                    // longer. (Aug 2026: fixed tool timeouts were removed; a
                    // timeout here only exists when explicitly configured.)
                    return Err(err_str!(
                        "MCP server '{}' tool '{}' timed out after {} seconds",
                        self.config.name,
                        name,
                        dur.as_secs(),
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
        let text = extract_tool_result_text(&result);
        Ok(McpToolResult {
            call_id: String::new(),
            content: text,
            is_error: result.is_error,
        })
    }

    async fn shutdown(&self) -> AppResult<()> {
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

        // Cancel all pending requests
        {
            let mut pending = self.pending.lock();
            pending.clear();
        }

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

impl Drop for StdioMcpClient {
    fn drop(&mut self) {
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
        if let Ok(mut guard) = self.child.try_lock() {
            if let Some(mut child) = guard.take() {
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
            crate::mcp::external::config::McpTransport::Stdio => Arc::new(StdioMcpClient::new(cfg)),
            crate::mcp::external::config::McpTransport::Http => Arc::new(HttpMcpClient::new(cfg)),
        };
        let tools = client.to_mcp_tools().await;
        let count = tools.len();
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
        crate::mcp::external::config::McpTransport::Stdio => Arc::new(StdioMcpClient::new(cfg)),
        crate::mcp::external::config::McpTransport::Http => Arc::new(HttpMcpClient::new(cfg)),
    };
    let tools = client.to_mcp_tools().await;

    if tools.is_empty() {
        return Err(format!(
            "MCP server '{}' initialized but returned no tools",
            server_name
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
}
