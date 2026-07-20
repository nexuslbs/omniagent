use axum::{extract::{Path, State}, http::StatusCode, response::IntoResponse, Json};
use std::sync::Arc;
use tracing::{error, info};
use crate::plugins_yaml;
use crate::server::AppState;
use super::plugins_reload::*;
use super::plugins_types::*;

pub(crate) async fn enable_plugin_handler(
    Path((p_type, source, name)): Path<(String, String, String)>,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    if let Err(e) = validate_plugin_type(&p_type) { return e.into_response(); }
    if let Err(e) = validate_source(&source) { return e.into_response(); }
    let yaml_type = plugins_yaml::PluginYamlType::from_type_str(&p_type);
    if let Ok(Some(entry)) = plugins_yaml::get_entry(&state.data_dir, &yaml_type, &name) {
        if entry.enabled && entry.source == source {
            if yaml_type == plugins_yaml::PluginYamlType::Platform {
                reload_platform_plugin(&state, &name).await;
            } else if yaml_type == plugins_yaml::PluginYamlType::Tool {
                crate::mcp::external::client::clear_server_pools(&name);
                crate::mcp::external::client::remove_server_config(&name);
                if let Ok(tools) = crate::mcp::external::client::initialize_single_server_tools(&state.data_dir, &name).await {
                    state.tool_registry.write().await.remove_by_server(&name);
                    state.tool_registry.write().await.register_all(tools);
                }
            }
            if let Ok(Some(detail)) = plugins_yaml::get_plugin(&state.data_dir, &name) {
                return (StatusCode::OK, Json(serde_json::json!({"success": true, "data": detail}))).into_response();
            }
        }
    }
    let existing_remote = plugins_yaml::get_remote_plugin(&state.data_dir, &yaml_type, &name);
    match plugins_yaml::set_entry_with_source(&state.data_dir, &yaml_type, &name, true, &source, serde_json::json!({})) {
        Ok(_entry) => {
            if source == "remote" {
                if let Some(remote) = existing_remote.as_ref() {
                    let _ = plugins_yaml::save_remote_plugin(&state.data_dir, &yaml_type, &name, remote);
                }
            }
            if yaml_type == plugins_yaml::PluginYamlType::Tool {
                if let Ok(tools) = crate::mcp::external::client::initialize_single_server_tools(&state.data_dir, &name).await {
                    state.tool_registry.write().await.register_all(tools);
                } else {
                    let _ = plugins_yaml::remove_entry(&state.data_dir, &yaml_type, &name);
                    return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"success": false, "error": format!("MCP server for '{}' failed to start", name)}))).into_response();
                }
            }
            if yaml_type == plugins_yaml::PluginYamlType::Platform {
                reload_platform_plugin(&state, &name).await;
            }
            match plugins_yaml::get_plugin(&state.data_dir, &name) {
                Ok(Some(detail)) => (StatusCode::OK, Json(serde_json::json!({"success": true, "data": detail}))).into_response(),
                _ => (StatusCode::OK, Json(serde_json::json!({"success": true, "data": {"name": name, "status": "enabled"}}))).into_response(),
            }
        }
        Err(e) => {
            error!("Failed to enable plugin '{}': {:?}", name, e);
            (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"success": false, "error": format!("Failed to enable plugin: {}", e)}))).into_response()
        }
    }
}

pub(crate) async fn disable_plugin_handler(
    Path((p_type, source, name)): Path<(String, String, String)>,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    if let Err(e) = validate_plugin_type(&p_type) { return e.into_response(); }
    if let Err(e) = validate_source(&source) { return e.into_response(); }
    let yaml_type = plugins_yaml::PluginYamlType::from_type_str(&p_type);
    match plugins_yaml::set_entry_with_source(&state.data_dir, &yaml_type, &name, false, &source, serde_json::json!({})) {
        Ok(_entry) => {
            if yaml_type == plugins_yaml::PluginYamlType::Tool {
                crate::mcp::external::client::clear_server_pools(&name);
                crate::mcp::external::client::remove_server_config(&name);
                state.tool_registry.write().await.remove_by_server(&name);
            }
            if yaml_type == plugins_yaml::PluginYamlType::Platform {
                reload_platform_plugin(&state, &name).await;
            }
            match plugins_yaml::get_plugin(&state.data_dir, &name) {
                Ok(Some(detail)) => (StatusCode::OK, Json(serde_json::json!({"success": true, "data": detail}))).into_response(),
                _ => (StatusCode::OK, Json(serde_json::json!({"success": true, "data": {"name": name, "status": "disabled"}}))).into_response(),
            }
        }
        Err(e) => {
            error!("Failed to disable plugin '{}': {:?}", name, e);
            (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"success": false, "error": format!("Failed to disable plugin: {}", e)}))).into_response()
        }
    }
}

pub(crate) async fn restart_plugin_handler(
    Path((_p_type, _source, name)): Path<(String, String, String)>,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    reload_platform_plugin(&state, &name).await;
    (StatusCode::OK, Json(serde_json::json!({"success": true}))).into_response()
}
