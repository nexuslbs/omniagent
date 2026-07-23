//! Plugin installation handlers (git, URL, download).
//!
//! Extracted from `plugins.rs` for separation of concerns.

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use std::sync::Arc;
use tracing::{error, info};

use crate::plugin;
use crate::plugins_yaml;
use crate::server::AppState;

use super::plugins_compile::*;
use super::plugins_reload::*;
use super::plugins_types::*;

pub(crate) async fn install_plugin_handler(
    Path((p_type, source, name)): Path<(String, String, String)>,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    let data_dir = &state.data_dir;

    // Validate type and source from path
    if let Err(e) = validate_plugin_type(&p_type) {
        return e.into_response();
    }
    if let Err(e) = validate_source(&source) {
        return e.into_response();
    }

    // Reject install for built-in plugins
    if let Err(e) = reject_builtin_operation(&source, "install", &name) {
        return e.into_response();
    }

    // 1. Resolve plugin source via shared preamble (detect type, resolve dir, verify source)
    let resolved = match resolve_plugin_for_compile(
        data_dir,
        &state.data_dir,
        &name,
        "Install",
        Some(&source),
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

    // 2. Compile FIRST: synchronous, no background spawn
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

/// POST /api/plugins/:name/reinstall: recompile and reload a plugin.
///
/// Handles all three plugin categories:
/// 1. Builtin: recompile, binary goes to get_bin_path()
/// 2. Omni-stack: recompile in place
/// 3. Remote: re-clone to .remote/, recompile
pub(crate) async fn reinstall_plugin_handler(
    Path((p_type, source, name)): Path<(String, String, String)>,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    let data_dir = &state.data_dir;

    // Validate type and source from path
    if let Err(e) = validate_plugin_type(&p_type) {
        return e.into_response();
    }
    if let Err(e) = validate_source(&source) {
        return e.into_response();
    }

    // Reject reinstall for built-in plugins
    if let Err(e) = reject_builtin_operation(&source, "reinstall", &name) {
        return e.into_response();
    }

    // 1. Resolve plugin source via shared preamble (detect type, resolve dir, verify source)
    let resolved = match resolve_plugin_for_compile(
        data_dir,
        &state.data_dir,
        &name,
        "Reinstall",
        Some(&source),
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

/// POST /api/plugins/install-url: install a plugin from a URL and register in YAML.
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

/// POST /api/plugins/install-git: clone a plugin repository.
///
/// Clones DIRECTLY to `data_dir/plugins/<type_dir>/.remote/<name>/` and persists
/// the remote info to `remote.yml`. Does NOT compile or register in plugins.yml
///: that happens via the separate Install action from the dashboard.
pub(crate) async fn install_git_handler(
    State(state): State<Arc<AppState>>,
    Json(body): Json<InstallGitRequest>,
) -> impl IntoResponse {
    info!(
        "Installing git plugin from {} (ref: {:?})",
        body.url, body.git_ref
    );

    // Resolve the target directory name: this is the FINAL name, no renames later.
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
        &state.data_dir,
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

    // Persist to remote.yml only: no YAML entry, no compilation.
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

/// POST /api/plugins/{type}/{source}/{name}/download: clone a remote plugin that has a remote.yml entry but no disk directory.
/// For `source=remote`: clones from git via remote.yml.
pub(crate) async fn download_plugin_handler(
    Path((p_type, source, name)): Path<(String, String, String)>,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    let data_dir = &state.data_dir;

    // Validate type and source from path
    if let Err(e) = validate_plugin_type(&p_type) {
        return e.into_response();
    }
    if let Err(e) = validate_source(&source) {
        return e.into_response();
    }
    if let Err(e) = reject_builtin_operation(&source, "download", &name) {
        return e.into_response();
    }

    // Validate that download only supports 'remote'
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
        data_dir,
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
    let _effective_dir = match remote_info.path {
        Some(ref p) if !p.is_empty() => format!("{}/{}", plugin_dir, p),
        _ => plugin_dir.clone(),
    };
    if !content_changed {
        info!("Download: no new commits fetched for '{}'", name);
    } else {
        info!(
            "Download: cloned source for '{}' (compile separately via Install)",
            name
        );
    }

    // Ensure YAML entry has the remote source field, preserving existing enabled state
    let current_enabled = plugins_yaml::get_entry(data_dir, &yaml_type, &name)
        .ok()
        .flatten()
        .map(|e| e.enabled)
        .unwrap_or(true);
    if let Err(e) = plugins_yaml::set_entry_with_source(
        data_dir,
        &yaml_type,
        &name,
        current_enabled,
        "remote",
        serde_json::json!({}),
    ) {
        tracing::warn!("[plugins] Download: failed to set YAML entry: {:?}", e);
    }

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

/// POST /api/plugins/{name}/rename: rename a remote plugin.
///
/// Updates remote.yml key, plugins.yml key (if an entry exists), and renames
/// the .remote/ directory from the old name to the new name.
pub(crate) async fn rename_plugin_handler(
    Path((p_type, source, name)): Path<(String, String, String)>,
    State(state): State<Arc<AppState>>,
    Json(body): Json<RenameRequest>,
) -> impl IntoResponse {
    if let Err(e) = validate_plugin_type(&p_type) {
        return e.into_response();
    }
    if let Err(e) = validate_source(&source) {
        return e.into_response();
    }
    if let Err(e) = reject_builtin_operation(&source, "rename", &name) {
        return e.into_response();
    }
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
    if let Err(e) = plugins_yaml::remove_remote_plugin(data_dir, &yaml_type, &name) {
        tracing::warn!(
            "[plugins] Rename: failed to remove old remote YAML: {:?}",
            e
        );
    }
    if let Err(e) = plugins_yaml::save_remote_plugin(data_dir, &yaml_type, &new_name, &remote_info)
    {
        tracing::warn!("[plugins] Rename: failed to save new remote YAML: {:?}", e);
    }

    // 6. Update plugins.yml if entry exists: rename the key
    if let Ok(Some(entry)) = plugins_yaml::get_entry(data_dir, &yaml_type, &name) {
        if let Err(e) = plugins_yaml::remove_entry(data_dir, &yaml_type, &name) {
            tracing::warn!("[plugins] Rename: failed to remove old YAML: {:?}", e);
        }
        if let Err(e) = plugins_yaml::set_entry_with_source(
            data_dir,
            &yaml_type,
            &new_name,
            entry.enabled,
            &entry.source,
            entry.config,
        ) {
            tracing::warn!("[plugins] Rename: failed to save new YAML entry: {:?}", e);
        }
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
