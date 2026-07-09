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
    http::{StatusCode, Response},
    response::IntoResponse,
    routing::{delete, get, post},
    body::Body,
    Json, Router,
};
use serde::Deserialize;
use sql_forge::sql_forge;
use sqlx;
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
    pub source: Option<String>,
    /// Optional remote config to set when enabling a remote source.
    /// When source is "remote" and this is provided, the remote URL/path
    /// is written to the YAML entry. Required when re-enabling a remote
    /// source after it was previously cleared (by switching to built-in
    /// or bundled).
    #[serde(default)]
    pub remote: Option<plugins_yaml::PluginRemote>,
}

/// Validate that a source was provided. Returns an error response if missing.
pub(crate) fn require_source(source: &Option<String>) -> Result<&str, (StatusCode, Json<serde_json::Value>)> {
    match source.as_deref() {
        Some(s) => Ok(s),
        None => Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "success": false,
                "error": "Source is required. Provide a `source` parameter: 'built-in', 'bundled', or 'remote'."
            })),
        )),
    }
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

#[derive(Deserialize)]
pub(crate) struct RenameRequest {
    /// The new name for the plugin.
    pub new_name: String,
}

// ---------------------------------------------------------------------------
// Plugin type detection helpers
// ---------------------------------------------------------------------------

/// The three plugin location types.
#[derive(Debug, Clone)]
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
    // Check YAML entry first — source field is authoritative
    if let Ok(Some(entry)) = plugins_yaml::get_entry(data_dir, yaml_type, name) {
        match entry.source.as_str() {
            "built-in" => return PluginCategory::Builtin,
            "remote" => return PluginCategory::Remote,
            _ => return PluginCategory::OmniStack,
        }
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
fn detect_plugin_category_cross_type(
    data_dir: &str,
    name: &str,
) -> Option<(plugins_yaml::PluginYamlType, PluginCategory)> {
    for pt in &[
        plugins_yaml::PluginYamlType::Tool,
        plugins_yaml::PluginYamlType::Platform,
        plugins_yaml::PluginYamlType::Provider,
    ] {
        // Check YAML entry
        if let Ok(Some(entry)) = plugins_yaml::get_entry(data_dir, pt, name) {
            let cat = match entry.source.as_str() {
                "built-in" => PluginCategory::Builtin,
                "remote" => PluginCategory::Remote,
                _ => PluginCategory::OmniStack,
            };
            return Some((pt.clone(), cat));
        }
    }
    // No YAML entry → return None. The caller (install/reinstall handler) has its own
    // multi-source fallback logic that checks all physical locations independently.
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
        pkg_name
            .as_ref()
            .and_then(|p| crate::mcp::external::config::get_bin_path(p))
            .filter(|p| std::path::Path::new(p).exists())
            .is_some()
    } else {
        let local = pkg_name
            .as_ref()
            .map(|p| format!("{}/target/release/{}", plugin_dir, p))
            .unwrap_or_else(|| format!("{}/target/release/{}", plugin_dir, name));
        std::path::Path::new(&local).exists()
    };

    if bin_exists {
        info!(
            "Compile: binary for '{}' already exists — no compilation needed",
            name
        );
        return Ok(true);
    }

    // Build from the EXACT source — no guessing
    if source == "built-in" {
        // Workspace build (all builtins are workspace members)
        if let Some(ref pn) = pkg_name {
            let mut cmd = tokio::process::Command::new("cargo");
            cmd.args([
                "build",
                "--release",
                "--manifest-path",
                "/app/Cargo.toml",
                "-p",
                pn,
            ]);
            cmd.current_dir("/app");
            let st = cmd
                .status()
                .await
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
        let st = cmd
            .status()
            .await
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

/// Result of resolving a plugin's source directory for compile operations.
struct ResolvedPlugin {
    yaml_type: plugins_yaml::PluginYamlType,
    category: PluginCategory,
    plugin_dir: String,
    has_cargo_toml: bool,
    has_entrypoint: bool,
}

/// Shared preamble for install/reinstall: detect type, resolve directory,
/// handle remote subpath, verify source code availability.
/// Returns a resolved plugin ready for compilation, or an HTTP error response.
async fn resolve_plugin_for_compile(
    data_dir: &str,
    workspace_dir: &str,
    name: &str,
    handler_name: &str,
    explicit_source: Option<&str>,
) -> Result<ResolvedPlugin, (StatusCode, Json<serde_json::Value>)> {
    let (yaml_type, category) = if let Some(source) = explicit_source {
        // Use the explicit source to determine category, skipping auto-detection
        let yaml_type = match plugins_yaml::get_disk_plugin_type(data_dir, name) {
            Ok(Some(t)) => plugins_yaml::PluginYamlType::from_type_str(&t),
            _ => {
                let err = Json(serde_json::json!({
                    "success": false,
                    "error": format!("Plugin '{}' not found on disk", name)
                }));
                return Err((StatusCode::NOT_FOUND, err));
            }
        };
        let category = match source {
            "built-in" => PluginCategory::Builtin,
            "remote" => PluginCategory::Remote,
            "bundled" => PluginCategory::OmniStack,
            _ => {
                let err = Json(serde_json::json!({
                    "success": false,
                    "error": format!("Invalid source '{}': must be 'built-in', 'bundled', or 'remote'", source)
                }));
                return Err((StatusCode::BAD_REQUEST, err));
            }
        };
        (yaml_type, category)
    } else {
        match detect_plugin_category_cross_type(data_dir, name) {
            Some((t, c)) => {
                info!(
                    "{}: detected '{}' as type={:?} category={:?}",
                    handler_name, name, t, c
                );
                (t, c)
            }
            None => {
                // Try to determine type from disk discovery
                let disk_type = match plugins_yaml::get_disk_plugin_type(data_dir, name) {
                    Ok(Some(t)) => plugins_yaml::PluginYamlType::from_type_str(&t),
                    _ => {
                        let err = Json(serde_json::json!({
                            "success": false,
                            "error": format!("Plugin '{}' not found on disk", name)
                        }));
                        return Err((StatusCode::NOT_FOUND, err));
                    }
                };
                let category = if plugins_yaml::has_remote_entry(data_dir, &disk_type, name) {
                    info!(
                        "{}: detected '{}' as remote plugin from remote.yml",
                        handler_name, name
                    );
                    PluginCategory::Remote
                } else if plugins_yaml::is_plugin_builtin(data_dir, name, &disk_type) {
                    PluginCategory::Builtin
                } else {
                    PluginCategory::OmniStack
                };
                (disk_type, category)
            }
        }
    };

    // 2. Get plugin source directory with Builtin fallback
    let (mut plugin_dir, mut category) =
        match get_plugin_dir_for_category(data_dir, workspace_dir, &yaml_type, name, &category) {
            Some(d) => (d, category),
            None => {
                // Fallback: try Builtin source before giving up
                if !matches!(category, PluginCategory::Builtin) {
                    let mut found_builtin_dir = None;
                    let builtin_dir =
                        format!("/app/plugins/{}/{}", yaml_type.type_dir_name(), name);
                    if std::path::Path::new(&builtin_dir)
                        .join("Cargo.toml")
                        .exists()
                    {
                        found_builtin_dir = Some(builtin_dir);
                    } else if matches!(yaml_type, plugins_yaml::PluginYamlType::Tool) {
                        let legacy_dir = format!("/app/plugins/mcp/{}", name);
                        if std::path::Path::new(&legacy_dir)
                            .join("Cargo.toml")
                            .exists()
                        {
                            found_builtin_dir = Some(legacy_dir);
                        }
                    }
                    if let Some(dir) = found_builtin_dir {
                        info!(
                            "{}: falling back to built-in source for '{}'",
                            handler_name, name
                        );
                        (dir, PluginCategory::Builtin)
                    } else if matches!(category, PluginCategory::Remote) {
                        let err = Json(serde_json::json!({
                            "success": false,
                            "error": format!("Remote plugin '{}' has not been cloned yet. Use /api/plugins/install-git first.", name)
                        }));
                        return Err((StatusCode::BAD_REQUEST, err));
                    } else {
                        let err = Json(serde_json::json!({
                            "success": false,
                            "error": format!("Plugin '{}' source directory not found", name)
                        }));
                        return Err((StatusCode::NOT_FOUND, err));
                    }
                } else {
                    let err = Json(serde_json::json!({
                        "success": false,
                        "error": format!("Plugin '{}' source directory not found", name)
                    }));
                    return Err((StatusCode::NOT_FOUND, err));
                }
            }
        };

    // Check if there's actual source code to work with
    // For remote plugins with a path subdirectory, also check the subpath
    let mut dir_path = std::path::Path::new(&plugin_dir).to_path_buf();
    if matches!(category, PluginCategory::Remote) {
        if let Some(remote) = plugins_yaml::get_remote_plugin(data_dir, &yaml_type, name) {
            if let Some(ref subpath) = remote.path {
                let sub = dir_path.join(subpath);
                if sub.join("Cargo.toml").exists() || sub.join("plugin.json").exists() {
                    dir_path = sub;
                    plugin_dir = dir_path.to_string_lossy().to_string();
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

    // If the source dir has no Cargo.toml, fall back to Builtin
    if !has_cargo_toml && !matches!(category, PluginCategory::Builtin) {
        let builtin_dir = format!("/app/plugins/{}/{}", yaml_type.type_dir_name(), name);
        let builtin_cargo = std::path::Path::new(&builtin_dir).join("Cargo.toml");
        if builtin_cargo.exists() {
            info!(
                "{}: falling back to built-in source for '{}' (dir has no Cargo.toml)",
                handler_name, name
            );
            plugin_dir = builtin_dir;
            category = PluginCategory::Builtin;
            has_cargo_toml = true;
        }
    }

    if !has_cargo_toml && !has_entrypoint {
        let err = Json(serde_json::json!({
            "success": false,
            "error": format!("Plugin '{}' has no source code — only a pre-built binary exists. {} requires source code (Cargo.toml or plugin.json entrypoint).", name, handler_name)
        }));
        return Err((StatusCode::BAD_REQUEST, err));
    }

    info!(
        "{}: {} plugin '{}' from {} (category: {:?})",
        handler_name,
        yaml_type.file_name(),
        name,
        plugin_dir,
        category
    );

    Ok(ResolvedPlugin {
        yaml_type,
        category,
        plugin_dir,
        has_cargo_toml,
        has_entrypoint,
    })
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
        .route(
            "/api/plugins/{name}/reinstall",
            post(reinstall_plugin_handler),
        )
        .route(
            "/api/plugins/{name}/refresh-models",
            post(refresh_models_handler),
        )
        .route("/api/plugins/{name}/setup", post(setup_plugin_handler))
        .route(
            "/api/plugins/{name}/download",
            post(download_plugin_handler),
        )
        .route("/api/plugins/{name}/rename", post(rename_plugin_handler))
        .route("/api/plugins/{name}", delete(delete_plugin_handler))
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
            // if a plugin is marked "enabled" but its server has zero
            // registered tools, the server failed to initialize — set
            // status to "error" so the frontend shows the right badge.
            {
                let registry = state.tool_registry.read().await;
                let all_tools = registry.all();
                for detail in details.iter_mut() {
                    if detail.plugin_type == "tool" && detail.status == "enabled" {
                        let has_tools = all_tools
                            .iter()
                            .any(|t| t.server_name.as_deref() == Some(&detail.name));
                        if !has_tools {
                            detail.status = "error".to_string();
                            let no_source = !detail.has_source_code;
                            let no_binary_note = if no_source {
                                " — no source code (no Cargo.toml) and pre-compiled binary not found"
                            } else {
                                " — binary may not have compiled successfully"
                            };
                            detail.status_message =
                                format!("MCP server failed to start{}", no_binary_note);
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
            // registered tools, the server failed to initialize — set
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
                        " — no source code (no Cargo.toml) and pre-compiled binary not found"
                    } else {
                        " — binary may not have compiled successfully"
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

    // Check if plugin already in the desired state — skip only if source matches
    if let Ok(Some(entry)) = plugins_yaml::get_entry(&state.data_dir, &yaml_type, &name) {
        if entry.enabled && entry.source == source {
            // Already enabled with matching source — no change needed
            if let Ok(Some(detail)) = plugins_yaml::get_plugin(&state.data_dir, &name) {
                info!(
                    "Plugin '{}' is already enabled with matching source — no change needed",
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
                    let _ = plugins_yaml::save_remote_plugin(
                        &state.data_dir,
                        &yaml_type,
                        &name,
                        remote,
                    );
                }
            }
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
                        state.tool_registry.write().await.register_all(tools);
                        info!(
                            "Hot-reloaded {} tool(s) from MCP server '{}' (no restart needed)",
                            count, name
                        );
                    }
                    Err(e) => {
                        // MCP server failed to start — roll back the YAML enable
                        // so the plugin doesn't show as "enabled" when it can't serve tools.
                        tracing::warn!(
                            "Hot-reload of MCP server '{}' failed, rolling back enable: {}",
                            name,
                            e
                        );
                        let _ = plugins_yaml::remove_entry(
                            &state.data_dir,
                            &yaml_type,
                            &name,
                        );
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
                            let mut registry = crate::provider::registry::PROVIDER_REGISTRY.write().unwrap();
                            registry.register(&name, &command, &args);
                        }
                        // Start the subprocess (async — drop registry lock first to avoid Send issues)
                        let start_result = {
                            let registry = crate::provider::registry::PROVIDER_REGISTRY.read().unwrap();
                            registry.get_cloned(&name)
                        };
                        // Registry guard dropped — we have an independent Arc
                        match start_result {
                            Some(client) => {
                                if let Err(e) = client.start().await {
                                    tracing::warn!(
                                        "Failed to start external provider '{}', rolling back enable: {}",
                                        name, e
                                    );
                                    {
                                        let mut registry = crate::provider::registry::PROVIDER_REGISTRY.write().unwrap();
                                        registry.remove(&name);
                                    }
                                    let _ = plugins_yaml::remove_entry(&state.data_dir, &yaml_type, &name);
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

    // Upsert with enabled=false — preserve existing source field
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
    body: Option<Json<PluginSourceRequest>>,
) -> impl IntoResponse {
    let data_dir = &state.data_dir;
    let explicit_source = body.as_ref().and_then(|b| b.source.as_deref());

    // Require source parameter
    let source = match explicit_source {
        Some(s) => s,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "success": false,
                    "error": "Source is required. Provide a `source` parameter: 'built-in', 'bundled', or 'remote'."
                })),
            )
                .into_response();
        }
    };

    // 1. Resolve plugin source via shared preamble (detect type, resolve dir, verify source)
    let resolved = match resolve_plugin_for_compile(
        data_dir,
        &state.workspace_dir,
        &name,
        "Install",
        Some(source),
    )
    .await
    {
        Ok(r) => r,
        Err(response) => return response.into_response(),
    };

    let yaml_type = resolved.yaml_type;
    let category = resolved.category.clone();
    let yaml_category = category.clone(); // preserve original for YAML
    let plugin_dir = resolved.plugin_dir;
    let category_source = category_to_source(&category);
    let yaml_source = category_to_source(&yaml_category);

    // 2. Compile FIRST — synchronous, no background spawn
    info!(
        "Install: compiling plugin '{}' from {} (source: {})",
        name, plugin_dir, category_source
    );
    match compile_rust_crate(&plugin_dir, &name, category_source).await {
        Ok(true) | Ok(false) => {
            info!("Install: compilation succeeded for '{}'", name);
        }
        Err(e) => {
            let msg = format!("Install: compilation failed for '{}': {}", name, e);
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

    // 4. Register in YAML with enabled=true only after successful compile
    info!(
        "Install: registering plugin '{}' in YAML with enabled=true",
        name
    );
    match plugins_yaml::set_entry_with_source(
        data_dir,
        &yaml_type,
        &name,
        true,
        yaml_source,
        serde_json::json!({}),
    ) {
        Ok(_entry) => {
            // 5. Hot-reload the tool plugin so the MCP server starts immediately
            if yaml_type == plugins_yaml::PluginYamlType::Tool {
                reload_tool_plugin(&state, &name).await;
            }

            // 6. Return the installed plugin detail
            match plugins_yaml::get_plugin(data_dir, &name) {
                Ok(Some(detail)) => {
                    info!("Installed plugin '{}' successfully", name);
                    (
                        StatusCode::OK,
                        Json(serde_json::json!({
                            "success": true,
                            "data": detail,
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
    body: Option<Json<PluginSourceRequest>>,
) -> impl IntoResponse {
    let data_dir = &state.data_dir;
    let explicit_source = body.as_ref().and_then(|b| b.source.as_deref());

    // Require source parameter
    let source = match explicit_source {
        Some(s) => s,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "success": false,
                    "error": "Source is required. Provide a `source` parameter: 'built-in', 'bundled', or 'remote'."
                })),
            )
                .into_response();
        }
    };

    // 1. Resolve plugin source via shared preamble (detect type, resolve dir, verify source)
    let resolved = match resolve_plugin_for_compile(
        data_dir,
        &state.workspace_dir,
        &name,
        "Reinstall",
        Some(source),
    )
    .await
    {
        Ok(r) => r,
        Err(response) => return response.into_response(),
    };

    let _yaml_type = resolved.yaml_type;
    let category = resolved.category.clone();
    let plugin_dir = resolved.plugin_dir;
    let _source = category_to_source(&category);

    // Note: For remote plugins, this does NOT re-clone the git repository.
    // It only recompiles the existing source code in .remote/<name>/.
    // To update from git, use the Download endpoint instead.

    // 2. Compile
    let compiled = match compile_rust_crate(&plugin_dir, &name, category_to_source(&category)).await
    {
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
            format!("{}/plugins/tools/{}", data_dir, name),
            format!("{}/plugins/providers/{}", data_dir, name),
            format!("/app/plugins/platforms/{}", name),
            format!("/app/plugins/tools/{}", name),
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
                if let Some(bin_path) =
                    crate::mcp::external::config::get_bin_path(&entrypoint.command)
                {
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
    if let Ok(manifest) = serde_json::from_value::<plugin::PluginManifest>(detail.manifest.clone())
    {
        for (env_key, env_val) in &manifest.env {
            let resolved = if let Some(var_name) =
                env_val.strip_prefix("${").and_then(|s| s.strip_suffix('}'))
            {
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
            if !v.is_empty() {
                return v.clone();
            }
        }
        if let Some(raw) = config
            .get(key)
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
        {
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

    let request_str = serde_json::to_string(&request_body).unwrap_or_else(|_| "{}".to_string());

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
        .env("OMNI_DIR", &state.data_dir)
        .env("MATTERMOST_SERVER_URL", &setup_val("server_url"))
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
        if let Err(e) = writeln!(
            stdin,
            "{}",
            serde_json::to_string(&init_req).unwrap_or_default()
        ) {
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
        if let Err(e) = writeln!(
            stdin,
            "{}",
            serde_json::to_string(&configure_req).unwrap_or_default()
        ) {
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

            if let Some(channel_id) = result
                .get("channel_id")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
            {
                let channel_name = result
                    .get("channel_name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("setup");
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
                )
                .await
                {
                    Ok(ch) => tracing::info!(
                        "Created omniagent channel '{}' (id={}) for Mattermost channel '{}'",
                        ch.name,
                        ch.id,
                        channel_name
                    ),
                    Err(e) => tracing::warn!(
                        "Failed to create omniagent channel for Mattermost channel '{}': {:?}",
                        channel_name,
                        e
                    ),
                }
            }

            if let Some(bot_token) = result
                .get("bot_token")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
            {
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
    let is_uninstall_mode = params
        .get("mode")
        .map(|s| s == "uninstall")
        .unwrap_or(false);
    let explicit_source = params.get("source").map(|s| s.as_str());

    if is_uninstall_mode {
        // ── Uninstall mode ──
        // For remote: remove target/ dir, set enabled=false (keep .remote/ source)
        // For non-remote: remove from YAML + remove compiled target/ directory
        let is_remote = if let Some(source) = explicit_source {
            source == "remote"
        } else {
            matches!(
                detect_plugin_category_cross_type(data_dir, &name),
                Some((_, PluginCategory::Remote))
            )
        };

        if is_remote {
            // Remove target/ directory (compiled binary), keep .remote/ source
            if let Some((yaml_type, _category)) = detect_plugin_category_cross_type(data_dir, &name)
            {
                let type_dir = yaml_type.file_name();
                // Remove base target/ dir
                let base_dir = format!("{}/plugins/{}/.remote/{}", data_dir, type_dir, name);
                let target_dir = format!("{}/target", base_dir);
                let target_path = std::path::Path::new(&target_dir);
                if target_path.exists() && target_path.is_dir() {
                    let _ = std::fs::remove_dir_all(target_path);
                    tracing::info!(
                        "Uninstall: removed target/ directory for remote plugin '{}'",
                        name
                    );
                }

                // Also remove target/ in the remote.yml subpath (e.g. tools/test-rust-tool)
                if let Some(remote) = plugins_yaml::get_remote_plugin(data_dir, &yaml_type, &name) {
                    if let Some(ref subpath) = remote.path {
                        if !subpath.is_empty() {
                            let sub_target = format!("{}/{}/target", base_dir, subpath);
                            let sub_path = std::path::Path::new(&sub_target);
                            if sub_path.exists() && sub_path.is_dir() {
                                let _ = std::fs::remove_dir_all(sub_path);
                                tracing::info!(
                                    "Uninstall: removed target/ directory at subpath '{}' for remote plugin '{}'",
                                    subpath, name
                                );
                            }
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

            // Stop the MCP server and remove its tools from the registry
            tracing::info!(
                "Uninstall: stopping MCP server for remote plugin '{}'",
                name
            );
            crate::mcp::external::client::clear_server_pools(&name);
            crate::mcp::external::client::remove_server_config(&name);
            state.tool_registry.write().await.remove_by_server(&name);

            return (
                StatusCode::OK,
                Json(serde_json::json!({
                    "success": true,
                    "data": {"uninstalled": true}
                })),
            )
                .into_response();
        } else {
            // Non-remote: remove from YAML + remove compiled target/ directory

            // Remove target/ directory if it exists (the compiled binary)
            if let Some((yaml_type, _category)) = detect_plugin_category_cross_type(data_dir, &name)
            {
                let type_dirs = [yaml_type.type_dir_name(), "tools", "platforms", "providers"];
                let search_dirs = [data_dir, &state.workspace_dir];
                for type_dir in &type_dirs {
                    for base in &search_dirs {
                        let plugin_dir = format!("{}/plugins/{}/{}", base, type_dir, name);
                        let target_dir = format!("{}/target", plugin_dir);
                        let target_path = std::path::Path::new(&target_dir);
                        if target_path.exists() && target_path.is_dir() {
                            let _ = std::fs::remove_dir_all(target_path);
                            tracing::info!(
                                "Uninstall: removed target/ directory at {}",
                                target_dir
                            );
                        }
                    }
                }
            }

            // Stop MCP server if it was running
            tracing::info!(
                "Uninstall: stopping MCP server for non-remote plugin '{}'",
                name
            );
            crate::mcp::external::client::clear_server_pools(&name);
            crate::mcp::external::client::remove_server_config(&name);
            state.tool_registry.write().await.remove_by_server(&name);
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
                )
                    .into_response();
            } else {
                return (
                    StatusCode::OK,
                    Json(serde_json::json!({
                        "success": true,
                        "data": {"uninstalled": true, "note": "not found in YAML"}
                    })),
                )
                    .into_response();
            }
        }
    }

    // ── Remove mode (default) —─
    // Source is required for all remove operations.
    match &explicit_source {
        Some(source) => {
            return handle_remove_by_source(data_dir, &state.workspace_dir, &name, source, &state)
                .await
                .into_response();
        }
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "success": false,
                    "error": "Source is required. Provide a `source` query parameter: 'built-in', 'bundled', or 'remote'."
                })),
            )
                .into_response();
        }
    }

    // The following auto-detection code is preserved for reference but
    // unreachable — source must be provided explicitly.
    #[allow(unreachable_code)]
    {
    let mut removed = false;

    // 1. Find YAML entry (if any)
    let mut yaml_info: Option<(plugins_yaml::PluginYamlType, String)> = None; // (type, source)
    for pt in &[
        plugins_yaml::PluginYamlType::Platform,
        plugins_yaml::PluginYamlType::Tool,
        plugins_yaml::PluginYamlType::Provider,
    ] {
        if let Ok(Some(entry)) = plugins_yaml::get_entry(data_dir, pt, &name) {
            yaml_info = Some((pt.clone(), entry.source.clone()));
            break;
        }
    }

    // 2. Check if plugin exists as built-in on disk
    let on_disk_builtin = plugins_yaml::is_plugin_builtin(data_dir, &name, &plugins_yaml::PluginYamlType::Tool)
        || plugins_yaml::is_plugin_builtin(data_dir, &name, &plugins_yaml::PluginYamlType::Platform)
        || plugins_yaml::is_plugin_builtin(data_dir, &name, &plugins_yaml::PluginYamlType::Provider);

    // 3. Check if plugin exists as bundled on disk
    let on_disk_bundled = {
        let dirs = ["tools", "platforms", "providers"];
        dirs.iter().any(|d| {
            let p = format!("{}/plugins/{}/{}", state.workspace_dir, d, name);
            std::path::Path::new(&p).join("plugin.json").exists()
        })
    };

    // 4. Check if plugin exists as remote on disk (remote.yml is the source of truth)
    let on_disk_remote = {
        let ryml = plugins_yaml::load_remote_plugins(data_dir);
        ryml.tools.as_ref().map(|m| m.contains_key(&name)).unwrap_or(false)
            || ryml.platforms.as_ref().map(|m| m.contains_key(&name)).unwrap_or(false)
            || ryml.providers.as_ref().map(|m| m.contains_key(&name)).unwrap_or(false)
    };

    let yaml_source = yaml_info.as_ref().map(|(_, s)| s.as_str());

    // ── Rule 1: Built-in plugins (on disk) cannot be removed ──
    // If YAML says built-in AND it exists on disk → error
    // If no YAML but is built-in on disk → error
    if yaml_source == Some("built-in") && on_disk_builtin {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "success": false,
                "error": format!("Cannot remove built-in plugin '{}'. Built-in plugins are part of the application and can only be disabled.", name)
            })),
        ).into_response();
    }
    if yaml_source.is_none() && on_disk_builtin {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "success": false,
                "error": format!("Cannot remove built-in plugin '{}'. Built-in plugins are part of the application and can only be disabled.", name)
            })),
        ).into_response();
    }

    // ── Rule 2: YAML says built-in but not on disk → just remove YAML entry ──
    // This handles: plugin was registered as built-in but the source was removed.
    if yaml_source == Some("built-in") {
        let (yaml_type, _) = yaml_info.as_ref().unwrap();
        if let Ok(true) = plugins_yaml::remove_entry(data_dir, yaml_type, &name) {
            tracing::info!("Remove: removed YAML entry for built-in plugin '{}' (source not on disk)", name);
        }
        removed = true;
    }

    // ── Rule 3: Remote plugin ──
    // Remove .remote/ directory + remote.yml entry + YAML entry (if source matches)
    if yaml_source == Some("remote") || (yaml_source.is_none() && on_disk_remote) {
        let yaml_type = yaml_info.as_ref().map(|(t, _)| t.clone());
        let actual_type = match yaml_type {
            Some(ref t) => t.clone(),
            None => {
                // Detect type from disk
                let dirs = [
                    (plugins_yaml::PluginYamlType::Tool, "tools"),
                    (plugins_yaml::PluginYamlType::Platform, "platforms"),
                    (plugins_yaml::PluginYamlType::Provider, "providers"),
                ];
                let found = dirs.iter().find(|(_, d)| {
                    let p = format!("{}/plugins/{}/.remote/{}", data_dir, d, name);
                    std::path::Path::new(&p).join("plugin.json").exists()
                });
                match found {
                    Some((t, _)) => t.clone(),
                    None => plugins_yaml::PluginYamlType::Tool,
                }
            }
        };

        let type_dir = actual_type.type_dir_name();

        // Remove .remote/ directory
        let remote_dir = format!("{}/plugins/{}/.remote/{}", data_dir, type_dir, name);
        let remote_path = std::path::Path::new(&remote_dir);
        if remote_path.exists() && remote_path.is_dir() {
            match std::fs::remove_dir_all(remote_path) {
                Ok(()) => {
                    tracing::info!("Remove: removed .remote/ directory for plugin '{}'", name);
                    removed = true;
                }
                Err(e) => {
                    tracing::warn!("Remove: failed to remove .remote/ directory for '{}': {:?}", name, e);
                }
            }
        }

        // Remove remote.yml entry (for all types)
        for pt in &[
            plugins_yaml::PluginYamlType::Platform,
            plugins_yaml::PluginYamlType::Tool,
            plugins_yaml::PluginYamlType::Provider,
        ] {
            let _ = plugins_yaml::remove_remote_plugin(data_dir, pt, &name);
        }

        // Remove stale .remote/ directories from other types
        for t in &["tools", "platforms", "providers"] {
            if *t == type_dir { continue; }
            let alt = format!("{}/plugins/{}/.remote/{}", data_dir, t, name);
            let alt_path = std::path::Path::new(&alt);
            if alt_path.exists() && alt_path.is_dir() {
                let _ = std::fs::remove_dir_all(alt_path);
                tracing::info!("Remove: cleaned up stale .remote/ directory at {}", alt);
            }
        }

        // Remove YAML entry ONLY if source matches "remote"
        if yaml_source == Some("remote") {
            let _ = plugins_yaml::remove_entry(data_dir, &actual_type, &name);
            tracing::info!("Remove: removed YAML entry for remote plugin '{}'", name);
            removed = true;
        }

        // Stop MCP server
        tracing::info!("Remove: stopping MCP server for plugin '{}'", name);
        crate::mcp::external::client::clear_server_pools(&name);
        crate::mcp::external::client::remove_server_config(&name);
        state.tool_registry.write().await.remove_by_server(&name);

        if removed {
            info!("Deleted remote plugin '{}'", name);
            return (StatusCode::OK, Json(serde_json::json!({
                "success": true, "data": {"deleted": true}
            }))).into_response();
        }
    }

    // ── Rule 4: Bundled (omni-stack) plugin ──
    // Remove workspace directory + YAML entry (if source matches bundled)
    if yaml_source == Some("bundled") || yaml_source.as_deref() == Some("omni-stack")
        || (yaml_source.is_none() && on_disk_bundled)
    {
        let yaml_type = yaml_info.as_ref().map(|(t, _)| t.clone());
        let actual_type = match yaml_type {
            Some(ref t) => t.clone(),
            None => {
                let dirs = [
                    (plugins_yaml::PluginYamlType::Tool, "tools"),
                    (plugins_yaml::PluginYamlType::Platform, "platforms"),
                    (plugins_yaml::PluginYamlType::Provider, "providers"),
                ];
                let found = dirs.iter().find(|(_, d)| {
                    let p = format!("{}/plugins/{}/{}", state.workspace_dir, d, name);
                    std::path::Path::new(&p).join("plugin.json").exists()
                });
                match found {
                    Some((t, _)) => t.clone(),
                    None => plugins_yaml::PluginYamlType::Tool,
                }
            }
        };

        let type_dir = actual_type.type_dir_name();

        // Remove the plugin directory from workspace
        let plugin_dir = format!("{}/plugins/{}/{}", state.workspace_dir, type_dir, name);
        let plugin_path = std::path::Path::new(&plugin_dir);
        if plugin_path.exists() && plugin_path.is_dir() {
            match std::fs::remove_dir_all(plugin_path) {
                Ok(()) => {
                    tracing::info!("Remove: removed workspace directory for bundled plugin '{}'", name);
                    removed = true;
                }
                Err(e) => {
                    tracing::warn!("Remove: failed to remove workspace directory for '{}': {:?}", name, e);
                }
            }
        }

        // Also check data_dir for bundled plugin directory
        let data_plugin_dir = format!("{}/plugins/{}/{}", data_dir, type_dir, name);
        let data_plugin_path = std::path::Path::new(&data_plugin_dir);
        if data_plugin_path.exists() && data_plugin_path.is_dir() {
            let _ = std::fs::remove_dir_all(data_plugin_path);
            tracing::info!("Remove: removed data directory for bundled plugin '{}'", name);
        }

        // Remove YAML entry ONLY if source matches "bundled" (or default/omni-stack)
        if yaml_source == Some("bundled") || yaml_source.is_none() {
            let _ = plugins_yaml::remove_entry(data_dir, &actual_type, &name);
            tracing::info!("Remove: removed YAML entry for bundled plugin '{}'", name);
            removed = true;
        }

        // Stop MCP server
        tracing::info!("Remove: stopping MCP server for plugin '{}'", name);
        crate::mcp::external::client::clear_server_pools(&name);
        crate::mcp::external::client::remove_server_config(&name);
        state.tool_registry.write().await.remove_by_server(&name);

        if removed {
            info!("Deleted bundled plugin '{}'", name);
            return (StatusCode::OK, Json(serde_json::json!({
                "success": true, "data": {"deleted": true}
            }))).into_response();
        }
    }

    // ── Rule 5: YAML entry exists but plugin not on disk (any source type) ──
    // Just remove the YAML entry. This handles orphaned entries.
    if let Some((yaml_type, source)) = yaml_info {
        if let Ok(true) = plugins_yaml::remove_entry(data_dir, &yaml_type, &name) {
            tracing::info!("Remove: removed orphaned YAML entry for plugin '{}' (source={}, not on disk)", name, source);
            removed = true;
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
    } // close unreachable_code block
}

/// Handle remove when an explicit `?source=` parameter is provided.
/// Bypasses auto-detection and directly targets the specified source variant.
async fn handle_remove_by_source(
    data_dir: &str,
    workspace_dir: &str,
    name: &str,
    source: &str,
    state: &Arc<AppState>,
) -> impl IntoResponse {
    match source {
        "built-in" => {
            // Built-in plugins cannot be removed
            let on_disk = plugins_yaml::is_plugin_builtin(data_dir, name, &plugins_yaml::PluginYamlType::Tool)
                || plugins_yaml::is_plugin_builtin(data_dir, name, &plugins_yaml::PluginYamlType::Platform)
                || plugins_yaml::is_plugin_builtin(data_dir, name, &plugins_yaml::PluginYamlType::Provider);
            if on_disk {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({
                        "success": false,
                        "error": format!("Cannot remove built-in plugin '{}'. Built-in plugins are part of the application and can only be disabled.", name)
                    })),
                ).into_response();
            }
            // YAML says built-in but not on disk → remove YAML only
            let mut removed = false;
            for pt in &[
                plugins_yaml::PluginYamlType::Platform,
                plugins_yaml::PluginYamlType::Tool,
                plugins_yaml::PluginYamlType::Provider,
            ] {
                if let Ok(Some(entry)) = plugins_yaml::get_entry(data_dir, pt, name) {
                    if entry.source == "built-in" {
                        let _ = plugins_yaml::remove_entry(data_dir, pt, name);
                        removed = true;
                        break;
                    }
                }
            }
            return respond_removed(name, removed);
        }
        "remote" => {
            let mut removed = false;
            // Find which type has this remote plugin, remove .remote/ dir + remote.yml
            for (pt, type_str) in &[
                (plugins_yaml::PluginYamlType::Tool, "tools"),
                (plugins_yaml::PluginYamlType::Platform, "platforms"),
                (plugins_yaml::PluginYamlType::Provider, "providers"),
            ] {
                let remote_dir = format!("{}/plugins/{}/.remote/{}", data_dir, type_str, name);
                let remote_path = std::path::Path::new(&remote_dir);
                if remote_path.exists() && remote_path.is_dir() {
                    let _ = std::fs::remove_dir_all(remote_path);
                    tracing::info!("Remove: removed .remote/ directory for '{}' (source=remote)", name);
                    removed = true;
                }
                let _ = plugins_yaml::remove_remote_plugin(data_dir, pt, name);
            }
            // Remove YAML only if source matches
            for pt in &[
                plugins_yaml::PluginYamlType::Platform,
                plugins_yaml::PluginYamlType::Tool,
                plugins_yaml::PluginYamlType::Provider,
            ] {
                if let Ok(Some(entry)) = plugins_yaml::get_entry(data_dir, pt, name) {
                    if entry.source == "remote" {
                        let _ = plugins_yaml::remove_entry(data_dir, pt, name);
                        removed = true;
                        break;
                    }
                }
            }
            // Stop MCP server
            crate::mcp::external::client::clear_server_pools(name);
            crate::mcp::external::client::remove_server_config(name);
            state.tool_registry.write().await.remove_by_server(name);
            return respond_removed(name, removed);
        }
        "bundled" => {
            let mut removed = false;
            // Remove workspace + data dirs
            for type_str in &["tools", "platforms", "providers"] {
                let plugin_dir = format!("{}/plugins/{}/{}", workspace_dir, type_str, name);
                let plugin_path = std::path::Path::new(&plugin_dir);
                if plugin_path.exists() && plugin_path.is_dir() {
                    let _ = std::fs::remove_dir_all(plugin_path);
                    tracing::info!("Remove: removed workspace directory for '{}' (source=bundled)", name);
                    removed = true;
                }
                let data_plugin_dir = format!("{}/plugins/{}/{}", data_dir, type_str, name);
                let data_plugin_path = std::path::Path::new(&data_plugin_dir);
                if data_plugin_path.exists() && data_plugin_path.is_dir() {
                    let _ = std::fs::remove_dir_all(data_plugin_path);
                    tracing::info!("Remove: removed data directory for '{}' (source=bundled)", name);
                }
            }
            // Remove YAML only if source matches
            for pt in &[
                plugins_yaml::PluginYamlType::Platform,
                plugins_yaml::PluginYamlType::Tool,
                plugins_yaml::PluginYamlType::Provider,
            ] {
                if let Ok(Some(entry)) = plugins_yaml::get_entry(data_dir, pt, name) {
                    if entry.source == "bundled" || entry.source == "omni-stack" {
                        let _ = plugins_yaml::remove_entry(data_dir, pt, name);
                        removed = true;
                        break;
                    }
                }
            }
            // Stop MCP server
            crate::mcp::external::client::clear_server_pools(name);
            crate::mcp::external::client::remove_server_config(name);
            state.tool_registry.write().await.remove_by_server(name);
            return respond_removed(name, removed);
        }
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "success": false,
                    "error": format!("Invalid source '{}': must be 'built-in', 'bundled', or 'remote'", source)
                })),
            ).into_response();
        }
    }
}

/// Helper: build a consistent remove response.
/// Returns a concrete Response<Body> so it can be used inside async fns returning impl IntoResponse.
fn respond_removed(name: &str, removed: bool) -> Response<Body> {
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
                "data": {"deleted": true, "note": "not found"}
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

/// POST /api/plugins/install-git — clone a plugin repository.
///
/// Clones DIRECTLY to `data_dir/plugins/<type_dir>/.remote/<name>/` and persists
/// the remote info to `remote.yml`. Does NOT compile or register in plugins.yml
/// — that happens via the separate Install action from the dashboard.
pub(crate) async fn install_git_handler(
    State(state): State<Arc<AppState>>,
    Json(body): Json<InstallGitRequest>,
) -> impl IntoResponse {
    info!(
        "Installing git plugin from {} (ref: {:?})",
        body.url, body.git_ref
    );

    // Resolve the target directory name — this is the FINAL name, no renames later.
    // Priority: 1) explicit name 2) last segment of path 3) repo name from URL
    let target_name = {
        let raw = if let Some(ref n) = body.name {
            n.clone()
        } else if let Some(ref p) = body.path {
            p.rsplit('/').next().unwrap_or(p).to_string()
        } else {
            // Extract repo name from URL
            body.url
                .trim_end_matches('/')
                .rsplit('/')
                .next()
                .unwrap_or("plugin")
                .trim_end_matches(".git")
                .to_string()
        };
        sanitize_plugin_name(&raw)
    };

    info!("Installing git plugin: target_name='{}'", target_name);

    let (manifest, _content_changed) = match plugin::installer::install_from_git(
        &body.url,
        &target_name,
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

    // The directory name IS the plugin key.
    if target_name != manifest.name {
        tracing::warn!(
            "Requested name '{}' differs from manifest name '{}'. Using requested name as the key.",
            target_name,
            manifest.name
        );
    }

    // Persist to remote.yml only — no YAML entry, no compilation.
    let yaml_type = plugins_yaml::PluginYamlType::from_plugin_type(&manifest.plugin_type);
    let remote_info = plugins_yaml::PluginRemote {
        url: body.url.clone(),
        git_ref: body.git_ref,
        path: body.path,
    };
    if let Err(e) =
        plugins_yaml::save_remote_plugin(&state.data_dir, &yaml_type, &target_name, &remote_info)
    {
        error!("Failed to persist remote info to remote.yml: {:?}", e);
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "success": false,
                "error": format!("Failed to persist remote info: {}", e)
            })),
        )
            .into_response();
    }

    info!(
        "Successfully cloned git plugin '{}' (manifest name '{}') into .remote/",
        target_name, manifest.name
    );

    (
        StatusCode::CREATED,
        Json(serde_json::json!({
            "success": true,
            "data": {
                "name": target_name,
                "manifest_name": manifest.name,
                "plugin_type": yaml_type.to_type_str(),
            }
        })),
    )
        .into_response()
}

/// POST /api/plugins/{name}/rename — rename a remote plugin.
///
/// Updates remote.yml key, plugins.yml key (if an entry exists), and renames
/// the .remote/ directory from the old name to the new name.
pub(crate) async fn rename_plugin_handler(
    Path(name): Path<String>,
    State(state): State<Arc<AppState>>,
    Json(body): Json<RenameRequest>,
) -> impl IntoResponse {
    let new_name = sanitize_plugin_name(&body.new_name);
    if new_name.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "success": false,
                "error": "New name cannot be empty"
            })),
        )
            .into_response();
    }
    if new_name == name {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "success": false,
                "error": "New name is the same as the current name"
            })),
        )
            .into_response();
    }

    let data_dir = &state.data_dir;

    // 1. Find the plugin in remote.yml across all types
    let store = plugins_yaml::load_remote_plugins(data_dir);
    let yaml_type = {
        let mut found: Option<plugins_yaml::PluginYamlType> = None;
        for (pt, entries) in [
            (&store.tools, plugins_yaml::PluginYamlType::Tool),
            (&store.platforms, plugins_yaml::PluginYamlType::Platform),
            (&store.providers, plugins_yaml::PluginYamlType::Provider),
        ] {
            if let Some(ref map) = pt {
                if map.contains_key(&name) {
                    found = Some(entries);
                    break;
                }
            }
        }
        match found {
            Some(t) => t,
            None => {
                return (
                    StatusCode::NOT_FOUND,
                    Json(serde_json::json!({
                        "success": false,
                        "error": format!("Plugin '{}' not found in remote.yml", name)
                    })),
                )
                    .into_response();
            }
        }
    };

    // 2. Get the remote info
    let remote_info = match plugins_yaml::get_remote_plugin(data_dir, &yaml_type, &name) {
        Some(r) => r,
        None => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "success": false,
                    "error": "Plugin found in remote.yml store but no data returned"
                })),
            )
                .into_response();
        }
    };

    // 3. Check that new_name doesn't already exist in remote.yml for this type
    if let Some(_existing) = plugins_yaml::get_remote_plugin(data_dir, &yaml_type, &new_name) {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "success": false,
                "error": format!("Plugin '{}' already exists in remote.yml", new_name)
            })),
        )
            .into_response();
    }

    // 4. Rename directory
    let type_dir = yaml_type.type_dir_name();
    let old_dir = format!("{}/plugins/{}/.remote/{}", data_dir, type_dir, name);
    let new_dir = format!("{}/plugins/{}/.remote/{}", data_dir, type_dir, new_name);
    let old_path = std::path::Path::new(&old_dir);
    let new_path = std::path::Path::new(&new_dir);

    if old_path.exists() {
        if new_path.exists() {
            if let Err(e) = std::fs::remove_dir_all(new_path) {
                error!(
                    "Failed to remove existing directory at {}: {:?}",
                    new_dir, e
                );
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({
                        "success": false,
                        "error": format!("Failed to remove existing directory: {}", e)
                    })),
                )
                    .into_response();
            }
        }
        if let Some(parent) = new_path.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                error!("Failed to create parent dirs: {:?}", e);
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({
                        "success": false,
                        "error": format!("Failed to create directory: {}", e)
                    })),
                )
                    .into_response();
            }
        }
        if let Err(e) = std::fs::rename(old_path, new_path) {
            error!(
                "Failed to rename directory from {} to {}: {:?}",
                old_dir, new_dir, e
            );
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "success": false,
                    "error": format!("Failed to rename directory: {}", e)
                })),
            )
                .into_response();
        }
    }

    // 5. Update remote.yml: remove old key, add new key
    let _ = plugins_yaml::remove_remote_plugin(data_dir, &yaml_type, &name);
    let _ = plugins_yaml::save_remote_plugin(data_dir, &yaml_type, &new_name, &remote_info);

    // 6. Update plugins.yml if entry exists: rename the key
    if let Ok(Some(entry)) = plugins_yaml::get_entry(data_dir, &yaml_type, &name) {
        let _ = plugins_yaml::remove_entry(data_dir, &yaml_type, &name);
        let _ = plugins_yaml::set_entry_with_source(
            data_dir,
            &yaml_type,
            &new_name,
            entry.enabled,
            &entry.source,
            entry.config,
        );
    }

    info!("Renamed remote plugin '{}' to '{}'", name, new_name);

    (
        StatusCode::OK,
        Json(serde_json::json!({
            "success": true,
            "data": {
                "old_name": name,
                "new_name": new_name,
            }
        })),
    )
        .into_response()
}

/// POST /api/plugins/{name}/download — download a remote plugin that exists in YAML but not on disk.
/// Reads the remote field from the YAML entry and runs git clone + compile.
pub(crate) async fn download_plugin_handler(
    Path(name): Path<String>,
    State(state): State<Arc<AppState>>,
    body: Option<Json<PluginSourceRequest>>,
) -> impl IntoResponse {
    let data_dir = &state.data_dir;
    let workspace_dir = &state.workspace_dir;

    // Validate source — download only makes sense for remote plugins
    let source = match &body {
        Some(req) => match require_source(&req.source) {
            Ok(s) => s,
            Err(e) => return e.into_response(),
        },
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "success": false,
                    "error": "Source is required. Provide a `source` parameter: 'built-in', 'bundled', or 'remote'."
                })),
            ).into_response();
        }
    };
    if source != "remote" {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "success": false,
                "error": format!("Invalid source '{}': download only supports 'remote' source", source)
            })),
        ).into_response();
    }

    // Find the YAML entry and extract remote info
    let (_yaml_type, remote_info) = match plugins_yaml::get_entry_with_type(data_dir, &name) {
        Ok(Some((pt, _entry))) => {
            if let Some(remote) = plugins_yaml::get_remote_plugin(data_dir, &pt, &name) {
                (pt, remote.clone())
            } else {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({
                        "success": false,
                        "error": format!("Plugin '{}' has remote source but no remote.yml entry", name)
                    })),
                ).into_response();
            }
        }
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({
                    "success": false,
                    "error": format!("Plugin '{}' not found in YAML configuration", name)
                })),
            )
                .into_response();
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "success": false,
                    "error": format!("Failed to read YAML: {}", e)
                })),
            )
                .into_response();
        }
    };

    info!(
        "Download: cloning remote plugin '{}' from {} (path: {:?})",
        name, remote_info.url, remote_info.path
    );

    // Clone from git
    let (manifest, content_changed) = match plugin::installer::install_from_git(
        &remote_info.url,
        &name,
        remote_info.git_ref.as_deref(),
        workspace_dir,
        data_dir,
        remote_info.path.as_deref(),
    ) {
        Ok(m) => m,
        Err(e) => {
            let msg = format!("Download: failed to clone git plugin '{}': {}", name, e);
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
    // Determine type directory from manifest
    let yaml_type = plugins_yaml::PluginYamlType::from_plugin_type(&manifest.plugin_type);
    let type_dir_str = yaml_type.type_dir_name();
    let plugin_dir = format!("{}/plugins/{}/.remote/{}", data_dir, type_dir_str, name);
    let effective_dir = match remote_info.path {
        Some(ref p) if !p.is_empty() => format!("{}/{}", plugin_dir, p),
        _ => plugin_dir.clone(),
    };
    let cargo_toml = std::path::Path::new(&effective_dir).join("Cargo.toml");
    if !content_changed {
        info!(
            "Download: skipping compilation for '{}' — no new commits fetched",
            name
        );
    } else if cargo_toml.exists() {
        info!("Download: compiling Rust crate at {}", effective_dir);
        match tokio::task::spawn_blocking({
            let dir = effective_dir.clone();
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
                info!("Download: Rust compilation succeeded for '{}'", name);
            }
            Ok(Ok(status)) => {
                let msg = format!(
                    "Download: compilation failed for '{}' with exit code {}",
                    name, status
                );
                tracing::error!("{}", msg);
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({"success": false, "error": msg})),
                )
                    .into_response();
            }
            Ok(Err(e)) => {
                let msg = format!("Download: failed to run cargo for '{}': {}", name, e);
                tracing::error!("{}", msg);
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({"success": false, "error": msg})),
                )
                    .into_response();
            }
            Err(e) => {
                let msg = format!("Download: task join error for '{}': {}", name, e);
                tracing::error!("{}", msg);
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({"success": false, "error": msg})),
                )
                    .into_response();
            }
        }
    }

    // Ensure YAML entry has the remote source field, preserving existing enabled state
    let current_enabled = plugins_yaml::get_entry(data_dir, &yaml_type, &name)
        .ok()
        .flatten()
        .map(|e| e.enabled)
        .unwrap_or(true);
    let _ = plugins_yaml::set_entry_with_source(
        data_dir,
        &yaml_type,
        &name,
        current_enabled,
        "remote",
        serde_json::json!({}),
    );

    match plugins_yaml::get_plugin(data_dir, &name) {
        Ok(Some(detail)) => {
            info!("Downloaded remote plugin '{}' successfully", name);
            (
                StatusCode::OK,
                Json(serde_json::json!({"success": true, "data": detail})),
            )
        }
        _ => {
            info!(
                "Downloaded remote plugin '{}' but could not re-read detail",
                name
            );
            (StatusCode::OK, Json(serde_json::json!({"success": true})))
        }
    }
    .into_response()
}

use std::future::Future;
use std::pin::Pin;
use tokio::sync::oneshot;

/// POST /api/reload — reload environment variables + plugins.
///
/// Re-reads .env and synchronizes runtime plugin state:
/// starts newly enabled MCP servers / providers, stops newly disabled ones,
/// cleans up orphaned state. No process restart needed.
pub(crate) fn reload_env_handler(
    State(state): State<Arc<AppState>>,
) -> Pin<Box<dyn Future<Output = Response<Body>> + Send>> {
    Box::pin(async move {
        let env_refreshed = refresh_env_from_file(&state.env_path);
        let result = reload_plugins(state).await;
        format_reload_response(env_refreshed, result)
    })
}

/// Format the reload response (extracted to reduce handler complexity for axum's trait solver).
fn format_reload_response(
    env_refreshed: u32,
    result: Result<(u32, u32, Vec<String>), String>,
) -> Response<Body> {
    let is_ok = result.is_ok();
    let body = match result {
        Ok((started, stopped, errors)) => serde_json::json!({
            "success": true,
            "data": {
                "env_vars_refreshed": env_refreshed,
                "plugins": {
                    "started": started,
                    "stopped": stopped,
                    "errors": errors,
                }
            }
        }),
        Err(ref e) => serde_json::json!({
            "success": false,
            "error": e,
        }),
    };
    let status = if is_ok {
        StatusCode::OK
    } else {
        StatusCode::INTERNAL_SERVER_ERROR
    };
    (status, Json(body)).into_response()
}

/// Helper: reload plugin runtime state from YAML on disk.
async fn reload_plugins(state: Arc<AppState>) -> Result<(u32, u32, Vec<String>), String> {
    let data_dir = state.data_dir.clone();
    let all_plugins =
        tokio::task::spawn_blocking(move || crate::plugins_yaml::list_plugins(&data_dir))
            .await
            .map_err(|e| format!("Task join: {}", e))?
            .map_err(|e| format!("list_plugins: {}", e))?;

    // 2. Snapshot current runtime state (tokio locks, Send-safe)
    let active_mcp: std::collections::HashSet<String> = {
        let reg = state.tool_registry.read().await;
        reg.all().iter().filter_map(|t| t.server_name.clone()).collect()
    };
    let active_platforms: std::collections::HashSet<String> = {
        let sigs = state.platform_restart_signals.lock().await;
        sigs.keys().cloned().collect()
    };

    let mut started = 0u32;
    let mut stopped = 0u32;
    let mut errors: Vec<String> = Vec::new();

    // 3. Diff and apply
    for plugin in &all_plugins {
        let name = &plugin.name;
        let enabled = plugin.status == "enabled";
        match plugin.plugin_type.as_str() {
            "tool" => {
                let running = active_mcp.contains(name);
                if enabled && !running {
                    match crate::mcp::external::client::initialize_single_server_tools(
                        &state.data_dir,
                        &state.workspace_dir,
                        name,
                    )
                    .await
                    {
                        Ok(ts) => {
                            state.tool_registry.write().await.register_all(ts);
                            started += 1;
                        }
                        Err(e) => errors.push(format!("{} MCP: {}", name, e)),
                    }
                } else if !enabled && running {
                    state.tool_registry.write().await.remove_by_server(name);
                    stopped += 1;
                }
            }
            "provider" => {
                let running = crate::provider::registry::PROVIDER_REGISTRY
                    .read()
                    .unwrap()
                    .has_provider(name);
                if enabled && !running {
                    let provider = {
                        let guard = crate::provider::registry::PROVIDER_REGISTRY
                            .read()
                            .unwrap();
                        if let Some(ep) = plugin.manifest.get("entrypoint") {
                            if let Some(cmd) = ep.get("command").and_then(|c| c.as_str()) {
                                let args: Vec<String> = ep
                                    .get("args")
                                    .and_then(|a| a.as_array())
                                    .map(|a| {
                                        a.iter()
                                            .filter_map(|v| v.as_str().map(String::from))
                                            .collect()
                                    })
                                    .unwrap_or_default();
                                crate::provider::registry::PROVIDER_REGISTRY
                                    .write()
                                    .unwrap()
                                    .register(name, cmd, &args);
                                guard.get_cloned(name)
                            } else {
                                None
                            }
                        } else {
                            None
                        }
                    };
                    if let Some(c) = provider {
                        match c.start().await {
                            Ok(()) => started += 1,
                            Err(e) => {
                                crate::provider::registry::PROVIDER_REGISTRY
                                    .write()
                                    .unwrap()
                                    .remove(name);
                                errors.push(format!("{} provider: {}", name, e));
                            }
                        }
                    }
                } else if !enabled && running {
                    crate::provider::registry::PROVIDER_REGISTRY
                        .write()
                        .unwrap()
                        .remove(name);
                    stopped += 1;
                }
            }
            "platform" => {
                if enabled && active_platforms.contains(name) {
                    if let Some((flag, note)) = state
                        .platform_restart_signals
                        .lock()
                        .await
                        .get(name)
                        .cloned()
                    {
                        flag.store(true, std::sync::atomic::Ordering::SeqCst);
                        note.notify_one();
                        started += 1;
                    }
                }
            }
            _ => {}
        }
    }

    // 4. Clean orphaned MCP servers (in YAML but not in runtime)
    let yaml_names: std::collections::HashSet<&str> =
        all_plugins.iter().map(|p| p.name.as_str()).collect();
    for srv in &active_mcp {
        if !yaml_names.contains(srv.as_str()) {
            state.tool_registry.write().await.remove_by_server(srv);
            stopped += 1;
        }
    }

    Ok((started, stopped, errors))
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
            state.tool_registry.write().await.remove_by_server(name);
            state.tool_registry.write().await.register_all(tools);
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
