//! Plugin management API endpoints.
//!
//! Provides REST endpoints for listing, installing, configuring, and
//! managing plugin lifecycle — using YAML files for plugin state
//! instead of the old `plugin_registry` database table.

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{delete, get, post},
    Json, Router,
};
use serde::Deserialize;
use sqlx;
use std::sync::Arc;
use tracing::{error, info};

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
pub(crate) struct InstallUrlRequest {
    url: String,
}

/// Build the plugin management router, reusing the main server's state.
#[allow(dead_code)]
pub(crate) fn plugin_router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/plugins", get(list_plugins_handler))
        .route("/api/plugins/{name}", get(get_plugin_handler))
        .route("/api/plugins/{name}/config", post(update_config_handler))
        .route("/api/plugins/{name}/enable", post(enable_plugin_handler))
        .route("/api/plugins/{name}/disable", post(disable_plugin_handler))
        .route(
            "/api/plugins/{name}/install",
            post(install_plugin_handler),
        )
        .route(
            "/api/plugins/{name}/reinstall",
            post(reinstall_plugin_handler),
        )
        .route(
            "/api/plugins/{name}/refresh-models",
            post(refresh_models_handler),
        )
        .route("/api/plugins/{name}", delete(delete_plugin_handler))
        .route("/api/plugins/install-url", post(install_url_handler))
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
                        let lookup = sqlx::query_scalar::<_, String>(
                            "SELECT current_value FROM secrets WHERE name = $1"
                        )
                        .bind(secret_name)
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
                    let lookup = sqlx::query_scalar::<_, String>(
                        "SELECT current_value FROM secrets WHERE name = $1"
                    )
                    .bind(secret_name)
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
            // If saving an api_key for a provider, also write to .env as {NAME}_API_KEY
            if yaml_type == plugins_yaml::PluginYamlType::Provider {
                if let Some(config_obj) = body.config.as_object() {
                    if let Some(api_key_val) = config_obj.get("api_key").and_then(|v| v.as_str()) {
                        let name_upper = name.to_uppercase().replace('-', "_");
                        let env_key = format!("{}_API_KEY", name_upper);

                        let env_path = state.env_path.clone();
                        let env_key_clone = env_key.clone();
                        let api_key_owned = api_key_val.to_string();
                        let result = tokio::task::spawn_blocking(move || {
                            let content = std::fs::read_to_string(&env_path).unwrap_or_default();
                            let mut lines: Vec<String> =
                                content.lines().map(|l| l.to_string()).collect();

                            let mut found = false;
                            for line in lines.iter_mut() {
                                let trimmed = line.trim();
                                if trimmed.starts_with(&env_key_clone) && trimmed.contains('=') {
                                    *line = format!("{}={}", env_key_clone, api_key_owned);
                                    found = true;
                                    break;
                                }
                            }
                            if !found {
                                lines.push(format!("{}={}", env_key_clone, api_key_owned));
                            }

                            let new_content = lines.join("\n") + "\n";
                            std::fs::write(&env_path, new_content).ok();
                        })
                        .await;

                        if result.is_ok() {
                            std::env::set_var(&env_key, api_key_val);
                        }

                        info!("Saved api_key for plugin '{}' to .env as {}", name, env_key);
                    }
                }
            }

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

    // Upsert with enabled=true (creates YAML entry if not exists)
    match plugins_yaml::set_entry(
        &state.data_dir,
        &yaml_type,
        &name,
        true,
        serde_json::json!({}),
    ) {
        Ok(_entry) => match plugins_yaml::get_plugin(&state.data_dir, &name) {
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
        },
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

    // Upsert with enabled=false
    match plugins_yaml::set_entry(
        &state.data_dir,
        &yaml_type,
        &name,
        false,
        serde_json::json!({}),
    ) {
        Ok(_entry) => match plugins_yaml::get_plugin(&state.data_dir, &name) {
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
        },
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

/// POST /api/plugins/:name/install — compile and register a bundled plugin.
pub(crate) async fn install_plugin_handler(
    Path(name): Path<String>,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    let data_dir = &state.data_dir;

    // 1. Find the plugin directory
    let plugin_dirs = [
        format!("{}/plugins/mcp/{}", data_dir, name),
        format!("{}/plugins/installed/{}", data_dir, name),
        format!("{}/plugins/providers/{}", data_dir, name),
        format!("{}/plugins/platforms/{}", data_dir, name),
    ];

    let mut found_dir = None;
    for dir in &plugin_dirs {
        let plugin_json = std::path::Path::new(dir).join("plugin.json");
        if plugin_json.exists() {
            found_dir = Some(dir.clone());
            break;
        }
    }

    let plugin_dir = match found_dir {
        Some(d) => d,
        None => {
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

    // 2. Compile if Rust crate
    let cargo_toml = std::path::Path::new(&plugin_dir).join("Cargo.toml");
    if cargo_toml.exists() {
        tracing::info!("Install: compiling Rust crate at {}", plugin_dir);
        match tokio::task::spawn_blocking({
            let dir = plugin_dir.clone();
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
                tracing::info!("Install: Rust compilation succeeded for '{}'", name);
            }
            Ok(Ok(status)) => {
                let msg = format!("Compilation failed for '{}' with exit code {}", name, status);
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
                let msg = format!("Failed to run cargo for '{}': {}", name, e);
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
                let msg = format!("Task join error for '{}': {}", name, e);
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

    // 3. Determine YAML type and register with enabled=false
    let yaml_type = match plugins_yaml::get_disk_plugin_type(data_dir, &name) {
        Ok(Some(t)) => plugins_yaml::PluginYamlType::from_type_str(&t),
        _ => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({
                    "success": false,
                    "error": "Plugin not found after compilation"
                })),
            )
                .into_response();
        }
    };

    // Register with enabled=false (installed but not active)
    match plugins_yaml::set_entry(
        data_dir,
        &yaml_type,
        &name,
        false,
        serde_json::json!({}),
    ) {
        Ok(_entry) => match plugins_yaml::get_plugin(data_dir, &name) {
            Ok(Some(detail)) => {
                info!("Installed plugin '{}' (compiled + registered with disabled state)", name);
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
        },
        Err(e) => {
            error!("Failed to register plugin '{}' in YAML: {:?}", name, e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "success": false,
                    "error": format!("Compilation succeeded but YAML registration failed: {}", e)
                })),
            )
                .into_response()
        }
    }
}

/// POST /api/plugins/:name/reinstall — re-scan from disk, recompile if Rust, and reload.
pub(crate) async fn reinstall_plugin_handler(
    Path(name): Path<String>,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    // 1. Try to compile the plugin if it's a Rust crate
    let data_dir = &state.data_dir;
    let plugin_dirs = [
        format!("{}/plugins/mcp/{}", data_dir, name),
        format!("{}/plugins/installed/{}", data_dir, name),
        format!("{}/plugins/providers/{}", data_dir, name),
        format!("{}/plugins/platforms/{}", data_dir, name),
    ];

    let mut compiled = false;
    for dir in &plugin_dirs {
        let cargo_toml = std::path::Path::new(dir).join("Cargo.toml");
        if cargo_toml.exists() {
            tracing::info!("Reinstall: Rust crate detected at {}, compiling...", dir);
            match tokio::task::spawn_blocking({
                let dir = dir.clone();
                move || {
                    let status = std::process::Command::new("cargo")
                        .args(["build", "--release", "--manifest-path", &cargo_toml.to_string_lossy()])
                        .current_dir(dir)
                        .status();
                    status
                }
            })
            .await
            {
                Ok(Ok(status)) if status.success() => {
                    tracing::info!("Reinstall: Rust compilation succeeded for '{}'", name);
                    compiled = true;
                }
                Ok(Ok(status)) => {
                    let msg = format!("Reinstall: Rust compilation failed for '{}' with exit code {}", name, status);
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
                    let msg = format!("Reinstall: Failed to run cargo for '{}': {}", name, e);
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
                    let msg = format!("Reinstall: Task join error for '{}': {}", name, e);
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
            break;
        }
    }

    // 2. Re-scan from disk
    match plugins_yaml::get_plugin(data_dir, &name) {
        Ok(Some(detail)) => {
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

/// DELETE /api/plugins/:name — uninstall and remove from YAML.
pub(crate) async fn delete_plugin_handler(
    Path(name): Path<String>,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    // Remove from YAML (all three types, just in case)
    let mut removed = false;
    for yaml_type in &[
        plugins_yaml::PluginYamlType::Platform,
        plugins_yaml::PluginYamlType::Tool,
        plugins_yaml::PluginYamlType::Provider,
    ] {
        if let Ok(true) = plugins_yaml::remove_entry(&state.data_dir, yaml_type, &name) {
            removed = true;
        }
    }

    // Also remove from disk — scan all possible locations
    // For installed plugins: remove entire directory.
    // For bundled plugins: remove target/ (compiled binary) but keep source.
    let plugin_dirs = [
        format!("{}/plugins/installed/{}", &state.data_dir, name),
        format!("{}/plugins/mcp/{}", &state.data_dir, name),
        format!("{}/plugins/providers/{}", &state.data_dir, name),
        format!("{}/plugins/platforms/{}", &state.data_dir, name),
    ];
    let mut disk_removed = false;
    for dir in &plugin_dirs {
        let path = std::path::Path::new(dir);
        if path.exists() && path.is_dir() {
            // Check if this is a bundled plugin (has Cargo.toml or plugin.json from omni-stack)
            let is_bundled = path.join("Cargo.toml").exists()
                || (path.join("plugin.json").exists()
                    && dir.contains("/plugins/mcp/")
                    || dir.contains("/plugins/platforms/")
                    || dir.contains("/plugins/providers/"));
            if is_bundled {
                // Bundled plugin: remove target/ (compiled artifacts) only
                let target_dir = path.join("target");
                if target_dir.exists() {
                    match std::fs::remove_dir_all(&target_dir) {
                        Ok(()) => {
                            tracing::info!("Removed compiled artifacts for bundled plugin '{}'", name);
                            disk_removed = true;
                        }
                        Err(e) => {
                            tracing::warn!("Failed to remove target/ for '{}': {:?}", name, e);
                        }
                    }
                }
                // Also remove Cargo.lock if present (generated by build)
                let cargo_lock = path.join("Cargo.lock");
                if cargo_lock.exists() {
                    let _ = std::fs::remove_file(&cargo_lock);
                }
            } else {
                // Installed plugin: remove entire directory
                match std::fs::remove_dir_all(path) {
                    Ok(()) => {
                        tracing::info!("Deleted plugin directory: {}", dir);
                        disk_removed = true;
                    }
                    Err(e) => {
                        tracing::warn!("Failed to delete plugin directory '{}': {:?}", dir, e);
                    }
                }
            }
        }
    }

    if removed || disk_removed {
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
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({
                "success": false,
                "error": format!("Plugin '{}' not found on disk or in YAML", name)
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
