//! MCP tool for plugin management.
//!
//! Provides a unified tool interface for listing, installing, uninstalling,
//! enabling, disabling, and configuring plugins.
//!
//! Tool: plugin_manager
//! Parameters:
//!   action: "list" | "install" | "uninstall" | "enable" | "disable" | "config"
//!   name: string (required for all except list)
//!   url: string (required for install)
//!   config: object (required for config action)

use crate::mcp::{truncate_content, AppContext, McpTool, McpToolResult, DEFAULT_MAX_TOOL_OUTPUT_CHARS};
use crate::plugin;
use anyhow::Result;
use serde_json::Value;
use std::sync::Arc;

pub fn plugin_manager_tool() -> McpTool {
    McpTool {
        name: "plugin_manager".to_string(),
        description: "Manage plugins: list, install, uninstall, enable, disable, or update config. \
                      Use action='list' to see all plugins. \
                      Use action='install' with a url to install from a tarball/zip. \
                      Use action='uninstall' with a name to remove a plugin. \
                      Use action='enable' or 'disable' with a name to toggle plugin status. \
                      Use action='config' with a name and config object to update plugin settings."
            .to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "description": "Action to perform: list, install, uninstall, enable, disable, config",
                    "enum": ["list", "install", "uninstall", "enable", "disable", "config"]
                },
                "name": {
                    "type": "string",
                    "description": "Plugin name (required for all actions except list)"
                },
                "url": {
                    "type": "string",
                    "description": "Download URL for install action (.tar.gz, .tgz, or .zip)"
                },
                "config": {
                    "type": "object",
                    "description": "Configuration object for config action"
                }
            },
            "required": ["action"]
        }),
        handler: Arc::new(|args: Value, ctx: AppContext| -> Result<McpToolResult> {
            let action = args["action"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("Missing required argument: 'action'"))?;

            let pool = ctx.pool.clone();
            let data_dir = ctx.data_dir.clone();

            match action {
                "list" => {
                    tokio::task::block_in_place(|| {
                        let handle = tokio::runtime::Handle::current();
                        handle.block_on(async {
                            let rows = plugin::list_plugins(&pool).await
                                .map_err(|e| anyhow::anyhow!("Failed to list plugins: {}", e))?;

                            let details: Vec<plugin::PluginDetail> =
                                rows.iter().map(|r| plugin::enrich_plugin(r)).collect();

                            let output = serde_json::to_string_pretty(&details)?;
                            Ok(McpToolResult {
                                call_id: String::new(),
                                content: truncate_content(&output, DEFAULT_MAX_TOOL_OUTPUT_CHARS),
                                is_error: false,
                            })
                        })
                    })
                }

                "install" => {
                    let url = args["url"]
                        .as_str()
                        .ok_or_else(|| anyhow::anyhow!("Missing required argument for install: 'url'"))?;

                    let manifest = plugin::installer::install_from_url(url, &data_dir)
                        .map_err(|e| anyhow::anyhow!("Installation failed: {}", e))?;

                    let manifest_json = serde_json::to_value(&manifest)?;
                    let plugin_type_str = match manifest.plugin_type {
                        plugin::PluginType::Platform => "platform",
                        plugin::PluginType::Mcp => "mcp",
                    };

                    let row = tokio::task::block_in_place(|| {
                        let handle = tokio::runtime::Handle::current();
                        handle.block_on(async {
                            plugin::upsert_plugin(
                                &pool,
                                &manifest.name,
                                plugin_type_str,
                                &manifest.version,
                                Some(url),
                                &manifest_json,
                                &serde_json::json!({}),
                            )
                            .await
                            .map_err(|e| anyhow::anyhow!("Failed to register plugin in DB: {}", e))
                        })
                    })?;

                    let detail = plugin::enrich_plugin(&row);
                    let output = serde_json::to_string_pretty(&detail)?;

                    Ok(McpToolResult {
                        call_id: String::new(),
                        content: format!(
                            "Plugin '{}' version {} installed successfully.\n\n{}",
                            manifest.name, manifest.version, output
                        ),
                        is_error: false,
                    })
                }

                "uninstall" => {
                    let name = args["name"]
                        .as_str()
                        .ok_or_else(|| anyhow::anyhow!("Missing required argument for uninstall: 'name'"))?;

                    // Remove from database
                    let deleted = tokio::task::block_in_place(|| {
                        let handle = tokio::runtime::Handle::current();
                        handle.block_on(async {
                            plugin::delete_plugin(&pool, name).await
                                .map_err(|e| anyhow::anyhow!("Failed to delete plugin from DB: {}", e))
                        })
                    })?;

                    // Remove from disk
                    let disk_result = plugin::installer::uninstall(name, &data_dir);

                    let mut parts = vec![];
                    if deleted {
                        parts.push("Removed from registry".to_string());
                    } else {
                        parts.push("Not found in registry".to_string());
                    }
                    match disk_result {
                        Ok(_) => parts.push("Removed from disk".to_string()),
                        Err(e) => parts.push(format!("Disk removal note: {}", e)),
                    }

                    Ok(McpToolResult {
                        call_id: String::new(),
                        content: format!("Plugin '{}': {}", name, parts.join("; ")),
                        is_error: false,
                    })
                }

                "enable" => {
                    let name = args["name"]
                        .as_str()
                        .ok_or_else(|| anyhow::anyhow!("Missing required argument for enable: 'name'"))?;

                    let row = tokio::task::block_in_place(|| {
                        let handle = tokio::runtime::Handle::current();
                        handle.block_on(async {
                            plugin::update_plugin_status(&pool, name, "enabled").await
                                .map_err(|e| anyhow::anyhow!("Failed to enable plugin: {}", e))
                        })
                    })?;

                    Ok(McpToolResult {
                        call_id: String::new(),
                        content: format!("Plugin '{}' enabled (current status: {})", name, row.status),
                        is_error: false,
                    })
                }

                "disable" => {
                    let name = args["name"]
                        .as_str()
                        .ok_or_else(|| anyhow::anyhow!("Missing required argument for disable: 'name'"))?;

                    let row = tokio::task::block_in_place(|| {
                        let handle = tokio::runtime::Handle::current();
                        handle.block_on(async {
                            plugin::update_plugin_status(&pool, name, "disabled").await
                                .map_err(|e| anyhow::anyhow!("Failed to disable plugin: {}", e))
                        })
                    })?;

                    Ok(McpToolResult {
                        call_id: String::new(),
                        content: format!("Plugin '{}' disabled (current status: {})", name, row.status),
                        is_error: false,
                    })
                }

                "config" => {
                    let name = args["name"]
                        .as_str()
                        .ok_or_else(|| anyhow::anyhow!("Missing required argument for config: 'name'"))?;

                    let config = args.get("config")
                        .ok_or_else(|| anyhow::anyhow!("Missing required argument for config: 'config'"))?;

                    let row = tokio::task::block_in_place(|| {
                        let handle = tokio::runtime::Handle::current();
                        handle.block_on(async {
                            plugin::update_plugin_config(&pool, name, config).await
                                .map_err(|e| anyhow::anyhow!("Failed to update plugin config: {}", e))
                        })
                    })?;

                    let detail = plugin::enrich_plugin(&row);
                    let output = serde_json::to_string_pretty(&detail)?;

                    Ok(McpToolResult {
                        call_id: String::new(),
                        content: format!(
                            "Plugin '{}' config updated.\n\n{}",
                            name, output
                        ),
                        is_error: false,
                    })
                }

                _ => {
                    anyhow::bail!(
                        "Unknown action '{}'. Valid actions: list, install, uninstall, enable, disable, config",
                        action
                    );
                }
            }
        }),
    }
}
