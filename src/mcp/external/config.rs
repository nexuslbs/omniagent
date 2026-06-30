//! External MCP server configuration.
//!
//! Servers are configured via a JSON/YAML file pointed to by the
//! `MCP_SERVERS_CONFIG` environment variable, or at a default path
//! `<data_dir>/config/mcp-servers.json`.
//!
//! Each server has a name, transport type (stdio or http), and
//! server-specific settings (command/args for stdio, url for http).

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::error::{AppResult, ErrorContext};

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
    /// Number of per-channel subprocesses in the connection pool.
    /// Each channel gets its own pool of this many processes, so
    /// channels never block each other. Default 1 = one process per
    /// channel (no intra-channel blocking, but still serial within
    /// the same channel for a single-threaded channel handler).
    /// Increase for servers where multi-tool calls within the same
    /// channel issue concurrent tool calls (e.g. test tools with
    /// long-duration waits).
    #[serde(default = "default_pool_size")]
    pub pool_size: u32,
}

fn default_timeout() -> u64 {
    30
}
fn default_max_retries() -> u32 {
    3
}
fn default_allowed_tools() -> Vec<String> {
    vec!["*".to_string()]
}
fn default_pool_size() -> u32 {
    1
}

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
pub fn load_servers_config(data_dir: &str, workspace_dir: &str) -> Vec<McpServerConfig> {
    let mut all_servers = Vec::new();

    // Try config file first
    let config_path = std::env::var("MCP_SERVERS_CONFIG").ok().or_else(|| {
        let default = format!("{}/config/mcp-servers.json", data_dir);
        let path = std::path::Path::new(&default);
        if path.exists() {
            Some(default)
        } else {
            None
        }
    });

    match config_path {
        Some(path) => match read_config_file(&path) {
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
        },
        None => {
            tracing::info!("No MCP servers config file found (set MCP_SERVERS_CONFIG env var)");
        }
    }

    // Also scan plugins/mcp/ directories for mcp-config.json files
    let plugin_servers = discover_plugin_servers(data_dir, workspace_dir);
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
/// Scans two locations:
/// 1. `{data_dir}/plugins/mcp/` — installed/data-level plugins (primary)
/// 2. `{workspace_dir}/plugins/mcp/` — bundled workspace plugins (secondary, dedupped)
pub fn discover_plugin_servers(data_dir: &str, workspace_dir: &str) -> Vec<McpServerConfig> {
    let mut servers = Vec::new();

    // Scan data_dir/plugins/mcp (installed/data-level plugins) — primary source
    let plugins_dir = format!("{}/plugins/mcp", data_dir);
    let plugins_path = std::path::Path::new(&plugins_dir);
    if plugins_path.exists() && plugins_path.is_dir() {
        servers.extend(scan_plugin_servers(&plugins_dir, data_dir));
    }

    // Also scan workspace_dir/plugins/mcp (bundled workspace plugins) — secondary source
    // Dedup by server name against already-discovered data_dir plugins.
    let ws_plugins_dir = format!("{}/plugins/mcp", workspace_dir);
    let ws_plugins_path = std::path::Path::new(&ws_plugins_dir);
    if ws_plugins_path.exists() && ws_plugins_path.is_dir() && ws_plugins_dir != plugins_dir {
        let existing_names: std::collections::HashSet<String> =
            servers.iter().map(|s| s.name.clone()).collect();
        let ws_servers = scan_plugin_servers(&ws_plugins_dir, data_dir);
        for srv in ws_servers {
            if !existing_names.contains(&srv.name) {
                servers.push(srv);
            }
        }
    }

    // Fallback: scan ./plugins/mcp relative to CWD for backward compatibility
    // Only used when neither canonical path exists, to avoid
    // duplicate server registrations when both directories have configs.
    if servers.is_empty() {
        if let Ok(cwd) = std::env::current_dir() {
            let cwd_plugins = cwd.join("plugins").join("mcp");
            if cwd_plugins.exists() && cwd_plugins.is_dir() {
                let cwd_str = cwd_plugins.to_string_lossy().to_string();
                if cwd_str != plugins_dir && cwd_str != ws_plugins_dir {
                    servers.extend(scan_plugin_servers(&cwd_str, data_dir));
                }
            }
        }
    }

    servers
}

/// Resolve a plugin binary path relative to the omniagent binary's directory.
///
/// Workspace member MCP servers (mcp-server-*) live next to the omniagent
/// binary in both dev (`/app/target/release/`) and production (Docker image),
/// so we resolve by convention without hardcoded paths or env vars.
fn get_bin_path(name: &str) -> Option<String> {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| format!("{}/{}", d.display(), name)))
}

/// Scan a single `plugins/mcp/` directory for `mcp-config.json` files.
fn scan_plugin_servers(plugins_dir: &str, data_dir: &str) -> Vec<McpServerConfig> {
    let plugins_path = std::path::Path::new(plugins_dir);
    if !plugins_path.exists() || !plugins_path.is_dir() {
        return vec![];
    }

    let mut servers = Vec::new();
    tracing::info!("Scanning for MCP plugin configs in: {}", plugins_dir);

    let entries = match std::fs::read_dir(plugins_path) {
        Ok(entries) => entries,
        Err(e) => {
            tracing::warn!(
                "Failed to read MCP plugin directory {}: {:?}",
                plugins_dir,
                e
            );
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

                // Resolve command by convention for Rust plugins where command is not set
                let plugin_dir_str = path.to_string_lossy().to_string();
                let has_cargo_toml = path.join("Cargo.toml").exists();

                // Read package name from Cargo.toml for proper binary resolution
                let cargo_package_name = if has_cargo_toml {
                    std::fs::read_to_string(path.join("Cargo.toml"))
                        .ok()
                        .and_then(|content| {
                            content
                                .lines()
                                .find_map(|line| {
                                    let trimmed = line.trim();
                                    if let Some(name) = trimmed.strip_prefix("name = \"") {
                                        name.strip_suffix('\"').map(|s| s.to_string())
                                    } else {
                                        None
                                    }
                                })
                        })
                } else {
                    None
                };

                let resolved_servers: Vec<McpServerConfig> = config
                    .servers
                    .into_iter()
                    .map(|mut srv| {
                        if srv.transport == McpTransport::Stdio && srv.command.is_none() {
                            if has_cargo_toml {
                                // Resolve binary path by convention:
                                // 1. {plugin_dir}/target/release/{package_name} — standalone plugin
                                // 2. {get_bin_path(package_name)} — workspace member (next to omniagent binary)
                                // 3. {plugin_dir}/target/release/{name} — fallback using server name
                                let pkg = cargo_package_name
                                    .as_deref()
                                    .unwrap_or(&srv.name);

                                let mut candidates = vec![
                                    format!("{}/target/release/{}", plugin_dir_str, pkg),
                                ];
                                if let Some(w) = get_bin_path(pkg) {
                                    candidates.push(w);
                                }
                                candidates.push(format!(
                                    "{}/target/release/{}",
                                    plugin_dir_str, srv.name
                                ));

                                let found = candidates
                                    .into_iter()
                                    .find(|p| std::path::Path::new(p).exists());

                                match found {
                                    Some(ref path) => {
                                        tracing::info!(
                                            "Resolved command for '{}' by convention: {}",
                                            srv.name, path
                                        );
                                        srv.command = Some(path.clone());
                                    }
                                    None => {
                                        tracing::warn!(
                                            "MCP server '{}' has no binary found at any convention path",
                                            srv.name,
                                        );
                                    }
                                }
                            } else {
                                // No Cargo.toml — try workspace member convention
                                // Binary is next to the omniagent binary: {get_bin_path("mcp-server-{name}")}
                                let workspace_path = get_bin_path(&format!("mcp-server-{}", srv.name));
                                match workspace_path {
                                    Some(ref path) if std::path::Path::new(path).exists() => {
                                        tracing::info!(
                                            "Resolved command for '{}' by workspace convention: {}",
                                            srv.name, path
                                        );
                                        srv.command = Some(path.clone());
                                    }
                                    _ => {
                                        tracing::warn!(
                                            "MCP server '{}' has no command configured and no Cargo.toml or workspace binary found",
                                            srv.name
                                        );
                                    }
                                }
                            }
                        }

                        // Merge tools.yml config values into server's env map
                        let plugin_name = path
                            .file_name()
                            .and_then(|n| n.to_str())
                            .unwrap_or(&srv.name);
                        crate::plugins_yaml::merge_yaml_config_into_env(
                            &mut srv.env,
                            plugin_name,
                            data_dir,
                            &crate::plugins_yaml::PluginYamlType::Tool,
                        );

                        srv
                    })
                    .collect();

                servers.extend(resolved_servers);
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
fn read_config_file(path: &str) -> AppResult<McpServersConfig> {
    let content = std::fs::read_to_string(path)
        .ctx(format!("Failed to read MCP servers config: {}", path))?;

    // Try JSON first
    if let Ok(config) = serde_json::from_str::<McpServersConfig>(&content) {
        return Ok(config);
    }

    // Fallback: try YAML
    let config: McpServersConfig = serde_yaml::from_str(&content).ctx(
        format!(
            "Failed to parse MCP servers config (tried JSON and YAML): {}",
            path
        )
    )?;
    Ok(config)
}

/// Resolve environment variable references in a config value.
/// Supports `${VAR_NAME}` syntax.
pub fn resolve_env_vars(value: &str) -> String {
    let mut result = value.to_string();
    while let Some(start) = result.find("${") {
        if let Some(end) = result[start..].find('}') {
            let raw = &result[start + 2..start + end];
            // Support ${VAR:-default} syntax
            let (var_name, default_val) = if let Some(delim) = raw.find(":-") {
                let var = &raw[..delim];
                let default = &raw[delim + 2..];
                (var, Some(default.to_string()))
            } else {
                (raw, None)
            };
            let env_val = std::env::var(var_name)
                .ok()
                .filter(|v| !v.is_empty())
                .or(default_val)
                .unwrap_or_default();
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
            pool_size: 1,
        };
        assert_eq!(config.timeout_secs, 30);
        assert_eq!(config.max_retries, 3);
        assert_eq!(config.allowed_tools, vec!["*"]);
    }
}
