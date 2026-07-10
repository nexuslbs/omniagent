//! Shared MCP server framework — JSON-RPC stdio protocol, types, and helpers.
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
use std::pin::Pin;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

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

/// Handler function type — receives tool arguments (+ meta context), returns result text + error flag.
pub type ToolHandler = Box<
    dyn Fn(Value, Option<McpMeta>) -> Pin<Box<dyn Future<Output = Result<(String, bool)>> + Send>> + Send + Sync,
>;

/// A registered tool definition + handler.
pub struct McpToolEntry {
    pub def: McpToolDef,
    pub handler: ToolHandler,
}

// ---------------------------------------------------------------------------
// Server loop
// ---------------------------------------------------------------------------

/// Run the MCP stdio event loop.
///
/// `server_info`: identity reported in initialize response.
/// `tools`: list of (tool_def, handler) pairs.
pub async fn run_server(server_info: ServerInfo, tools: Vec<McpToolEntry>) -> Result<()> {
    // Initialize tracing — log to stderr
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| format!("info").into()),
        )
        .with_writer(std::io::stderr)
        .try_init();

    tracing::info!("{} MCP server starting", server_info.name);

    let index: HashMap<String, &McpToolEntry> =
        tools.iter().map(|t| (t.def.name.clone(), t)).collect();

    let stdin = tokio::io::stdin();
    let reader = BufReader::new(stdin);
    let mut lines = reader.lines();

    let stdout = tokio::io::stdout();
    let mut writer = tokio::io::BufWriter::new(stdout);

    let mut initialized = false;

    while let Some(line) = lines.next_line().await? {
        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }

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
                    handle_initialize(&mut writer, id, &server_info).await?;
                    initialized = true;
                }
            }
            "notifications/initialized" => {
                tracing::info!("Client initialized notification received");
            }
            "tools/list" => {
                if !initialized {
                    send_error(
                        &mut writer,
                        req_id.unwrap_or(0),
                        -32000,
                        "Server not initialized",
                    )
                    .await?;
                    continue;
                }
                if let Some(id) = req_id {
                    handle_tools_list(&mut writer, id, &tools).await?;
                }
            }
            "tools/call" => {
                if !initialized {
                    send_error(
                        &mut writer,
                        req_id.unwrap_or(0),
                        -32000,
                        "Server not initialized",
                    )
                    .await?;
                    continue;
                }
                if let Some(id) = req_id {
                    let params = request.params.unwrap_or_default();
                    let call_params: CallToolParams = serde_json::from_value(params)
                        .map_err(|e| anyhow::anyhow!("Invalid tools/call params: {e}"))?;
                    handle_tools_call(&mut writer, id, &call_params, &index).await?;
                }
            }
            _ => {
                tracing::warn!("Unknown method: {method}");
                if let Some(id) = req_id {
                    send_error(
                        &mut writer,
                        id,
                        -32601,
                        format!("Method not found: {method}"),
                    )
                    .await?;
                }
            }
        }
    }

    tracing::info!(
        "{} MCP server shutting down (stdin closed)",
        server_info.name
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Handler implementations
// ---------------------------------------------------------------------------

async fn handle_initialize<W: AsyncWriteExt + Unpin>(
    writer: &mut tokio::io::BufWriter<W>,
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

    let json = serde_json::to_string(&response)?;
    writer.write_all(json.as_bytes()).await?;
    writer.write_all(b"\n").await?;
    writer.flush().await?;

    tracing::info!("Initialized: {} v{}", server_info.name, server_info.version);
    Ok(())
}

async fn handle_tools_list<W: AsyncWriteExt + Unpin>(
    writer: &mut tokio::io::BufWriter<W>,
    req_id: u64,
    tools: &[McpToolEntry],
) -> Result<()> {
    let defs: Vec<McpToolDef> = tools.iter().map(|t| t.def.clone()).collect();
    let result = ListToolsResult { tools: defs };

    let response = JsonRpcSuccess {
        jsonrpc: "2.0".to_string(),
        id: req_id,
        result: serde_json::to_value(result)?,
    };

    let json = serde_json::to_string(&response)?;
    writer.write_all(json.as_bytes()).await?;
    writer.write_all(b"\n").await?;
    writer.flush().await?;

    tracing::info!("tools/list returned {} tool(s)", tools.len());
    Ok(())
}

async fn handle_tools_call<W: AsyncWriteExt + Unpin>(
    writer: &mut tokio::io::BufWriter<W>,
    req_id: u64,
    params: &CallToolParams,
    index: &HashMap<String, &McpToolEntry>,
) -> Result<()> {
    tracing::info!("tools/call: name='{}'", params.name);

    let entry = match index.get(&params.name) {
        Some(e) => e,
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

    let json = serde_json::to_string(&response)?;
    writer.write_all(json.as_bytes()).await?;
    writer.write_all(b"\n").await?;
    writer.flush().await?;

    tracing::info!(
        "tools/call '{}' completed (is_error={})",
        params.name,
        is_error
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

async fn send_error<W: AsyncWriteExt + Unpin>(
    writer: &mut tokio::io::BufWriter<W>,
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

    let json = serde_json::to_string(&response)?;
    writer.write_all(json.as_bytes()).await?;
    writer.write_all(b"\n").await?;
    writer.flush().await?;
    Ok(())
}
