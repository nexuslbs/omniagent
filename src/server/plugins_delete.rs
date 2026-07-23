//! Plugin deletion/removal handlers.
//!
//! Extracted from `plugins.rs` for separation of concerns.

use axum::{
    extract::{Path, Query, State},
    http::{StatusCode, Response},
    response::IntoResponse,
    body::Body,
    Json,
};
use std::collections::HashMap;
use std::sync::Arc;
use tracing::info;

use crate::plugins_yaml;
use crate::server::AppState;

use super::plugins_compile::*;
use super::plugins_types::*;

pub(crate) async fn delete_plugin_handler(
    Path((p_type, source, name)): Path<(String, String, String)>,
    State(state): State<Arc<AppState>>,
    Query(params): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    let data_dir = &state.data_dir;

    if let Err(e) = validate_plugin_type(&p_type) {
        return e.into_response();
    }
    if let Err(e) = validate_source(&source) {
        return e.into_response();
    }
    if let Err(e) = reject_builtin_operation(&source, "delete", &name) {
        return e.into_response();
    }

    let is_uninstall_mode = params
        .get("mode")
        .map(|s| s == "uninstall")
        .unwrap_or(false);
    let explicit_source = Some(&source[..]);

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
                    if let Err(e) = std::fs::remove_dir_all(target_path) {
                        tracing::warn!("[plugins] Failed to remove target dir for '{}': {:?}", name, e);
                    }
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
                                if let Err(e) = std::fs::remove_dir_all(sub_path) {
                                    tracing::warn!("[plugins] Failed to remove subpath target dir: {:?}", e);
                                }
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
                    if let Err(e) = plugins_yaml::set_enabled(data_dir, yaml_type, &name, false) {
                        tracing::error!("[plugins] Failed to set disabled in YAML: {:?}", e);
                    }
                    break;
                }
            }

            // Stop the MCP server and remove its tools from the registry
            tracing::info!(
                "Uninstall: stopping MCP server for remote plugin '{}'",
                name
            );
            state.plugin_manager.remove_client(&name);
            state.plugin_manager.remove_server_tools(&name).await;

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
                let search_dirs = [data_dir, &state.data_dir];
                for type_dir in &type_dirs {
                    for base in &search_dirs {
                        let plugin_dir = format!("{}/plugins/{}/{}", base, type_dir, name);
                        let target_dir = format!("{}/target", plugin_dir);
                        let target_path = std::path::Path::new(&target_dir);
                        if target_path.exists() && target_path.is_dir() {
                            if let Err(e) = std::fs::remove_dir_all(target_path) {
                                tracing::warn!("[plugins] Failed to remove non-remote target dir: {:?}", e);
                            }
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
            state.plugin_manager.remove_client(&name);
            state.plugin_manager.remove_server_tools(&name).await;
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

    // Remove mode (default)
    // Source is required for all remove operations.
    match &explicit_source {
        Some(source) => {
            return handle_remove_by_source(data_dir, &state.data_dir, &name, source, &state)
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
    // unreachable: source must be provided explicitly.
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
            let p = format!("{}/plugins/{}/{}", state.data_dir, d, name);
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
        if let Some((yaml_type, _)) = yaml_info.as_ref() {
            if let Ok(true) = plugins_yaml::remove_entry(data_dir, yaml_type, &name) {
                tracing::info!("Remove: removed YAML entry for built-in plugin '{}' (source not on disk)", name);
            }
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
            if let Err(e) = plugins_yaml::remove_remote_plugin(data_dir, pt, &name) {
                tracing::warn!("[plugins] Failed to remove remote YAML entry: {:?}", e);
            }
        }

        // Remove stale .remote/ directories from other types
        for t in &["tools", "platforms", "providers"] {
            if *t == type_dir { continue; }
            let alt = format!("{}/plugins/{}/.remote/{}", data_dir, t, name);
            let alt_path = std::path::Path::new(&alt);
            if alt_path.exists() && alt_path.is_dir() {
                if let Err(e) = std::fs::remove_dir_all(alt_path) {
                    tracing::warn!("[plugins] Failed to remove stale remote dir: {:?}", e);
                }
                    tracing::info!("Remove: cleaned up stale .remote/ directory at {}", alt);
            }
        }

        // Remove YAML entry ONLY if source matches "remote"
        if yaml_source == Some("remote") {
            if let Err(e) = plugins_yaml::remove_entry(data_dir, &actual_type, &name) {
                tracing::warn!("[plugins] Failed to remove remote YAML entry: {:?}", e);
            }
            tracing::info!("Remove: removed YAML entry for remote plugin '{}'", name);
            removed = true;
        }

        // Stop MCP server
        tracing::info!("Remove: stopping MCP server for plugin '{}'", name);
        state.plugin_manager.remove_client(&name);
        state.plugin_manager.remove_server_tools(&name).await;

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
                    let p = format!("{}/plugins/{}/{}", state.data_dir, d, name);
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
        let plugin_dir = format!("{}/plugins/{}/{}", state.data_dir, type_dir, name);
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
            if let Err(e) = std::fs::remove_dir_all(data_plugin_path) {
                tracing::warn!("[plugins] Failed to remove data dir: {:?}", e);
            }
            tracing::info!("Remove: removed data directory for bundled plugin '{}'", name);
        }

        // Remove YAML entry ONLY if source matches "bundled" (or default/omni-stack)
        if yaml_source == Some("bundled") || yaml_source.is_none() {
            if let Err(e) = plugins_yaml::remove_entry(data_dir, &actual_type, &name) {
                tracing::warn!("[plugins] Failed to remove bundled YAML entry: {:?}", e);
            }
            tracing::info!("Remove: removed YAML entry for bundled plugin '{}'", name);
            removed = true;
        }

        // Stop MCP server
        tracing::info!("Remove: stopping MCP server for plugin '{}'", name);
        state.plugin_manager.remove_client(&name);
        state.plugin_manager.remove_server_tools(&name).await;

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
pub(crate) async fn handle_remove_by_source(
    data_dir: &str,
    _workspace_dir: &str,
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
                        if let Err(e) = plugins_yaml::remove_entry(data_dir, pt, name) {
                            tracing::warn!("[plugins] Failed to remove built-in phantom YAML: {:?}", e);
                        }
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
                    if let Err(e) = std::fs::remove_dir_all(remote_path) {
                        tracing::warn!("[plugins] Failed to remove remote dir: {:?}", e);
                    }
                    tracing::info!("Remove: removed .remote/ directory for '{}' (source=remote)", name);
                    removed = true;
                }
                if let Err(e) = plugins_yaml::remove_remote_plugin(data_dir, pt, name) {
                    tracing::warn!("[plugins] Failed to remove remote YAML: {:?}", e);
                }
            }
            // Remove YAML only if source matches
            for pt in &[
                plugins_yaml::PluginYamlType::Platform,
                plugins_yaml::PluginYamlType::Tool,
                plugins_yaml::PluginYamlType::Provider,
            ] {
                if let Ok(Some(entry)) = plugins_yaml::get_entry(data_dir, pt, name) {
                    if entry.source == "remote" {
                        if let Err(e) = plugins_yaml::remove_entry(data_dir, pt, name) {
                            tracing::warn!("[plugins] Failed to remove remote YAML: {:?}", e);
                        }
                        removed = true;
                        break;
                    }
                }
            }
            // Stop MCP server (only if we found and removed the plugin)
            if removed {
                state.plugin_manager.remove_client(name);
                state.plugin_manager.remove_server_tools(name).await;
            }
            return respond_removed(name, removed);
        }
        "bundled" => {
            let mut removed = false;
            // Remove workspace + data dirs
            for type_str in &["tools", "platforms", "providers"] {
                let plugin_dir = format!("{}/plugins/{}/{}", data_dir, type_str, name);
                let plugin_path = std::path::Path::new(&plugin_dir);
                if plugin_path.exists() && plugin_path.is_dir() {
                    if let Err(e) = std::fs::remove_dir_all(plugin_path) {
                        tracing::warn!("[plugins] Failed to remove workspace dir: {:?}", e);
                    }
                    tracing::info!("Remove: removed workspace directory for '{}' (source=bundled)", name);
                    removed = true;
                }
                let data_plugin_dir = format!("{}/plugins/{}/{}", data_dir, type_str, name);
                let data_plugin_path = std::path::Path::new(&data_plugin_dir);
                if data_plugin_path.exists() && data_plugin_path.is_dir() {
                    if let Err(e) = std::fs::remove_dir_all(data_plugin_path) {
                        tracing::warn!("[plugins] Failed to remove data dir: {:?}", e);
                    }
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
                        if let Err(e) = plugins_yaml::remove_entry(data_dir, pt, name) {
                            tracing::error!("Remove: failed to remove YAML entry for '{}': {}", name, e);
                        }
                        removed = true;
                        break;
                    }
                }
            }
            // Stop MCP server (only if we found and removed the plugin)
            if removed {
                state.plugin_manager.remove_client(name);
                state.plugin_manager.remove_server_tools(name).await;
            }
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
pub(crate) fn respond_removed(name: &str, removed: bool) -> Response<Body> {
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

