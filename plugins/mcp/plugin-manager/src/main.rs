//! mcp-server-plugin-manager — standalone MCP server for plugin management.
//! Communicates via stdio JSON-RPC (MCP protocol).
//!
//! Tool: plugin_manager
//! Parameters:
//!   action: "list" | "install" | "uninstall" | "enable" | "disable" | "config"
//!   name: string (required for all except list)
//!   url: string (required for install)
//!   config: object (required for config action)

use anyhow::{Context, Result};
use mcp_server_util::*;
use omniagent::plugin;
use serde_json::Value;
use sqlx::PgPool;
use std::sync::Arc;

// ---------------------------------------------------------------------------
// Environment helpers
// ---------------------------------------------------------------------------

/// Read DATA_DIR from env with a default fallback.
fn data_dir() -> String {
    std::env::var("DATA_DIR")
        .or_else(|_| std::env::var("HOME").map(|h| format!("{}/.omniagent", h)))
        .unwrap_or_else(|_| "/opt/data".to_string())
}

// ---------------------------------------------------------------------------
// Tool: plugin_manager — list
// ---------------------------------------------------------------------------

async fn handle_list(pool: &PgPool, _args: &Value) -> Result<(String, bool)> {
    let rows = plugin::list_plugins(pool)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to list plugins: {}", e))?;

    let details: Vec<plugin::PluginDetail> =
        rows.iter().map(|r| plugin::enrich_plugin(r)).collect();

    let output = serde_json::to_string_pretty(&details)?;
    Ok((output, false))
}

// ---------------------------------------------------------------------------
// Tool: plugin_manager — install
// ---------------------------------------------------------------------------

async fn handle_install(pool: &PgPool, args: &Value) -> Result<(String, bool)> {
    let url = args["url"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing required argument for install: 'url'"))?;

    let dir = data_dir();
    let manifest = plugin::installer::install_from_url(url, &dir)
        .map_err(|e| anyhow::anyhow!("Installation failed: {}", e))?;

    let manifest_json = serde_json::to_value(&manifest)?;
    let plugin_type_str = match manifest.plugin_type {
        plugin::PluginType::Platform => "platform",
        plugin::PluginType::Mcp => "mcp",
        plugin::PluginType::Provider => "provider",
    };

    let row = plugin::upsert_plugin(
        pool,
        plugin::UpsertPluginParams {
            name: &manifest.name,
            plugin_type: plugin_type_str,
            version: &manifest.version,
            source: Some(url),
            manifest: &manifest_json,
            config: &serde_json::json!({}),
        },
    )
    .await
    .map_err(|e| anyhow::anyhow!("Failed to register plugin in DB: {}", e))?;

    Ok((
        format!("Plugin '{}' installed successfully (id: {})", manifest.name, row.id),
        false,
    ))
}

// ---------------------------------------------------------------------------
// Tool: plugin_manager — uninstall
// ---------------------------------------------------------------------------

async fn handle_uninstall(pool: &PgPool, args: &Value) -> Result<(String, bool)> {
    let name = args["name"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing required argument: 'name'"))?;

    let deleted = plugin::delete_plugin(pool, name)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to delete plugin: {}", e))?;

    if deleted {
        Ok((format!("Plugin '{}' uninstalled successfully.", name), false))
    } else {
        Ok((format!("Plugin '{}' not found.", name), false))
    }
}

// ---------------------------------------------------------------------------
// Tool: plugin_manager — enable
// ---------------------------------------------------------------------------

async fn handle_enable(pool: &PgPool, args: &Value) -> Result<(String, bool)> {
    let name = args["name"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing required argument: 'name'"))?;

    let row = plugin::update_plugin_status(pool, name, true)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to enable plugin: {}", e))?;

    if let Some(r) = row {
        Ok((format!("Plugin '{}' enabled (id: {}).", r.name, r.id), false))
    } else {
        Ok((format!("Plugin '{}' not found.", name), false))
    }
}

// ---------------------------------------------------------------------------
// Tool: plugin_manager — disable
// ---------------------------------------------------------------------------

async fn handle_disable(pool: &PgPool, args: &Value) -> Result<(String, bool)> {
    let name = args["name"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing required argument: 'name'"))?;

    let row = plugin::update_plugin_status(pool, name, false)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to disable plugin: {}", e))?;

    if let Some(r) = row {
        Ok((format!("Plugin '{}' disabled (id: {}).", r.name, r.id), false))
    } else {
        Ok((format!("Plugin '{}' not found.", name), false))
    }
}

// ---------------------------------------------------------------------------
// Tool: plugin_manager — config
// ---------------------------------------------------------------------------

async fn handle_config(pool: &PgPool, args: &Value) -> Result<(String, bool)> {
    let name = args["name"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing required argument: 'name'"))?;
    let config = args.get("config")
        .ok_or_else(|| anyhow::anyhow!("Missing required argument: 'config'"))?;

    let row = plugin::update_plugin_config(pool, name, config)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to update plugin config: {}", e))?;

    if let Some(r) = row {
        Ok((
            format!(
                "Plugin '{}' config updated (id: {}). Current config: {}",
                r.name,
                r.id,
                serde_json::to_string_pretty(&r.config)?
            ),
            false,
        ))
    } else {
        Ok((format!("Plugin '{}' not found.", name), false))
    }
}

// ---------------------------------------------------------------------------
// Tool: plugin_manager — main dispatch
// ---------------------------------------------------------------------------

async fn handle_plugin_manager(pool: &PgPool, args: &Value) -> Result<(String, bool)> {
    let action = args["action"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing required argument: 'action'"))?;

    match action {
        "list" => handle_list(pool, args).await,
        "install" => handle_install(pool, args).await,
        "uninstall" => handle_uninstall(pool, args).await,
        "enable" => handle_enable(pool, args).await,
        "disable" => handle_disable(pool, args).await,
        "config" => handle_config(pool, args).await,
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
// Main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<()> {
    let pool = plugin::create_pool()
        .await
        .context("Failed to create database pool")?;
    let pool = Arc::new(pool);

    let p = pool.clone();

    let handler: ToolHandler = Box::new(move |args: Value| {
        let p_inner = p.clone();
        Box::pin(async move { handle_plugin_manager(&p_inner, &args).await })
    });

    let tools = vec![McpToolEntry {
        def: McpToolDef {
            name: "plugin_manager".to_string(),
            description: "Manage plugins: list, install, uninstall, enable, disable, or configure.".to_string(),
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

    run_server(server_info, tools).await
}
