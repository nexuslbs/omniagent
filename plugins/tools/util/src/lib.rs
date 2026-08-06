//! Shared MCP server framework: JSON-RPC stdio protocol, types, and helpers.
//!
//! Provides the runtime loop and type definitions needed by any stdio-based
//! MCP server.  Each server binary:
//!
//! 1. Defines its tools in `handle_tools_list()`
//! 2. Dispatches tool calls via `handle_tools_call()`
//! 3. Calls `run_server(server_info, handlers)` to start the loop
//!
//! # Meta context
//!
//! Every `tools/call` request can include a `_meta` field in the params. This
//! is injected by the MCP client (e.g. omniagent) and contains runtime context
//! like `channel_id`, `thread_id`, `profile_name`, `platform`. The handler
//! receives `_meta` as the first argument and tool-specific `arguments` as the
//! second. Tools that don't need `_meta` can ignore it.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::future::Future;
use std::io::Read;
use std::pin::Pin;
use std::sync::Arc;
use tokio::io::AsyncWriteExt;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// MCP protocol version (2025-03-26 is the current stable).
pub const MCP_PROTOCOL_VERSION: &str = "2025-03-26";

// ---------------------------------------------------------------------------
// JSON-RPC types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: String,
    #[serde(default)]
    pub id: Option<u64>,
    pub method: String,
    #[serde(default)]
    pub params: Option<Value>,
}

#[derive(Debug, Serialize)]
pub struct JsonRpcSuccess {
    pub jsonrpc: String,
    pub id: u64,
    pub result: Value,
}

#[derive(Debug, Serialize)]
pub struct JsonRpcErrorResponse {
    pub jsonrpc: String,
    pub id: u64,
    pub error: JsonRpcError,
}

#[derive(Debug, Serialize)]
pub struct JsonRpcError {
    pub code: i64,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

// ---------------------------------------------------------------------------
// MCP Initialize types
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct InitializeResult {
    #[serde(rename = "protocolVersion")]
    pub protocol_version: String,
    pub capabilities: ServerCapabilities,
    #[serde(rename = "serverInfo")]
    pub server_info: Implementation,
}

#[derive(Debug, Serialize)]
pub struct ServerCapabilities {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<ToolCapabilities>,
}

#[derive(Debug, Serialize)]
pub struct ToolCapabilities {
    #[serde(rename = "listChanged")]
    pub list_changed: bool,
}

#[derive(Debug, Serialize)]
pub struct Implementation {
    pub name: String,
    pub version: String,
}

// ---------------------------------------------------------------------------
// MCP tools/list types
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct ListToolsResult {
    pub tools: Vec<McpToolDef>,
}

#[derive(Debug, Clone, Serialize)]
pub struct McpToolDef {
    pub name: String,
    pub description: String,
    #[serde(rename = "inputSchema")]
    pub input_schema: Value,
}

// ---------------------------------------------------------------------------
// MCP tools/call types
// ---------------------------------------------------------------------------

/// Metadata context injected by the MCP client (omniagent) with each tool call.
/// Contains runtime information like channel, thread, profile.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct McpMeta {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel_id: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct CallToolParams {
    pub name: String,
    #[serde(default)]
    pub arguments: Option<Value>,
    /// Runtime context injected by the MCP client (underscore prefix = framework-managed).
    /// Not part of the tool's input schema.
    #[serde(default, rename = "_meta")]
    pub meta: Option<McpMeta>,
}

#[derive(Debug, Serialize)]
pub struct CallToolResult {
    pub content: Vec<ToolContent>,
    #[serde(default, rename = "isError")]
    pub is_error: bool,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type")]
pub enum ToolContent {
    #[serde(rename = "text")]
    Text { text: String },
}

// ---------------------------------------------------------------------------
// Server info
// ---------------------------------------------------------------------------

/// Server identity.
#[derive(Debug, Clone)]
pub struct ServerInfo {
    pub name: String,
    pub version: String,
}

/// Handler function type: receives tool arguments (+ meta context), returns result text + error flag.
pub type ToolHandler = Box<
    dyn Fn(Value, Option<McpMeta>) -> Pin<Box<dyn Future<Output = Result<(String, bool)>> + Send>>
        + Send
        + Sync,
>;

/// Wrap an ASYNC handler so any Err(e) becomes Ok((error_msg, true)).
///
/// Same contract as `soft_error` but for async handlers: an expected failure
/// (invalid input, sandbox rejection, command not found) must surface as a
/// tool error result — NOT as a handler Err — so it never trips the MCP
/// circuit breaker on the client side. Plugins that validate user input
/// (docker, git, filesystem, ...) should route their handlers through this.
pub fn soft_error_async<F, Fut>(handler: F) -> ToolHandler
where
    F: Fn(Value, Option<McpMeta>) -> Fut + Clone + Send + Sync + 'static,
    Fut: Future<Output = Result<(String, bool)>> + Send + 'static,
{
    Box::new(move |args: Value, meta: Option<McpMeta>| {
        let h = handler.clone();
        Box::pin(async move {
            match h(args, meta).await {
                Ok((text, is_error)) => Ok((text, is_error)),
                Err(e) => Ok((format!("{}", e), true)),
            }
        })
    })
}

/// A registered tool definition + handler.
pub struct McpToolEntry {
    pub def: McpToolDef,
    pub handler: ToolHandler,
}

// ---------------------------------------------------------------------------
// Server loop
// ---------------------------------------------------------------------------

/// Shared stdout writer: every `tools/call` request is handled in its own
/// spawned task, so a long-running tool (docker exec, git clone, ...) never
/// blocks subsequent calls to the plugin. Tasks share this mutex-protected
/// writer; the lock is held ONLY for the short JSON write — never while a tool
/// handler runs — so concurrent tool executions proceed in parallel.
type SharedWriter = Arc<tokio::sync::Mutex<tokio::io::BufWriter<tokio::io::Stdout>>>;

/// Serialize + write a single JSON-RPC response line under the shared writer lock.
async fn write_json(writer: &SharedWriter, value: &impl serde::Serialize) -> Result<()> {
    let json = serde_json::to_string(value)?;
    let mut w = writer.lock().await;
    w.write_all(json.as_bytes()).await?;
    w.write_all(b"\n").await?;
    w.flush().await?;
    Ok(())
}

/// Run the MCP stdio event loop.
///
/// `server_info`: identity reported in initialize response.
/// `tools`: list of (tool_def, handler) pairs.
pub async fn run_server(server_info: ServerInfo, tools: Vec<McpToolEntry>) -> Result<()> {
    run_server_inner(server_info, tools, None::<fn(Value)>).await
}

/// Run the MCP stdio event loop with an optional config handler.
///
/// When omniagent sends a `configure` message after initialization, the
/// `on_configure` callback is invoked with the plugin's config values as JSON.
/// The plugin can use this to receive its configuration directly from the plugin
/// config system, rather than reading env vars or YAML files.
pub async fn run_server_with_config<F>(
    server_info: ServerInfo,
    tools: Vec<McpToolEntry>,
    on_configure: Option<F>,
) -> Result<()>
where
    F: Fn(Value) + Send + Sync + 'static,
{
    run_server_inner(server_info, tools, on_configure).await
}

async fn run_server_inner<F>(
    server_info: ServerInfo,
    tools: Vec<McpToolEntry>,
    on_configure: Option<F>,
) -> Result<()>
where
    F: Fn(Value) + Send + Sync + 'static,
{
    // Initialize tracing: log to stderr
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".to_string().into()),
        )
        .with_writer(std::io::stderr)
        .try_init();

    tracing::info!("{} MCP server starting", server_info.name);

    let tools = Arc::new(tools);
    let index: Arc<HashMap<String, usize>> = Arc::new(
        tools
            .iter()
            .enumerate()
            .map(|(i, t)| (t.def.name.clone(), i))
            .collect(),
    );

    // ─────────────────────────────────────────────────────────────────────
    // stdin reader: DEDICATED OS THREAD feeding an unbounded channel.
    //
    // The reader MUST NEVER be starved by the tokio runtime. If the read
    // loop ran as a tokio task (as it did before Aug 2026), a burst of
    // concurrent `tools/call` handlers could occupy every worker thread,
    // the reader task would stop being polled, the OS pipe would backfill,
    // and the client's writes would block — the server appeared to "stop
    // reading after ~30 requests" with NO error anywhere (G17b, Aug 2026).
    //
    // A dedicated std::thread doing BLOCKING read_line is immune: it is
    // scheduled by the OS, not the tokio runtime, so it drains the pipe
    // unconditionally and forwards every line into the channel. The
    // dispatcher below consumes the channel at whatever rate the runtime
    // can sustain. Requests are never lost and never silently dropped.
    //
    // The channel is UNBOUNDED on purpose: backpressure belongs to the
    // machine, not the protocol. If the host genuinely cannot keep up
    // (OOM, fork failures), the OS will error loudly — a read error on the
    // thread exits with a FATAL log instead of stalling silently.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    {
        let tx = tx.clone();
        let server_name = server_info.name.clone();
        std::thread::Builder::new()
            .name(format!("{}-stdin-reader", server_name))
            .spawn(move || {
                // Explicit chunked framing. We deliberately do NOT depend on
                // read_line()'s internal buffering semantics: read(2) returns
                // whatever bytes are currently available, which may contain
                // zero, one, or many newline-delimited requests, or a partial
                // request with no trailing newline yet. We split on '\n'
                // ourselves and CARRY any partial remainder across reads, so
                // every request is delivered to the dispatcher as exactly one
                // independent String no matter how the kernel chunks the
                // stream. (Aug 2026, G17b: per-request framing must not be at
                // the mercy of a fragile read_line — each task/execution is
                // identified solely by its own delimited frame.)
                let mut stdin = std::io::stdin();
                let mut buf = [0u8; 64 * 1024];
                let mut pending: Vec<u8> = Vec::with_capacity(1024);
                'reader: loop {
                    match stdin.read(&mut buf) {
                        Ok(0) => {
                            // EOF. Deliver any final request that lacked a
                            // trailing newline, then exit.
                            if !pending.is_empty() {
                                let line = String::from_utf8_lossy(&pending);
                                let trimmed = line.trim().to_string();
                                if !trimmed.is_empty() {
                                    let _ = tx.send(trimmed);
                                }
                            }
                            break; // EOF: client closed stdin
                        }
                        Ok(n) => {
                            pending.extend_from_slice(&buf[..n]);
                            let mut consumed = 0;
                            while let Some(pos) =
                                pending[consumed..].iter().position(|&b| b == b'\n')
                            {
                                let line =
                                    String::from_utf8_lossy(&pending[consumed..consumed + pos]);
                                consumed += pos + 1;
                                let trimmed = line.trim().to_string();
                                if trimmed.is_empty() {
                                    continue;
                                }
                                if tx.send(trimmed).is_err() {
                                    break 'reader; // receiver dropped (shutting down)
                                }
                            }
                            // Keep any trailing partial request (no '\n' yet).
                            pending.drain(..consumed);
                        }
                        Err(e) => {
                            // NEVER fail silently: a machine-level read error
                            // must be loud so the operator sees it.
                            eprintln!(
                                "[{}] FATAL stdin read error: {} — reader thread exiting",
                                server_name, e
                            );
                            break;
                        }
                    }
                }
            })
            .map_err(|e| anyhow::anyhow!("failed to spawn stdin reader thread: {e}"))?;
    }
    drop(tx); // dispatcher holds the only remaining sender via rx ownership

    let stdout = tokio::io::stdout();
    let writer: SharedWriter = Arc::new(tokio::sync::Mutex::new(tokio::io::BufWriter::new(stdout)));

    let mut initialized = false;

    // Tracks every spawned tools/call task. JoinSet keeps the JoinHandles
    // alive so a panicking handler is OBSERVED (logged) instead of silently
    // swallowed by a discarded tokio::spawn handle.
    let mut in_flight: tokio::task::JoinSet<()> = tokio::task::JoinSet::new();

    // In-flight tools/call request ids → their cancel signal. When the client
    // sends `notifications/cancelled` (thread ended, /stop-thread, channel
    // close, client-side timeout), the matching task is signalled and DROPS
    // its handler future — plugins that wrap subprocesses in kill-on-drop
    // guards (docker compose) then kill the child, so no stale process
    // survives the thread that spawned it (thread 73, Aug 2026).
    let cancel_map: Arc<std::sync::Mutex<HashMap<u64, tokio::sync::oneshot::Sender<()>>>> =
        Arc::default();

    while let Some(line) = rx.recv().await {
        let request: JsonRpcRequest = match serde_json::from_str(&line) {
            Ok(req) => req,
            Err(e) => {
                tracing::error!("Failed to parse JSON-RPC: {e}");
                continue;
            }
        };

        let req_id = request.id;
        let method = request.method.as_str();

        match method {
            "initialize" => {
                if let Some(id) = req_id {
                    handle_initialize(&writer, id, &server_info).await?;
                    initialized = true;
                }
            }
            "notifications/initialized" => {
                tracing::info!("Client initialized notification received");
            }
            "notifications/cancelled" => {
                // Client aborted an in-flight request (thread-end, /stop-thread,
                // channel close, client-side timeout, ...). Signal the
                // tools/call task so its handler future is DROPPED mid-await;
                // plugins owning OS subprocesses behind KillOnDrop guards then
                // kill those children. Notifications have no request id — do
                // NOT respond.
                let request_id = request
                    .params
                    .as_ref()
                    .and_then(|p| p.get("requestId"))
                    .and_then(|v| v.as_u64());
                match request_id {
                    Some(request_id) => {
                        if let Some(tx) = cancel_map.lock().unwrap().remove(&request_id) {
                            tracing::info!(
                                "notifications/cancelled: aborting tools/call {request_id}"
                            );
                            let _ = tx.send(());
                        } else {
                            tracing::debug!(
                                "notifications/cancelled: unknown or finished request {request_id}"
                            );
                        }
                    }
                    None => tracing::warn!("notifications/cancelled without requestId"),
                }
            }
            "configure" => {
                if let Some(ref cb) = on_configure {
                    if let Some(params) = request.params {
                        cb(params);
                    }
                }
                // Acknowledge configure
                if let Some(id) = req_id {
                    let response = JsonRpcSuccess {
                        jsonrpc: "2.0".to_string(),
                        id,
                        result: serde_json::json!({"configured": true}),
                    };
                    write_json(&writer, &response).await?;
                }
            }
            "tools/list" => {
                if !initialized {
                    send_error(
                        &writer,
                        req_id.unwrap_or(0),
                        -32000,
                        "Server not initialized",
                    )
                    .await?;
                    continue;
                }
                if let Some(id) = req_id {
                    handle_tools_list(&writer, id, &tools).await?;
                }
            }
            "tools/call" => {
                if !initialized {
                    send_error(
                        &writer,
                        req_id.unwrap_or(0),
                        -32000,
                        "Server not initialized",
                    )
                    .await?;
                    continue;
                }
                if let Some(id) = req_id {
                    let params = request.params.unwrap_or_default();
                    let call_params: CallToolParams = match serde_json::from_value(params) {
                        Ok(cp) => cp,
                        Err(e) => {
                            send_error(
                                &writer,
                                id,
                                -32602,
                                format!("Invalid tools/call params: {e}"),
                            )
                            .await?;
                            continue;
                        }
                    };
                    // CONCURRENT: each tools/call runs in its own spawned task so
                    // a long-running tool (docker exec, git clone, ...) never
                    // blocks other calls to this plugin. The shared writer lock is
                    // held only for the short JSON response write — never while a
                    // handler executes — so N calls proceed in parallel.
                    //
                    // The task handle is tracked in `in_flight` (JoinSet) — NOT
                    // discarded — so a panicking handler is logged loudly rather
                    // than failing silently.
                    let writer = writer.clone();
                    let tools = tools.clone();
                    let index = index.clone();
                    let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel::<()>();
                    cancel_map.lock().unwrap().insert(id, cancel_tx);
                    let cancel_map = cancel_map.clone();
                    in_flight.spawn(async move {
                        // Run the handler under a cancel signal. If the client
                        // sends `notifications/cancelled` for this request id
                        // (thread ended, /stop-thread, channel close, client-side
                        // timeout), the handler future is DROPPED mid-await —
                        // plugins with kill-on-drop subprocess guards (docker)
                        // then kill the underlying OS process, so no stale
                        // `docker compose exec …` chain outlives its thread.
                        let call_fut =
                            handle_tools_call(&writer, id, &call_params, &tools, &index);
                        tokio::pin!(call_fut);
                        tokio::select! {
                            res = &mut call_fut => {
                                if let Err(e) = res {
                                    tracing::error!("tools/call '{}' error: {e:#}", call_params.name);
                                }
                            }
                            _ = cancel_rx => {
                                tracing::info!("tools/call {id} cancelled by client");
                                // Respond with the JSON-RPC "Request cancelled"
                                // error so the client's pending oneshot resolves
                                // instead of leaking.
                                let _ = send_error(&writer, id, -32800, "Request cancelled").await;
                            }
                        }
                        // Drop the cancel entry so a late duplicate cancel is a no-op.
                        cancel_map.lock().unwrap().remove(&id);
                    });
                    // Reap finished tasks to surface panics promptly (and keep
                    // the set from growing without bound on long-running servers).
                    while let Some(res) = in_flight.try_join_next() {
                        if let Err(e) = res {
                            tracing::error!("tools/call task panicked: {e}");
                        }
                    }
                }
            }
            _ => {
                tracing::warn!("Unknown method: {method}");
                if let Some(id) = req_id {
                    send_error(&writer, id, -32601, format!("Method not found: {method}")).await?;
                }
            }
        }
    }

    // stdin closed: drain remaining in-flight tasks so responses aren't
    // dropped mid-flight; log any panics. A small grace timeout bounds the
    // shutdown so a wedged handler can't hang the server forever. Tasks still
    // running past the grace period are aborted when `in_flight` is dropped at
    // the end of this function — dropping the handler futures is what triggers
    // the plugins' kill-on-drop guards, so tracked subprocesses die with the
    // server (detached ops like `up -d` intentionally survive).
    tracing::info!(
        "{} MCP server shutting down (stdin closed); {} task(s) in flight",
        server_info.name,
        in_flight.len()
    );
    let drain = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while let Some(res) = in_flight.join_next().await {
            if let Err(e) = res {
                tracing::error!("tools/call task panicked during drain: {e}");
            }
        }
    })
    .await;
    if drain.is_err() {
        tracing::warn!(
            "{} MCP server shutdown: timed out draining {} in-flight task(s)",
            server_info.name,
            in_flight.len()
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Handler implementations
// ---------------------------------------------------------------------------

async fn handle_initialize(
    writer: &SharedWriter,
    req_id: u64,
    server_info: &ServerInfo,
) -> Result<()> {
    let result = InitializeResult {
        protocol_version: MCP_PROTOCOL_VERSION.to_string(),
        capabilities: ServerCapabilities {
            tools: Some(ToolCapabilities {
                list_changed: false,
            }),
        },
        server_info: Implementation {
            name: server_info.name.clone(),
            version: server_info.version.clone(),
        },
    };

    let response = JsonRpcSuccess {
        jsonrpc: "2.0".to_string(),
        id: req_id,
        result: serde_json::to_value(result)?,
    };

    write_json(writer, &response).await?;

    tracing::info!("Initialized: {} v{}", server_info.name, server_info.version);
    Ok(())
}

async fn handle_tools_list(
    writer: &SharedWriter,
    req_id: u64,
    tools: &Arc<Vec<McpToolEntry>>,
) -> Result<()> {
    let defs: Vec<McpToolDef> = tools.iter().map(|t| t.def.clone()).collect();
    let result = ListToolsResult { tools: defs };

    let response = JsonRpcSuccess {
        jsonrpc: "2.0".to_string(),
        id: req_id,
        result: serde_json::to_value(result)?,
    };

    write_json(writer, &response).await?;

    tracing::info!("tools/list returned {} tool(s)", tools.len());
    Ok(())
}

async fn handle_tools_call(
    writer: &SharedWriter,
    req_id: u64,
    params: &CallToolParams,
    tools: &Arc<Vec<McpToolEntry>>,
    index: &HashMap<String, usize>,
) -> Result<()> {
    let entry_idx = match index.get(&params.name) {
        Some(i) => *i,
        None => {
            send_error(
                writer,
                req_id,
                -32602,
                format!("Unknown tool: {}", params.name),
            )
            .await?;
            return Ok(());
        }
    };
    let entry = &tools[entry_idx];

    let args = params.arguments.clone().unwrap_or(serde_json::Value::Null);
    let meta = params.meta.clone();

    let (text, is_error) = match (entry.handler)(args, meta).await {
        Ok(result) => result,
        Err(e) => {
            send_error(writer, req_id, -32603, format!("Handler error: {e}")).await?;
            return Ok(());
        }
    };

    let result = CallToolResult {
        content: vec![ToolContent::Text { text }],
        is_error,
    };

    let response = JsonRpcSuccess {
        jsonrpc: "2.0".to_string(),
        id: req_id,
        result: serde_json::to_value(result)?,
    };

    write_json(writer, &response).await?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

async fn send_error(
    writer: &SharedWriter,
    req_id: u64,
    code: i64,
    message: impl Into<String>,
) -> Result<()> {
    let response = JsonRpcErrorResponse {
        jsonrpc: "2.0".to_string(),
        id: req_id,
        error: JsonRpcError {
            code,
            message: message.into(),
            data: None,
        },
    };

    write_json(writer, &response).await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// HashVectorizer — deterministic local text embedding for semantic search
// ---------------------------------------------------------------------------

use std::hash::{DefaultHasher, Hash, Hasher};

/// Lightweight local vectorizer using character trigram feature hashing.
/// Algorithm: split text into overlapping 3-character windows, hash each
/// trigram to a bucket (0..1535) using DefaultHasher, increment the bucket
/// value, then normalize to unit length. Deterministic, zero dependencies.
pub struct HashVectorizer;

impl HashVectorizer {
    pub async fn generate_embedding(&self, text: &str) -> Vec<f32> {
        let dim = 1536;
        let mut vec = vec![0.0f32; dim];

        let chars: Vec<char> = text.chars().collect();
        if chars.len() < 3 {
            return vec;
        }

        for window in chars.windows(3) {
            let trigram: String = window.iter().collect();
            let mut hasher = DefaultHasher::new();
            trigram.hash(&mut hasher);
            let hash = hasher.finish();
            let bucket = (hash as usize) % dim;
            vec[bucket] += 1.0;
        }

        // Normalize to unit length
        let magnitude: f32 = vec.iter().map(|v| v * v).sum::<f32>().sqrt();
        if magnitude > 0.0 {
            for val in vec.iter_mut() {
                *val /= magnitude;
            }
        }

        vec
    }
}

/// Convert a Vec<f32> to Postgres-compatible text representation.
/// Used for building embedding query vectors in SQL.
pub fn vector_to_string(vec: &[f32]) -> String {
    let parts: Vec<String> = vec.iter().map(|v| v.to_string()).collect();
    format!("[{}]", parts.join(","))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn soft_error_async_converts_err_to_tool_error() {
        // An async handler that fails must come back as Ok((msg, true)) —
        // never as Err — so the MCP circuit breaker stays closed.
        let failing = soft_error_async(|_args: Value, _meta: Option<McpMeta>| async move {
            anyhow::bail!("expected failure: bad verb")
        });
        let (msg, is_error) = failing(serde_json::json!({}), None)
            .await
            .expect("soft_error_async always returns Ok");
        assert!(is_error);
        assert!(msg.contains("expected failure"));
    }

    #[tokio::test]
    async fn soft_error_async_passes_through_success() {
        let ok_handler = soft_error_async(|_args: Value, _meta: Option<McpMeta>| async move {
            Ok(("all good".to_string(), false))
        });
        let (msg, is_error) = ok_handler(serde_json::json!({}), None)
            .await
            .expect("soft_error_async always returns Ok");
        assert!(!is_error);
        assert_eq!(msg, "all good");
    }

    #[tokio::test]
    async fn soft_error_async_passes_through_existing_tool_error() {
        // Handlers that ALREADY return (msg, true) keep their shape.
        let handler = soft_error_async(|_args: Value, _meta: Option<McpMeta>| async move {
            Ok(("already an error".to_string(), true))
        });
        let (msg, is_error) = handler(serde_json::json!({}), None)
            .await
            .expect("soft_error_async always returns Ok");
        assert!(is_error);
        assert_eq!(msg, "already an error");
    }
}
