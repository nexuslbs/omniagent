use super::plugins_reload::*;
use super::plugins_types::*;
use crate::plugins_yaml;
use crate::server::AppState;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use std::sync::Arc;
use tracing::error;

pub(crate) async fn enable_plugin_handler(
    Path((p_type, source, name)): Path<(String, String, String)>,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    if let Err(e) = validate_plugin_type(&p_type) {
        return e.into_response();
    }
    if let Err(e) = validate_source(&source) {
        return e.into_response();
    }
    let yaml_type = plugins_yaml::PluginYamlType::from_type_str(&p_type);
    if let Ok(Some(entry)) = plugins_yaml::get_entry(&state.data_dir, &yaml_type, &name) {
        if entry.enabled && entry.source == source {
            // Already enabled — idempotent no-op: just return the plugin detail.
            // (Previously this branch force-restarted the plugin, which is the
            // job of the dedicated /restart endpoint, not /enable.)
            //
            // EXCEPT for providers: reload_plugins is the ONLY place that
            // spawns the provider subprocess, and on a cold stack (fresh
            // deploy, container restart) the subprocess has not been started
            // yet — nothing triggers the startup reload. If we return here
            // without reloading, an enabled provider stays subprocess-less
            // until some unrelated API call happens to run reload_plugins, and
            // the first LLM completion falls back to HTTP and fails. Reload is
            // idempotent for already-running providers (entrypoint unchanged ->
            // no restart), so it is safe to run it in the idempotent branch.
            if yaml_type == plugins_yaml::PluginYamlType::Provider {
                crate::llm::refresh_provider_metadata();
                let _ = super::plugins_env::reload_plugins(state.clone()).await;
            }
            if let Ok(Some(detail)) = plugins_yaml::get_plugin(&state.data_dir, &name, &yaml_type) {
                return (
                    StatusCode::OK,
                    Json(serde_json::json!({"success": true, "data": detail})),
                )
                    .into_response();
            }
        }
    }
    let existing_remote = plugins_yaml::get_remote_plugin(&state.data_dir, &yaml_type, &name);
    // Preserve the existing config (access_token_name, etc.) when enabling.
    // The old call passed `serde_json::json!({})` which erased all config.
    let existing_config = plugins_yaml::get_entry(&state.data_dir, &yaml_type, &name)
        .ok()
        .flatten()
        .map(|e| e.config)
        .unwrap_or(serde_json::json!({}));
    match plugins_yaml::set_entry_with_source(
        &state.data_dir,
        &yaml_type,
        &name,
        true,
        &source,
        existing_config,
    ) {
        Ok(_entry) => {
            if source == "remote" {
                if let Some(remote) = existing_remote.as_ref() {
                    let _ = plugins_yaml::save_remote_plugin(
                        &state.data_dir,
                        &yaml_type,
                        &name,
                        remote,
                    );
                }
            }
            if yaml_type == plugins_yaml::PluginYamlType::Tool {
                if let Ok(tools) = state
                    .plugin_manager
                    .initialize_single_server(&state.data_dir, &name)
                    .await
                {
                    state.plugin_manager.register_tools(tools).await;
                } else {
                    let _ = plugins_yaml::remove_entry(&state.data_dir, &yaml_type, &name);
                    return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"success": false, "error": format!("MCP server for '{}' failed to start", name)}))).into_response();
                }
            }
            if yaml_type == plugins_yaml::PluginYamlType::Platform {
                reload_platform_plugin(&state, &name).await;
            }
            if yaml_type == plugins_yaml::PluginYamlType::Provider {
                crate::llm::refresh_provider_metadata();
                // Trigger plugin reload to start/stop external provider subprocess
                let _ = super::plugins_env::reload_plugins(state.clone()).await;
            }
            match plugins_yaml::get_plugin(&state.data_dir, &name, &yaml_type) {
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
    if let Err(e) = validate_plugin_type(&p_type) {
        return e.into_response();
    }
    if let Err(e) = validate_source(&source) {
        return e.into_response();
    }
    let yaml_type = plugins_yaml::PluginYamlType::from_type_str(&p_type);
    // Preserve existing config when disabling — only toggle the enabled flag.
    let existing_config = plugins_yaml::get_entry(&state.data_dir, &yaml_type, &name)
        .ok()
        .flatten()
        .map(|e| e.config)
        .unwrap_or(serde_json::json!({}));
    match plugins_yaml::set_entry_with_source(
        &state.data_dir,
        &yaml_type,
        &name,
        false,
        &source,
        existing_config,
    ) {
        Ok(_entry) => {
            if yaml_type == plugins_yaml::PluginYamlType::Tool {
                state.plugin_manager.remove_client(&name);
                state.plugin_manager.remove_server_tools(&name).await;
            }
            if yaml_type == plugins_yaml::PluginYamlType::Platform {
                stop_platform_plugin(&state, &name).await;
            }
            if yaml_type == plugins_yaml::PluginYamlType::Provider {
                crate::llm::refresh_provider_metadata();
                // Trigger plugin reload to start/stop external provider subprocess
                let _ = super::plugins_env::reload_plugins(state.clone()).await;
            }
            match plugins_yaml::get_plugin(&state.data_dir, &name, &yaml_type) {
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

/// Which restart action applies to a given plugin type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RestartAction {
    Tool,
    Platform,
    Provider,
}

/// Map a plugin YAML type to its restart action. Pure helper, unit-tested.
fn restart_action_for(yaml_type: &plugins_yaml::PluginYamlType) -> RestartAction {
    match yaml_type {
        plugins_yaml::PluginYamlType::Tool => RestartAction::Tool,
        plugins_yaml::PluginYamlType::Platform => RestartAction::Platform,
        plugins_yaml::PluginYamlType::Provider => RestartAction::Provider,
    }
}

pub(crate) async fn restart_plugin_handler(
    Path((p_type, _source, name)): Path<(String, String, String)>,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    if let Err(e) = validate_plugin_type(&p_type) {
        return e.into_response();
    }
    let yaml_type = plugins_yaml::PluginYamlType::from_type_str(&p_type);
    match restart_action_for(&yaml_type) {
        RestartAction::Tool => {
            reload_tool_plugin(&state, &name).await;
        }
        RestartAction::Platform => {
            reload_platform_plugin(&state, &name).await;
        }
        RestartAction::Provider => {
            crate::llm::refresh_provider_metadata();
            let _ = super::plugins_env::reload_plugins(state.clone()).await;
        }
    }
    (StatusCode::OK, Json(serde_json::json!({"success": true}))).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restart_action_dispatches_by_plugin_type() {
        assert_eq!(
            restart_action_for(&plugins_yaml::PluginYamlType::Tool),
            RestartAction::Tool
        );
        assert_eq!(
            restart_action_for(&plugins_yaml::PluginYamlType::Platform),
            RestartAction::Platform
        );
        assert_eq!(
            restart_action_for(&plugins_yaml::PluginYamlType::Provider),
            RestartAction::Provider
        );
    }

    #[test]
    fn from_type_str_maps_api_path_types() {
        assert_eq!(
            plugins_yaml::PluginYamlType::from_type_str("tools"),
            plugins_yaml::PluginYamlType::Tool
        );
        assert_eq!(
            plugins_yaml::PluginYamlType::from_type_str("tool"),
            plugins_yaml::PluginYamlType::Tool
        );
        assert_eq!(
            plugins_yaml::PluginYamlType::from_type_str("platforms"),
            plugins_yaml::PluginYamlType::Platform
        );
        assert_eq!(
            plugins_yaml::PluginYamlType::from_type_str("platform"),
            plugins_yaml::PluginYamlType::Platform
        );
        assert_eq!(
            plugins_yaml::PluginYamlType::from_type_str("providers"),
            plugins_yaml::PluginYamlType::Provider
        );
        assert_eq!(
            plugins_yaml::PluginYamlType::from_type_str("provider"),
            plugins_yaml::PluginYamlType::Provider
        );
    }
}
