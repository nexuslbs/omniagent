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
pub fn load_servers_config(data_dir: &str) -> Vec<McpServerConfig> {
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

    // Also scan plugins/tools/ directories for mcp-config.json files
    let plugin_servers = discover_plugin_servers(data_dir);
    if !plugin_servers.is_empty() {
        tracing::info!(
            "Loaded {} MCP server(s) from plugins/tools/ directories",
            plugin_servers.len()
        );
        all_servers.extend(plugin_servers);
    }

    all_servers
}

/// Scan `plugins/tools/` subdirectories for `mcp-config.json` files — SOURCE-AWARE.
///
/// Instead of blindly scanning all directories, this reads `plugins.yml` to
/// determine the active source for each enabled tool plugin and only scans the
/// correct location:
///
/// - `source: built-in` → `/app/plugins/tools/{name}/` (or `/app/plugins/mcp/{name}/`)
/// - `source: bundled`  → `{data_dir}/plugins/tools/{name}/` or `{workspace_dir}/plugins/tools/{name}/`
/// - `source: remote`   → `{data_dir}/plugins/tools/.remote/{repo}/{path}/` (resolved from `remote.yml`)
pub fn discover_plugin_servers(data_dir: &str) -> Vec<McpServerConfig> {
    let mut servers = Vec::new();

    // Read tools from plugins.yml — only scan enabled plugins at their correct source location
    let tools =
        match crate::plugins_yaml::load_raw(data_dir, &crate::plugins_yaml::PluginYamlType::Tool) {
            Ok(tools) => tools,
            Err(_) => {
                // Fallback: if plugins.yml can't be read, scan all directories as before
                return discover_plugin_servers_fallback(data_dir);
            }
        };

    for (name, entry) in &tools {
        if !entry.enabled {
            continue;
        }

        match entry.source.as_str() {
            "built-in" => {
                // Builtins live at /app/plugins/tools/{name}/ or /app/plugins/mcp/{name}/
                for dir in &[
                    format!("/app/plugins/tools/{}", name),
                    format!("/app/plugins/mcp/{}", name),
                ] {
                    if let Some(found) = scan_plugin_dir(dir, data_dir) {
                        servers.extend(found);
                        break; // found it, don't check the fallback dir
                    }
                }
            }
            "bundled" => {
                // Bundled plugins: check data_dir only
                let bundled_path = format!("{}/plugins/tools/{}", data_dir, name);
                if let Some(found) = scan_plugin_dir(&bundled_path, data_dir) {
                    servers.extend(found);
                }
            }
            "remote" => {
                // Remote plugins: look up remote.yml for the path, then scan .remote/{repo}/{path}/
                if let Some(remote) = crate::plugins_yaml::get_remote_plugin(
                    data_dir,
                    &crate::plugins_yaml::PluginYamlType::Tool,
                    name,
                ) {
                    let subpath = remote.path.as_deref().unwrap_or("");
                    let remote_dir =
                        format!("{}/plugins/tools/.remote/{}/{}", data_dir, name, subpath);
                    if let Some(found) = scan_plugin_dir(&remote_dir, data_dir) {
                        servers.extend(found);
                    }
                }
            }
            _ => {}
        }
    }

    servers
}

/// Fallback: scan all directories blindly (used when plugins.yml can't be read).
fn discover_plugin_servers_fallback(data_dir: &str) -> Vec<McpServerConfig> {
    let mut servers = Vec::new();

    let plugins_dir = format!("{}/plugins/tools", data_dir);
    let plugins_path = std::path::Path::new(&plugins_dir);
    if plugins_path.exists() && plugins_path.is_dir() {
        servers.extend(scan_plugin_servers(&plugins_dir, data_dir));
    }

    let app_plugins_dir = "/app/plugins/tools";
    let app_plugins_path = std::path::Path::new(app_plugins_dir);
    if app_plugins_path.exists()
        && app_plugins_path.is_dir()
        && app_plugins_dir != plugins_dir
    {
        let existing_names: std::collections::HashSet<String> =
            servers.iter().map(|s| s.name.clone()).collect();
        let app_servers = scan_plugin_servers(app_plugins_dir, data_dir);
        for srv in app_servers {
            if !existing_names.contains(&srv.name) {
                servers.push(srv);
            }
        }
    }

    if servers.is_empty() {
        if let Ok(cwd) = std::env::current_dir() {
            let cwd_plugins = cwd.join("plugins").join("tools");
            if cwd_plugins.exists() && cwd_plugins.is_dir() {
                let cwd_str = cwd_plugins.to_string_lossy().to_string();
                if cwd_str != plugins_dir {
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
pub(crate) fn get_bin_path(name: &str) -> Option<String> {
    // Look for the binary in multiple locations:
    // 1. Next to the current executable (the canonical location)
    // 2. /app/target/release/<name> (development workspace)
    // 3. Same with mcp-server-{name} convention (package name may differ from plugin name)

    // Primary: next to executable
    if let Some(path) = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| format!("{}/{}", d.display(), name)))
    {
        return Some(path);
    }

    // Fallback: check /app/target/release/<name>
    let dev_path = format!("/app/target/release/{}", name);
    if std::path::Path::new(&dev_path).exists() {
        return Some(dev_path);
    }

    // Fallback: check mcp-server-{name} convention next to executable
    let mcp_server_name = format!("mcp-server-{}", name);
    if let Some(path) = std::env::current_exe().ok().and_then(|p| {
        p.parent()
            .map(|d| format!("{}/{}", d.display(), &mcp_server_name))
    }) {
        if std::path::Path::new(&path).exists() {
            return Some(path);
        }
    }

    // Fallback: check /app/target/release/mcp-server-{name}
    let mcp_dev_path = format!("/app/target/release/mcp-server-{}", name);
    if std::path::Path::new(&mcp_dev_path).exists() {
        return Some(mcp_dev_path);
    }

    // Check for directory-name binary (plugin dir name may be the binary name)
    // e.g. test-rust-tool directory produces test-rust-tool binary
    if let Some(path) = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| format!("{}/{}", d.display(), name)))
    {
        if std::path::Path::new(&path).exists() {
            return Some(path);
        }
    }

    None
}

/// Process a single plugin directory — handles mcp-config.json or Cargo.toml + plugin.json.
/// Returns None if the directory doesn't exist or has no valid plugin manifest.
fn scan_plugin_dir(plugin_dir: &str, data_dir: &str) -> Option<Vec<McpServerConfig>> {
    let path = std::path::Path::new(plugin_dir);
    if !path.exists() || !path.is_dir() {
        return None;
    }

    let dir_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .to_string();

    let config_file = path.join("mcp-config.json");
    let has_cargo_toml = path.join("Cargo.toml").exists();
    let has_plugin_json = path.join("plugin.json").exists();

    // Skip utility libs (no manifest files at all)
    if !(config_file.exists() || has_cargo_toml && has_plugin_json) {
        return None;
    }

    let mut servers = Vec::new();

    // Builtin crate: no mcp-config.json but has Cargo.toml + plugin.json
    // Create a synthetic server config with binary resolved via get_bin_path
    if !config_file.exists() && has_cargo_toml && has_plugin_json {
        let pkg = std::fs::read_to_string(path.join("Cargo.toml"))
            .ok()
            .and_then(|content| {
                content.lines().find_map(|line| {
                    let trimmed = line.trim();
                    if let Some(name) = trimmed.strip_prefix("name = \"") {
                        name.strip_suffix('"').map(|s| s.to_string())
                    } else {
                        None
                    }
                })
            })
            .unwrap_or_else(|| format!("mcp-server-{}", dir_name));

        if let Some(cmd) = get_bin_path(&pkg) {
            tracing::info!(
                "Builtin crate '{}' at {}: resolved binary via get_bin_path: {}",
                dir_name,
                path.display(),
                cmd
            );
            let mut srv = McpServerConfig {
                name: dir_name.clone(),
                transport: McpTransport::Stdio,
                command: Some(cmd),
                args: vec![],
                url: None,
                env: HashMap::new(),
                timeout_secs: default_timeout(),
                max_retries: default_max_retries(),
                allowed_tools: default_allowed_tools(),
                pool_size: default_pool_size(),
            };
            crate::plugins_yaml::merge_yaml_config_into_env(
                &mut srv.env,
                &dir_name,
                data_dir,
                &crate::plugins_yaml::PluginYamlType::Tool,
            );
            servers.push(srv);
        }
        return Some(servers);
    }

    // Has mcp-config.json
    if !config_file.exists() {
        return None;
    }

    let config_path_str = config_file.to_string_lossy().to_string();
    match read_config_file(&config_path_str) {
        Ok(config) => {
            tracing::info!(
                "Loaded {} MCP server(s) from plugin config: {}",
                config.servers.len(),
                config_path_str
            );

            let plugin_dir_str = path.to_string_lossy().to_string();

            let cargo_package_name = if has_cargo_toml {
                std::fs::read_to_string(path.join("Cargo.toml"))
                    .ok()
                    .and_then(|content| {
                        content.lines().find_map(|line| {
                            let trimmed = line.trim();
                            if let Some(name) = trimmed.strip_prefix("name = \"") {
                                name.strip_suffix('"').map(|s| s.to_string())
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
                                Some(ref bin_path) => {
                                    tracing::info!(
                                        "Resolved command for '{}' by convention: {}",
                                        srv.name, bin_path
                                    );
                                    srv.command = Some(bin_path.clone());
                                }
                                None => {
                                    tracing::warn!(
                                        "MCP server '{}' has no binary found at any convention path",
                                        srv.name,
                                    );
                                }
                            }
                        } else {
                            // No Cargo.toml — try workspace member convention, then
                            // plugin-local target/release.
                            let mut candidates = Vec::new();

                            // 1. Workspace: binary next to omniagent binary
                            if let Some(w) = get_bin_path(&format!("mcp-server-{}", srv.name)) {
                                candidates.push(w);
                            }

                            // 2. Plugin-local target/release
                            let ws_name = format!("mcp-server-{}", srv.name);
                            candidates.push(format!(
                                "{}/target/release/{}",
                                plugin_dir_str, ws_name
                            ));
                            let found = candidates
                                .into_iter()
                                .find(|p| std::path::Path::new(p).exists());

                            match found {
                                Some(ref bin_path) => {
                                    tracing::info!(
                                        "Resolved command for '{}' by convention: {}",
                                        srv.name, bin_path
                                    );
                                    srv.command = Some(bin_path.clone());
                                }
                                None => {
                                    tracing::warn!(
                                        "MCP server '{}' has no command configured, no Cargo.toml, and no binary found in workspace or plugin target/release",
                                        srv.name
                                    );
                                }
                            }
                        }
                    }

                    crate::plugins_yaml::merge_yaml_config_into_env(
                        &mut srv.env, &dir_name, data_dir,
                        &crate::plugins_yaml::PluginYamlType::Tool,
                    );

                    srv
                })
                .collect();

            servers.extend(resolved_servers);
            Some(servers)
        }
        Err(e) => {
            tracing::warn!(
                "Failed to parse MCP plugin config from {}: {:?}",
                config_path_str,
                e
            );
            None
        }
    }
}

/// Scan a `plugins/tools/` directory for MCP config files (used as fallback for directory-level scans).
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
        let dir_str = path.to_string_lossy().to_string();
        if let Some(found) = scan_plugin_dir(&dir_str, data_dir) {
            servers.extend(found);
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
    let config: McpServersConfig = serde_yaml::from_str(&content).ctx(format!(
        "Failed to parse MCP servers config (tried JSON and YAML): {}",
        path
    ))?;
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
