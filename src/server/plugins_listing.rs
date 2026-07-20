//! Plugin listing and discovery handlers.
//!
//! Extracted from `plugins.rs` for separation of concerns.

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use sql_forge::sql_forge;
use std::sync::Arc;
use tracing::error;

use crate::err_str;
use crate::plugins_yaml;
use crate::server::AppState;

pub(crate) async fn list_plugins_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let data_dir = state.data_dir.clone();
    match tokio::task::spawn_blocking(move || plugins_yaml::list_plugins(&data_dir))
        .await
        .unwrap_or_else(|e| Err(err_str!("Task join error: {}", e)))
    {
        Ok(mut details) => {
            // Resolve $secret: references in resolved_env for all plugins
            for detail in details.iter_mut() {
                for val in detail.resolved_env.values_mut() {
                    if let Some(secret_name) = val.strip_prefix("$secret:") {
                        let lookup = sql_forge!(
                            String,
                            "SELECT current_value FROM secrets WHERE name = :name",
                            ( :name = secret_name )
                        )
                        .fetch_optional(&state.pool)
                        .await;
                        match lookup {
                            Ok(Some(secret_val)) => {
                                *val = secret_val;
                            }
                            Ok(None) => {
                                tracing::warn!(
                                    "Secret '{}' referenced in plugin config but not found in DB",
                                    secret_name
                                );
                            }
                            Err(e) => {
                                tracing::error!(
                                    "DB error looking up secret '{}': {:?}",
                                    secret_name,
                                    e
                                );
                            }
                        }
                    }
                }
            }

            // Cross-reference MCP plugins with the MCP registry:
            // 1. if a plugin is marked "enabled" but its server has zero
            //    registered tools, the server failed to initialize: set
            //    status to "error" so the frontend shows the right badge.
            // 2. Populate tool_names for all tool plugins (including disabled ones)
            //    so the frontend can show tool counts even when the server isn't running.
            {
                let registry = state.tool_registry.read().await;
                let all_tools = registry.all();
                let mut server_tools: std::collections::HashMap<&str, Vec<String>> =
                    std::collections::HashMap::new();
                for t in all_tools.iter() {
                    if let Some(ref sn) = t.server_name {
                        server_tools
                            .entry(sn.as_str())
                            .or_default()
                            .push(t.full_name.clone());
                    }
                }
                for detail in details.iter_mut() {
                    if detail.plugin_type == "tool" {
                        if detail.status == "enabled" {
                            let has_tools = server_tools.contains_key(detail.name.as_str());
                            if !has_tools {
                                detail.status = "error".to_string();
                                let no_source = !detail.has_source_code;
                                let no_binary_note = if no_source {
                                    ": no source code (no Cargo.toml) and pre-compiled binary not found"
                                } else {
                                    ": binary may not have compiled successfully"
                                };
                                detail.status_message =
                                    format!("MCP server failed to start{}", no_binary_note);
                            }
                        }
                        // Populate tool_names from the registry regardless of status
                        if let Some(tools) = server_tools.get(detail.name.as_str()) {
                            detail.tool_names.clone_from(tools);
                        }
                    }
                }
            }

            (
                StatusCode::OK,
                Json(serde_json::json!({
                    "success": true,
                    "data": details
                })),
            )
                .into_response()
        }
        Err(e) => {
            error!("Failed to list plugins: {:?}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "success": false,
                    "error": format!("Failed to list plugins: {}", e)
                })),
            )
                .into_response()
        }
    }
}


/// GET /api/plugins/{type}/{source}/{name}: get single plugin detail.
pub(crate) async fn get_plugin_handler(
    Path((_p_type, _source, name)): Path<(String, String, String)>,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    let data_dir = state.data_dir.clone();
    let name_clone = name.clone();
    match tokio::task::spawn_blocking(move || plugins_yaml::get_plugin(&data_dir, &name_clone))
        .await
        .unwrap_or_else(|e| Err(err_str!("Task join error: {}", e)))
    {
        Ok(Some(mut detail)) => {
            // Resolve $secret: references in resolved_env
            for val in detail.resolved_env.values_mut() {
                if let Some(secret_name) = val.strip_prefix("$secret:") {
                    let lookup = sql_forge!(
                        String,
                        "SELECT current_value FROM secrets WHERE name = :name",
                        ( :name = secret_name )
                    )
                    .fetch_optional(&state.pool)
                    .await;
                    match lookup {
                        Ok(Some(secret_val)) => {
                            *val = secret_val;
                        }
                        Ok(None) => {
                            tracing::warn!(
                                "Secret '{}' referenced in plugin config but not found in DB",
                                secret_name
                            );
                        }
                        Err(e) => {
                            tracing::error!(
                                "DB error looking up secret '{}': {:?}",
                                secret_name,
                                e
                            );
                        }
                    }
                }
            }

            // Cross-reference MCP plugins with the MCP registry:
            // if a plugin is marked "enabled" but its server has zero
            // registered tools, the server failed to initialize: set
            // status to "error" so the frontend shows the right badge.
            if detail.plugin_type == "tool" && detail.status == "enabled" {
                let registry = state.tool_registry.read().await;
                let has_tools = registry
                    .all()
                    .iter()
                    .any(|t| t.server_name.as_deref() == Some(&detail.name));
                if !has_tools {
                    detail.status = "error".to_string();
                    let no_source = !detail.has_source_code;
                    let no_binary_note = if no_source {
                        ": no source code (no Cargo.toml) and pre-compiled binary not found"
                    } else {
                        ": binary may not have compiled successfully"
                    };
                    detail.status_message = format!("MCP server failed to start{}", no_binary_note);
                }
            }

            (
                StatusCode::OK,
                Json(serde_json::json!({
                    "success": true,
                    "data": detail
                })),
            )
                .into_response()
        }
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({
                "success": false,
                "error": "Plugin not found"
            })),
        )
            .into_response(),
        Err(e) => {
            error!("Failed to get plugin '{}': {:?}", name, e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "success": false,
                    "error": format!("Failed to get plugin: {}", e)
                })),
            )
                .into_response()
        }
    }
}

