//! mcp-server-plugin-manager: standalone MCP server for plugin management.
//! Communicates via stdio JSON-RPC (MCP protocol).
//!
//! Tool: plugin_manager
//! Parameters:
//!   action: "list" | "install" | "uninstall" | "enable" | "disable" | "config"
//!   name: string (required for all except list)
//!   url: string (required for install)
//!   config: object (required for config action)

use anyhow::Result;
use std::sync::Arc;
use tokio::sync::RwLock;
use mcp_server_util::*;
use omniagent::plugin;
use omniagent::plugins_yaml;
use serde_json::Value;

// ---------------------------------------------------------------------------
// Environment helpers
// ---------------------------------------------------------------------------

/// Read DATA_DIR from env with a default fallback.
fn data_dir() -> String {
    std::env::var("DATA_DIR")
        .or_else(|_| std::env::var("HOME").map(|h| format!("{}/.omniagent", h)))
        .unwrap_or_else(|_| {
            eprintln!("FATAL: OMNI_DIR must be set");
            std::process::exit(1);
        })
}

// ---------------------------------------------------------------------------
// Tool: plugin_manager: list
// ---------------------------------------------------------------------------

async fn handle_list(data_dir: &str, _args: &Value) -> Result<(String, bool)> {
    let details = plugins_yaml::list_plugins(data_dir)
        .map_err(|e| anyhow::anyhow!("Failed to list plugins: {:#}", e))?;

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
    match plugins_yaml::get_plugin(data_dir, name) {
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
// Tool: plugin_manager: main dispatch
// ---------------------------------------------------------------------------

async fn handle_plugin_manager(data_dir: &str, args: &Value) -> Result<(String, bool)> {
    let action = args["action"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing required argument: 'action'"))?;

    match action {
        "list" => handle_list(data_dir, args).await,
        "install" => handle_install(data_dir, args).await,
        "uninstall" => handle_uninstall(data_dir, args).await,
        "enable" => handle_enable(data_dir, args).await,
        "disable" => handle_disable(data_dir, args).await,
        "config" => handle_config(data_dir, args).await,
        _ => Ok((
            format!(
                "Unknown action '{}'. Valid actions: list, install, uninstall, enable, disable, config",
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
/// Plugin config — received via configure message.
#[derive(Debug, Clone)]
struct PluginConfig {
    pub omni_dir: String,
}

impl PluginConfig {
    fn from_json(v: &serde_json::Value) -> Self {
        Self {
            omni_dir: v.get("omni_dir")
                .and_then(|v| v.as_str())
                .map(String::from)
                .unwrap_or_else(|| std::env::var("HOME").map(|h| format!("{}/.omniagent", h)).unwrap_or_default()),
        }
    }
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<()> {
    // Shared data_dir — populated by configure callback before any tool call
    let data_dir: Arc<RwLock<Option<String>>> = Arc::new(RwLock::new(None));

    let dd = data_dir.clone();
    let handler: ToolHandler = Box::new(move |args: Value, _meta: Option<McpMeta>| {
        let dd_inner = dd.clone();
        Box::pin(async move {
            let guard = dd_inner.read().await;
            let data_dir = guard.as_ref().expect("data_dir not initialized").clone();
            handle_plugin_manager(&data_dir, &args).await
        })
    });

    let tools = vec![McpToolEntry {
        def: McpToolDef {
            name: "plugin_manager".to_string(),
            description: "Manage plugins: list, install, uninstall, enable, disable, or configure."
                .to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["list", "install", "uninstall", "enable", "disable", "config"],
                        "description": "Action to perform"
                    },
                    "name": {
                        "type": "string",
                        "description": "Plugin name (required for all except list)"
                    },
                    "url": {
                        "type": "string",
                        "description": "Plugin URL (required for install)"
                    },
                    "config": {
                        "type": "object",
                        "description": "Config object (required for config action)"
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
            tokio::task::block_in_place(|| {
                *dd.blocking_write() = Some(config.omni_dir.clone());
            });
            tracing::info!("Plugin-manager configured with omni_dir");
        })
    }).await
}
