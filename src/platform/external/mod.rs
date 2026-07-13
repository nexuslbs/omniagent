//! External platform plugin integration - config types, protocol types, and helpers.
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
use std::sync::Arc;

use crate::err_str;
use crate::error::AppResult;

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
    /// Only contains runtime env vars from the plugin.json `env` block
    /// (e.g. RUST_LOG). NOT used for plugin config values.
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// Plugin config values from platforms.yml, with original field names
    /// (e.g. "access_token", "server_url"). Resolved from $env:/$secret:
    /// references. Sent as configure params - plugins are env-var agnostic.
    #[serde(default)]
    pub config: HashMap<String, String>,
    /// Maximum consecutive failures before circuit breaker opens.
    #[serde(default = "default_max_retries")]
    pub max_retries: u32,
}

fn default_enabled() -> bool {
    true
}
fn default_max_retries() -> u32 {
    3
}

/// Collection of platform plugin configurations.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlatformPluginsConfig {
    /// List of external platform plugins.
    pub platforms: Vec<PlatformPluginConfig>,
}

/// Load platform plugin configurations by discovering plugin.json files
/// under `<data_dir>/plugins/platforms/`.
///
/// Each subdirectory containing a `plugin.json` with `"type": "platform"`
/// is loaded as a platform plugin. Platforms are enabled by default.
pub fn load_plugins_config(data_dir: &str) -> Vec<PlatformPluginConfig> {
    let mut results: Vec<PlatformPluginConfig> = Vec::new();
    let platforms_dir = format!("{}/plugins/platforms", data_dir);

    let entries = match std::fs::read_dir(&platforms_dir) {
        Ok(e) => e,
        Err(_) => return results,
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let plugin_json_path = path.join("plugin.json");
        if !plugin_json_path.exists() {
            continue;
        }
        let content = match std::fs::read_to_string(&plugin_json_path) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(
                    "Failed to read platform manifest {}: {:?}",
                    plugin_json_path.display(),
                    e
                );
                continue;
            }
        };
        let manifest = match serde_json::from_str::<crate::plugin::PluginManifest>(&content) {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(
                    "Failed to parse platform manifest {}: {:?}",
                    plugin_json_path.display(),
                    e
                );
                continue;
            }
        };
        if manifest.plugin_type != crate::plugin::PluginType::Platform {
            continue;
        }
        let config = PlatformPluginConfig {
            name: manifest.name.clone(),
            enabled: true,
            command: manifest.entrypoint.clone().unwrap().command,
            args: manifest.entrypoint.unwrap().args,
            env: manifest.env,
            config: std::collections::HashMap::new(),
            max_retries: 3,
        };

        // Validate that the entrypoint binary exists (for file-path commands).
        // Commands that look like PATH lookups (no slash) are assumed to exist.
        let command = &config.command;
        let has_binary = if command.contains('/') {
            let path = std::path::Path::new(command);
            if path.is_relative() {
                std::env::current_dir()
                    .ok()
                    .map(|cwd| cwd.join(path).exists())
                    .unwrap_or(false)
            } else {
                path.exists()
            }
        } else {
            // PATH-based command - assume it's available
            true
        };

        if !has_binary {
            tracing::warn!(
                "Skipping platform plugin '{}': entrypoint binary not found at '{}' (resolved relative to CWD: {:?})",
                manifest.name,
                command,
                std::env::current_dir().ok().map(|cwd| cwd.join(command)),
            );
            continue;
        }

        let merged = merge_platform_config_env(
            &config,
            &serde_json::json!(manifest.config_schema),
            data_dir,
        );
        tracing::info!(
            "Loaded platform plugin '{}' from plugins/platforms/",
            manifest.name
        );
        results.push(merged);
    }

    results
}

/// Convenience wrapper: merge platforms.yml config into a PlatformPluginConfig.
///
/// Populates the `config` field with YAML config values using their original
/// field names (e.g. "access_token", "server_url") - NOT prefixed env keys.
/// The `env` field keeps only subprocess runtime vars (e.g. RUST_LOG from plugin.json).
/// Config $env:/$secret: references are resolved inline.
fn merge_platform_config_env(
    config: &PlatformPluginConfig,
    _config_schema: &serde_json::Value,
    data_dir: &str,
) -> PlatformPluginConfig {
    let mut merged_env = config.env.clone();
    // Only runtime env vars from plugin.json - do NOT merge YAML config into env.
    // YAML config values go to the `config` field with original field names.
    crate::plugins_yaml::merge_yaml_config_into_env(
        &mut merged_env,
        &config.name,
        data_dir,
        &crate::plugins_yaml::PluginYamlType::Platform,
    );

    // Load YAML config with original field names (unprefixed) for configure params.
    let mut config_map = std::collections::HashMap::new();
    let yaml_config = crate::plugins_yaml::load_plugin_yaml_config(
        &config.name,
        data_dir,
        &crate::plugins_yaml::PluginYamlType::Platform,
    );
    if let Some(ref cfg) = yaml_config {
        if let Some(obj) = cfg.as_object() {
            for (key, val) in obj {
                let raw = match val {
                    serde_json::Value::String(s) => s.clone(),
                    serde_json::Value::Number(n) => n.to_string(),
                    serde_json::Value::Bool(b) => b.to_string(),
                    _ => String::new(),
                };
                let resolved = crate::plugins_yaml::resolve_config_value(&raw);
                if !resolved.is_empty() {
                    config_map.insert(key.clone(), resolved);
                }
            }
        }
    }

    PlatformPluginConfig {
        env: merged_env,
        config: config_map,
        ..config.clone()
    }
}

/// Resolve `$env:VAR` and `$secret:NAME` references in a single value.
///
/// Delegates to the shared `crate::plugins_yaml::resolve_config_ref_value`.
/// - `$env:VAR` - reads from process environment via `std::env::var`
/// - `$secret:NAME` - reads from the `secrets` table in the DB
pub async fn resolve_env_ref_value(value: &str, pool: &sqlx::PgPool) -> String {
    crate::plugins_yaml::resolve_config_ref_value(value, pool).await
}

/// Resolve `$env:VAR` and `$secret:NAME` references in all values of an env map.
/// Also resolves legacy `${VAR}` references.
///
/// Delegates to the shared `crate::plugins_yaml::resolve_config_refs`.
pub async fn resolve_env_refs(
    env: &mut std::collections::HashMap<String, String>,
    pool: &sqlx::PgPool,
) {
    crate::plugins_yaml::resolve_config_refs(env, pool).await
}

/// Resolve environment variable references in a config value.
/// Supports `${VAR_NAME}` syntax for legacy `plugin.json` env blocks.
///
/// Delegates to the shared `crate::plugins_yaml::resolve_legacy_env_vars`.
pub fn resolve_env_vars(value: &str) -> String {
    crate::plugins_yaml::resolve_legacy_env_vars(value)
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
    Success { id: u64, result: Value },
    /// Error response.
    Error { id: u64, error: PluginError },
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
    #[serde(default)]
    pub thread_sequence: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cause_external_id: Option<String>,
    /// If the cause message was itself a reply in a thread, this is the
    /// thread root's external_id (e.g. root_id in Mattermost) - used by
    /// platform plugins that don't allow nested threads (Mattermost).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cause_root_id: Option<String>,
    #[serde(default)]
    pub is_summary: bool,
    #[serde(default)]
    pub is_user_thread: bool,
}

/// Result of a deliver operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(dead_code)]
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
#[allow(dead_code)]
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
#[allow(dead_code)]
pub struct DeleteResult {
    pub deleted: bool,
}

// ---------------------------------------------------------------------------
// Inbound message notification (plugin → agent)
// ---------------------------------------------------------------------------

/// A file attachment sent by a platform plugin with the inbound message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileAttachment {
    pub name: String,
    pub size: i64,
    pub mime_type: String,
    /// Raw file content bytes, base64-encoded. Omitted for files > 10MB.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
}

/// An inbound message received by the plugin from the external service.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InboundMessage {
    pub resource_identifier: String,
    pub text: String,
    pub external_id: String,
    #[serde(default)]
    pub files: Vec<FileAttachment>,
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

/// Build a configure request JSON string from the plugin's env map.
pub fn build_configure_request(id: u64, env: &HashMap<String, String>) -> String {
    let req = PluginRequest {
        id: Some(id),
        method: "configure".to_string(),
        params: Some(serde_json::json!(env)),
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
#[allow(dead_code)]
pub fn build_edit_request(id: u64, params: &EditParams) -> String {
    let req = PluginRequest {
        id: Some(id),
        method: "edit_message".to_string(),
        params: Some(serde_json::to_value(params).unwrap_or_default()),
    };
    serde_json::to_string(&req).unwrap_or_default()
}

/// Build a delete_message request JSON string.
#[allow(dead_code)]
pub fn build_delete_request(id: u64, params: &DeleteParams) -> String {
    let req = PluginRequest {
        id: Some(id),
        method: "delete_message".to_string(),
        params: Some(serde_json::to_value(params).unwrap_or_default()),
    };
    serde_json::to_string(&req).unwrap_or_default()
}

// ---------------------------------------------------------------------------
// React
// ---------------------------------------------------------------------------

/// Parameters for the react method.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReactParams {
    pub resource_identifier: String,
    pub external_id: String,
    pub emoji: String,
}

/// Build a react request JSON string.
#[allow(dead_code)]
pub fn build_react_request(id: u64, params: &ReactParams) -> String {
    let req = PluginRequest {
        id: Some(id),
        method: "react".to_string(),
        params: Some(serde_json::to_value(params).unwrap_or_default()),
    };
    serde_json::to_string(&req).unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Response parser
// ---------------------------------------------------------------------------

/// Parse a JSON response line from a plugin.
pub fn parse_response(line: &str) -> AppResult<PluginResponse> {
    serde_json::from_str(line).map_err(|e| err_str!("Failed to parse plugin response: {}", e))
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
#[allow(dead_code)]
pub fn register_platform_client(name: &str, client: client::ExternalPlatformClient) {
    if let Ok(mut registry) = PLATFORM_CLIENT_REGISTRY.lock() {
        registry.insert(name.to_string(), client);
    }
}

/// Decode a base64-encoded string to raw bytes.
pub fn decode_base64(encoded: &str) -> Result<Vec<u8>, anyhow::Error> {
    use base64::engine::general_purpose;
    use base64::Engine;
    general_purpose::STANDARD
        .decode(encoded)
        .map_err(|e| anyhow::anyhow!("Base64 decode error: {}", e))
}

// ---------------------------------------------------------------------------
// File reading - generic trait + HTTP Bearer implementation
// ---------------------------------------------------------------------------

/// A platform-specific file reader.
/// Each platform plugin can provide an implementation that knows how to
/// fetch file content by file_id. The MCP tool `read_attached_file`
/// delegates to the platform-appropriate reader transparently.
#[async_trait::async_trait]
pub trait FileReader: Send + Sync + std::fmt::Debug {
    /// Read a file from the platform's API.
    ///
    /// * `file_id` - the file identifier returned by the platform plugin
    /// * `server_url` - the platform server's base URL (from message metadata)
    ///
    /// Returns raw file bytes on success.
    async fn read_file(&self, file_id: &str, server_url: &str) -> crate::error::AppResult<Vec<u8>>;
}

/// A generic HTTP Bearer-token file reader.
///
/// Fetches files from any platform that exposes a REST API at
/// `{server_url}/api/v4/files/{file_id}` authenticated via Bearer token.
/// Platforms register their own reader instance during setup or at startup.
#[derive(Debug)]
pub struct HttpBearerFileReader {
    access_token: String,
}

impl HttpBearerFileReader {
    /// Create a new HTTP Bearer file reader with the given access token.
    pub fn new(access_token: String) -> Self {
        Self { access_token }
    }
}

#[async_trait::async_trait]
impl FileReader for HttpBearerFileReader {
    async fn read_file(&self, file_id: &str, server_url: &str) -> crate::error::AppResult<Vec<u8>> {
        let base_url = server_url.trim_end_matches('/');
        let url = format!("{}/api/v4/files/{}", base_url, file_id);

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(60))
            .build()
            .map_err(|e| {
                crate::error::Error::Message(format!("Failed to create HTTP client: {}", e))
            })?;

        let resp = client
            .get(&url)
            .header("Authorization", format!("Bearer {}", self.access_token))
            .send()
            .await
            .map_err(|e| {
                crate::error::Error::Message(format!(
                    "HTTP request failed for file {}: {}",
                    file_id, e
                ))
            })?;

        if !resp.status().is_success() {
            return Err(crate::error::Error::Message(format!(
                "File fetch failed: status {} for file_id={} on {}",
                resp.status(),
                file_id,
                url
            )));
        }

        let bytes = resp.bytes().await.map_err(|e| {
            crate::error::Error::Message(format!("Failed to read file body: {}", e))
        })?;

        Ok(bytes.to_vec())
    }
}

/// Scan platform plugin configs and register HTTP Bearer file readers for any
/// plugin that has an `access_token` in its config.
///
/// This is completely generic - no plugin name is hardcoded. Any platform
/// plugin that exposes a REST API with Bearer token auth at
/// `{server_url}/api/v4/files/{file_id}` will automatically get a file reader.
pub fn build_platform_file_readers(
    plugins: &[PlatformPluginConfig],
) -> HashMap<String, Arc<dyn FileReader + Send + Sync>> {
    let mut readers: HashMap<String, Arc<dyn FileReader + Send + Sync>> = HashMap::new();
    for plugin in plugins {
        if let Some(token) = plugin.config.get("access_token") {
            if !token.is_empty() {
                let reader = HttpBearerFileReader::new(token.clone());
                readers.insert(plugin.name.clone(), Arc::new(reader));
                tracing::info!(
                    "Registered file reader for platform '{}' (has access_token)",
                    plugin.name
                );
            }
        }
    }
    readers
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
            thread_sequence: 2,
            cause_external_id: Some("789".to_string()),
            cause_root_id: None,
            is_summary: true,
            is_user_thread: true,
        };
        let req = build_deliver_request(2, &params);
        let parsed: PluginRequest = serde_json::from_str(&req).unwrap();
        assert_eq!(parsed.id, Some(2));
        assert_eq!(parsed.method, "deliver");
    }
}
