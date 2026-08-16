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

    // 1. Resolve plugin source via shared preamble (uses type+source from URL path deterministically)
    let resolved =
        match resolve_plugin_for_compile(data_dir, &p_type, &source, &name, "Install").await {
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
    match compile_rust_crate(&plugin_dir, &name, category_source, false).await {
        Ok(true) => {
            info!("Install: compilation succeeded for '{}'", name);
        }
        Ok(false) => {
            // Ok(false) means no Cargo.toml found — the plugin directory exists but
            // isn't a Rust crate, so it's a non-Rust plugin (Python/NodeJS). Install
            // its declared dependencies (requirements.txt / pyproject.toml /
            // package.json) hermetically so it can actually run. Remote plugins with
            // NO dependency manifest at all are an error: install-git should have
            // cloned a proper project. Bundled plugins without a manifest (e.g. a
            // dependency-free script) still pass through.
            match install_non_rust_deps(&plugin_dir).await {
                Ok(Some(desc)) => {
                    info!("Install: {} for '{}'", desc, name);
                }
                Ok(None) => {
                    if category_source == "remote" {
                        let msg = format!(
                            "Install: no Cargo.toml and no dependency manifest \
                             (requirements.txt/pyproject.toml/package.json) found for remote \
                             plugin '{}' at {}",
                            name, plugin_dir
                        );
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
                    info!(
                        "Install: compilation skipped for '{}' (no Cargo.toml, no deps)",
                        name
                    );
                }
                Err(e) => {
                    let msg = format!(
                        "Install: dependency installation failed for '{}': {}",
                        name, e
                    );
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
            match plugins_yaml::get_plugin(data_dir, &name, &yaml_type) {
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

    // 1. Resolve plugin source via shared preamble (uses type+source from URL path deterministically)
    let resolved =
        match resolve_plugin_for_compile(data_dir, &p_type, &source, &name, "Reinstall").await {
            Ok(r) => r,
            Err(response) => return response.into_response(),
        };

    let yaml_type = resolved.yaml_type;
    let category = resolved.category.clone();
    let plugin_dir = resolved.plugin_dir;
    let _source = category_to_source(&category);

    // Note: For remote plugins, this does NOT re-clone the git repository.
    // It only recompiles the existing source code in .remote/<name>/.
    // To update from git, use the Download endpoint instead.

    // 2. Compile (force rebuild: remove stale binary so cargo actually recompiles)
    let compiled =
        match compile_rust_crate(&plugin_dir, &name, category_to_source(&category), true).await {
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
    match plugins_yaml::get_plugin(data_dir, &name, &yaml_type) {
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

    // Download and extract (async — never blocks the core's runtime)
    let manifest = match plugin::installer::install_from_url(&body.url, &state.data_dir).await {
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
        Ok(_entry) => match plugins_yaml::get_plugin(&state.data_dir, &manifest.name, &yaml_type) {
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

    // Find remote info from remote.yml using type+source from URL path (no guessing)
    let yaml_type = plugins_yaml::PluginYamlType::from_type_str(&p_type);
    let remote_info = match plugins_yaml::get_remote_plugin(data_dir, &yaml_type, &name) {
        Some(r) => r.clone(),
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({
                    "success": false,
                    "error": format!("Download: remote plugin '{}' (type={}) not found in remote.yml", name, p_type)
                })),
            ).into_response();
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

    match plugins_yaml::get_plugin(data_dir, &name, &yaml_type) {
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

    // 1. Determine type from URL path (no guessing across types)
    let yaml_type = plugins_yaml::PluginYamlType::from_type_str(&p_type);

    // 2. Get remote info from remote.yml
    let remote_info = match plugins_yaml::get_remote_plugin(data_dir, &yaml_type, &name) {
        Some(r) => r,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({
                    "success": false,
                    "error": format!("Plugin '{}' (type={}) not found in remote.yml", name, p_type)
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

// ---------------------------------------------------------------------------
// Non-Rust (Python/NodeJS) dependency installation
// ---------------------------------------------------------------------------

/// Install dependencies for a non-Rust (Python/NodeJS) plugin.
///
/// Hermetic by design — no global package pollution:
/// - **Python** (`requirements.txt` or `pyproject.toml`): creates a venv at
///   `{plugin_dir}/.venv` and pip-installs into it. Falls back to
///   `pip install --target {plugin_dir}/pylib` when `python3 -m venv` is
///   unavailable (e.g. missing `python3-venv` on minimal images).
/// - **NodeJS** (`package.json`): `npm ci` when a lockfile exists, else
///   `npm install`; then `npm run build` when the package declares a build
///   script (TypeScript MCP servers compile to `dist/` this way, matching the
///   reference servers' own Dockerfiles: `npm ci && npm run build`).
///
/// Returns `Ok(Some(summary))` when dependencies were installed, `Ok(None)`
/// when the directory declares no dependency manifest (nothing to do), and
/// `Err` on failure.
async fn install_non_rust_deps(plugin_dir: &str) -> Result<Option<String>, String> {
    let dir = std::path::Path::new(plugin_dir);
    let has_python = dir.join("requirements.txt").exists() || dir.join("pyproject.toml").exists();
    let has_node = dir.join("package.json").exists();

    if has_python {
        let desc = install_python_deps(plugin_dir).await?;
        return Ok(Some(desc));
    }
    if has_node {
        let desc = install_node_deps(plugin_dir).await?;
        return Ok(Some(desc));
    }
    Ok(None)
}

/// Install Python dependencies into a hermetic venv (or `pylib/` fallback).
async fn install_python_deps(plugin_dir: &str) -> Result<String, String> {
    let venv_python = format!("{}/.venv/bin/python", plugin_dir);
    let venv_pip = format!("{}/.venv/bin/pip", plugin_dir);
    let has_requirements =
        std::path::Path::new(&format!("{}/requirements.txt", plugin_dir)).exists();

    // Prefer a hermetic venv; fall back to `pip install --target` when venv
    // creation is unavailable in the image (no python3-venv / ensurepip).
    if !std::path::Path::new(&venv_python).exists() {
        match run_capture(
            "python3",
            &["-m", "venv", &format!("{}/.venv", plugin_dir)],
            None,
        )
        .await
        {
            Ok(_) => info!("Install: created Python venv at {}", venv_python),
            Err(venv_err) => {
                tracing::warn!(
                    "Install: python venv unavailable ({}); falling back to pip install --target",
                    venv_err
                );
                let target = format!("{}/pylib", plugin_dir);
                let mut args = vec![
                    "-m",
                    "pip",
                    "install",
                    "--disable-pip-version-check",
                    "--target",
                    &target,
                ];
                let req_path = format!("{}/requirements.txt", plugin_dir);
                if has_requirements {
                    args.push("-r");
                    args.push(&req_path);
                } else {
                    args.push(".");
                }
                run_capture("python3", &args, Some(plugin_dir)).await?;
                return Ok(format!(
                    "Python dependencies installed into {}/pylib",
                    plugin_dir
                ));
            }
        }
    }

    let mut args = vec!["install", "--disable-pip-version-check"];
    let req_path = format!("{}/requirements.txt", plugin_dir);
    if has_requirements {
        args.push("-r");
        args.push(&req_path);
    } else {
        args.push(".");
    }
    run_capture(&venv_pip, &args, Some(plugin_dir)).await?;
    Ok(format!(
        "Python dependencies installed into {}/.venv",
        plugin_dir
    ))
}

/// Install NodeJS dependencies into the local `node_modules/` (hermetic).
async fn install_node_deps(plugin_dir: &str) -> Result<String, String> {
    let has_lockfile = std::path::Path::new(&format!("{}/package-lock.json", plugin_dir)).exists()
        || std::path::Path::new(&format!("{}/npm-shrinkwrap.json", plugin_dir)).exists();

    if has_lockfile {
        run_capture("npm", &["ci", "--no-audit", "--no-fund"], Some(plugin_dir)).await?;
    } else {
        run_capture(
            "npm",
            &["install", "--no-audit", "--no-fund"],
            Some(plugin_dir),
        )
        .await?;
    }

    // TypeScript servers compile to dist/ via a build script when declared.
    let has_build_script = std::fs::read_to_string(&format!("{}/package.json", plugin_dir))
        .ok()
        .and_then(|content| serde_json::from_str::<serde_json::Value>(&content).ok())
        .map(|v| {
            v.get("scripts")
                .and_then(|s| s.get("build"))
                .and_then(|b| b.as_str())
                .map(|s| !s.is_empty())
                .unwrap_or(false)
        })
        .unwrap_or(false);
    if has_build_script {
        run_capture("npm", &["run", "build"], Some(plugin_dir)).await?;
    }

    Ok(format!(
        "NodeJS dependencies installed into {}/node_modules",
        plugin_dir
    ))
}

/// Run a command, capturing stdout/stderr. Returns Err with a truncated tail
/// of the combined output on non-zero exit.
async fn run_capture(
    program: &str,
    args: &[&str],
    cwd: Option<&str>,
) -> Result<(String, String), String> {
    let mut cmd = tokio::process::Command::new(program);
    cmd.args(args);
    if let Some(c) = cwd {
        cmd.current_dir(c);
    }
    let output = cmd
        .output()
        .await
        .map_err(|e| format!("failed to spawn '{}': {}", program, e))?;
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    if !output.status.success() {
        let combined = format!("{}{}", stdout, stderr);
        let tail: Vec<&str> = combined.lines().rev().take(40).collect();
        let tail_str = tail.iter().rev().cloned().collect::<Vec<_>>().join("\n");
        return Err(format!(
            "'{}' exited with {}:\n{}",
            program, output.status, tail_str
        ));
    }
    Ok((stdout, stderr))
}
