//! External MCP server configuration.
//!
//! Servers are configured via a JSON/YAML file pointed to by the
//! `MCP_SERVERS_CONFIG` environment variable, or at a default path
//! `<data_dir>/config/mcp-servers.json`.
//!
//! Each server has a name, transport type (stdio or http), and
//! server-specific settings (command/args for stdio, url for http).

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Supported MCP transport types.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum McpTransport {
    Stdio,
    Http,
}

/// Configuration for a single external MCP server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerConfig {
    /// Unique name for this server (used as tool name prefix).
    pub name: String,
    /// Transport type: "stdio" or "http".
    pub transport: McpTransport,
    /// For stdio: command to execute (e.g. "node", "python3").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// For stdio: arguments to the command.
    #[serde(default)]
    pub args: Vec<String>,
    /// For HTTP: base URL of the MCP server.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Environment variables to set for the subprocess.
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// Maximum time in seconds to wait for a tool call response.
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
    /// Maximum consecutive failures before circuit breaker opens.
    #[serde(default = "default_max_retries")]
    pub max_retries: u32,
    /// List of allowed tool names from this server ("*" = all).
    #[serde(default = "default_allowed_tools")]
    pub allowed_tools: Vec<String>,
}

fn default_timeout() -> u64 { 30 }
fn default_max_retries() -> u32 { 3 }
fn default_allowed_tools() -> Vec<String> { vec!["*".to_string()] }

/// Collection of external MCP server configurations.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServersConfig {
    /// List of external MCP servers.
    pub servers: Vec<McpServerConfig>,
}

/// Load external MCP server configurations from the config file
/// AND from any plugins/mcp/ directories.
///
/// Looks for the config file at:
/// 1. Path specified in `MCP_SERVERS_CONFIG` env var
/// 2. `<data_dir>/config/mcp-servers.json`
///
/// Additionally scans `plugins/mcp/` subdirectories for `mcp-config.json` files.
/// Returns the merged list of all discovered servers.
pub fn load_servers_config(data_dir: &str) -> Vec<McpServerConfig> {
    let mut all_servers = Vec::new();

    // Try config file first
    let config_path = std::env::var("MCP_SERVERS_CONFIG")
        .ok()
        .or_else(|| {
            let default = format!("{}/config/mcp-servers.json", data_dir);
            let path = std::path::Path::new(&default);
            if path.exists() { Some(default) } else { None }
        });

    match config_path {
        Some(path) => {
            match read_config_file(&path) {
                Ok(config) => {
                    tracing::info!(
                        "Loaded {} external MCP server(s) from {}",
                        config.servers.len(),
                        path
                    );
                    all_servers.extend(config.servers);
                }
                Err(e) => {
                    tracing::warn!("Failed to load MCP servers config from {}: {:?}", path, e);
                }
            }
        }
        None => {
            tracing::info!("No MCP servers config file found (set MCP_SERVERS_CONFIG env var)");
        }
    }

    // Also scan plugins/mcp/ directories for mcp-config.json files
    let plugin_servers = discover_plugin_servers(data_dir);
    if !plugin_servers.is_empty() {
        tracing::info!(
            "Loaded {} MCP server(s) from plugins/mcp/ directories",
            plugin_servers.len()
        );
        all_servers.extend(plugin_servers);
    }

    all_servers
}

/// Scan `plugins/mcp/` subdirectories for `mcp-config.json` files.
///
/// Each subdirectory under `plugins/mcp/` is expected to optionally contain
/// an `mcp-config.json` file that defines one or more MCP server configurations.
/// This allows MCP servers to be packaged as self-contained plugins.
///
/// The workspace directory is determined from the `WORKSPACE_DIR` env var
/// (default: `/opt/workspace`).
pub fn discover_plugin_servers(_data_dir: &str) -> Vec<McpServerConfig> {
    // The plugins/mcp/ directory is relative to the workspace directory
    let workspace_dir = std::env::var("WORKSPACE_DIR")
        .unwrap_or_else(|_| "/opt/workspace".to_string());
    let plugins_dir = format!("{}/plugins/mcp", workspace_dir);
    let plugins_path = std::path::Path::new(&plugins_dir);

    if !plugins_path.exists() || !plugins_path.is_dir() {
        return vec![];
    }

    let mut servers = Vec::new();

    // Scan subdirectories
    let entries = match std::fs::read_dir(plugins_path) {
        Ok(entries) => entries,
        Err(e) => {
            tracing::warn!("Failed to read plugins/mcp directory {}: {:?}", plugins_dir, e);
            return vec![];
        }
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }

        let config_file = path.join("mcp-config.json");
        if !config_file.exists() {
            continue;
        }

        let config_path_str = config_file.to_string_lossy().to_string();
        match read_config_file(&config_path_str) {
            Ok(config) => {
                tracing::info!(
                    "Loaded {} MCP server(s) from plugin config: {}",
                    config.servers.len(),
                    config_path_str
                );
                servers.extend(config.servers);
            }
            Err(e) => {
                tracing::warn!(
                    "Failed to parse MCP plugin config from {}: {:?}",
                    config_path_str,
                    e
                );
            }
        }
    }

    servers
}

/// Read and parse the MCP servers config file (JSON or YAML).
fn read_config_file(path: &str) -> Result<McpServersConfig> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read MCP servers config: {}", path))?;

    // Try JSON first
    if let Ok(config) = serde_json::from_str::<McpServersConfig>(&content) {
        return Ok(config);
    }

    // Fallback: try YAML
    let config: McpServersConfig = serde_yaml::from_str(&content)
        .with_context(|| format!("Failed to parse MCP servers config (tried JSON and YAML): {}", path))?;
    Ok(config)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolve_env_vars() {
        std::env::set_var("TEST_MCP_KEY", "secret-key-123");
        let resolved = resolve_env_vars("${TEST_MCP_KEY}");
        assert_eq!(resolved, "secret-key-123");
    }

    #[test]
    fn test_resolve_env_vars_missing() {
        let resolved = resolve_env_vars("${NONEXISTENT_VAR}");
        assert_eq!(resolved, "");
    }

    #[test]
    fn test_resolve_env_vars_mixed() {
        std::env::set_var("MCP_HOST", "localhost");
        let resolved = resolve_env_vars("http://${MCP_HOST}:8080/mcp");
        assert_eq!(resolved, "http://localhost:8080/mcp");
    }

    #[test]
    fn test_default_config_values() {
        let config = McpServerConfig {
            name: "test".to_string(),
            transport: McpTransport::Stdio,
            command: Some("echo".to_string()),
            args: vec![],
            url: None,
            env: HashMap::new(),
            timeout_secs: default_timeout(),
            max_retries: default_max_retries(),
            allowed_tools: default_allowed_tools(),
        };
        assert_eq!(config.timeout_secs, 30);
        assert_eq!(config.max_retries, 3);
        assert_eq!(config.allowed_tools, vec!["*"]);
    }
}
