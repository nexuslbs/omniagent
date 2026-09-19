//! mcp-server-plugin-manager: standalone MCP server for plugin management.
//! Communicates via stdio JSON-RPC (MCP protocol).
//!
//! Tool: plugin_manager
//! Parameters:
//!   action: "list" | "get" | "install" | "uninstall" | "enable" | "disable" | "config"
//!   name: string (required for all except list)
//!   url: string (required for install)
//!   config: object (required for config action)
//!
//! PARITY (2026-09-15, kanban-tool-parity task): this tool is a convenience
//! SUBSET of the plugin HTTP API (`src/server/plugins.rs`). Deliberate gaps
//! (audited; each remains reachable through `core__omniagent_api`
//! /api/plugins/... and the dashboard, so nothing is unreachable):
//!   - install-git (POST /api/plugins/install-git)
//!   - reinstall (POST /api/plugins/{type}/{source}/{name}/reinstall)
//!   - setup (POST /api/plugins/{type}/{source}/{name}/setup)
//!   - download (POST /api/plugins/{type}/{source}/{name}/download)
//!   - refresh-models (POST /api/plugins/{type}/{source}/{name}/refresh-models)
//!   - rename (POST /api/plugins/{type}/{source}/{name}/rename)
//!
//! These are administrative/dashboard lifecycle operations needing full
//! {type}/{source}/{name} addressing and long-running download/setup semantics;
//! exposing them as agent MCP actions would duplicate the API surface without
//! adding capability. The `get` action WAS added because reading one plugin's
//! detail (incl. kind/source) is a normal agent need.
//!
//! LIVE STATUS (2026-09-19): `list` and `get` return the SAME live view as the
//! HTTP API and the dashboard. The plugin config files alone cannot tell whether
//! an enabled tool plugin ACTUALLY started: an enabled plugin whose MCP server
//! failed to spawn reads `status=enabled` with an empty tool list, which is
//! exactly how the operator was misled into "the plugin is enabled but exposes
//! no tool". This tool therefore cross-references `GET {core_api}/api/plugins`,
//! whose `tool_names` / `status` / `status_message` are computed from the live
//! MCP registry by `apply_tool_runtime_status_all` (src/server/plugins_reload.rs):
//! a running plugin lists its tools, an enabled-but-not-running one reads
//! `status=error` with the truthful start-failure reason. When the core API is
//! unreachable both actions return the config view with an EXPLICIT
//! `status_message` saying the live status is unknown, so "enabled but no tool"
//! is never ambiguous. Reporting only: plugin start/load is untouched.

use anyhow::Result;
use mcp_server_util::*;
use omniagent::plugin;
use omniagent::plugins_yaml;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Arc, OnceLock};
use tokio::sync::RwLock;

// ---------------------------------------------------------------------------
// Live runtime status (mirrors the HTTP API / dashboard)
// ---------------------------------------------------------------------------

/// Core API base URL from the configure message (`base_url`). Set at most once;
/// when it is not configured the env/default resolution below is used.
static CONFIGURED_BASE_URL: OnceLock<String> = OnceLock::new();

/// Base URL of the core omniagent HTTP API, as addressed from this plugin
/// subprocess. Resolution: configure-message `base_url`, else `OMNIAGENT_API_URL`,
/// else loopback on `OMNIAGENT_PORT` (default 8080) - the loopback default is
/// correct because the plugin runs in the same container as the core.
fn core_api_base_url() -> String {
    core_api_base_url_from(
        CONFIGURED_BASE_URL.get().map(String::as_str),
        std::env::var("OMNIAGENT_API_URL").ok().as_deref(),
        std::env::var("OMNIAGENT_PORT").ok().as_deref(),
    )
}

/// Pure resolution logic behind [`core_api_base_url`] (unit-testable without
/// touching the process environment).
fn core_api_base_url_from(
    configured: Option<&str>,
    env_api_url: Option<&str>,
    env_port: Option<&str>,
) -> String {
    fn clean(value: &str) -> Option<String> {
        let trimmed = value.trim().trim_end_matches('/');
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    }

    if let Some(url) = configured.and_then(clean) {
        return url;
    }
    if let Some(url) = env_api_url.and_then(clean) {
        return url;
    }
    let port = env_port
        .and_then(|p| p.trim().parse::<u16>().ok())
        .unwrap_or(8080);
    format!("http://127.0.0.1:{}", port)
}

/// Live runtime view of one plugin, as computed by the core HTTP API.
#[derive(Debug, Clone, Default)]
struct LivePluginStatus {
    status: String,
    status_message: String,
    tool_names: Vec<String>,
    /// Source reported by the API (`yaml`/`built-in`/`remote`/...), used to
    /// line a live entry up with the matching YAML detail when a plugin name is
    /// configured by several sources.
    source: Option<String>,
    /// True when this live entry is a NON-primary source of a duplicated name.
    is_duplicated: bool,
}

/// Rank live entries carrying the same plugin name: an entry that is not
/// disabled and exposes more tools is the more informative one.
fn live_rank(s: &LivePluginStatus) -> (u8, usize) {
    ((s.status != "disabled") as u8, s.tool_names.len())
}

/// Pick the live entry matching a YAML detail: same name AND the same
/// source/duplication flag when the API reports one, else the most informative
/// entry of that name.
fn select_live<'a>(
    entries: &'a [LivePluginStatus],
    detail: &plugins_yaml::PluginDetail,
) -> Option<&'a LivePluginStatus> {
    entries
        .iter()
        .filter(|e| e.source == detail.source && e.is_duplicated == detail.is_duplicated)
        .max_by_key(|e| live_rank(e))
        .or_else(|| entries.iter().max_by_key(|e| live_rank(e)))
}

/// Fetch the live plugin listing from the core API.
///
/// ONLY `name`, `status`, `status_message` and `tool_names` are read: the API
/// response also carries `resolved_env` with `$secret:` references RESOLVED to
/// their values, and those must never be copied into an agent-visible result.
async fn fetch_live_status() -> Result<HashMap<String, Vec<LivePluginStatus>>, String> {
    let url = format!("{}/api/plugins", core_api_base_url());
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .map_err(|e| format!("HTTP client error: {e}"))?;
    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("{url}: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("{url}: HTTP {}", resp.status()));
    }
    let body: Value = resp
        .json()
        .await
        .map_err(|e| format!("{url}: invalid JSON: {e}"))?;
    let items = body
        .get("data")
        .and_then(|d| d.as_array())
        .ok_or_else(|| format!("{url}: unexpected response shape"))?;

    // Several entries may share a plugin name (duplicate sources), so group
    // them by name and pick the matching one per YAML detail later.
    let mut live: HashMap<String, Vec<LivePluginStatus>> = HashMap::new();
    for item in items {
        let Some(name) = item.get("name").and_then(|n| n.as_str()) else {
            continue;
        };
        let entry = LivePluginStatus {
            status: item
                .get("status")
                .and_then(|s| s.as_str())
                .unwrap_or_default()
                .to_string(),
            status_message: item
                .get("status_message")
                .and_then(|s| s.as_str())
                .unwrap_or_default()
                .to_string(),
            tool_names: item
                .get("tool_names")
                .and_then(|t| t.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default(),
            source: item
                .get("source")
                .and_then(|s| s.as_str())
                .map(String::from),
            is_duplicated: item
                .get("is_duplicated")
                .and_then(|d| d.as_bool())
                .unwrap_or(false),
        };
        live.entry(name.to_string()).or_default().push(entry);
    }
    Ok(live)
}

/// Overwrite the configuration view of `details` with the LIVE runtime view.
///
/// Mirrors `apply_tool_runtime_status_all` (src/server/plugins_reload.rs): the
/// core computes that in-process, and an out-of-process MCP server cannot read
/// the in-process registry, so this tool consumes the very same data through the
/// HTTP API the dashboard uses. When the live view is unavailable, tool plugins
/// say so explicitly instead of leaving an ambiguous ``enabled with no tools''.
async fn apply_live_status(details: &mut [plugins_yaml::PluginDetail]) {
    match fetch_live_status().await {
        Ok(live) => apply_live_entries(details, &live),
        Err(err) => apply_live_unavailable(details, &err),
    }
}

/// Merge the live view into the configuration view (pure).
fn apply_live_entries(
    details: &mut [plugins_yaml::PluginDetail],
    live: &HashMap<String, Vec<LivePluginStatus>>,
) {
    for detail in details.iter_mut() {
        match live
            .get(&detail.name)
            .and_then(|entries| select_live(entries, detail))
        {
            Some(l) => {
                detail.status = l.status.clone();
                detail.status_message = l.status_message.clone();
                detail.tool_names = l.tool_names.clone();
            }
            None => {
                if detail.plugin_type == "tool" && detail.status == "enabled" {
                    detail.status_message =
                        "live runtime status unknown: the plugin is absent from the core live plugin listing"
                            .to_string();
                }
            }
        }
    }
}

/// The live view could not be read: keep the configuration view but make the
/// uncertainty EXPLICIT for tool plugins (pure).
fn apply_live_unavailable(details: &mut [plugins_yaml::PluginDetail], err: &str) {
    for detail in details.iter_mut() {
        if detail.plugin_type == "tool" && detail.status == "enabled" {
            detail.status_message = format!(
                "live runtime status unavailable ({err}); the status shown is the CONFIGURATION state, so an empty tool list is NOT proof that the plugin is not running"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Tool: plugin_manager: list
// ---------------------------------------------------------------------------

/// `list` returns every plugin with the LIVE runtime status merged in (same
/// view as the HTTP API / dashboard): tool_names for a running tool plugin,
/// status=error plus the start-failure reason for an enabled one that did not
/// start.
async fn handle_list(data_dir: &str, _args: &Value) -> Result<(String, bool)> {
    let mut details = plugins_yaml::list_plugins(data_dir)
        .map_err(|e| anyhow::anyhow!("Failed to list plugins: {:#}", e))?;

    apply_live_status(&mut details).await;

    let output = serde_json::to_string_pretty(&details)?;
    Ok((output, false))
}

// ---------------------------------------------------------------------------
// Tool: plugin_manager: install
// ---------------------------------------------------------------------------

async fn handle_install(data_dir: &str, args: &Value) -> Result<(String, bool)> {
    let url = args["url"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing required argument for install: 'url'"))?;

    let manifest = plugin::installer::install_from_url(url, data_dir)
        .await
        .map_err(|e| anyhow::anyhow!("Installation failed: {:#}", e))?;

    // Register in YAML state
    let yaml_type = plugins_yaml::PluginYamlType::from_plugin_type(&manifest.plugin_type);
    plugins_yaml::set_entry(
        data_dir,
        &yaml_type,
        &manifest.name,
        true,
        serde_json::json!({}),
    )
    .map_err(|e| anyhow::anyhow!("Failed to register plugin in YAML: {:#}", e))?;

    Ok((
        format!("Plugin '{}' installed successfully.", manifest.name),
        false,
    ))
}

// ---------------------------------------------------------------------------
// Tool: plugin_manager: uninstall
// ---------------------------------------------------------------------------

async fn handle_uninstall(data_dir: &str, args: &Value) -> Result<(String, bool)> {
    let name = args["name"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing required argument: 'name'"))?;

    // Remove from YAML files (all types)
    let mut deleted = false;
    for yaml_type in &[
        plugins_yaml::PluginYamlType::Platform,
        plugins_yaml::PluginYamlType::Tool,
        plugins_yaml::PluginYamlType::Provider,
    ] {
        if let Ok(true) = plugins_yaml::remove_entry(data_dir, yaml_type, name) {
            deleted = true;
        }
    }

    // Remove from disk: detect type to pass correct arguments
    let is_remote = plugins_yaml::get_disk_plugin_type(data_dir, name)
        .ok()
        .flatten()
        .map(|t| {
            let yaml_type = plugins_yaml::PluginYamlType::from_type_str(&t);
            plugins_yaml::get_entry(data_dir, &yaml_type, name)
                .ok()
                .flatten()
                .and_then(|e| if e.source == "remote" { Some(()) } else { None })
                .is_some()
        })
        .unwrap_or(false);
    let type_dir = plugins_yaml::get_disk_plugin_type(data_dir, name)
        .ok()
        .flatten()
        .unwrap_or_else(|| "mcp".to_string());
    let _ = plugin::installer::uninstall(name, data_dir, &type_dir, is_remote);

    if deleted {
        Ok((
            format!("Plugin '{}' uninstalled successfully.", name),
            false,
        ))
    } else {
        Ok((format!("Plugin '{}' not found.", name), false))
    }
}

// ---------------------------------------------------------------------------
// Tool: plugin_manager: enable
// ---------------------------------------------------------------------------

async fn handle_enable(data_dir: &str, args: &Value) -> Result<(String, bool)> {
    let name = args["name"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing required argument: 'name'"))?;

    let yaml_type = match plugins_yaml::get_disk_plugin_type(data_dir, name) {
        Ok(Some(t)) => plugins_yaml::PluginYamlType::from_type_str(&t),
        _ => return Ok((format!("Plugin '{}' not found.", name), false)),
    };

    plugins_yaml::set_enabled(data_dir, &yaml_type, name, true)
        .map_err(|e| anyhow::anyhow!("Failed to enable plugin: {:#}", e))?;

    Ok((format!("Plugin '{}' enabled.", name), false))
}

// ---------------------------------------------------------------------------
// Tool: plugin_manager: disable
// ---------------------------------------------------------------------------

async fn handle_disable(data_dir: &str, args: &Value) -> Result<(String, bool)> {
    let name = args["name"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing required argument: 'name'"))?;

    let yaml_type = match plugins_yaml::get_disk_plugin_type(data_dir, name) {
        Ok(Some(t)) => plugins_yaml::PluginYamlType::from_type_str(&t),
        _ => return Ok((format!("Plugin '{}' not found.", name), false)),
    };

    plugins_yaml::set_enabled(data_dir, &yaml_type, name, false)
        .map_err(|e| anyhow::anyhow!("Failed to disable plugin: {:#}", e))?;

    Ok((format!("Plugin '{}' disabled.", name), false))
}

// ---------------------------------------------------------------------------
// Tool: plugin_manager: config
// ---------------------------------------------------------------------------

async fn handle_config(data_dir: &str, args: &Value) -> Result<(String, bool)> {
    let name = args["name"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing required argument: 'name'"))?;
    let config = args
        .get("config")
        .ok_or_else(|| anyhow::anyhow!("Missing required argument: 'config'"))?;

    let yaml_type = match plugins_yaml::get_disk_plugin_type(data_dir, name) {
        Ok(Some(t)) => plugins_yaml::PluginYamlType::from_type_str(&t),
        _ => return Ok((format!("Plugin '{}' not found.", name), false)),
    };

    plugins_yaml::update_config(data_dir, &yaml_type, name, config.clone())
        .map_err(|e| anyhow::anyhow!("Failed to update plugin config: {:#}", e))?;

    // Return the updated plugin detail
    match plugins_yaml::get_plugin(data_dir, name, &yaml_type) {
        Ok(Some(detail)) => Ok((
            format!(
                "Plugin '{}' config updated. Current config: {}",
                detail.name,
                serde_json::to_string_pretty(&detail.config)?
            ),
            false,
        )),
        Ok(None) => Ok((format!("Plugin '{}' not found.", name), false)),
        Err(e) => Ok((format!("Failed to read plugin after update: {:#}", e), true)),
    }
}

// ---------------------------------------------------------------------------
// Tool: plugin_manager: get
// ---------------------------------------------------------------------------

/// Single-plugin detail (incl. kind/source, enabled state and config).
async fn handle_get(data_dir: &str, args: &Value) -> Result<(String, bool)> {
    let name = args["name"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing required argument: 'name'"))?;

    let yaml_type = match plugins_yaml::get_disk_plugin_type(data_dir, name) {
        Ok(Some(t)) => plugins_yaml::PluginYamlType::from_type_str(&t),
        _ => return Ok((format!("Plugin '{}' not found.", name), false)),
    };

    match plugins_yaml::get_plugin(data_dir, name, &yaml_type) {
        Ok(Some(mut detail)) => {
            apply_live_status(std::slice::from_mut(&mut detail)).await;
            Ok((serde_json::to_string_pretty(&detail)?, false))
        }
        Ok(None) => Ok((format!("Plugin '{}' not found.", name), false)),
        Err(e) => Ok((format!("Failed to read plugin: {:#}", e), true)),
    }
}

// ---------------------------------------------------------------------------
// Tool: plugin_manager: main dispatch
// ---------------------------------------------------------------------------

async fn handle_plugin_manager(data_dir: &str, args: &Value) -> Result<(String, bool)> {
    let action = args["action"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing required argument: 'action'"))?;

    match action {
        "list" => handle_list(data_dir, args).await,
        "get" => handle_get(data_dir, args).await,
        "install" => handle_install(data_dir, args).await,
        "uninstall" => handle_uninstall(data_dir, args).await,
        "enable" => handle_enable(data_dir, args).await,
        "disable" => handle_disable(data_dir, args).await,
        "config" => handle_config(data_dir, args).await,
        _ => Ok((
            format!(
                "Unknown action '{}'. Valid actions: list, get, install, uninstall, enable, disable, config. (install-git, reinstall, setup, download, refresh-models and rename are administrative API/dashboard operations: use core__omniagent_api /api/plugins/...)",
                action
            ),
            true,
        )),
    }
}

// ---------------------------------------------------------------------------
// Plugin config hook
// ---------------------------------------------------------------------------

/// Callback invoked when the host sends configuration via configure message.
/// Plugin config - received via configure message.
#[derive(Debug, Clone)]
struct PluginConfig {
    pub omni_dir: String,
    /// Optional core API base URL override (configure message `base_url`).
    pub base_url: Option<String>,
}

impl PluginConfig {
    fn from_json(v: &serde_json::Value) -> Self {
        Self {
            omni_dir: v
                .get("omni_dir")
                .and_then(|v| v.as_str())
                .map(String::from)
                .unwrap_or_else(|| "/opt/omni".to_string()),
            base_url: v.get("base_url").and_then(|v| v.as_str()).map(String::from),
        }
    }
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<()> {
    // Shared data_dir - populated by configure callback before any tool call
    let data_dir: Arc<RwLock<Option<String>>> = Arc::new(RwLock::new(None));

    let dd = data_dir.clone();
    let handler: ToolHandler = Box::new(move |args: Value, _meta: Option<McpMeta>| {
        let dd_inner = dd.clone();
        Box::pin(async move {
            let guard = dd_inner.read().await;
            let data_dir = match guard.as_ref() {
                Some(d) => d.clone(),
                None => return Ok(("Plugin manager not configured: omni_dir not set. The plugin needs an omni_dir in its config.".to_string(), true)),
            };
            handle_plugin_manager(&data_dir, &args).await
        })
    });

    let tools = vec![McpToolEntry {
        def: McpToolDef {
            name: "plugin_manager".to_string(),
            description: "Manage plugins: list, get, install, uninstall, enable, disable, or configure. list/get report the LIVE runtime status (the same view as the dashboard and the plugin HTTP API): a running tool plugin lists its tool names, an enabled plugin whose MCP server did not start reads status=error with the real failure reason, and when the live view cannot be reached the status_message says so explicitly. This tool covers the common lifecycle actions; the administrative ones (install-git, reinstall, setup, download, refresh-models, rename) stay on the HTTP API (/api/plugins/..., reachable via core__omniagent_api) and the dashboard."
                .to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["list", "get", "install", "uninstall", "enable", "disable", "config"],
                        "description": "Action to perform"
                    },
                    "name": {
                        "type": "string",
                        "description": "Plugin name (required for all except list; resolved across plugin kinds by name)"
                    },
                    "url": {
                        "type": "string",
                        "description": "Plugin URL (required for install)"
                    },
                    "config": {
                        "type": "object",
                        "description": "Config object (required for config action); values may use $env:VAR and $secret:NAME references and boolean/number types"
                    }
                },
                "required": ["action"]
            }),
        },
        handler,
    }];

    let server_info = ServerInfo {
        name: "mcp-server-plugin-manager".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
    };

    run_server_with_config(server_info, tools, {
        let dd = data_dir.clone();
        Some(move |params: serde_json::Value| {
            let config = PluginConfig::from_json(&params);
            if let Some(base_url) = config
                .base_url
                .as_deref()
                .map(str::trim)
                .filter(|u| !u.is_empty())
            {
                let _ = CONFIGURED_BASE_URL.set(base_url.trim_end_matches('/').to_string());
                tracing::info!("Plugin-manager core API base URL: {base_url}");
            }
            if !config.omni_dir.is_empty() {
                tokio::task::block_in_place(|| {
                    *dd.blocking_write() = Some(config.omni_dir.clone());
                });
                tracing::info!("Plugin-manager configured with omni_dir");
            } else {
                tracing::warn!("Plugin-manager configure called without omni_dir");
            }
        })
    })
    .await
}
// ---------------------------------------------------------------------------
// Tests: live-status resolution and merging (pure helpers)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn detail(name: &str, status: &str) -> plugins_yaml::PluginDetail {
        serde_json::from_value(json!({
            "id": 1,
            "name": name,
            "plugin_type": "tool",
            "version": "0.1.0",
            "status": status,
            "manifest": {},
            "config": {},
            "config_schema": [],
            "created_at": "",
            "updated_at": ""
        }))
        .expect("PluginDetail fixture")
    }

    fn live(status: &str, status_message: &str, tools: &[&str]) -> LivePluginStatus {
        live_with_source(status, status_message, tools, None, false)
    }

    fn live_with_source(
        status: &str,
        status_message: &str,
        tools: &[&str],
        source: Option<&str>,
        is_duplicated: bool,
    ) -> LivePluginStatus {
        LivePluginStatus {
            status: status.to_string(),
            status_message: status_message.to_string(),
            tool_names: tools.iter().map(|t| t.to_string()).collect(),
            source: source.map(String::from),
            is_duplicated,
        }
    }

    #[test]
    fn base_url_prefers_configured_then_env_then_loopback() {
        assert_eq!(
            core_api_base_url_from(Some("http://core:9000/"), None, None),
            "http://core:9000"
        );
        assert_eq!(
            core_api_base_url_from(None, Some(" http://api:1234/ "), Some("1")),
            "http://api:1234"
        );
        assert_eq!(
            core_api_base_url_from(None, None, Some("8123")),
            "http://127.0.0.1:8123"
        );
        // Blank values fall through to the loopback default.
        assert_eq!(
            core_api_base_url_from(Some("   "), Some(""), None),
            "http://127.0.0.1:8080"
        );
    }

    #[test]
    fn live_entry_with_more_tools_outranks_duplicate() {
        let running = live("enabled", "", &["workbench__tool"]);
        let disabled = live("disabled", "", &[]);
        assert!(live_rank(&running) > live_rank(&disabled));
    }

    #[test]
    fn enabled_but_not_running_reads_error_with_reason() {
        let mut details = vec![detail("workbench", "enabled")];
        let mut live_map = HashMap::new();
        live_map.insert(
            "workbench".to_string(),
            vec![live(
                "error",
                "MCP server failed to start: no startable MCP server config found for 'workbench'",
                &[],
            )],
        );
        apply_live_entries(&mut details, &live_map);
        assert_eq!(details[0].status, "error");
        assert!(details[0]
            .status_message
            .starts_with("MCP server failed to start:"));
        assert!(details[0].tool_names.is_empty());
    }

    #[test]
    fn running_plugin_lists_its_live_tools() {
        let mut details = vec![detail("workbench", "enabled")];
        let mut live_map = HashMap::new();
        live_map.insert(
            "workbench".to_string(),
            vec![live("enabled", "", &["workbench__tool"])],
        );
        apply_live_entries(&mut details, &live_map);
        assert_eq!(details[0].status, "enabled");
        assert_eq!(details[0].tool_names, vec!["workbench__tool".to_string()]);
    }

    #[test]
    fn duplicate_name_picks_the_entry_from_the_same_source() {
        // Two live entries share the name: the YAML detail from the 'remote'
        // source must get the remote entry (disabled), not the enabled one.
        let mut d = detail("prompt", "enabled");
        d.source = Some("remote".to_string());
        d.is_duplicated = true;
        let mut details = vec![d];
        let mut live_map = HashMap::new();
        live_map.insert(
            "prompt".to_string(),
            vec![
                live_with_source(
                    "enabled",
                    "",
                    &["prompt__generate"],
                    Some("built-in"),
                    false,
                ),
                live_with_source("disabled", "", &[], Some("remote"), true),
            ],
        );
        apply_live_entries(&mut details, &live_map);
        assert_eq!(details[0].status, "disabled");
        assert!(details[0].tool_names.is_empty());
    }

    #[test]
    fn unavailable_live_view_is_explicit_not_ambiguous() {
        let mut details = vec![detail("workbench", "enabled")];
        apply_live_unavailable(&mut details, "connection refused");
        assert_eq!(details[0].status, "enabled");
        assert!(details[0]
            .status_message
            .contains("live runtime status unavailable (connection refused)"));
        assert!(details[0].status_message.contains("NOT proof"));
    }

    #[test]
    fn unavailable_live_view_leaves_non_tool_plugins_untouched() {
        let mut p = detail("mattermost", "enabled");
        p.plugin_type = "platform".to_string();
        let mut details = vec![p];
        apply_live_unavailable(&mut details, "connection refused");
        assert!(details[0].status_message.is_empty());
    }
}
