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

/// Build an MCP `notifications/cancelled` notification JSON string.
///
/// Sent by the client when an in-flight request is abandoned (thread ended,
/// /stop-thread, channel close, client-side timeout). The shared server
/// framework (mcp-server-util) aborts the matching `tools/call` handler;
/// plugins that wrap subprocesses in kill-on-drop guards (docker compose) then
/// kill the underlying OS process, so no stale tool-spawned subprocess
/// survives the thread that issued it (thread 73, Aug 2026).
pub fn build_cancel_notification(id: u64) -> String {
    let notif = JsonRpcNotification {
        jsonrpc: "2.0".to_string(),
        method: "notifications/cancelled".to_string(),
        params: Some(serde_json::json!({ "requestId": id })),
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
pub fn build_call_tool_request(
    id: u64,
    name: &str,
    arguments: &Value,
    meta: Option<Value>,
) -> String {
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ── build_initialize_request ────────────────────────────────────────────

    #[test]
    fn test_build_initialize_request_basic() {
        let s = build_initialize_request(1);
        let v: Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["jsonrpc"], "2.0");
        assert_eq!(v["id"], 1);
        assert_eq!(v["method"], "initialize");
        assert!(v["params"].is_object());
        assert_eq!(v["params"]["protocolVersion"], "2025-03-26");
        assert_eq!(v["params"]["clientInfo"]["name"], "omniagent");
        assert!(v["params"]["capabilities"]["tools"].is_object());
    }

    #[test]
    fn test_build_initialize_request_different_id() {
        let s = build_initialize_request(42);
        let v: Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["id"], 42);
    }

    // ── build_initialized_notification ──────────────────────────────────────

    #[test]
    fn test_build_initialized_notification() {
        let s = build_initialized_notification();
        let v: Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["jsonrpc"], "2.0");
        assert_eq!(v["method"], "notifications/initialized");
        assert!(v.get("id").is_none());
        assert!(v.get("params").is_none() || v["params"].is_null());
    }

    // ── build_cancel_notification ───────────────────────────────────────────

    #[test]
    fn test_build_cancel_notification() {
        let s = build_cancel_notification(42);
        let v: Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["jsonrpc"], "2.0");
        assert_eq!(v["method"], "notifications/cancelled");
        assert!(v.get("id").is_none(), "notifications must have no id");
        assert_eq!(v["params"]["requestId"], 42);
    }

    // ── build_list_tools_request ────────────────────────────────────────────

    #[test]
    fn test_build_list_tools_request() {
        let s = build_list_tools_request(2);
        let v: Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["jsonrpc"], "2.0");
        assert_eq!(v["id"], 2);
        assert_eq!(v["method"], "tools/list");
        // params is optional; check it's absent or null
        assert!(v.get("params").is_none() || v["params"].is_null());
    }

    // ── build_configure_request ─────────────────────────────────────────────

    #[test]
    fn test_build_configure_request_empty() {
        let cfg = HashMap::new();
        let s = build_configure_request(&cfg);
        let v: Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["jsonrpc"], "2.0");
        assert_eq!(v["id"], 0);
        assert_eq!(v["method"], "configure");
        assert!(v["params"].is_object());
        assert!(v["params"].as_object().unwrap().is_empty());
    }

    #[test]
    fn test_build_configure_request_with_values() {
        let mut cfg = HashMap::new();
        cfg.insert("api_key".to_string(), "sk-test".to_string());
        cfg.insert("model".to_string(), "gpt-4".to_string());
        let s = build_configure_request(&cfg);
        let v: Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["jsonrpc"], "2.0");
        assert_eq!(v["method"], "configure");
        assert_eq!(v["params"]["api_key"], "sk-test");
        assert_eq!(v["params"]["model"], "gpt-4");
    }

    // ── build_call_tool_request ─────────────────────────────────────────────

    #[test]
    fn test_build_call_tool_request_no_meta() {
        let s = build_call_tool_request(3, "test_tool", &json!({"arg": "val"}), None);
        let v: Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["jsonrpc"], "2.0");
        assert_eq!(v["id"], 3);
        assert_eq!(v["method"], "tools/call");
        assert_eq!(v["params"]["name"], "test_tool");
        assert_eq!(v["params"]["arguments"]["arg"], "val");
        // _meta should not be present when None
        assert!(v["params"].get("_meta").is_none());
    }

    #[test]
    fn test_build_call_tool_request_with_meta() {
        let meta = json!({"channel_id": "123", "profile": "test"});
        let s = build_call_tool_request(5, "weather", &json!({"city": "NYC"}), Some(meta));
        let v: Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["params"]["name"], "weather");
        assert_eq!(v["params"]["arguments"]["city"], "NYC");
        assert_eq!(v["params"]["_meta"]["channel_id"], "123");
        assert_eq!(v["params"]["_meta"]["profile"], "test");
    }

    #[test]
    fn test_build_call_tool_request_empty_args() {
        let s = build_call_tool_request(7, "ping", &json!({}), None);
        let v: Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["params"]["name"], "ping");
        assert!(v["params"]["arguments"].as_object().unwrap().is_empty());
    }

    // ── parse_response ──────────────────────────────────────────────────────

    #[test]
    fn test_parse_response_success() {
        let raw = r#"{"jsonrpc":"2.0","id":1,"result":{"ok": true}}"#;
        let resp = parse_response(raw).unwrap();
        match resp {
            JsonRpcResponse::Success {
                jsonrpc,
                id,
                result,
            } => {
                assert_eq!(jsonrpc, "2.0");
                assert_eq!(id, 1);
                assert_eq!(result, json!({"ok": true}));
            }
            _ => panic!("Expected Success variant"),
        }
    }

    #[test]
    fn test_parse_response_error() {
        let raw =
            r#"{"jsonrpc":"2.0","id":2,"error":{"code":-32601,"message":"Method not found"}}"#;
        let resp = parse_response(raw).unwrap();
        match resp {
            JsonRpcResponse::Error { jsonrpc, id, error } => {
                assert_eq!(jsonrpc, "2.0");
                assert_eq!(id, 2);
                assert_eq!(error.code, -32601);
                assert_eq!(error.message, "Method not found");
                assert!(error.data.is_none());
            }
            _ => panic!("Expected Error variant"),
        }
    }

    #[test]
    fn test_parse_response_empty_string() {
        let result = parse_response("");
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_response_malformed_json() {
        let result = parse_response("not json at all");
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_response_partial_json() {
        let result = parse_response(r#"{"jsonrpc":"2.0""#);
        assert!(result.is_err());
    }

    // ── extract_tool_result_text ────────────────────────────────────────────

    #[test]
    fn test_extract_tool_result_text_single() {
        let result = CallToolResult {
            content: vec![ToolContent::Text {
                text: "Hello world".to_string(),
            }],
            is_error: false,
        };
        assert_eq!(extract_tool_result_text(&result), "Hello world");
    }

    #[test]
    fn test_extract_tool_result_text_multiple() {
        let result = CallToolResult {
            content: vec![
                ToolContent::Text {
                    text: "First".to_string(),
                },
                ToolContent::Text {
                    text: "Second".to_string(),
                },
                ToolContent::Text {
                    text: "Third".to_string(),
                },
            ],
            is_error: false,
        };
        assert_eq!(extract_tool_result_text(&result), "First\nSecond\nThird");
    }

    #[test]
    fn test_extract_tool_result_text_empty() {
        let result = CallToolResult {
            content: vec![],
            is_error: false,
        };
        assert_eq!(extract_tool_result_text(&result), "");
    }

    #[test]
    fn test_extract_tool_result_text_mixed_content() {
        let result = CallToolResult {
            content: vec![
                ToolContent::Text {
                    text: "text block".to_string(),
                },
                ToolContent::Resource {
                    resource: ResourceContent {
                        text: "resource text".to_string(),
                        uri: Some("file:///test".to_string()),
                        mime_type: Some("text/plain".to_string()),
                    },
                },
            ],
            is_error: false,
        };
        assert_eq!(
            extract_tool_result_text(&result),
            "text block\nresource text"
        );
    }

    #[test]
    fn test_extract_tool_result_text_resource_no_uri() {
        let result = CallToolResult {
            content: vec![ToolContent::Resource {
                resource: ResourceContent {
                    text: "just text".to_string(),
                    uri: None,
                    mime_type: None,
                },
            }],
            is_error: false,
        };
        assert_eq!(extract_tool_result_text(&result), "just text");
    }

    // ── ToolContent::text helper ────────────────────────────────────────────

    #[test]
    fn test_tool_content_text_from_text() {
        let tc = ToolContent::Text {
            text: "hello".to_string(),
        };
        assert_eq!(tc.text(), "hello");
    }

    #[test]
    fn test_tool_content_text_from_resource() {
        let tc = ToolContent::Resource {
            resource: ResourceContent {
                text: "resource hello".to_string(),
                uri: None,
                mime_type: None,
            },
        };
        assert_eq!(tc.text(), "resource hello");
    }
}
