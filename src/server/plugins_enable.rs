//! Plugin enable/disable/restart handlers.
//!
//! Extracted from `plugins.rs` for separation of concerns.

use axum::{
    extract::{Path, State},
    http::{StatusCode, Response},
    response::IntoResponse,
    body::Body,
    Json,
};
use std::sync::Arc;
use tracing::{error, info};

use crate::plugins_yaml;
use crate::server::AppState;

use super::plugins_reload::*;
use super::plugins_types::*;

pub(crate) async fn enable_plugin_handler(
    Path(name): Path<String>,
    State(state): State<Arc<AppState>>,
    Json(req): Json<PluginSourceRequest>,
) -> impl IntoResponse {
    let yaml_type = match plugins_yaml::get_disk_plugin_type(&state.data_dir, &name) {
        Ok(Some(t)) => plugins_yaml::PluginYamlType::from_type_str(&t),
        _ => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({
                    "success": false,
                    "error": "Plugin not found"
                })),
            )
                .into_response();
        }
    };

    // Require and validate source parameter
    let source = match require_source(&req.source) {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    match source {
        "built-in" | "bundled" | "remote" => {}
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "success": false,
                    "error": format!("Invalid source '{}': must be 'built-in', 'bundled', or 'remote'", source)
                })),
            )
                .into_response();
        }
    };

    // Check if plugin already in the desired state: skip only if source matches
    if let Ok(Some(entry)) = plugins_yaml::get_entry(&state.data_dir, &yaml_type, &name) {
        if entry.enabled && entry.source == source {
            // Already enabled with matching source: still reload subprocess
            // so the plugin picks up any config/environment changes or re-spawns
            // a crashed MCP server process.
            if yaml_type == plugins_yaml::PluginYamlType::Platform {
                reload_platform_plugin(&state, &name).await;
            } else if yaml_type == plugins_yaml::PluginYamlType::Tool {
                // Re-initialize the MCP server (handles re-spawn after crash)
                crate::mcp::external::client::clear_server_pools(&name);
                crate::mcp::external::client::remove_server_config(&name);
                match crate::mcp::external::client::initialize_single_server_tools(
                    &state.data_dir,
                    &name,
                )
                .await
                {
                    Ok(tools) => {
                        let count = tools.len();
                        state.tool_registry.write().await.remove_by_server(&name);
                        state.tool_registry.write().await.register_all(tools);
                        tracing::info!(
                            "Hot-reloaded {} tool(s) from MCP server '{}' on re-enable",
                            count, name
                        );
                    }
                    Err(e) => {
                        tracing::warn!(
                            "Hot-reload of MCP server '{}' on re-enable failed: {}",
                            name, e
                        );
                    }
                }
            }

            if let Ok(Some(detail)) = plugins_yaml::get_plugin(&state.data_dir, &name) {
                info!(
                    "Plugin '{}' is already enabled with matching source: no change needed",
                    name
                );
                return (
                    StatusCode::OK,
                    Json(serde_json::json!({
                        "success": true,
                        "data": detail
                    })),
                )
                    .into_response();
            }
        }
    }

    // Look up remote info from remote.yml (needed when re-enabling remote source)
    let existing_remote = plugins_yaml::get_remote_plugin(&state.data_dir, &yaml_type, &name);

    // Upsert with enabled=true and explicit source
    // Do this FIRST so the YAML is written before server initialization.
    // If the server fails to start, we'll roll back by deleting the YAML entry.
    match plugins_yaml::set_entry_with_source(
        &state.data_dir,
        &yaml_type,
        &name,
        true,
        &source,
        serde_json::json!({}),
    ) {
        Ok(_entry) => {
            // Save remote info to remote.yml when enabling remote source
            if source == "remote" {
                let remote_to_set = req.remote.as_ref().or(existing_remote.as_ref());
                if let Some(remote) = remote_to_set {
                    if let Err(e) = plugins_yaml::save_remote_plugin(
                        &state.data_dir,
                        &yaml_type,
                        &name,
                        remote,
                    ) {
                        tracing::warn!("[plugins] Failed to save remote info for '{}': {:?}", name, e);
                    }
                }
            }
            // Hot-reload: if this is an MCP tool plugin, initialize the server
            // and register its tools in the shared registry immediately.
            if yaml_type == plugins_yaml::PluginYamlType::Tool {
                match crate::mcp::external::client::initialize_single_server_tools(
                    &state.data_dir,
                    &name,
                )
                .await
                {
                    Ok(tools) => {
                        let count = tools.len();
                        state.tool_registry.write().await.register_all(tools);
                        info!(
                            "Hot-reloaded {} tool(s) from MCP server '{}' (no restart needed)",
                            count, name
                        );
                    }
                    Err(e) => {
                        // MCP server failed to start: roll back the YAML enable
                        // so the plugin doesn't show as "enabled" when it can't serve tools.
                        tracing::warn!(
                            "Hot-reload of MCP server '{}' failed, rolling back enable: {}",
                            name,
                            e
                        );
                        if let Err(e) = plugins_yaml::remove_entry(
                            &state.data_dir,
                            &yaml_type,
                            &name,
                        ) {
                            tracing::error!("[plugins] Failed to roll back YAML entry for '{}': {:?}", name, e);
                        }
                        return (
                            StatusCode::BAD_REQUEST,
                            Json(serde_json::json!({
                                "success": false,
                                "error": format!(
                                    "MCP server for '{}' failed to start. {}",
                                    name, e
                                )
                            })),
                        )
                            .into_response();
                    }
                }
            }

            // Hot-reload: if this is a platform plugin, trigger subprocess restart
            if yaml_type == plugins_yaml::PluginYamlType::Platform {
                reload_platform_plugin(&state, &name).await;
            }

            // Hot-reload: if this is a provider plugin with an entrypoint, start the subprocess
            if yaml_type == plugins_yaml::PluginYamlType::Provider {
                if let Ok(Some(detail)) = plugins_yaml::get_plugin(&state.data_dir, &name) {
                    // Extract entrypoint from the raw JSON manifest (it's serde_json::Value, not PluginManifest)
                    let entrypoint = detail.manifest.get("entrypoint").and_then(|ep| {
                        let command = ep.get("command").and_then(|c| c.as_str())?;
                        let args: Vec<String> = ep.get("args")
                            .and_then(|a| a.as_array())
                            .map(|arr| arr.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                            .unwrap_or_default();
                        Some((command.to_string(), args))
                    });
                    if let Some((command, args)) = entrypoint {
                        info!(
                            "Starting external provider subprocess '{}' ({} {})",
                            name, command, args.join(" ")
                        );
                        // Register the provider (no async, just inserts into the map)
                        {
                            let mut registry = crate::provider::registry::PROVIDER_REGISTRY.write().expect("PROVIDER_REGISTRY lock poisoned");
                            registry.register(&name, &command, &args);
                        }
                        // Start the subprocess (async: drop registry lock first to avoid Send issues)
                        let start_result = {
                            let registry = crate::provider::registry::PROVIDER_REGISTRY.read().expect("PROVIDER_REGISTRY lock poisoned");
                            registry.get_cloned(&name)
                        };
                        // Registry guard dropped: we have an independent Arc
                        match start_result {
                            Some(client) => {
                                if let Err(e) = client.start().await {
                                    tracing::warn!(
                                        "Failed to start external provider '{}', rolling back enable: {}",
                                        name, e
                                    );
                                    {
                                        let mut registry = crate::provider::registry::PROVIDER_REGISTRY.write().expect("PROVIDER_REGISTRY lock poisoned");
                                        registry.remove(&name);
                                    }
                                    if let Err(e) = plugins_yaml::remove_entry(&state.data_dir, &yaml_type, &name) {
                                        tracing::error!("[plugins] Failed to roll back YAML for provider '{}': {:?}", name, e);
                                    }
                                    return (
                                        StatusCode::BAD_REQUEST,
                                        Json(serde_json::json!({
                                            "success": false,
                                            "error": format!(
                                                "External provider '{}' failed to start. {}",
                                                name, e
                                            )
                                        })),
                                    )
                                        .into_response();
                                }
                            }
                            None => {
                                tracing::warn!(
                                    "Provider '{}' registered but not found in registry",
                                    name
                                );
                            }
                        }
                    }
                }
            }

            match plugins_yaml::get_plugin(&state.data_dir, &name) {
                Ok(Some(detail)) => {
                    info!(
                        "Enabled plugin '{}'",
                        name
                    );
                    (
                        StatusCode::OK,
                        Json(serde_json::json!({
                            "success": true,
                            "data": detail
                        })),
                    )
                        .into_response()
                }
                _ => (
                    StatusCode::OK,
                    Json(serde_json::json!({
                        "success": true,
                        "data": { "name": name, "status": "enabled" }
                    })),
                )
                    .into_response(),
            }
        }
        Err(e) => {
            error!("Failed to enable plugin '{}': {:?}", name, e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "success": false,
                    "error": format!("Failed to enable plugin: {}", e)
                })),
            )
                .into_response()
        }
    }
}


/// POST /api/plugins/:name/disable: disable plugin (writes to YAML).
pub(crate) async fn disable_plugin_handler(
    Path(name): Path<String>,
    State(state): State<Arc<AppState>>,
    Json(req): Json<PluginSourceRequest>,
) -> impl IntoResponse {
    let yaml_type = match plugins_yaml::get_disk_plugin_type(&state.data_dir, &name) {
        Ok(Some(t)) => plugins_yaml::PluginYamlType::from_type_str(&t),
        _ => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({
                    "success": false,
                    "error": "Plugin not found"
                })),
            )
                .into_response();
        }
    };

    // Require and validate source
    let source = match require_source(&req.source) {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    match source {
        "built-in" | "bundled" | "remote" => {}
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "success": false,
                    "error": format!("Invalid source '{}': must be 'built-in', 'bundled', or 'remote'", source)
                })),
            )
                .into_response();
        }
    };

    // Upsert with enabled=false: preserve existing source field
    match plugins_yaml::set_entry(
        &state.data_dir,
        &yaml_type,
        &name,
        false,
        serde_json::json!({}),
    ) {
        Ok(_entry) => {
            // Hot-reload: remove this MCP server's tools from the shared registry.
            if yaml_type == plugins_yaml::PluginYamlType::Tool {
                let removed = state.tool_registry.write().await.remove_by_server(&name);
                if !removed.is_empty() {
                    info!(
                        "Removed {} tool(s) from disabled MCP server '{}' (no restart needed): {:?}",
                        removed.len(),
                        name,
                        removed
                    );
                }
            }

            // Hot-reload: if this is a platform plugin, trigger subprocess restart
            if yaml_type == plugins_yaml::PluginYamlType::Platform {
                reload_platform_plugin(&state, &name).await;
            }

            match plugins_yaml::get_plugin(&state.data_dir, &name) {
                Ok(Some(detail)) => {
                    info!("Disabled plugin '{}'", name);
                    (
                        StatusCode::OK,
                        Json(serde_json::json!({
                            "success": true,
                            "data": detail
                        })),
                    )
                        .into_response()
                }
                _ => (
                    StatusCode::OK,
                    Json(serde_json::json!({
                        "success": true,
                        "data": { "name": name, "status": "disabled" }
                    })),
                )
                    .into_response(),
            }
        }
        Err(e) => {
            error!("Failed to disable plugin '{}': {:?}", name, e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "success": false,
                    "error": format!("Failed to disable plugin: {}", e)
                })),
            )
                .into_response()
        }
    }
}


/// Re-start a plugin: disable then enable cycle.
/// This stops and re-starts the plugin's subprocess/MCP server.
pub(crate) async fn restart_plugin_handler(
    Path(name): Path<String>,
    State(state): State<Arc<AppState>>,
) -> Response<Body> {
    // Look up the plugin's source from the YAML config first
    let (_yaml_type, yaml_entry) = match plugins_yaml::get_entry_with_type(&state.data_dir, &name) {
        Ok(Some((t, e))) => (t, e),
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({
                    "success": false,
                    "error": format!("Plugin '{}' not found in any YAML config", name)
                })),
            )
                .into_response();
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "success": false,
                    "error": format!("Failed to read plugin config: {}", e)
                })),
            )
                .into_response();
        }
    };
    let source = Some(yaml_entry.source.clone());

    // First disable the plugin
    let disable_req = PluginSourceRequest {
        source: source.clone(),
        remote: None,
    };
    let disable_resp = disable_plugin_handler(
        Path(name.clone()),
        State(state.clone()),
        Json(disable_req),
    )
    .await
    .into_response();

    // Check if disable succeeded : if it returned non-OK, propagate the error
    if disable_resp.status() != StatusCode::OK {
        return disable_resp;
    }

    // Then re-enable
    let enable_req = PluginSourceRequest {
        source: source.clone(),
        remote: None,
    };
    let enable_resp = enable_plugin_handler(
        Path(name.clone()),
        State(state.clone()),
        Json(enable_req),
    )
    .await
    .into_response();

    if enable_resp.status() == StatusCode::OK {
        (StatusCode::OK, Json(serde_json::json!({
            "success": true,
            "data": { "restarted": true, "name": name }
        }))).into_response()
    } else {
        enable_resp
    }
}

