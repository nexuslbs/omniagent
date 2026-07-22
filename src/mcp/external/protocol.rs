//! MCP JSON-RPC protocol types.
//!
//! Based on the Model Context Protocol specification:
//! https://spec.modelcontextprotocol.io/

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;

/// Supported MCP protocol version.
pub const MCP_PROTOCOL_VERSION: &str = "2025-03-26";

// ---------------------------------------------------------------------------
// JSON-RPC base types
// ---------------------------------------------------------------------------

/// A JSON-RPC request (client → server).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<u64>,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

/// A JSON-RPC notification (no id: no response expected).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcNotification {
    pub jsonrpc: String,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

/// A JSON-RPC response (server → client).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum JsonRpcResponse {
    Success {
        jsonrpc: String,
        id: u64,
        result: Value,
    },
    Error {
        jsonrpc: String,
        id: u64,
        error: JsonRpcError,
    },
}

/// JSON-RPC error object.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcError {
    pub code: i64,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

// ---------------------------------------------------------------------------
// MCP Initialize
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InitializeParams {
    #[serde(rename = "protocolVersion")]
    pub protocol_version: String,
    pub capabilities: ClientCapabilities,
    #[serde(rename = "clientInfo")]
    pub client_info: Implementation,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientCapabilities {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<serde_json::Map<String, Value>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Implementation {
    pub name: String,
    pub version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(dead_code)]
pub struct InitializeResult {
    #[serde(rename = "protocolVersion")]
    pub protocol_version: String,
    pub capabilities: ServerCapabilities,
    #[serde(rename = "serverInfo")]
    pub server_info: Implementation,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerCapabilities {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<ToolCapabilities>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCapabilities {
    #[serde(default, skip_serializing_if = "is_false")]
    pub list_changed: bool,
}

fn is_false(b: &bool) -> bool {
    !b
}

// ---------------------------------------------------------------------------
// MCP Tools
// ---------------------------------------------------------------------------

/// External tool definition from MCP tools/list.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpExternalTool {
    pub name: String,
    pub description: String,
    #[serde(rename = "inputSchema")]
    pub input_schema: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListToolsResult {
    pub tools: Vec<McpExternalTool>,
}

// ---------------------------------------------------------------------------
// MCP Tool Call
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CallToolParams {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub arguments: Option<Value>,
    /// Runtime context injected by the framework (_meta = underscore prefix = framework-managed).
    /// Contains channel_id, thread_id, profile_name, platform for tools that need it.
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "_meta")]
    pub meta: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CallToolResult {
    pub content: Vec<ToolContent>,
    #[serde(default, skip_serializing_if = "is_false", rename = "isError")]
    pub is_error: bool,
}

/// Tool result content item (MCP supports multiple types).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ToolContent {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "resource")]
    Resource { resource: ResourceContent },
}

/// A resource embedded in tool output.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceContent {
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uri: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
}

impl ToolContent {
    /// Extract the text representation from any content type.
    pub fn text(&self) -> &str {
        match self {
            ToolContent::Text { text } => text.as_str(),
            ToolContent::Resource { resource } => resource.text.as_str(),
        }
    }
}

// ---------------------------------------------------------------------------
// Helper: build MCP request/response strings
// ---------------------------------------------------------------------------

/// Build an initialize request JSON string.
pub fn build_initialize_request(id: u64) -> String {
    let req = JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: Some(id),
        method: "initialize".to_string(),
        params: Some(
            serde_json::to_value(InitializeParams {
                protocol_version: MCP_PROTOCOL_VERSION.to_string(),
                capabilities: ClientCapabilities {
                    tools: Some(serde_json::Map::new()),
                },
                client_info: Implementation {
                    name: "omniagent".to_string(),
                    version: "0.1.0".to_string(),
                },
            })
            .unwrap_or_default(),
        ),
    };
    serde_json::to_string(&req).unwrap_or_default()
}

/// Build an initialized notification JSON string.
pub fn build_initialized_notification() -> String {
    let notif = JsonRpcNotification {
        jsonrpc: "2.0".to_string(),
        method: "notifications/initialized".to_string(),
        params: None,
    };
    serde_json::to_string(&notif).unwrap_or_default()
}

/// Build a tools/list request JSON string.
pub fn build_list_tools_request(id: u64) -> String {
    let req = JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: Some(id),
        method: "tools/list".to_string(),
        params: None,
    };
    serde_json::to_string(&req).unwrap_or_default()
}

/// Build a configure request with plugin config values.
/// Always uses id=0 (notification-style) — no response expected.
pub fn build_configure_request(config: &HashMap<String, String>) -> String {
    let config_obj: serde_json::Map<String, serde_json::Value> = config
        .iter()
        .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
        .collect();
    let req = JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: Some(0),
        method: "configure".to_string(),
        params: Some(serde_json::Value::Object(config_obj)),
    };
    serde_json::to_string(&req).unwrap_or_default()
}

/// Build a tools/call request. Accepts optional _meta context injected by the framework.
pub fn build_call_tool_request(id: u64, name: &str, arguments: &Value, meta: Option<Value>) -> String {
    let req = JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: Some(id),
        method: "tools/call".to_string(),
        params: Some(
            serde_json::to_value(CallToolParams {
                name: name.to_string(),
                arguments: Some(arguments.clone()),
                meta,
            })
            .unwrap_or_default(),
        ),
    };
    serde_json::to_string(&req).unwrap_or_default()
}

/// Parse a JSON-RPC response from a string.
pub fn parse_response(line: &str) -> anyhow::Result<JsonRpcResponse> {
    serde_json::from_str(line).map_err(|e| anyhow::anyhow!("Failed to parse MCP response: {}", e))
}

/// Extract text from a call_tool result.
pub fn extract_tool_result_text(result: &CallToolResult) -> String {
    result
        .content
        .iter()
        .map(|c| c.text())
        .collect::<Vec<_>>()
        .join("\n")
}
