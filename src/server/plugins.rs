//! Plugin management API endpoints.
//!
//! Provides REST endpoints for listing, installing, configuring, and
//! managing plugin lifecycle: using YAML files for plugin state
//! instead of the old `plugin_registry` database table.
//!
//! THREE PLUGIN LOCATION TYPES:
//!
//! 1. Builtin plugins (tools.yml/providers.yml/platforms.yml entry has `builtin: true`):
//!    Source: /app/plugins/{type_dir}/{name}/
//!    Binary: get_bin_path("mcp-server-{name}"): next to omniagent binary
//!    Install: verify binary exists at get_bin_path(), compile if missing
//!    Uninstall: YAML removal only (binary stays in get_bin_path())
//!
//! 2. Omni-stack plugins (workspace dir, no remote, not builtin):
//!    Source: {workspace_dir}/plugins/{type_dir}/{name}/
//!    Binary: {workspace_dir}/plugins/{type_dir}/{name}/target/release/{pkg}
//!    Install: compile in place
//!    Uninstall: YAML removal only (source in git repo)
//!
//! 3. Remote plugins (git-installed, has `remote` field in YAML):
//!    Source: {data_dir}/plugins/{type_dir}/.remote/{name}/
//!    Binary: {data_dir}/plugins/{type_dir}/.remote/{name}/target/release/{pkg}
//!    Install: clone to .remote/, compile
//!    Uninstall: remove .remote/ dir + YAML removal

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{delete, get, post},
    Json, Router,
};
use std::sync::Arc;
use tracing::{error, info};

use crate::plugins_yaml;
use crate::server::AppState;

use super::plugins_reload::*;
use super::plugins_types::*;

// ── Re-exports from submodules ──
pub(crate) use super::plugins_delete::delete_plugin_handler;
pub(crate) use super::plugins_enable::{
    disable_plugin_handler, enable_plugin_handler, restart_plugin_handler,
};
pub(crate) use super::plugins_env::reload_env_handler;
pub(crate) use super::plugins_install::{
    download_plugin_handler, install_git_handler, install_plugin_handler, install_url_handler,
    reinstall_plugin_handler, rename_plugin_handler,
};
pub(crate) use super::plugins_listing::{get_plugin_handler, list_plugins_handler};
pub(crate) use super::plugins_setup::setup_plugin_handler;

// ── Router (references handlers from all submodules) ──

/// Build the plugin management router, reusing the main server's state.
#[allow(dead_code)]
pub(crate) fn plugin_router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/plugins/install-git", post(install_git_handler))
        .route("/api/plugins/install-url", post(install_url_handler))
        .route("/api/plugins", get(list_plugins_handler))
        .route(
            "/api/plugins/{type}/{source}/{name}",
            get(get_plugin_handler),
        )
        .route(
            "/api/plugins/{type}/{source}/{name}/config",
            post(update_config_handler),
        )
        .route(
            "/api/plugins/{type}/{source}/{name}/enable",
            post(enable_plugin_handler),
        )
        .route(
            "/api/plugins/{type}/{source}/{name}/disable",
            post(disable_plugin_handler),
        )
        .route(
            "/api/plugins/{type}/{source}/{name}/install",
            post(install_plugin_handler),
        )
        .route(
            "/api/plugins/{type}/{source}/{name}/reinstall",
            post(reinstall_plugin_handler),
        )
        .route(
            "/api/plugins/{type}/{source}/{name}/refresh-models",
            post(refresh_models_handler),
        )
        .route(
            "/api/plugins/{type}/{source}/{name}/setup",
            post(setup_plugin_handler),
        )
        .route(
            "/api/plugins/{type}/{source}/{name}/download",
            post(download_plugin_handler),
        )
        .route(
            "/api/plugins/{type}/{source}/{name}/rename",
            post(rename_plugin_handler),
        )
        .route(
            "/api/plugins/{type}/{source}/{name}",
            delete(delete_plugin_handler),
        )
}

// ── Handlers remaining in this file ──

/// POST /api/plugins/{type}/{source}/{name}/config: update a plugin's YAML config.
pub(crate) async fn update_config_handler(
    Path((p_type, source, name)): Path<(String, String, String)>,
    State(state): State<Arc<AppState>>,
    Json(body): Json<UpdateConfigRequest>,
) -> impl IntoResponse {
    // Validate type and source from path
    if let Err(e) = validate_plugin_type(&p_type) {
        return e.into_response();
    }
    if let Err(e) = validate_source(&source) {
        return e.into_response();
    }

    // Determine the YAML type from the path type
    let yaml_type = plugins_yaml::PluginYamlType::from_type_str(&p_type);

    // Update config in YAML
    match plugins_yaml::update_config(&state.data_dir, &yaml_type, &name, body.config.clone()) {
        Ok(_entry) => {
            // If this is a platform plugin, trigger a hot-reload of the subprocess
            if yaml_type == plugins_yaml::PluginYamlType::Platform {
                reload_platform_plugin(&state, &name).await;
            }

            // If this is a tool (MCP) plugin, clear connection pools, update
            // the config registry, and re-initialize the server's tools.
            // This takes effect without needing to restart omniagent.
            if yaml_type == plugins_yaml::PluginYamlType::Tool {
                reload_tool_plugin(&state, &name).await;
            }

            // Provider plugin config is read from YAML on each use, so
            // the changes take effect without any additional action needed.

            // Return updated plugin detail
            match plugins_yaml::get_plugin(&state.data_dir, &name) {
                Ok(Some(detail)) => {
                    info!("Updated config for plugin '{}'", name);
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
                        "error": "Plugin not found after update"
                    })),
                )
                    .into_response(),
                Err(e) => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({
                        "success": false,
                        "error": format!("Failed to read plugin after update: {}", e)
                    })),
                )
                    .into_response(),
            }
        }
        Err(e) => {
            if e.to_string().contains("not found") {
                (
                    StatusCode::NOT_FOUND,
                    Json(serde_json::json!({
                        "success": false,
                        "error": "Plugin not found"
                    })),
                )
                    .into_response()
            } else {
                error!("Failed to update config for plugin '{}': {:?}", name, e);
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({
                        "success": false,
                        "error": format!("Failed to update config: {}", e)
                    })),
                )
                    .into_response()
            }
        }
    }
}

/// POST /api/plugins/{type}/{source}/{name}/refresh-models: refresh dynamic model list from external API.
pub(crate) async fn refresh_models_handler(
    Path((_p_type, _source, name)): Path<(String, String, String)>,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    match plugins_yaml::refresh_plugin_models(&state.data_dir, &name).await {
        Ok(Some(detail)) => {
            let model_count = detail
                .config_schema
                .iter()
                .filter(|f| f.allowed_values.is_some())
                .map(|f| f.allowed_values.as_ref().map(|v| v.len()).unwrap_or(0))
                .sum::<usize>();
            info!(
                "Refreshed dynamic models for plugin '{}' ({} models)",
                name, model_count
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
        Ok(None) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "success": false,
                "error": format!("Plugin '{}' has no refresh_url fields", name)
            })),
        )
            .into_response(),
        Err(e) => {
            let msg = format!("Failed to refresh models for plugin '{}': {}", name, e);
            error!("{}", msg);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "success": false,
                    "error": msg
                })),
            )
                .into_response()
        }
    }
}
