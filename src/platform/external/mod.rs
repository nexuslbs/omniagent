//! External platform plugin integration — config types, protocol types, and helpers.
//!
//! Platform plugins communicate via JSON-lines over stdin/stdout using a simple
//! JSON-RPC-like protocol (similar to MCP external plugins but simplified for
//! message delivery rather than tool invocation).
//!
//! Each plugin is a subprocess that the agent spawns and communicates with
//! over stdio. The plugin can receive outbound messages (deliver, edit, delete)
//! and optionally send inbound message notifications back to the agent.

pub mod client;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::path::Path;

// ---------------------------------------------------------------------------
// Config types
// ---------------------------------------------------------------------------

/// Configuration for a single external platform plugin.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlatformPluginConfig {
    /// Unique name for this platform (e.g. "telegram", "discord").
    pub name: String,
    /// Whether this platform is enabled.
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    /// Command to execute (e.g. "python3", "./telegram-platform").
    pub command: String,
    /// Arguments to the command.
    #[serde(default)]
    pub args: Vec<String>,
    /// Environment variables to set for the subprocess.
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// Maximum consecutive failures before circuit breaker opens.
    #[serde(default = "default_max_retries")]
    pub max_retries: u32,
}

fn default_enabled() -> bool { true }
fn default_max_retries() -> u32 { 3 }

/// Collection of platform plugin configurations.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlatformPluginsConfig {
    /// List of external platform plugins.
    pub platforms: Vec<PlatformPluginConfig>,
}

/// Load platform plugin configurations from config file.
///
/// Looks for the config file at:
/// 1. Path specified in `PLATFORMS_CONFIG` env var
/// 2. `<data_dir>/config/platforms.json`
///
/// Returns an empty list if no config file is found.
pub fn load_plugins_config(data_dir: &str) -> Vec<PlatformPluginConfig> {
    let config_path = std::env::var("PLATFORMS_CONFIG").ok().or_else(|| {
        let default = format!("{}/config/platforms.json", data_dir);
        let path = Path::new(&default);
        if path.exists() {
            Some(default)
        } else {
            None
        }
    });

    match config_path {
        Some(path) => match std::fs::read_to_string(&path) {
            Ok(content) => match serde_json::from_str::<PlatformPluginsConfig>(&content) {
                Ok(config) => {
                    tracing::info!(
                        "Loaded {} external platform plugin(s) from {}",
                        config.platforms.len(),
                        path
                    );
                    config.platforms
                }
                Err(e) => {
                    tracing::warn!(
                        "Failed to parse platforms config from {}: {:?}",
                        path,
                        e
                    );
                    vec![]
                }
            },
            Err(e) => {
                tracing::warn!(
                    "Failed to read platforms config from {}: {:?}",
                    path,
                    e
                );
                vec![]
            }
        },
        None => {
            tracing::info!("No platforms config found (set PLATFORMS_CONFIG env var)");
            vec![]
        }
    }
}

/// Resolve environment variable references in a config value.
/// Supports `${VAR_NAME}` syntax.
pub fn resolve_env_vars(value: &str) -> String {
    let mut result = value.to_string();
    while let Some(start) = result.find("${") {
        if let Some(end) = result[start..].find('}') {
            let var_name = &result[start + 2..start + end];
            let env_val = std::env::var(var_name).unwrap_or_default();
            result.replace_range(start..start + end + 1, &env_val);
        } else {
            break;
        }
    }
    result
}

// ---------------------------------------------------------------------------
// Platform Plugin Protocol Types
// ---------------------------------------------------------------------------

/// A request sent from the agent to a platform plugin.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginRequest {
    /// Optional request id (absent for notifications).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<u64>,
    /// Method name: "initialize", "deliver", "edit_message", "delete_message".
    pub method: String,
    /// Method-specific parameters.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

/// A response sent from a platform plugin to the agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PluginResponse {
    /// Successful response.
    Success {
        id: u64,
        result: Value,
    },
    /// Error response.
    Error {
        id: u64,
        error: PluginError,
    },
}

/// Error payload from a platform plugin.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginError {
    pub code: i64,
    pub message: String,
}

/// A notification sent from a platform plugin to the agent (no id).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginNotification {
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

// ---------------------------------------------------------------------------
// Initialize
// ---------------------------------------------------------------------------

/// Result of the initialize handshake.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InitializeResult {
    pub name: String,
    pub capabilities: PlatformCapabilities,
}

/// Capabilities advertised by a platform plugin.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlatformCapabilities {
    /// Whether the plugin can receive inbound messages from the external service.
    #[serde(default)]
    pub inbound: bool,
    /// Whether the plugin can send outbound messages to the external service.
    #[serde(default)]
    pub outbound: bool,
}

// ---------------------------------------------------------------------------
// Deliver
// ---------------------------------------------------------------------------

/// Parameters for the deliver method.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeliverParams {
    pub resource_identifier: String,
    pub content: String,
    pub msg_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub msg_subtype: Option<String>,
    #[serde(default)]
    pub thread_id: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cause_external_id: Option<String>,
    #[serde(default)]
    pub is_summary: bool,
    #[serde(default)]
    pub is_user_thread: bool,
}

/// Result of a deliver operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeliverResult {
    pub delivered: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub external_id: Option<String>,
}

// ---------------------------------------------------------------------------
// Edit message
// ---------------------------------------------------------------------------

/// Parameters for the edit_message method.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EditParams {
    pub resource_identifier: String,
    pub external_id: String,
    pub content: String,
}

/// Result of an edit_message operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EditResult {
    pub edited: bool,
}

// ---------------------------------------------------------------------------
// Delete message
// ---------------------------------------------------------------------------

/// Parameters for the delete_message method.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeleteParams {
    pub resource_identifier: String,
    pub external_id: String,
}

/// Result of a delete_message operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeleteResult {
    pub deleted: bool,
}

// ---------------------------------------------------------------------------
// Inbound message notification (plugin → agent)
// ---------------------------------------------------------------------------

/// An inbound message received by the plugin from the external service.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InboundMessage {
    pub resource_identifier: String,
    pub text: String,
    pub external_id: String,
    #[serde(default)]
    pub metadata: Value,
}

/// A notification from the plugin to the agent (e.g. status update).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NotifyMessage {
    pub resource_identifier: String,
    pub content: String,
}

// ---------------------------------------------------------------------------
// Request builders
// ---------------------------------------------------------------------------

/// Build an initialize request JSON string.
pub fn build_initialize_request(id: u64) -> String {
    let req = PluginRequest {
        id: Some(id),
        method: "initialize".to_string(),
        params: Some(serde_json::json!({})),
    };
    serde_json::to_string(&req).unwrap_or_default()
}

/// Build a deliver request JSON string.
pub fn build_deliver_request(id: u64, params: &DeliverParams) -> String {
    let req = PluginRequest {
        id: Some(id),
        method: "deliver".to_string(),
        params: Some(serde_json::to_value(params).unwrap_or_default()),
    };
    serde_json::to_string(&req).unwrap_or_default()
}

/// Build an edit_message request JSON string.
pub fn build_edit_request(id: u64, params: &EditParams) -> String {
    let req = PluginRequest {
        id: Some(id),
        method: "edit_message".to_string(),
        params: Some(serde_json::to_value(params).unwrap_or_default()),
    };
    serde_json::to_string(&req).unwrap_or_default()
}

/// Build a delete_message request JSON string.
pub fn build_delete_request(id: u64, params: &DeleteParams) -> String {
    let req = PluginRequest {
        id: Some(id),
        method: "delete_message".to_string(),
        params: Some(serde_json::to_value(params).unwrap_or_default()),
    };
    serde_json::to_string(&req).unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Response parser
// ---------------------------------------------------------------------------

/// Parse a JSON response line from a plugin.
pub fn parse_response(line: &str) -> anyhow::Result<PluginResponse> {
    serde_json::from_str(line)
        .map_err(|e| anyhow::anyhow!("Failed to parse plugin response: {}", e))
}

// ---------------------------------------------------------------------------
// Global client registry (for health checks)
// ---------------------------------------------------------------------------

use once_cell::sync::Lazy;
use std::sync::Mutex;

/// Global registry of active external platform clients, keyed by platform name.
pub static PLATFORM_CLIENT_REGISTRY: Lazy<Mutex<HashMap<String, client::ExternalPlatformClient>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

/// Register an external platform client for health checks and diagnostics.
pub fn register_platform_client(name: &str, client: client::ExternalPlatformClient) {
    if let Ok(mut registry) = PLATFORM_CLIENT_REGISTRY.lock() {
        registry.insert(name.to_string(), client);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolve_env_vars() {
        std::env::set_var("TEST_PLATFORM_KEY", "my-token");
        let resolved = resolve_env_vars("${TEST_PLATFORM_KEY}");
        assert_eq!(resolved, "my-token");
    }

    #[test]
    fn test_resolve_env_vars_missing() {
        let resolved = resolve_env_vars("${NONEXISTENT_VAR}");
        assert_eq!(resolved, "");
    }

    #[test]
    fn test_resolve_env_vars_mixed() {
        std::env::set_var("PLATFORM_TOKEN", "abc123");
        let resolved = resolve_env_vars("token=${PLATFORM_TOKEN}");
        assert_eq!(resolved, "token=abc123");
    }

    #[test]
    fn test_parse_initialize_response() {
        let json = r#"{"id": 1, "result": {"name": "telegram", "capabilities": {"inbound": true, "outbound": true}}}"#;
        let response = parse_response(json).unwrap();
        match response {
            PluginResponse::Success { id, result } => {
                assert_eq!(id, 1);
                let init: InitializeResult = serde_json::from_value(result).unwrap();
                assert_eq!(init.name, "telegram");
                assert!(init.capabilities.inbound);
                assert!(init.capabilities.outbound);
            }
            _ => panic!("Expected success"),
        }
    }

    #[test]
    fn test_parse_deliver_response() {
        let json = r#"{"id": 2, "result": {"delivered": true, "external_id": "999"}}"#;
        let response = parse_response(json).unwrap();
        match response {
            PluginResponse::Success { result, .. } => {
                let dr: DeliverResult = serde_json::from_value(result).unwrap();
                assert!(dr.delivered);
                assert_eq!(dr.external_id, Some("999".to_string()));
            }
            _ => panic!("Expected success"),
        }
    }

    #[test]
    fn test_parse_error_response() {
        let json = r#"{"id": 1, "error": {"code": -1, "message": "Failed"}}"#;
        let response = parse_response(json).unwrap();
        match response {
            PluginResponse::Error { error, .. } => {
                assert_eq!(error.code, -1);
                assert_eq!(error.message, "Failed");
            }
            _ => panic!("Expected error"),
        }
    }

    #[test]
    fn test_build_initialize_request() {
        let req = build_initialize_request(1);
        let parsed: PluginRequest = serde_json::from_str(&req).unwrap();
        assert_eq!(parsed.id, Some(1));
        assert_eq!(parsed.method, "initialize");
    }

    #[test]
    fn test_build_deliver_request() {
        let params = DeliverParams {
            resource_identifier: "-10012345".to_string(),
            content: "Hello".to_string(),
            msg_type: "summary".to_string(),
            msg_subtype: None,
            thread_id: 456,
            cause_external_id: Some("789".to_string()),
            is_summary: true,
            is_user_thread: true,
        };
        let req = build_deliver_request(2, &params);
        let parsed: PluginRequest = serde_json::from_str(&req).unwrap();
        assert_eq!(parsed.id, Some(2));
        assert_eq!(parsed.method, "deliver");
    }
}
