//! External MCP server configuration.
//!
//! Servers are configured via a JSON/YAML file pointed to by the
//! `MCP_SERVERS_CONFIG` environment variable, or at a default path
//! `<data_dir>/config/mcp-servers.json`.
//!
//! Each server has a name, transport type (stdio or http), and
//! server-specific settings (command/args for stdio, url for http).
//!
//! LOG HYGIENE: config discovery/refresh functions run repeatedly (per server
//! init, per config refresh), so discovery logging MUST stay at debug level.
//! Never log discovery events at info/error: a repeated refresh would flood the
//! journal (2026-09-05 incident: ~286k INFO lines in 21h). Guarded by
//! tests/log_hygiene.rs.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::error::{AppResult, ErrorContext};
use crate::plugins_yaml;

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
    /// Per-tool behaviour declared in the plugin manifest (audit V-2).
    /// Carried onto every registered tool so the core agent loop derives its
    /// guards from descriptors, never from hardcoded tool names.
    #[serde(default, skip_serializing_if = "std::collections::HashMap::is_empty")]
    pub tool_behavior: crate::mcp::behavior::ToolBehaviorMap,
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
    /// Working directory for the spawned process (only for stdio transport).
    /// If not set, inherits the omniagent process CWD.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_dir: Option<String>,
    /// Maximum time in seconds to wait for a tool call response.
    /// `None` = NO timeout: the server may take as long as it needs; the
    /// caller (agent) tracks/cancels via background tasks. A timeout applies
    /// ONLY when explicitly configured. Fixed default timeouts were removed
    /// (Aug 2026) - a tool must never be killed by an invisible clock.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
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

    // Use default path in data_dir
    let default = format!("{}/config/mcp-servers.json", data_dir);
    let config_path = if std::path::Path::new(&default).exists() {
        Some(default)
    } else {
        None
    };

    match config_path {
        Some(path) => match read_config_file(&path) {
            Ok(config) => {
                tracing::debug!(
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
            tracing::debug!("No MCP servers config file found (set MCP_SERVERS_CONFIG env var)");
        }
    }

    // Also scan plugins/tools/ directories for mcp-config.json files
    let plugin_servers = discover_plugin_servers(data_dir);
    if !plugin_servers.is_empty() {
        tracing::debug!(
            "Loaded {} MCP server(s) from plugins/tools/ directories",
            plugin_servers.len()
        );
        all_servers.extend(plugin_servers);
    }

    all_servers
}

/// Scan `plugins/tools/` subdirectories for `mcp-config.json` files: SOURCE-AWARE.
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

    // Read tools from plugins.yml: only scan enabled plugins at their correct source location
    let tools =
        match crate::plugins_yaml::load_raw(data_dir, &crate::plugins_yaml::PluginYamlType::Tool) {
            Ok(tools) => {
                tracing::debug!(
                    "discover_plugin_servers: load_raw OK, {} entries",
                    tools.len()
                );
                tools
            }
            Err(e) => {
                tracing::debug!(
                    "discover_plugin_servers: load_raw failed: {:?}, falling back",
                    e
                );
                return discover_plugin_servers_fallback(data_dir);
            }
        };

    for (name, entry) in &tools {
        if !entry.enabled {
            continue;
        }

        tracing::debug!(
            "discover: tool '{}' source='{}' enabled={}",
            name,
            entry.source,
            entry.enabled
        );

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
    if app_plugins_path.exists() && app_plugins_path.is_dir() && app_plugins_dir != plugins_dir {
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

/// Resolve a workspace-member binary path deterministically.
///
/// Built-in MCP server binaries are workspace members compiled by
/// `cargo build --release --workspace` and live next to the omniagent
/// executable. The path is computed by convention - no existence checks,
/// no fallback chain. Each plugin has exactly one deterministic path.
pub(crate) fn get_bin_path(name: &str) -> String {
    // Binary lives next to the omniagent executable (workspace target/release).
    // Fallback to /app/target/release/ if current_exe() is unavailable.
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| format!("{}/{}", d.display(), name)))
        .unwrap_or_else(|| format!("/app/target/release/{}", name))
}

/// Read a plugin's declared tool behaviours from its `plugin.json`
/// (audit V-2). A missing or unparsable manifest declares nothing: the tools
/// stay behaviour-neutral (fail CLOSED) and the registry warns loudly.
fn manifest_tool_behavior(plugin_dir: &str) -> crate::mcp::behavior::ToolBehaviorMap {
    let manifest_path = format!("{}/plugin.json", plugin_dir);
    match crate::plugin::load_manifest(&manifest_path) {
        Ok(manifest) if !manifest.tools.is_empty() => {
            let map = crate::mcp::behavior::behavior_map(&manifest.tools);
            tracing::debug!(
                "Plugin '{}': {} tool behaviour descriptor(s)",
                plugin_dir,
                map.len()
            );
            map
        }
        Ok(_) => crate::mcp::behavior::ToolBehaviorMap::new(),
        Err(e) => {
            tracing::debug!(
                "No tool behaviour descriptors for '{}' ({}): {}",
                plugin_dir,
                manifest_path,
                e
            );
            crate::mcp::behavior::ToolBehaviorMap::new()
        }
    }
}

/// Process a single plugin directory: handles mcp-config.json or Cargo.toml + plugin.json.
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

    // Skip utility libs (no manifest files at all). A plugin.json alone is
    // enough: it may declare an entrypoint/binary (prebuilt artifact).
    if !(config_file.exists() || has_plugin_json) {
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

        let cmd = get_bin_path(&pkg);
        tracing::debug!(
            "Builtin crate '{}' at {}: resolved binary: {}",
            dir_name,
            path.display(),
            cmd
        );
        let mut srv = McpServerConfig {
            tool_behavior: manifest_tool_behavior(plugin_dir),
            name: dir_name.clone(),
            transport: McpTransport::Stdio,
            command: Some(cmd),
            args: vec![],
            url: None,
            env: HashMap::new(),
            current_dir: None,
            timeout_secs: None,
            max_retries: default_max_retries(),
            allowed_tools: default_allowed_tools(),
            pool_size: default_pool_size(),
        };
        // Load YAML config values with original (non-prefixed) keys
        if let Some(yaml_config) = crate::plugins_yaml::load_plugin_yaml_config(
            &dir_name,
            data_dir,
            &crate::plugins_yaml::PluginYamlType::Tool,
        ) {
            if let Some(obj) = yaml_config.as_object() {
                for (key, val) in obj {
                    // YAML config values can be strings, numbers, or booleans
                    // (e.g. `github_app_id: 3967918`). Serialize non-strings to
                    // their literal form instead of silently dropping them -
                    // as_str() on a Number returns None, which used to make
                    // numeric plugin config (like git's github_app_id /
                    // github_installation_id) vanish and auth fail with
                    // "must be set in the plugin config".
                    let raw = match val {
                        serde_json::Value::String(s) => s.clone(),
                        serde_json::Value::Number(n) => n.to_string(),
                        serde_json::Value::Bool(b) => b.to_string(),
                        _ => String::new(),
                    };
                    if !raw.is_empty() {
                        let resolved = crate::plugins_yaml::resolve_config_value(&raw);
                        if !resolved.is_empty() {
                            srv.env.insert(key.clone(), resolved);
                        }
                    }
                }
            }
        }
        // Apply config_schema defaults from plugin.json (fills missing fields)
        apply_config_schema_defaults(&mut srv.env, &path.to_string_lossy());

        servers.push(srv);
        return Some(servers);
    }

    // Binary/script plugin: plugin.json declares an `entrypoint` (and possibly a
    // downloadable `binary` artifact), with no Cargo.toml and no mcp-config.json.
    // Synthesize the MCP server from the manifest so the plugin is startable, and
    // report clearly when the declared binary has not been installed yet.
    if !config_file.exists() && !has_cargo_toml && has_plugin_json {
        let manifest_path = path.join("plugin.json");
        let manifest = match crate::plugin::load_manifest(&manifest_path.to_string_lossy()) {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(
                    "MCP plugin '{}': cannot read {}: {:?}",
                    dir_name,
                    manifest_path.display(),
                    e
                );
                return None;
            }
        };

        if manifest.entrypoint.is_none() {
            if manifest.binary.is_some() {
                tracing::warn!(
                    "MCP plugin '{}': declares a `binary` artifact but no `entrypoint` -                      add an entrypoint with the command that starts its MCP server",
                    dir_name
                );
            }
            return None;
        }

        let ep = manifest
            .entrypoint
            .as_ref()
            .expect("entrypoint checked above");
        let plugin_dir_str = path.to_string_lossy().to_string();
        let (command, url) = if ep.transport == "http" {
            (None, ep.url.clone())
        } else {
            (
                Some(resolve_entrypoint_command(
                    ep,
                    path,
                    manifest.binary.as_ref(),
                )),
                None,
            )
        };

        let mut srv = McpServerConfig {
            tool_behavior: manifest_tool_behavior(plugin_dir),
            name: dir_name.clone(),
            transport: if ep.transport == "http" {
                McpTransport::Http
            } else {
                McpTransport::Stdio
            },
            command,
            args: ep.args.clone(),
            url,
            env: HashMap::new(),
            current_dir: Some(plugin_dir_str.clone()),
            timeout_secs: None,
            max_retries: default_max_retries(),
            allowed_tools: default_allowed_tools(),
            pool_size: default_pool_size(),
        };
        if let Some(yaml_config) = crate::plugins_yaml::load_plugin_yaml_config(
            &dir_name,
            data_dir,
            &crate::plugins_yaml::PluginYamlType::Tool,
        ) {
            if let Some(obj) = yaml_config.as_object() {
                for (key, val) in obj {
                    let raw = match val {
                        serde_json::Value::String(s) => s.clone(),
                        serde_json::Value::Number(n) => n.to_string(),
                        serde_json::Value::Bool(b) => b.to_string(),
                        _ => String::new(),
                    };
                    if !raw.is_empty() {
                        let resolved = crate::plugins_yaml::resolve_config_value(&raw);
                        if !resolved.is_empty() {
                            srv.env.insert(key.clone(), resolved);
                        }
                    }
                }
            }
        }
        apply_config_schema_defaults(&mut srv.env, &plugin_dir_str);
        tracing::debug!(
            "Binary/script plugin '{}': synthesized MCP server (command: {:?})",
            dir_name,
            srv.command
        );
        return Some(vec![srv]);
    }

    // Has mcp-config.json - parse it, or return None if no config file
    if !config_file.exists() {
        return None;
    }

    let config_path_str = config_file.to_string_lossy().to_string();
    match read_config_file(&config_path_str) {
        Ok(config) => {
            tracing::debug!(
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

            let declared_behavior = manifest_tool_behavior(&plugin_dir_str);
            let resolved_servers: Vec<McpServerConfig> = config
                .servers
                .into_iter()
                .map(|mut srv| {
                    if !declared_behavior.is_empty() {
                        srv.tool_behavior = declared_behavior.clone();
                    }
                    if srv.transport == McpTransport::Stdio && srv.command.is_none() {
                        if has_cargo_toml {
                            // Deterministic binary path by plugin location:
                            // - Under /app/plugins/ → workspace member (next to omniagent)
                            // - Elsewhere (bundled/remote) → own target/release/
                            let pkg = cargo_package_name
                                .as_deref()
                                .unwrap_or(&srv.name);

                            let bin_path = if plugin_dir_str.starts_with("/app/plugins/") {
                                get_bin_path(pkg)
                            } else {
                                format!("{}/target/release/{}", plugin_dir_str, pkg)
                            };

                            if std::path::Path::new(&bin_path).exists() {
                                tracing::debug!(
                                    "Resolved command for '{}': {}",
                                    srv.name, bin_path
                                );
                                srv.command = Some(bin_path);
                            } else {
                                tracing::warn!(
                                    "MCP server '{}' binary not found at expected path: {}",
                                    srv.name, bin_path,
                                );
                            }
                        } else {
                            // No Cargo.toml: binary must be pre-built (no source to compile).
                            // Deterministic path by plugin location:
                            // - Under /app/plugins/ → workspace member (next to omniagent)
                            // - Elsewhere (bundled/remote) → own target/release/
                            let bin_name = format!("mcp-server-{}", srv.name);
                            let bin_path = if plugin_dir_str.starts_with("/app/plugins/") {
                                get_bin_path(&bin_name)
                            } else {
                                format!("{}/target/release/{}", plugin_dir_str, bin_name)
                            };

                            if std::path::Path::new(&bin_path).exists() {
                                tracing::debug!(
                                    "Resolved command for '{}': {}",
                                    srv.name, bin_path
                                );
                                srv.command = Some(bin_path);
                            } else {
                                tracing::warn!(
                                    "MCP server '{}' has no command configured and no binary at expected path: {}",
                                    srv.name, bin_path,
                                );
                            }
                        }
                    }

                    // Load YAML config values with original (non-prefixed) keys
                    if let Some(yaml_config) = crate::plugins_yaml::load_plugin_yaml_config(
                        &dir_name, data_dir,
                        &crate::plugins_yaml::PluginYamlType::Tool,
                    ) {
                        if let Some(obj) = yaml_config.as_object() {
                            for (key, val) in obj {
                                // YAML config values can be strings, numbers, or
                                // booleans (e.g. `github_app_id: 3967918`).
                                // Serialize non-strings to their literal form
                                // instead of silently dropping them.
                                let raw = match val {
                                    serde_json::Value::String(s) => s.clone(),
                                    serde_json::Value::Number(n) => n.to_string(),
                                    serde_json::Value::Bool(b) => b.to_string(),
                                    _ => String::new(),
                                };
                                if !raw.is_empty() {
                                    let resolved = crate::plugins_yaml::resolve_config_value(&raw);
                                    if !resolved.is_empty() {
                                        srv.env.insert(key.clone(), resolved);
                                    }
                                }
                            }
                        }
                    }
                    // Apply config_schema defaults from plugin.json (fills missing fields)
                    apply_config_schema_defaults(&mut srv.env, &path.to_string_lossy());

                    // Set working directory to the plugin directory so relative
                    // args (e.g. ["server.py"]) resolve correctly.
                    if srv.current_dir.is_none() {
                        srv.current_dir = Some(plugin_dir_str.clone());
                    }

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
    tracing::debug!("Scanning for MCP plugin configs in: {}", plugins_dir);

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

/// Apply config_schema defaults from plugin.json into the env map.
///
/// For each field in config_schema that has a `default` value and whose key
/// is not already present in `env`, resolves `$env:` references and adds it
/// with the original (non-prefixed) key. This allows the configure message
/// sent to the plugin to include these values.
fn apply_config_schema_defaults(env: &mut HashMap<String, String>, plugin_dir: &str) {
    let plugin_json_path = std::path::Path::new(plugin_dir).join("plugin.json");
    if !plugin_json_path.exists() {
        return;
    }

    // Read plugin.json and extract config_schema programmatically
    let content = match std::fs::read_to_string(&plugin_json_path) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("Failed to read plugin.json from {}: {:?}", plugin_dir, e);
            return;
        }
    };

    // Parse as a generic JSON object so we don't need a full manifest struct here
    let parsed: serde_json::Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("Failed to parse plugin.json from {}: {:?}", plugin_dir, e);
            return;
        }
    };

    let schema_fields = match parsed.get("config_schema") {
        Some(arr) if arr.is_array() => arr.as_array().unwrap(),
        _ => return, // no config_schema
    };

    for field in schema_fields {
        let key = match field.get("key").and_then(|v| v.as_str()) {
            Some(k) => k,
            None => continue,
        };

        let default_val = match field.get("default") {
            Some(d) => d.as_str().unwrap_or(""),
            None => continue,
        };

        if default_val.is_empty() {
            continue;
        }

        // Skip if key is already in env (YAML config overrides schema defaults)
        if env.contains_key(key) {
            continue;
        }

        // Resolve $env: references
        let resolved = plugins_yaml::resolve_config_value(default_val);
        if !resolved.is_empty() {
            env.insert(key.to_string(), resolved);
        }
    }
}

/// Resolve environment variable references in a config value.
///
/// DEPRECATED: `${VAR_NAME}` is never interpolated - it is treated as a
/// literal string. Only `$env:` references are resolved (see
/// `plugins_yaml::resolve_config_value`).
pub fn resolve_env_vars(value: &str) -> String {
    value.to_string()
}

/// Resolve the executable for a manifest-declared plugin entrypoint.
///
/// Paths are resolved inside the plugin directory first (that is where the
/// plugin INSTALL action places a downloaded binary artifact), then as a bare
/// command (PATH lookup). A declared binary that is missing produces a clear,
/// actionable log instead of a silent no-tools plugin.
fn resolve_entrypoint_command(
    entrypoint: &crate::plugin::PluginEntrypoint,
    plugin_dir: &std::path::Path,
    binary: Option<&crate::plugin::PluginBinary>,
) -> String {
    let declared_file = binary
        .and_then(|b| b.file.clone().or_else(|| b.member.clone()))
        .unwrap_or_default();
    let cmd = entrypoint.command.trim();

    let mut candidates: Vec<std::path::PathBuf> = Vec::new();
    if !cmd.is_empty() && (!cmd.contains('/') || cmd.starts_with("./")) {
        candidates.push(plugin_dir.join(cmd.trim_start_matches("./")));
    }
    if !declared_file.is_empty() {
        candidates.push(plugin_dir.join(&declared_file));
    }
    if let Some(found) = candidates.iter().find(|c| c.is_file()) {
        return found.to_string_lossy().to_string();
    }

    if !declared_file.is_empty() {
        let expected = plugin_dir.join(&declared_file);
        tracing::warn!(
            "MCP plugin '{}': binary not installed at {} - run install from the dashboard to download it",
            plugin_dir
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("?"),
            expected.display()
        );
        return expected.to_string_lossy().to_string();
    }

    cmd.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn real_plugin_manifests_declare_tool_behaviours() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let dir = |name: &str| format!("{}/plugins/tools/{}", root.display(), name);

        // docker manifest declares the raw tool name "compose"; the
        // descriptor must resolve to the registry name docker_compose so the
        // self-restart guard protects the stack the agent runs in.
        let docker = manifest_tool_behavior(&dir("docker"));
        let compose = crate::mcp::behavior::for_tool(&docker, "docker", "docker__compose");
        assert!(compose.affects_own_stack);

        let subtasks = manifest_tool_behavior(&dir("subtasks"));
        let manage =
            crate::mcp::behavior::for_tool(&subtasks, "subtasks", "subtasks__manage_subtasks");
        assert_eq!(manage.family.as_deref(), Some("subtasks"));

        let fs = manifest_tool_behavior(&dir("filesystem"));
        let read = crate::mcp::behavior::for_tool(&fs, "filesystem", "filesystem__read");
        assert!(read.read_only && read.repeat_guard_enabled());
        // No descriptor for the write tool: fail closed.
        assert!(!crate::mcp::behavior::for_tool(&fs, "filesystem", "filesystem__write").read_only);
    }

    #[test]
    fn test_resolve_env_vars_are_literal() {
        std::env::set_var("TEST_MCP_KEY", "secret-key-123");
        // ${VAR} is NEVER interpolated - it stays as a literal string.
        let resolved = resolve_env_vars("${TEST_MCP_KEY}");
        assert_eq!(resolved, "${TEST_MCP_KEY}");
    }

    #[test]
    fn test_resolve_env_vars_missing_stays_literal() {
        let resolved = resolve_env_vars("${NONEXISTENT_VAR}");
        assert_eq!(resolved, "${NONEXISTENT_VAR}");
    }

    #[test]
    fn test_resolve_env_vars_mixed_stays_literal() {
        std::env::set_var("MCP_HOST", "localhost");
        let resolved = resolve_env_vars("http://${MCP_HOST}:8080/mcp");
        assert_eq!(resolved, "http://${MCP_HOST}:8080/mcp");
    }

    #[test]
    fn test_default_config_values() {
        let config = McpServerConfig {
            tool_behavior: Default::default(),
            name: "test".to_string(),
            transport: McpTransport::Stdio,
            command: Some("echo".to_string()),
            args: vec![],
            url: None,
            env: HashMap::new(),
            current_dir: None,
            timeout_secs: None,
            max_retries: default_max_retries(),
            allowed_tools: default_allowed_tools(),
            pool_size: 1,
        };
        assert_eq!(config.timeout_secs, None);
        assert_eq!(config.max_retries, 3);
        assert_eq!(config.allowed_tools, vec!["*"]);
    }
}
