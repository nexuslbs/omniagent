//! Plugin management system — manifest types, installer, and dynamic enum refresh.
//!
//! This module provides:
//! - Plugin manifest types (from plugin.json on disk)
//! - The installer module (install/uninstall/discover)
//! - Dynamic enum cache for refreshing model lists from external APIs
//!
//! Plugin state (enabled/disabled + config) is managed via YAML files
//! in the `plugins_yaml` module.

pub mod installer;

use anyhow::{Context, Result};
use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Mutex;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// Plugin manifest as loaded from plugin.json on disk
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginManifest {
    pub name: String,
    #[serde(default = "default_version")]
    pub version: String,
    #[serde(rename = "type")]
    pub plugin_type: PluginType,
    pub description: Option<String>,
    #[serde(default)]
    pub entrypoint: Option<PluginEntrypoint>,
    pub capabilities: Option<PluginCapabilities>,
    #[serde(default)]
    pub config_schema: Vec<ConfigSchemaField>,
    /// Environment variables for the plugin subprocess.
    /// Supports ${VAR} syntax for runtime resolution from the host env.
    /// Only used for platform plugins (ignored for MCP tools).
    #[serde(default)]
    pub env: std::collections::HashMap<String, String>,
    /// Default base URL for provider plugins.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_base_url: Option<String>,
    /// API mode for provider plugins ("chat_completions", "anthropic_messages", "dynamic").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_mode: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum PluginType {
    Platform,
    Mcp,
    Provider,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginEntrypoint {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default = "default_transport")]
    pub transport: String, // "stdio" or "http"
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>, // for HTTP transport
}

/// Capabilities a platform plugin can advertise
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginCapabilities {
    #[serde(default)]
    pub inbound: bool,
    #[serde(default)]
    pub outbound: bool,
}

/// Field definition for plugin config forms
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigSchemaField {
    pub key: String,
    pub label: String,
    #[serde(rename = "type")]
    pub field_type: FieldType,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub secret: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allowed_values: Option<Vec<String>>, // for enum fields
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub format: Option<String>, // "uri", "email", "password"
    /// URL to fetch dynamic enum values from (expects OpenAI `/v1/models` format: `{data: [{id:...}]}`).
    /// When set, `allowed_values` is populated from this endpoint instead of the hardcoded list.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refresh_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum FieldType {
    String,
    Secret,
    Boolean,
    Integer,
    Enum,
    MultiSelect,
}

// ---------------------------------------------------------------------------
// Default helpers
// ---------------------------------------------------------------------------

pub fn default_version() -> String {
    "0.1.0".to_string()
}

pub fn default_transport() -> String {
    "stdio".to_string()
}

// ---------------------------------------------------------------------------
// Manifest loading
// ---------------------------------------------------------------------------

/// Read and validate a plugin.json file from disk.
pub fn load_manifest(path: &str) -> Result<PluginManifest> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read plugin manifest: {}", path))?;
    let manifest: PluginManifest = serde_json::from_str(&content)
        .with_context(|| format!("Failed to parse plugin manifest: {}", path))?;
    if manifest.name.is_empty() {
        anyhow::bail!("Plugin manifest has empty 'name' field: {}", path);
    }
    Ok(manifest)
}

// ---------------------------------------------------------------------------
// Dynamic enum refresh (refresh_url support) — cache shared with plugins_yaml
// ---------------------------------------------------------------------------

/// A cached set of dynamic enum values (e.g., model IDs from a /v1/models endpoint).
pub struct DynamicEnumEntry {
    pub values: Vec<String>,
    pub fetched_at: std::time::Instant,
}

/// Global cache of dynamically fetched enum values, keyed by refresh_url.
pub static DYNAMIC_ENUM_CACHE: Lazy<Mutex<HashMap<String, DynamicEnumEntry>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

/// How long cached enum values are considered fresh (5 minutes).
pub const DYNAMIC_ENUM_TTL: std::time::Duration = std::time::Duration::from_secs(300);

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_load_manifest_valid() {
        let dir = tempfile::tempdir().unwrap();
        let manifest_path = dir.path().join("plugin.json");
        let manifest_content = r#"{
            "name": "test-plugin",
            "version": "1.0.0",
            "type": "mcp",
            "description": "A test plugin",
            "entrypoint": {
                "command": "python3",
                "args": ["server.py"],
                "transport": "stdio"
            },
            "config_schema": [
                {
                    "key": "api_key",
                    "label": "API Key",
                    "type": "secret",
                    "required": true,
                    "description": "Your API key"
                }
            ]
        }"#;
        std::fs::write(&manifest_path, manifest_content).unwrap();

        let manifest = load_manifest(manifest_path.to_str().unwrap()).unwrap();
        assert_eq!(manifest.name, "test-plugin");
        assert_eq!(manifest.version, "1.0.0");
        assert_eq!(manifest.plugin_type, PluginType::Mcp);
        assert_eq!(manifest.entrypoint.as_ref().unwrap().command, "python3");
        assert_eq!(manifest.config_schema.len(), 1);
        assert_eq!(manifest.config_schema[0].key, "api_key");
    }

    #[test]
    fn test_load_manifest_invalid_path() {
        let result = load_manifest("/nonexistent/plugin.json");
        assert!(result.is_err());
    }

    #[test]
    fn test_load_manifest_empty_name() {
        let dir = tempfile::tempdir().unwrap();
        let manifest_path = dir.path().join("plugin.json");
        let manifest_content = r#"{
            "name": "",
            "type": "mcp",
            "entrypoint": {
                "command": "test"
            }
        }"#;
        std::fs::write(&manifest_path, manifest_content).unwrap();
        let result = load_manifest(manifest_path.to_str().unwrap());
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("empty 'name'"));
    }

    #[test]
    fn test_manifest_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let manifest_path = dir.path().join("plugin.json");
        let manifest_content = r#"{
            "name": "minimal",
            "type": "platform",
            "entrypoint": {
                "command": "./run.sh"
            }
        }"#;
        std::fs::write(&manifest_path, manifest_content).unwrap();
        let manifest = load_manifest(manifest_path.to_str().unwrap()).unwrap();
        assert_eq!(manifest.version, "0.1.0");
        assert_eq!(manifest.entrypoint.as_ref().unwrap().transport, "stdio");
        assert!(manifest.entrypoint.as_ref().unwrap().url.is_none());
        assert!(manifest.config_schema.is_empty());
        assert!(manifest.capabilities.is_none());
    }

    #[test]
    fn test_parse_plugin_type() {
        assert_eq!(serde_json::from_str::<PluginType>("\"platform\"").unwrap(), PluginType::Platform);
        assert_eq!(serde_json::from_str::<PluginType>("\"mcp\"").unwrap(), PluginType::Mcp);
    }

    #[test]
    fn test_parse_field_type() {
        assert_eq!(serde_json::from_str::<FieldType>("\"string\"").unwrap(), FieldType::String);
        assert_eq!(serde_json::from_str::<FieldType>("\"secret\"").unwrap(), FieldType::Secret);
        assert_eq!(serde_json::from_str::<FieldType>("\"boolean\"").unwrap(), FieldType::Boolean);
        assert_eq!(serde_json::from_str::<FieldType>("\"integer\"").unwrap(), FieldType::Integer);
        assert_eq!(serde_json::from_str::<FieldType>("\"enum\"").unwrap(), FieldType::Enum);
        assert_eq!(serde_json::from_str::<FieldType>("\"multi_select\"").unwrap(), FieldType::MultiSelect);
    }

    #[test]
    fn test_config_schema_field_validation() {
        let json = r#"{
            "key": "api_key",
            "label": "API Key",
            "type": "secret",
            "required": true,
            "secret": true,
            "description": "Your API key",
            "min": 8,
            "max": 128,
            "format": "password"
        }"#;
        let field: ConfigSchemaField = serde_json::from_str(json).unwrap();
        assert_eq!(field.field_type, FieldType::Secret);
        assert!(field.required);
        assert!(field.secret);
        assert_eq!(field.min, Some(8));
        assert_eq!(field.max, Some(128));
        assert_eq!(field.format, Some("password".to_string()));
    }
}
