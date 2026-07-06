//! Plugin management API endpoints.
//!
//! Provides REST endpoints for listing, installing, configuring, and
//! managing plugin lifecycle — using YAML files for plugin state
//! instead of the old `plugin_registry` database table.
//!
//! THREE PLUGIN LOCATION TYPES:
//!
//! 1. Builtin plugins (tools.yml/providers.yml/platforms.yml entry has `builtin: true`):
//!    Source: /app/plugins/{type_dir}/{name}/
//!    Binary: get_bin_path("mcp-server-{name}") — next to omniagent binary
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
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{delete, get, post},
    Json, Router,
};
use serde::Deserialize;
use sqlx;
use sql_forge::sql_forge;
use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tracing::{error, info};

use crate::db::{channels, types::CreateChannelParams};
use crate::err_str;
use crate::plugin;
use crate::plugins_yaml;
use crate::server::AppState;

// ---------------------------------------------------------------------------
// Request/Response types
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub(crate) struct UpdateConfigRequest {
    config: serde_json::Value,
}

#[derive(Deserialize)]
pub(crate) struct PluginSourceRequest {
    /// Source identifier: "built-in", "bundled", or "remote".
    /// Required. The handler acts on this exact source.
    pub source: String,
}

#[derive(Deserialize)]
pub(crate) struct InstallUrlRequest {
    url: String,
}

#[derive(Deserialize)]
pub(crate) struct InstallGitRequest {
    url: String,
    /// Optional git ref (branch, tag, or commit SHA). Defaults to repo HEAD.
    git_ref: Option<String>,
    /// Optional name override. If not provided, extracted from plugin.json.
    name: Option<String>,
    /// Optional subdirectory path within the repo where plugin.json lives.
    /// Example: "tools/test-rust-tool" if plugin.json is not at the repo root.
    path: Option<String>,
}

// ---------------------------------------------------------------------------
// Plugin type detection helpers
// ---------------------------------------------------------------------------

/// The three plugin location types.
#[derive(Debug)]
enum PluginCategory {
    /// builtin: true in YAML, source at /app/plugins/
    Builtin,
    /// Workspace bundled (has plugin.json in workspace_dir/plugins/)
    OmniStack,
    /// Has `remote` field in YAML, source at data_dir/plugins/<type>/.remote/
    Remote,
}

/// Detect a plugin's category from its YAML entry and disk state.
fn detect_plugin_category(
    data_dir: &str,
    yaml_type: &plugins_yaml::PluginYamlType,
    name: &str,
) -> PluginCategory {
    // Check YAML entry first
    if let Ok(Some(entry)) = plugins_yaml::get_entry(data_dir, yaml_type, name) {
        if entry.remote.is_some() {
            return PluginCategory::Remote;
        }
        if entry.builtin.unwrap_or(false) {
            return PluginCategory::Builtin;
        }
        // YAML entry exists but builtin is not true → bundled/omni-stack
        return PluginCategory::OmniStack;
    }

    // No YAML entry — check disk for builtin source directory
    if plugins_yaml::is_plugin_builtin(data_dir, name, yaml_type) {
        return PluginCategory::Builtin;
    }

    // Check if it's remote by looking for .remote/ directory
    let type_dir = yaml_type.type_dir_name();
    let remote_dir = format!("{}/plugins/{}/.remote/{}", data_dir, type_dir, name);
    if std::path::Path::new(&remote_dir).exists() {
        return PluginCategory::Remote;
    }

    // Default to omni-stack
    PluginCategory::OmniStack
}

/// Get the canonical source directory for a plugin by category.
fn get_plugin_dir_for_category(
    data_dir: &str,
    workspace_dir: &str,
    yaml_type: &plugins_yaml::PluginYamlType,
    name: &str,
    category: &PluginCategory,
) -> Option<String> {
    let type_dir = yaml_type.type_dir_name();
    match category {
        PluginCategory::Builtin => {
            let dir = format!("/app/plugins/{}/{}", type_dir, name);
            if std::path::Path::new(&dir).exists() {
                Some(dir)
            } else {
                None
            }
        }
        PluginCategory::OmniStack => {
            // Check workspace_dir first (omni-stack source)
            let dir = format!("{}/plugins/{}/{}", workspace_dir, type_dir, name);
            if std::path::Path::new(&dir).exists() {
                Some(dir)
            } else {
                // Fallback: check data_dir (bundled plugins copied to data dir)
                let data_plugin_dir = format!("{}/plugins/{}/{}", data_dir, type_dir, name);
                if std::path::Path::new(&data_plugin_dir).exists() {
                    Some(data_plugin_dir)
                } else {
                    None
                }
            }
        }
        PluginCategory::Remote => {
            let dir = format!("{}/plugins/{}/.remote/{}", data_dir, type_dir, name);
            if std::path::Path::new(&dir).exists() {
                Some(dir)
            } else {
                None
            }
        }
    }
}

/// Detect plugin category from YAML, searching all three YAML types.
/// Falls back to disk state if no YAML entry exists (e.g., after uninstall).
fn detect_plugin_category_cross_type(data_dir: &str, name: &str) -> Option<(plugins_yaml::PluginYamlType, PluginCategory)> {
    for pt in &[
        plugins_yaml::PluginYamlType::Tool,
        plugins_yaml::PluginYamlType::Platform,
        plugins_yaml::PluginYamlType::Provider,
    ] {
        // Check YAML entry
        if let Ok(Some(entry)) = plugins_yaml::get_entry(data_dir, pt, name) {
            let cat = if entry.remote.is_some() {
                PluginCategory::Remote
            } else if entry.builtin.unwrap_or(false) {
                PluginCategory::Builtin
            } else {
                PluginCategory::OmniStack
            };
            return Some((pt.clone(), cat));
        }
    }
    // Fallback: check disk state — plugin exists on disk even without YAML entry
    if let Ok(Some(type_str)) = plugins_yaml::get_disk_plugin_type(data_dir, name) {
        let pt = plugins_yaml::PluginYamlType::from_type_str(&type_str);
        // Check if builtin source directory exists (e.g., /app/plugins/<type>/<name>/)
        // BEFORE defaulting to OmniStack so install/reinstall handlers find the right dir.
        if plugins_yaml::is_plugin_builtin(data_dir, name, &pt) {
            return Some((pt, PluginCategory::Builtin));
        }
        return Some((pt, PluginCategory::OmniStack));
    }
    None
}

/// Read package name from a Cargo.toml.
fn read_cargo_package_name(cargo_toml_path: &str) -> Option<String> {
    std::fs::read_to_string(cargo_toml_path)
        .ok()
        .and_then(|content| {
            content.lines().find_map(|line| {
                let trimmed = line.trim();
                if let Some(name) = trimmed.strip_prefix("name = \"") {
                    name.strip_suffix('\"').map(|s| s.to_string())
                } else {
                    None
                }
            })
        })
}

/// Compile a Rust crate. Uses the EXACT source to determine build strategy.
/// - "built-in": workspace build from /app/Cargo.toml
/// - "bundled" or "remote": standalone build from the plugin's own Cargo.toml
async fn compile_rust_crate(plugin_dir: &str, name: &str, source: &str) -> Result<bool, String> {
    let cargo_path = std::path::Path::new(plugin_dir);
    let cargo_toml = cargo_path.join("Cargo.toml");
    if !cargo_toml.exists() {
        return Ok(false);
    }

    let pkg_name = read_cargo_package_name(&cargo_toml.to_string_lossy());

    // Check if binary already exists (pre-built by cargo watch or release build)
    let bin_exists = if source == "built-in" {
        pkg_name.as_ref()
            .and_then(|p| crate::mcp::external::config::get_bin_path(p))
            .filter(|p| std::path::Path::new(p).exists())
            .is_some()
    } else {
        let local = pkg_name.as_ref()
            .map(|p| format!("{}/target/release/{}", plugin_dir, p))
            .unwrap_or_else(|| format!("{}/target/release/{}", plugin_dir, name));
        std::path::Path::new(&local).exists()
    };

    if bin_exists {
        info!("Compile: binary for '{}' already exists — no compilation needed", name);
        return Ok(true);
    }

    // Build from the EXACT source — no guessing
    if source == "built-in" {
        // Workspace build (all builtins are workspace members)
        if let Some(ref pn) = pkg_name {
            let mut cmd = tokio::process::Command::new("cargo");
            cmd.args(["build", "--release", "--manifest-path", "/app/Cargo.toml", "-p", pn]);
            cmd.current_dir("/app");
            let st = cmd.status().await
                .map_err(|e| format!("cargo failed: {}", e))?;
            if !st.success() {
                return Err(format!("Workspace build for '{}' failed: {}", name, st));
            }
            info!("Builtin crate '{}' compiled via workspace", name);
        } else {
            return Err(format!("Cannot determine package name for '{}'", name));
        }
    } else {
        // Standalone build for bundled or remote
        let cargo_s = cargo_toml.to_string_lossy().to_string();
        let mut cmd = tokio::process::Command::new("cargo");
        cmd.args(["build", "--release", "--manifest-path", &cargo_s]);
        cmd.current_dir(plugin_dir);
        let st = cmd.status().await
            .map_err(|e| format!("cargo failed: {}", e))?;
        if !st.success() {
            return Err(format!("Standalone build for '{}' failed: {}", name, st));
        }
        info!("Standalone crate '{}' compiled", name);
    }

    Ok(true)
}

/// Map PluginCategory to source string for compile_rust_crate.
fn category_to_source(category: &PluginCategory) -> &'static str {
    match category {
        PluginCategory::Builtin => "built-in",
        PluginCategory::OmniStack => "bundled",
        PluginCategory::Remote => "remote",
    }
}

/// Build the plugin management router, reusing the main server's state.
#[allow(dead_code)]
pub(crate) fn plugin_router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/plugins/install-git", post(install_git_handler))
        .route("/api/plugins/install-url", post(install_url_handler))
        .route("/api/plugins", get(list_plugins_handler))
        .route("/api/plugins/{name}", get(get_plugin_handler))
        .route("/api/plugins/{name}/config", post(update_config_handler))
        .route("/api/plugins/{name}/enable", post(enable_plugin_handler))
        .route("/api/plugins/{name}/disable", post(disable_plugin_handler))
        .route("/api/plugins/{name}/install", post(install_plugin_handler))
        .route("/api/plugins/{name}/reinstall", post(reinstall_plugin_handler))
        .route("/api/plugins/{name}/refresh-models", post(refresh_models_handler))
        .route("/api/plugins/{name}/setup", post(setup_plugin_handler))
        .route("/api/plugins/{name}", delete(delete_plugin_handler))
        .route("/api/reload", post(reload_env_handler))
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// GET /api/plugins — list all plugins (discover from disk + YAML overrides).
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
                                tracing::warn!("Secret '{}' referenced in plugin config but not found in DB", secret_name);
                            }
                            Err(e) => {
                                tracing::error!("DB error looking up secret '{}': {:?}", secret_name, e);
                            }
                        }
                    }
                }
            }

            // Cross-reference MCP plugins with the MCP registry:
            // if a plugin is marked "enabled" but its server has zero
            // registered tools, the server failed to initialize — set
            // status to "error" so the frontend shows the right badge.
            {
                let registry = state.mcp_registry.read().unwrap();
                let all_tools = registry.all();
                for detail in details.iter_mut() {
                    if detail.plugin_type == "mcp" && detail.status == "enabled" {
                        let has_tools = all_tools.iter().any(|t| {
                            t.server_name.as_deref() == Some(&detail.name)
                        });
                        if !has_tools {
                            detail.status = "error".to_string();
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

/// GET /api/plugins/:name — get single plugin detail.
pub(crate) async fn get_plugin_handler(
    Path(name): Path<String>,
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
                            tracing::warn!("Secret '{}' referenced in plugin config but not found in DB", secret_name);
                        }
                        Err(e) => {
                            tracing::error!("DB error looking up secret '{}': {:?}", secret_name, e);
                        }
                    }
                }
            }

            // Cross-reference MCP plugins with the MCP registry:
            // if a plugin is marked "enabled" but its server has zero
            // registered tools, the server failed to initialize — set
            // status to "error" so the frontend shows the right badge.
            if detail.plugin_type == "mcp" && detail.status == "enabled" {
                let registry = state.mcp_registry.read().unwrap();
                let has_tools = registry
                    .all()
                    .iter()
                    .any(|t| t.server_name.as_deref() == Some(&detail.name));
                if !has_tools {
                    detail.status = "error".to_string();
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

/// POST /api/plugins/:name/config — update plugin config (writes to YAML).
pub(crate) async fn update_config_handler(
    Path(name): Path<String>,
    State(state): State<Arc<AppState>>,
    Json(body): Json<UpdateConfigRequest>,
) -> impl IntoResponse {
    // Determine the YAML type from disk manifest
    let yaml_type = match plugins_yaml::get_disk_plugin_type(&state.data_dir, &name) {
        Ok(Some(t)) => plugins_yaml::PluginYamlType::from_type_str(&t),
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({
                    "success": false,
                    "error": "Plugin not found"
                })),
            )
                .into_response();
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "success": false,
                    "error": format!("Failed to determine plugin type: {}", e)
                })),
            )
                .into_response();
        }
    };

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

/// POST /api/plugins/:name/enable — enable plugin (writes to YAML).
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

    // Determine desired builtin flag from source parameter
    let desired_builtin = match req.source.as_str() {
        "built-in" => Some(true),
        "bundled" | "remote" => Some(false),
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "success": false,
                    "error": format!("Invalid source '{}': must be 'built-in' or 'bundled'", req.source)
                })),
            )
                .into_response();
        }
    };

    // Check if plugin already in the desired state — skip only if source matches
    if let Ok(Some(entry)) = plugins_yaml::get_entry(&state.data_dir, &yaml_type, &name) {
        let source_matches = match desired_builtin {
            Some(true) => entry.builtin.unwrap_or(false),
            Some(false) => !entry.builtin.unwrap_or(false),
            None => unreachable!(), // desired_builtin is always Some after validation
        };
        if entry.enabled && source_matches {
            // Already enabled with matching source — no change needed
            match plugins_yaml::get_plugin(&state.data_dir, &name) {
                Ok(Some(detail)) => {
                    info!("Plugin '{}' is already enabled with matching source — no change needed", name);
                    return (
                        StatusCode::OK,
                        Json(serde_json::json!({
                            "success": true,
                            "data": detail
                        })),
                    )
                        .into_response();
                }
                _ => {}
            }
        }
    }

    // Upsert with enabled=true and explicit builtin override
    match plugins_yaml::set_entry_with_builtin_override(
        &state.data_dir,
        &yaml_type,
        &name,
        true,
        desired_builtin,
        serde_json::json!({}),
    ) {
        Ok(_entry) => {
            // Hot-reload: if this is an MCP tool plugin, initialize the server
            // and register its tools in the shared registry immediately.
            if yaml_type == plugins_yaml::PluginYamlType::Tool {
                match crate::mcp::external::client::initialize_single_server_tools(
                    &state.data_dir,
                    &state.workspace_dir,
                    &name,
                )
                .await
                {
                    Ok(tools) => {
                        let count = tools.len();
                        state.mcp_registry.write().unwrap().register_all(tools);
                        info!(
                            "Hot-reloaded {} tool(s) from MCP server '{}' (no restart needed)",
                            count, name
                        );
                    }
                    Err(e) => {
                        tracing::warn!(
                            "Hot-reload of MCP server '{}' failed (will retry on next restart): {}",
                            name, e
                        );
                    }
                }
            }

            match plugins_yaml::get_plugin(&state.data_dir, &name) {
                Ok(Some(detail)) => {
                    info!("Enabled plugin '{}'", name);
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

/// POST /api/plugins/:name/disable — disable plugin (writes to YAML).
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

    // Validate source incl remote
    let _desired_builtin = match req.source.as_str() {
        "built-in" => Some(true),
        "bundled" | "remote" => Some(false),
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "success": false,
                    "error": format!("Invalid source '{}': must be 'built-in' or 'bundled'", req.source)
                })),
            )
                .into_response();
        }
    };

    // Upsert with enabled=false — preserve existing builtin flag
    match plugins_yaml::set_entry_with_builtin_override(
        &state.data_dir,
        &yaml_type,
        &name,
        false,
        None, // preserve existing builtin flag
        serde_json::json!({}),
    ) {
        Ok(_entry) => {
            // Hot-reload: remove this MCP server's tools from the shared registry.
            if yaml_type == plugins_yaml::PluginYamlType::Tool {
                let removed = state.mcp_registry.write().unwrap().remove_by_server(&name);
                if !removed.is_empty() {
                    info!(
                        "Removed {} tool(s) from disabled MCP server '{}' (no restart needed): {:?}",
                        removed.len(),
                        name,
                        removed
                    );
                }
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

/// POST /api/plugins/:name/install — compile and register a plugin.
///
/// Handles all three plugin categories:
/// 1. Builtin: verify binary at get_bin_path() or compile
/// 2. Omni-stack: compile from workspace_dir
/// 3. Remote: should already be cloned; compile from .remote/
pub(crate) async fn install_plugin_handler(
    Path(name): Path<String>,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    let data_dir = &state.data_dir;
    let workspace_dir = &state.workspace_dir;

    // 1. Find the plugin type
    let (yaml_type, category) = match detect_plugin_category_cross_type(data_dir, &name) {
        Some((t, c)) => (t, c),
        None => {
            // Try to determine type from disk discovery
            let disk_type = match plugins_yaml::get_disk_plugin_type(data_dir, &name) {
                Ok(Some(t)) => plugins_yaml::PluginYamlType::from_type_str(&t),
                _ => {
                    return (
                        StatusCode::NOT_FOUND,
                        Json(serde_json::json!({
                            "success": false,
                            "error": format!("Plugin '{}' not found on disk", name)
                        })),
                    )
                        .into_response();
                }
            };
            // Default to omni-stack for newly discovered plugins, but check
            // for builtin source first (the plugin may live at /app/plugins/ without YAML entry)
            let category = if plugins_yaml::is_plugin_builtin(data_dir, &name, &disk_type) {
                PluginCategory::Builtin
            } else {
                PluginCategory::OmniStack
            };
            (disk_type, category)
        }
    };

    // 2. Get the plugin source directory with fallback
    let (mut plugin_dir, mut category) = match get_plugin_dir_for_category(
        data_dir,
        workspace_dir,
        &yaml_type,
        &name,
        &category,
    ) {
        Some(d) => (d, category),
        None => {
            // Fallback: try Builtin source before giving up
            if !matches!(category, PluginCategory::Builtin) {
                let builtin_dir = format!("/app/plugins/{}/{}", yaml_type.type_dir_name(), name);
                if std::path::Path::new(&builtin_dir).join("Cargo.toml").exists() {
                    info!("Install: falling back to built-in source for '{}'", name);
                    (builtin_dir, PluginCategory::Builtin)
                } else if matches!(category, PluginCategory::Remote) {
                    return (
                        StatusCode::BAD_REQUEST,
                        Json(serde_json::json!({
                            "success": false,
                            "error": format!("Remote plugin '{}' has not been cloned yet. Use /api/plugins/install-git first.", name)
                        })),
                    )
                        .into_response();
                } else {
                    return (
                        StatusCode::NOT_FOUND,
                        Json(serde_json::json!({
                            "success": false,
                            "error": format!("Plugin '{}' source directory not found", name)
                        })),
                    )
                        .into_response();
                }
            } else {
                return (
                    StatusCode::NOT_FOUND,
                    Json(serde_json::json!({
                        "success": false,
                        "error": format!("Plugin '{}' source directory not found", name)
                    })),
                )
                    .into_response();
            }
        }
    };

    // Check if there's actual source code to work with
    // For remote plugins with a path subdirectory, also check the subpath
    let mut dir_path = std::path::Path::new(&plugin_dir).to_path_buf();
    if matches!(category, PluginCategory::Remote) {
        if let Ok(Some(entry)) = plugins_yaml::get_entry(data_dir, &yaml_type, &name) {
            if let Some(ref remote) = entry.remote {
                if let Some(ref subpath) = remote.path {
                    let sub = dir_path.join(subpath);
                    if sub.join("Cargo.toml").exists() || sub.join("plugin.json").exists() {
                        dir_path = sub;
                    }
                }
            }
        }
    }
    let mut has_cargo_toml = dir_path.join("Cargo.toml").exists();
    let has_plugin_json = dir_path.join("plugin.json").exists();
    let has_entrypoint = if has_plugin_json {
        std::fs::read_to_string(dir_path.join("plugin.json"))
            .ok()
            .and_then(|c| serde_json::from_str::<serde_json::Value>(&c).ok())
            .map(|v| {
                v.get("entrypoint")
                    .and_then(|e| e.get("command"))
                    .and_then(|c| c.as_str())
                    .map(|s| !s.is_empty())
                    .unwrap_or(false)
            })
            .unwrap_or(false)
    } else {
        false
    };

    // If the primary category's source dir has no compileable code, fall back to Builtin
    if !has_cargo_toml && !has_entrypoint && !matches!(category, PluginCategory::Builtin) {
        let builtin_dir = format!("/app/plugins/{}/{}", yaml_type.type_dir_name(), name);
        let builtin_cargo = std::path::Path::new(&builtin_dir).join("Cargo.toml");
        if builtin_cargo.exists() {
            info!("Install: falling back to built-in source for '{}' (bundled dir has no source code)", name);
            plugin_dir = builtin_dir;
            category = PluginCategory::Builtin;
            dir_path = std::path::Path::new(&plugin_dir).to_path_buf();
            has_cargo_toml = true;
        }
    }

    if !has_cargo_toml && !has_entrypoint {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "success": false,
                "error": format!("Plugin '{}' has no source code — only a pre-built binary exists. Install requires source code (Cargo.toml or plugin.json entrypoint).", name)
            })),
        )
            .into_response();
    }

    info!(
        "Install: {} plugin '{}' from {} (category: {:?})",
        yaml_type.file_name(), name, plugin_dir, category
    );

    let category_source = category_to_source(&category);

    // 3. Register in YAML with enabled=false FIRST (before async compile)
    info!(
        "Install: registering plugin '{}' in YAML before background compile",
        name
    );
    match plugins_yaml::set_entry(
        data_dir,
        &yaml_type,
        &name,
        false,
        serde_json::json!({}),
    ) {
        Ok(_entry) => {
            // Register in YAML succeeded — now spawn compile in background
            let plugin_dir_clone = plugin_dir.clone();
            let name_clone = name.clone();
            let source_clone = category_source.to_string();
            tokio::spawn(async move {
                info!(
                    "Background compile: starting for '{}' from {} (source: {})",
                    name_clone, plugin_dir_clone, source_clone
                );
                match compile_rust_crate(&plugin_dir_clone, &name_clone, &source_clone).await {
                    Ok(_) => info!(
                        "Background compile: '{}' completed successfully",
                        name_clone
                    ),
                    Err(e) => {
                        tracing::error!(
                            "Background compile: '{}' failed: {}",
                            name_clone,
                            e
                        );
                    }
                }
            });

            // Return immediately — plugin is registered even if compile hasn't finished
            match plugins_yaml::get_plugin(data_dir, &name) {
                Ok(Some(detail)) => {
                    info!("Installed plugin '{}' (registered, compile in background)", name);
                    (
                        StatusCode::OK,
                        Json(serde_json::json!({
                            "success": true,
                            "data": detail,
                            "background_compile": true,
                        })),
                    )
                        .into_response()
                }
                _ => (
                    StatusCode::OK,
                    Json(serde_json::json!({
                        "success": true,
                    })),
                )
                    .into_response(),
            }
        }
        Err(e) => {
            error!("Failed to register plugin '{}' in YAML: {:?}", name, e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "success": false,
                    "error": format!("YAML registration failed: {}", e)
                })),
            )
                .into_response()
        }
    }
}

/// POST /api/plugins/:name/reinstall — recompile and reload a plugin.
///
/// Handles all three plugin categories:
/// 1. Builtin: recompile, binary goes to get_bin_path()
/// 2. Omni-stack: recompile in place
/// 3. Remote: re-clone to .remote/, recompile
pub(crate) async fn reinstall_plugin_handler(
    Path(name): Path<String>,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    let data_dir = &state.data_dir;
    let workspace_dir = &state.workspace_dir;

    // 1. Detect plugin type
    let (yaml_type, category) = match detect_plugin_category_cross_type(data_dir, &name) {
        Some((t, c)) => (t, c),
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({
                    "success": false,
                    "error": format!("Plugin '{}' not found in YAML configuration", name)
                })),
            )
                .into_response();
        }
    };

    info!(
        "Reinstall: {} plugin '{}' (category: {:?})",
        yaml_type.file_name(), name, category
    );

    // 2. Handle remote plugins: re-clone first
    if matches!(category, PluginCategory::Remote) {
        let remote_info = plugins_yaml::get_entry(data_dir, &yaml_type, &name)
            .ok()
            .flatten()
            .and_then(|e| e.remote);

        if let Some(ref remote) = remote_info {
            info!(
                "Reinstall: re-cloning git plugin '{}' from {} (ref: {:?})",
                name, remote.url, remote.git_ref
            );
            if let Err(e) = plugin::installer::install_from_git(
                &remote.url,
                &name,
                remote.git_ref.as_deref(),
                workspace_dir,
                data_dir,
                remote.path.as_deref(),
            ) {
                let msg = format!("Reinstall: failed to re-clone git plugin '{}': {}", name, e);
                tracing::error!("{}", msg);
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({
                        "success": false,
                        "error": msg,
                    })),
                )
                    .into_response();
            }
        }
    }

    // 3. Get plugin source directory with Builtin fallback
    let (mut plugin_dir, mut category) = match get_plugin_dir_for_category(data_dir, workspace_dir, &yaml_type, &name, &category) {
        Some(d) => (d, category),
        None => {
            // Fallback: try Builtin source before giving up
            if !matches!(category, PluginCategory::Builtin) {
                let builtin_dir = format!("/app/plugins/{}/{}", yaml_type.type_dir_name(), name);
                if std::path::Path::new(&builtin_dir).join("Cargo.toml").exists() {
                    info!("Reinstall: falling back to built-in source for '{}'", name);
                    (builtin_dir, PluginCategory::Builtin)
                } else {
                    return (
                        StatusCode::NOT_FOUND,
                        Json(serde_json::json!({
                            "success": false,
                            "error": format!("Plugin '{}' source directory not found", name)
                        })),
                    )
                        .into_response();
                }
            } else {
                return (
                    StatusCode::NOT_FOUND,
                    Json(serde_json::json!({
                        "success": false,
                        "error": format!("Plugin '{}' source directory not found", name)
                    })),
                )
                    .into_response();
            }
        }
    };

    // Check if there's actual source code to work with
    // For remote plugins with a path subdirectory, also check the subpath
    let mut dir_path = std::path::Path::new(&plugin_dir).to_path_buf();
    if matches!(category, PluginCategory::Remote) {
        if let Ok(Some(entry)) = plugins_yaml::get_entry(data_dir, &yaml_type, &name) {
            if let Some(ref remote) = entry.remote {
                if let Some(ref subpath) = remote.path {
                    let sub = dir_path.join(subpath);
                    if sub.join("Cargo.toml").exists() || sub.join("plugin.json").exists() {
                        dir_path = sub;
                    }
                }
            }
        }
    }
    let mut has_cargo_toml = dir_path.join("Cargo.toml").exists();
    let has_plugin_json = dir_path.join("plugin.json").exists();
    let has_entrypoint = if has_plugin_json {
        std::fs::read_to_string(dir_path.join("plugin.json"))
            .ok()
            .and_then(|c| serde_json::from_str::<serde_json::Value>(&c).ok())
            .map(|v| {
                v.get("entrypoint")
                    .and_then(|e| e.get("command"))
                    .and_then(|c| c.as_str())
                    .map(|s| !s.is_empty())
                    .unwrap_or(false)
            })
            .unwrap_or(false)
    } else {
        false
    };

    // If the primary category's source dir has no compileable code, fall back to Builtin
    if !has_cargo_toml && !has_entrypoint && !matches!(category, PluginCategory::Builtin) {
        let builtin_dir = format!("/app/plugins/{}/{}", yaml_type.type_dir_name(), name);
        let builtin_cargo = std::path::Path::new(&builtin_dir).join("Cargo.toml");
        if builtin_cargo.exists() {
            info!("Reinstall: falling back to built-in source for '{}' (bundled dir has no source code)", name);
            plugin_dir = builtin_dir;
            category = PluginCategory::Builtin;
            dir_path = std::path::Path::new(&plugin_dir).to_path_buf();
            has_cargo_toml = true;
        }
    }

    if !has_cargo_toml && !has_entrypoint {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "success": false,
                "error": format!("Plugin '{}' has no source code — only a pre-built binary exists. Reinstall requires source code (Cargo.toml or plugin.json entrypoint).", name)
            })),
        )
            .into_response();
    }

    // 4. Compile
    let compiled = match compile_rust_crate(&plugin_dir, &name, category_to_source(&category)).await {
        Ok(true) => true,
        Ok(false) => false,
        Err(e) => {
            let msg = format!("Reinstall: compilation failed for '{}': {}", name, e);
            tracing::error!("{}", msg);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "success": false,
                    "error": msg,
                })),
            )
                .into_response();
        }
    };

    // 5. Re-scan from disk and hot-reload
    match plugins_yaml::get_plugin(data_dir, &name) {
        Ok(Some(detail)) => {
            // 6. If this is a tool (MCP) or platform plugin, restart the
            //    subprocess so the newly compiled binary takes effect immediately.
            if let Ok(Some(t)) = plugins_yaml::get_disk_plugin_type(data_dir, &name) {
                let yaml_type = plugins_yaml::PluginYamlType::from_type_str(&t);
                if yaml_type == plugins_yaml::PluginYamlType::Tool {
                    reload_tool_plugin(&state, &name).await;
                } else if yaml_type == plugins_yaml::PluginYamlType::Platform {
                    reload_platform_plugin(&state, &name).await;
                }
            }

            let compile_msg = if compiled { " (recompiled)" } else { "" };
            info!("Reinstalled plugin '{}'{}", name, compile_msg);
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
                "error": format!("Plugin '{}' not found on disk after re-scan", name)
            })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "success": false,
                "error": format!("Error checking plugin after reinstall: {}", e)
            })),
        )
            .into_response(),
    }
}

/// POST /api/plugins/:name/setup — run platform plugin setup procedure.
///
/// Only available for platform plugins that advertise `capabilities.setup = true`.
/// Spawns the plugin binary as a one-shot process with a `"setup"` JSON-RPC
/// request on stdin and returns the result.
pub(crate) async fn setup_plugin_handler(
    Path(name): Path<String>,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    // 1. Get plugin detail from disk
    let data_dir = state.data_dir.clone();
    let data_dir_for_blocking = data_dir.clone();
    let name_clone = name.clone();
    let detail = match tokio::task::spawn_blocking(move || {
        plugins_yaml::get_plugin(&data_dir_for_blocking, &name_clone)
    })
    .await
    .unwrap_or_else(|e| Err(err_str!("Task join error: {}", e)))
    {
        Ok(Some(d)) => d,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({
                    "success": false,
                    "error": format!("Plugin '{}' not found", name)
                })),
            )
                .into_response();
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "success": false,
                    "error": format!("Failed to look up plugin '{}': {}", name, e)
                })),
            )
                .into_response();
        }
    };

    // 2. Check that this platform supports setup
    let manifest: plugin::PluginManifest = match serde_json::from_value(detail.manifest.clone()) {
        Ok(m) => m,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "success": false,
                    "error": format!("Failed to parse manifest for '{}': {}", name, e)
                })),
            )
                .into_response();
        }
    };

    let has_setup = manifest
        .capabilities
        .as_ref()
        .map(|c| c.setup)
        .unwrap_or(false);

    if !has_setup {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "success": false,
                "error": format!("Plugin '{}' does not support setup", name)
            })),
        )
            .into_response();
    }

    // 3. Find the plugin entrypoint
    let entrypoint = match &manifest.entrypoint {
        Some(ep) => ep,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "success": false,
                    "error": format!("Plugin '{}' has no entrypoint defined", name)
                })),
            )
                .into_response();
        }
    };

    let cmd = std::path::Path::new(&entrypoint.command);
    let binary_path = if cmd.is_absolute() {
        cmd.to_path_buf()
    } else {
        // Try relative to plugin directory — scan possible locations
        let plugin_dirs = [
            format!("{}/plugins/platforms/{}", data_dir, name),
            format!("{}/plugins/mcp/{}", data_dir, name),
            format!("{}/plugins/providers/{}", data_dir, name),
            format!("/app/plugins/platforms/{}", name),
            format!("/app/plugins/mcp/{}", name),
            format!("/app/plugins/providers/{}", name),
        ];
        let mut found = None;
        for dir in &plugin_dirs {
            let candidate = std::path::Path::new(dir).join(&entrypoint.command);
            if candidate.exists() {
                found = Some(candidate);
                break;
            }
        }
        match found {
            Some(p) => p,
            None => {
                // Try get_bin_path() for builtin plugins
                if let Some(bin_path) = crate::mcp::external::config::get_bin_path(&entrypoint.command) {
                    if std::path::Path::new(&bin_path).exists() {
                        return (
                            StatusCode::OK,
                            Json(serde_json::json!({
                                "success": true,
                                "data": { "binary": bin_path }
                            })),
                        )
                            .into_response();
                    }
                }
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({
                        "success": false,
                        "error": format!("Plugin binary not found for '{}': {}", name, entrypoint.command)
                    })),
                )
                    .into_response();
            }
        }
    };

    // 4-8. Same setup logic as before (unchanged)
    let mut setup_env: std::collections::HashMap<String, String> = std::collections::HashMap::new();

    // Copy the resolved_env from the plugin detail
    for (k, v) in &detail.resolved_env {
        setup_env.insert(k.clone(), v.clone());
    }

    // Also set env vars from the env block in the manifest
    if let Some(manifest) = serde_json::from_value::<plugin::PluginManifest>(detail.manifest.clone()).ok()
    {
        for (env_key, env_val) in &manifest.env {
            let resolved = if let Some(var_name) = env_val.strip_prefix("${").and_then(|s| s.strip_suffix('}')) {
                std::env::var(var_name).unwrap_or_default()
            } else if let Some(var_name) = env_val.strip_prefix("$env:") {
                std::env::var(var_name).unwrap_or_default()
            } else {
                env_val.clone()
            };
            setup_env.insert(env_key.clone(), resolved);
        }
    }

    // Resolve $secret: references in setup_env
    crate::plugins_yaml::resolve_config_refs(&mut setup_env, &state.pool).await;

    // Build the setup params
    let config = &detail.config;
    let setup_val = |key: &str| -> String {
        if let Some(v) = setup_env.get(key) {
            if !v.is_empty() { return v.clone(); }
        }
        if let Some(raw) = config.get(key).and_then(|v| v.as_str()).filter(|s| !s.is_empty()) {
            if raw.starts_with("$env:") {
                std::env::var(raw.strip_prefix("$env:").unwrap()).unwrap_or_default()
            } else if raw.starts_with("$secret:") {
                String::new()
            } else {
                raw.to_string()
            }
        } else {
            String::new()
        }
    };
    let setup_params = serde_json::json!({
        "setup_team": setup_val("setup_team"),
        "setup_channel": setup_val("setup_channel"),
        "bot_user": setup_val("bot_user"),
        "admin_user": setup_val("admin_user"),
        "admin_password": setup_val("admin_password"),
        "test_user": setup_val("test_user"),
        "test_password": setup_val("test_password"),
        "bot_password": setup_val("bot_password"),
    });

    let request_body = serde_json::json!({
        "method": "setup",
        "id": 1,
        "params": setup_params,
    });

    let request_str = serde_json::to_string(&request_body)
        .unwrap_or_else(|_| "{}".to_string());

    tracing::info!(
        "Spawning plugin '{}' for setup: {}",
        name,
        binary_path.display()
    );

    let mut child = match std::process::Command::new(&binary_path)
        .arg("setup")
        .args(&entrypoint.args)
        .env_clear()
        .env("RUST_LOG", "info")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "success": false,
                    "error": format!("Failed to spawn plugin '{}' for setup: {}", name, e)
                })),
            )
                .into_response();
        }
    };

    let mut stdin = match child.stdin.take() {
        Some(s) => s,
        None => {
            let _ = child.kill();
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "success": false,
                    "error": format!("Failed to capture stdin for plugin '{}'", name)
                })),
            )
                .into_response();
        }
    };
    let mut stdout = match child.stdout.take() {
        Some(s) => s,
        None => {
            let _ = child.kill();
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "success": false,
                    "error": format!("Failed to capture stdout for plugin '{}'", name)
                })),
            )
                .into_response();
        }
    };
    let mut reader = std::io::BufReader::new(&mut stdout);

    // Send initialize request
    let init_req = serde_json::json!({"method": "initialize", "id": 1, "params": {}});
    {
        use std::io::Write;
        if let Err(e) = writeln!(stdin, "{}", serde_json::to_string(&init_req).unwrap_or_default()) {
            let _ = child.kill();
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "success": false,
                    "error": format!("Failed to send initialize request to '{}': {}", name, e)
                })),
            )
                .into_response();
        }
    }
    // Read initialize response
    {
        use std::io::BufRead;
        let mut line = String::new();
        if let Err(e) = reader.read_line(&mut line) {
            let _ = child.kill();
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "success": false,
                    "error": format!("Failed to read initialize response from '{}': {}", name, e)
                })),
            )
                .into_response();
        }
        tracing::debug!("Plugin '{}' initialize response: {}", name, line.trim());
    }

    // Send configure request with config values
    let configure_req = serde_json::json!({"method": "configure", "id": 2, "params": &setup_env});
    {
        use std::io::Write;
        if let Err(e) = writeln!(stdin, "{}", serde_json::to_string(&configure_req).unwrap_or_default()) {
            let _ = child.kill();
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "success": false,
                    "error": format!("Failed to send configure request to '{}': {}", name, e)
                })),
            )
                .into_response();
        }
    }
    // Read configure response
    {
        use std::io::BufRead;
        let mut line = String::new();
        if let Err(e) = reader.read_line(&mut line) {
            let _ = child.kill();
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "success": false,
                    "error": format!("Failed to read configure response from '{}': {}", name, e)
                })),
            )
                .into_response();
        }
        tracing::debug!("Plugin '{}' configure response: {}", name, line.trim());
    }

    // Write setup request to stdin
    {
        use std::io::Write;
        if let Err(e) = writeln!(stdin, "{}", request_str) {
            let _ = child.kill();
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "success": false,
                    "error": format!("Failed to send setup request to '{}': {}", name, e)
                })),
            )
                .into_response();
        }
    }
    // Close stdin (signals EOF to the plugin so it knows to exit after setup)
    drop(stdin);

    // Read all stdout output with timeout (120 seconds)
    let start = std::time::Instant::now();
    let max_wait = std::time::Duration::from_secs(120);

    let mut stdout_output = String::new();
    loop {
        if start.elapsed() >= max_wait {
            let _ = child.kill();
            return (
                StatusCode::REQUEST_TIMEOUT,
                Json(serde_json::json!({
                    "success": false,
                    "error": format!("Setup for '{}' timed out after 120 seconds", name)
                })),
            )
                .into_response();
        }

        match child.try_wait() {
            Ok(Some(status)) => {
                {
                    use std::io::Read;
                    let _ = reader.read_to_string(&mut stdout_output);
                }
                let stderr_output = child
                    .stderr
                    .take()
                    .map(|mut s| {
                        let mut buf = String::new();
                        use std::io::Read;
                        let _ = s.read_to_string(&mut buf);
                        buf
                    })
                    .unwrap_or_default();

                if !status.success() {
                    let err_detail = if stderr_output.is_empty() {
                        stdout_output.clone()
                    } else {
                        stderr_output.clone()
                    };
                    let truncated = if err_detail.len() > 500 {
                        format!("{}...", &err_detail[..500])
                    } else {
                        err_detail
                    };

                    tracing::error!(
                        "Setup for '{}' failed (exit: {}): {}",
                        name,
                        status,
                        truncated
                    );

                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(serde_json::json!({
                            "success": false,
                            "error": format!("Setup failed: {}", truncated)
                        })),
                    )
                        .into_response();
                }

                break;
            }
            Ok(None) => {
                std::thread::sleep(std::time::Duration::from_millis(500));
                continue;
            }
            Err(e) => {
                let _ = child.kill();
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({
                        "success": false,
                        "error": format!("Error waiting for setup process '{}': {}", name, e)
                    })),
                )
                    .into_response();
            }
        }
    }

    // Parse response
    let first_line = stdout_output.lines().next().unwrap_or("");

    match serde_json::from_str::<serde_json::Value>(first_line) {
        Ok(val) => {
            if let Some(error) = val.get("error") {
                let msg = error
                    .get("message")
                    .and_then(|m| m.as_str())
                    .unwrap_or("Setup failed with unknown error");

                return (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({
                        "success": false,
                        "error": msg.to_string()
                    })),
                )
                    .into_response();
            }

            let result = val.get("result").cloned().unwrap_or(val);

            tracing::info!("Setup completed for plugin '{}'", name);

            if let Some(channel_id) = result.get("channel_id").and_then(|v| v.as_str()).filter(|s| !s.is_empty()) {
                let channel_name = result.get("channel_name").and_then(|v| v.as_str()).unwrap_or("setup");
                let omni_channel_name = format!("mm-{}", channel_name);
                match channels::create_channel(
                    &state.pool,
                    CreateChannelParams {
                        name: omni_channel_name.clone(),
                        platform: "mattermost".to_string(),
                        external_id: channel_id.to_string(),
                        resource_identifier: channel_id.to_string(),
                        cause: "setup".to_string(),
                    },
                ).await {
                    Ok(ch) => tracing::info!(
                        "Created omniagent channel '{}' (id={}) for Mattermost channel '{}'",
                        ch.name, ch.id, channel_name
                    ),
                    Err(e) => tracing::warn!(
                        "Failed to create omniagent channel for Mattermost channel '{}': {:?}",
                        channel_name, e
                    ),
                }
            }

            if let Some(bot_token) = result.get("bot_token").and_then(|v| v.as_str()).filter(|s| !s.is_empty()) {
                let env_key = "MATTERMOST_ACCESS_TOKEN";
                let env_path = state.env_path.clone();
                let env_key_clone = env_key.to_string();
                let token_owned = bot_token.to_string();
                let write_result = tokio::task::spawn_blocking(move || {
                    let content = std::fs::read_to_string(&env_path).unwrap_or_default();
                    let mut lines: Vec<String> = content.lines().map(|l| l.to_string()).collect();
                    let mut found = false;
                    for line in lines.iter_mut() {
                        let trimmed = line.trim();
                        if trimmed.starts_with(&env_key_clone) && trimmed.contains('=') {
                            *line = format!("{}={}", env_key_clone, token_owned);
                            found = true;
                            break;
                        }
                    }
                    if !found {
                        lines.push(format!("{}={}", env_key_clone, token_owned));
                    }
                    let new_content = lines.join("\n") + "\n";
                    std::fs::write(&env_path, new_content).ok();
                })
                .await;
                if write_result.is_ok() {
                    std::env::set_var(env_key, bot_token);
                    tracing::info!("Saved bot access token to .env and process env");
                }
                reload_platform_plugin(&state, &name).await;
            }

            (
                StatusCode::OK,
                Json(serde_json::json!({
                    "success": true,
                    "data": result
                })),
            )
                .into_response()
        }
        Err(e) => {
            let msg = if stdout_output.trim().is_empty() {
                format!("Setup completed but returned no data for '{}'", name)
            } else {
                format!("Setup for '{}' returned unexpected output: {}", name, e)
            };

            tracing::warn!("{}", msg);

            (
                StatusCode::OK,
                Json(serde_json::json!({
                    "success": true,
                    "data": {
                        "raw_output": stdout_output.trim()
                    }
                })),
            )
                .into_response()
        }
    }
}

/// POST /api/plugins/:name/refresh-models — refresh dynamic model list from external API.
pub(crate) async fn refresh_models_handler(
    Path(name): Path<String>,
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

/// DELETE /api/plugins/:name — Remove or Uninstall plugin.
///
/// Default behavior (Remove): Removes YAML entry entirely. For remote, also removes .remote/ dir.
/// `?mode=uninstall` (Uninstall): For remote, removes .remote/ dir but keeps YAML entry
/// (sets enabled=false). For non-remote, removes YAML entry.
pub(crate) async fn delete_plugin_handler(
    Path(name): Path<String>,
    State(state): State<Arc<AppState>>,
    Query(params): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    let data_dir = &state.data_dir;
    let is_uninstall_mode = params.get("mode").map(|s| s == "uninstall").unwrap_or(false);

    if is_uninstall_mode {
        // ── Uninstall mode ──
        // For remote: remove .remote/ dir, set enabled=false (keep YAML entry)
        // For non-remote: remove from YAML (source stays in repo)
        let is_remote = match detect_plugin_category_cross_type(data_dir, &name) {
            Some((_, PluginCategory::Remote)) => true,
            _ => false,
        };

        if is_remote {
            let mut removed_binary = false;
            for t in &["mcp", "platforms", "providers"] {
                let remote_dir = format!("{}/plugins/{}/.remote/{}", data_dir, t, name);
                let remote_path = std::path::Path::new(&remote_dir);
                if remote_path.exists() && remote_path.is_dir() {
                    match std::fs::remove_dir_all(remote_path) {
                        Ok(()) => {
                            tracing::info!("Uninstall: removed .remote/ directory for plugin '{}'", name);
                            removed_binary = true;
                        }
                        Err(e) => {
                            tracing::warn!("Uninstall: failed to remove .remote/ for '{}': {:?}", name, e);
                        }
                    }
                }
            }

            // Set enabled=false in YAML (keep the entry)
            for yaml_type in &[
                plugins_yaml::PluginYamlType::Platform,
                plugins_yaml::PluginYamlType::Tool,
                plugins_yaml::PluginYamlType::Provider,
            ] {
                if let Ok(Some(_)) = plugins_yaml::get_entry(data_dir, yaml_type, &name) {
                    let _ = plugins_yaml::set_enabled(data_dir, yaml_type, &name, false);
                    break;
                }
            }

            return (
                StatusCode::OK,
                Json(serde_json::json!({
                    "success": true,
                    "data": {"uninstalled": true}
                })),
            ).into_response();
        } else {
            // Non-remote: remove from YAML + remove compiled target/ directory

            // Remove target/ directory if it exists (the compiled binary)
            if let Some((yaml_type, _category)) = detect_plugin_category_cross_type(data_dir, &name) {
                let type_dirs = [yaml_type.type_dir_name(), "mcp", "platforms", "providers"];
                let search_dirs = [
                    data_dir,
                    &state.workspace_dir,
                ];
                for type_dir in &type_dirs {
                    for base in &search_dirs {
                        let plugin_dir = format!("{}/plugins/{}/{}", base, type_dir, name);
                        let target_dir = format!("{}/target", plugin_dir);
                        let target_path = std::path::Path::new(&target_dir);
                        if target_path.exists() && target_path.is_dir() {
                            let _ = std::fs::remove_dir_all(target_path);
                            tracing::info!("Uninstall: removed target/ directory at {}", target_dir);
                        }
                    }
                }
            }
            let mut removed = false;
            for yaml_type in &[
                plugins_yaml::PluginYamlType::Platform,
                plugins_yaml::PluginYamlType::Tool,
                plugins_yaml::PluginYamlType::Provider,
            ] {
                if let Ok(true) = plugins_yaml::remove_entry(data_dir, yaml_type, &name) {
                    removed = true;
                }
            }
            if removed {
                return (
                    StatusCode::OK,
                    Json(serde_json::json!({
                        "success": true,
                        "data": {"uninstalled": true}
                    })),
                ).into_response();
            } else {
                return (
                    StatusCode::OK,
                    Json(serde_json::json!({
                        "success": true,
                        "data": {"uninstalled": true, "note": "not found in YAML"}
                    })),
                ).into_response();
            }
        }
    }

    // ── Remove mode (default) —─
    // Remove from YAML (all three types, just in case)
    let mut removed = false;
    for yaml_type in &[
        plugins_yaml::PluginYamlType::Platform,
        plugins_yaml::PluginYamlType::Tool,
        plugins_yaml::PluginYamlType::Provider,
    ] {
        if let Ok(true) = plugins_yaml::remove_entry(data_dir, yaml_type, &name) {
            removed = true;
        }
    }

    // For remote plugins: also remove .remote/ directory
    if let Some((yaml_type, _category)) = detect_plugin_category_cross_type(data_dir, &name) {
        let type_dir = yaml_type.file_name();

        let remote_dir = format!("{}/plugins/{}/.remote/{}", data_dir, type_dir, name);
        let remote_path = std::path::Path::new(&remote_dir);
        if remote_path.exists() && remote_path.is_dir() {
            match std::fs::remove_dir_all(remote_path) {
                Ok(()) => {
                    tracing::info!("Removed .remote/ directory for plugin '{}'", name);
                    removed = true;
                }
                Err(e) => {
                    tracing::warn!("Failed to remove .remote/ directory for '{}': {:?}", name, e);
                }
            }
        }

        for t in &["mcp", "platforms", "providers"] {
            if *t == type_dir { continue; }
            let alt_remote_dir = format!("{}/plugins/{}/.remote/{}", data_dir, t, name);
            let alt_remote_path = std::path::Path::new(&alt_remote_dir);
            if alt_remote_path.exists() && alt_remote_path.is_dir() {
                let _ = std::fs::remove_dir_all(alt_remote_path);
                tracing::info!("Cleaned up stale .remote/ directory at {}", alt_remote_dir);
            }
        }
    }

    if removed {
        info!("Deleted plugin '{}'", name);
        (
            StatusCode::OK,
            Json(serde_json::json!({
                "success": true,
                "data": {"deleted": true}
            })),
        )
            .into_response()
    } else {
        info!("Plugin '{}' not found on disk or in YAML", name);
        (
            StatusCode::OK,
            Json(serde_json::json!({
                "success": true,
                "data": {"deleted": true, "note": "not found in YAML"}
            })),
        )
            .into_response()
    }
}

/// POST /api/plugins/install-url — install a plugin from a URL and register in YAML.
pub(crate) async fn install_url_handler(
    State(state): State<Arc<AppState>>,
    Json(body): Json<InstallUrlRequest>,
) -> impl IntoResponse {
    info!("Installing plugin from URL: {}", body.url);

    // Download and extract
    let manifest = match plugin::installer::install_from_url(&body.url, &state.data_dir) {
        Ok(m) => m,
        Err(e) => {
            error!("Failed to install plugin from {}: {:?}", body.url, e);
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "success": false,
                    "error": format!("Installation failed: {}", e)
                })),
            )
                .into_response();
        }
    };

    // Register in YAML
    let yaml_type = plugins_yaml::PluginYamlType::from_plugin_type(&manifest.plugin_type);
    match plugins_yaml::set_entry(
        &state.data_dir,
        &yaml_type,
        &manifest.name,
        true,
        serde_json::json!({}),
    ) {
        Ok(_entry) => match plugins_yaml::get_plugin(&state.data_dir, &manifest.name) {
            Ok(Some(detail)) => {
                info!(
                    "Successfully installed plugin '{}' version {} from {}",
                    manifest.name, manifest.version, body.url
                );
                (
                    StatusCode::CREATED,
                    Json(serde_json::json!({
                        "success": true,
                        "data": detail
                    })),
                )
                    .into_response()
            }
            _ => {
                info!(
                    "Successfully installed plugin '{}' version {} from {}",
                    manifest.name, manifest.version, body.url
                );
                (
                    StatusCode::CREATED,
                    Json(serde_json::json!({
                        "success": true
                    })),
                )
                    .into_response()
            }
        },
        Err(e) => {
            error!(
                "Installed plugin from disk but failed to register in YAML: {:?}",
                e
            );
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "success": false,
                    "error": format!("Plugin extracted but YAML registration failed: {}", e)
                })),
            )
                .into_response()
        }
    }
}

/// POST /api/plugins/install-git — install a plugin from a git repository.
///
/// Clones DIRECTLY to `data_dir/plugins/<type_dir>/.remote/<name>/` with NO source copying.
/// Compiles if Rust crate, then registers in YAML.
pub(crate) async fn install_git_handler(
    State(state): State<Arc<AppState>>,
    Json(body): Json<InstallGitRequest>,
) -> impl IntoResponse {
    info!(
        "Installing git plugin from {} (ref: {:?})",
        body.url, body.git_ref
    );

    // Clone directly to .remote/ directory (no copy step)
    let initial_name = body.name.as_deref()
        .map(sanitize_plugin_name)
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| "temp-git-plugin".to_string());
    let manifest = match plugin::installer::install_from_git(
        &body.url,
        &initial_name,
        body.git_ref.as_deref(),
        &state.workspace_dir,
        &state.data_dir,
        body.path.as_deref(),
    ) {
        Ok(m) => m,
        Err(e) => {
            error!("Git install failed for {}: {:?}", body.url, e);
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "success": false,
                    "error": format!("Git install failed: {}", e)
                })),
            )
                .into_response();
        }
    };

    let raw_name = body.name.clone().unwrap_or_else(|| manifest.name.clone());
    if let Some(ref requested_name) = body.name {
        if *requested_name != manifest.name {
            tracing::warn!(
                "Requested name '{}' differs from manifest name '{}'. Using manifest name.",
                requested_name, manifest.name
            );
        }
    }

    let actual_name = sanitize_plugin_name(&raw_name);
    if actual_name != raw_name {
        tracing::info!(
            "Sanitized plugin name: '{}' -> '{}'",
            raw_name, actual_name
        );
    }

    // Determine type directory from manifest
    let type_dir_str = match manifest.plugin_type {
        plugin::PluginType::Platform => "platforms",
        plugin::PluginType::Mcp => "mcp",
        plugin::PluginType::Provider => "providers",
    };

    // Compile if Rust crate — compile from .remote/ location
    let plugin_dir = format!(
        "{}/plugins/{}/.remote/{}",
        state.data_dir, type_dir_str, actual_name
    );

    // Check if there's a sub-path within the remote
    let effective_plugin_dir = match body.path {
        Some(ref p) if !p.is_empty() => format!("{}/{}", plugin_dir, p),
        _ => plugin_dir.clone(),
    };

    let cargo_toml = std::path::Path::new(&effective_plugin_dir).join("Cargo.toml");
    if cargo_toml.exists() {
        info!("Git install: compiling Rust crate at {}", effective_plugin_dir);
        match tokio::task::spawn_blocking({
            let dir = effective_plugin_dir.clone();
            let cargo_path = cargo_toml.to_string_lossy().to_string();
            move || {
                let status = std::process::Command::new("cargo")
                    .args(["build", "--release", "--manifest-path", &cargo_path])
                    .current_dir(&dir)
                    .status();
                status
            }
        })
        .await
        {
            Ok(Ok(status)) if status.success() => {
                info!("Git install: Rust compilation succeeded for '{}'", actual_name);
            }
            Ok(Ok(status)) => {
                let msg = format!("Git install: compilation failed for '{}' with exit code {}", actual_name, status);
                tracing::error!("{}", msg);
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({
                        "success": false,
                        "error": msg,
                    })),
                )
                    .into_response();
            }
            Ok(Err(e)) => {
                let msg = format!("Git install: failed to run cargo for '{}': {}", actual_name, e);
                tracing::error!("{}", msg);
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({
                        "success": false,
                        "error": msg,
                    })),
                )
                    .into_response();
            }
            Err(e) => {
                let msg = format!("Git install: task join error for '{}': {}", actual_name, e);
                tracing::error!("{}", msg);
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({
                        "success": false,
                        "error": msg,
                    })),
                )
                    .into_response();
            }
        }
    }

    // Register in YAML with the remote field
    let yaml_type = plugins_yaml::PluginYamlType::from_plugin_type(&manifest.plugin_type);
    let mut remote_val_map = serde_json::Map::new();
    remote_val_map.insert("url".to_string(), serde_json::json!(body.url));
    if let Some(ref git_ref) = body.git_ref {
        remote_val_map.insert("git_ref".to_string(), serde_json::json!(git_ref));
    }
    if let Some(ref path) = body.path {
        remote_val_map.insert("path".to_string(), serde_json::json!(path));
    }
    let remote_val = serde_json::Value::Object(remote_val_map);

    match plugins_yaml::set_entry_with_remote(
        &state.data_dir,
        &yaml_type,
        &actual_name,
        false,
        serde_json::json!({}),
        Some(&serde_json::from_value(remote_val).unwrap()),
    ) {
        Ok(_entry) => match plugins_yaml::get_plugin(&state.data_dir, &actual_name) {
            Ok(Some(detail)) => {
                info!(
                    "Successfully installed git plugin '{}' version {} from {}",
                    actual_name, manifest.version, body.url
                );
                (
                    StatusCode::CREATED,
                    Json(serde_json::json!({
                        "success": true,
                        "data": detail
                    })),
                )
                    .into_response()
            }
            _ => {
                info!(
                    "Successfully installed git plugin '{}' version {} from {}",
                    actual_name, manifest.version, body.url
                );
                (
                    StatusCode::CREATED,
                    Json(serde_json::json!({
                        "success": true
                    })),
                )
                    .into_response()
            }
        },
        Err(e) => {
            error!(
                "Installed git plugin from disk but failed to register in YAML: {:?}",
                e
            );
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "success": false,
                    "error": format!("Plugin cloned but YAML registration failed: {}", e)
                })),
            )
                .into_response()
        }
    }
}

/// POST /api/reload — reload environment variables from .env file.
pub(crate) async fn reload_env_handler(
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    let refreshed = refresh_env_from_file(&state.env_path);
    (
        StatusCode::OK,
        Json(serde_json::json!({
            "success": true,
            "refreshed": refreshed,
        })),
    )
        .into_response()
}

/// Refresh the process environment by re-reading the .env file.
/// Returns the number of env vars that were refreshed.
pub fn refresh_env_from_file(env_path: &str) -> u32 {
    match std::fs::read_to_string(env_path) {
        Ok(content) => {
            let mut refreshed = 0u32;
            for line in content.lines() {
                let line = line.trim();
                if line.is_empty() || line.starts_with('#') {
                    continue;
                }
                if let Some((key, value)) = line.split_once('=') {
                    let k = key.trim();
                    let v = value.trim();
                    if !k.is_empty() {
                        std::env::set_var(k, v);
                        refreshed += 1;
                    }
                }
            }
            refreshed
        }
        Err(e) => {
            tracing::warn!(
                "Could not read .env at '{}' for env refresh: {:?}",
                env_path,
                e
            );
            0
        }
    }
}

/// Trigger a hot-reload of a platform plugin after its config has been updated.
async fn reload_platform_plugin(state: &Arc<AppState>, name: &str) {
    tracing::info!("Reloading platform plugin '{}' after config update", name);

    let refreshed = refresh_env_from_file(&state.env_path);
    if refreshed > 0 {
        tracing::info!(
            "Refreshed {} env var(s) from .env for platform plugin reload",
            refreshed
        );
    }

    let signal = {
        let signals = state.platform_restart_signals.lock().await;
        signals.get(name).cloned()
    };

    if let Some((restart_flag, restart_notify)) = signal {
        restart_flag.store(true, Ordering::SeqCst);
        restart_notify.notify_one();
        tracing::info!(
            "Set restart flag for platform plugin '{}' — subprocess will be respawned",
            name
        );
    } else {
        tracing::warn!(
            "Platform plugin '{}' is not currently registered — restart flag not found. \
             The new config will take effect on next omniagent start.",
            name
        );
    }
}

/// Trigger a hot-reload of a tool (MCP) plugin after its config has been updated.
async fn reload_tool_plugin(state: &Arc<AppState>, name: &str) {
    tracing::info!("Reloading tool plugin '{}' after config update", name);

    let refreshed = refresh_env_from_file(&state.env_path);
    if refreshed > 0 {
        tracing::info!(
            "Refreshed {} env var(s) from .env for tool plugin reload",
            refreshed
        );
    }

    crate::mcp::external::client::clear_server_pools(name);
    crate::mcp::external::client::remove_server_config(name);

    match crate::mcp::external::client::initialize_single_server_tools(
        &state.data_dir,
        &state.workspace_dir,
        name,
    )
    .await
    {
        Ok(tools) => {
            let count = tools.len();
            state.mcp_registry.write().unwrap().remove_by_server(name);
            state.mcp_registry.write().unwrap().register_all(tools);
            tracing::info!(
                "Hot-reloaded {} tool(s) from MCP server '{}' after config update (no restart needed)",
                count,
                name
            );
        }
        Err(e) => {
            tracing::warn!(
                "Hot-reload of MCP server '{}' after config update failed (config saved, will retry on next restart): {}",
                name,
                e
            );
        }
    }
}

/// Sanitize a plugin name for use as a YAML key and directory path.
/// - Trims whitespace
/// - NFD-normalizes to decompose diacritics
/// - Converts to lowercase
/// - Replaces runs of whitespace with a single hyphen
/// - Strips any character that isn't alphanumeric, hyphen, or underscore
fn sanitize_plugin_name(name: &str) -> String {
    use unicode_normalization::UnicodeNormalization;

    let trimmed = name.trim();
    let mut result = String::with_capacity(trimmed.len());
    let mut in_whitespace = false;

    for ch in trimmed.nfd() {
        // Skip combining diacritical marks
        let code = ch as u32;
        if (0x0300..=0x036F).contains(&code)
            || (0x1AB0..=0x1AFF).contains(&code)
            || (0x1DC0..=0x1DFF).contains(&code)
            || (0x20D0..=0x20FF).contains(&code)
            || (0xFE20..=0xFE2F).contains(&code)
        {
            continue;
        }

        if ch.is_whitespace() {
            if !in_whitespace {
                result.push('-');
                in_whitespace = true;
            }
        } else if ch.is_alphanumeric() || ch == '-' || ch == '_' {
            for lower in ch.to_lowercase() {
                result.push(lower);
            }
            in_whitespace = false;
        } else {
            in_whitespace = false;
        }
    }
    result
}
