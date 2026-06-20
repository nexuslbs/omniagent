//! Plugin management API endpoints.
//!
//! Provides REST endpoints for listing, installing, configuring, and
//! managing plugin lifecycle.

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{delete, get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use std::sync::Arc;
use tracing::{error, info};

use crate::plugin;
use crate::server::AppState;

// ---------------------------------------------------------------------------
// Request/Response types
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct UpdateConfigRequest {
    config: serde_json::Value,
}

#[derive(Deserialize)]
struct InstallUrlRequest {
    url: String,
}

#[derive(Serialize)]
struct ApiResponse<T: Serialize> {
    success: bool,
    data: Option<T>,
    error: Option<String>,
}

impl<T: Serialize> ApiResponse<T> {
    fn ok(data: T) -> Json<Self> {
        Json(Self {
            success: true,
            data: Some(data),
            error: None,
        })
    }
}

/// Helper to create an error response for list/detail endpoints.
fn err_response(msg: impl Into<String>) -> Json<ApiResponse<serde_json::Value>> {
    Json(ApiResponse {
        success: false,
        data: None,
        error: Some(msg.into()),
    })
}

/// Build the plugin management router, reusing the main server's state.
pub fn plugin_router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/plugins", get(list_plugins_handler))
        .route("/api/plugins/:name", get(get_plugin_handler))
        .route("/api/plugins/:name/config", post(update_config_handler))
        .route("/api/plugins/:name/enable", post(enable_plugin_handler))
        .route("/api/plugins/:name/disable", post(disable_plugin_handler))
        .route("/api/plugins/:name/reinstall", post(reinstall_plugin_handler))
        .route("/api/plugins/:name", delete(delete_plugin_handler))
        .route("/api/plugins/install-url", post(install_url_handler))
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// GET /api/plugins — list all plugins (with health/enriched data).
async fn list_plugins_handler(
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    match plugin::list_plugins(&state.pool).await {
        Ok(rows) => {
            let details: Vec<plugin::PluginDetail> =
                rows.iter().map(|r| plugin::enrich_plugin(r)).collect();
            (StatusCode::OK, Json(serde_json::json!({
                "success": true,
                "data": details
            }))).into_response()
        }
        Err(e) => {
            error!("Failed to list plugins: {:?}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({
                "success": false,
                "error": format!("Failed to list plugins: {}", e)
            }))).into_response()
        }
    }
}

/// GET /api/plugins/:name — get single plugin detail.
async fn get_plugin_handler(
    Path(name): Path<String>,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    match plugin::get_plugin_by_name(&state.pool, &name).await {
        Ok(Some(row)) => {
            let detail = plugin::enrich_plugin(&row);
            (StatusCode::OK, Json(serde_json::json!({
                "success": true,
                "data": detail
            }))).into_response()
        }
        Ok(None) => {
            (StatusCode::NOT_FOUND, Json(serde_json::json!({
                "success": false,
                "error": "Plugin not found"
            }))).into_response()
        }
        Err(e) => {
            error!("Failed to get plugin '{}': {:?}", name, e);
            (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({
                "success": false,
                "error": format!("Failed to get plugin: {}", e)
            }))).into_response()
        }
    }
}

/// POST /api/plugins/:name/config — update plugin config.
async fn update_config_handler(
    Path(name): Path<String>,
    State(state): State<Arc<AppState>>,
    Json(body): Json<UpdateConfigRequest>,
) -> impl IntoResponse {
    match plugin::update_plugin_config(&state.pool, &name, &body.config).await {
        Ok(row) => {
            let detail = plugin::enrich_plugin(&row);
            info!("Updated config for plugin '{}'", name);
            (StatusCode::OK, Json(serde_json::json!({
                "success": true,
                "data": detail
            }))).into_response()
        }
        Err(e) => {
            if e.to_string().contains("no rows") {
                (StatusCode::NOT_FOUND, Json(serde_json::json!({
                    "success": false,
                    "error": "Plugin not found"
                }))).into_response()
            } else {
                error!("Failed to update config for plugin '{}': {:?}", name, e);
                (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({
                    "success": false,
                    "error": format!("Failed to update config: {}", e)
                }))).into_response()
            }
        }
    }
}

/// POST /api/plugins/:name/enable — set status to 'enabled'.
async fn enable_plugin_handler(
    Path(name): Path<String>,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    match plugin::update_plugin_status(&state.pool, &name, "enabled").await {
        Ok(row) => {
            let detail = plugin::enrich_plugin(&row);
            info!("Enabled plugin '{}'", name);
            (StatusCode::OK, Json(serde_json::json!({
                "success": true,
                "data": detail
            }))).into_response()
        }
        Err(e) => {
            if e.to_string().contains("no rows") {
                (StatusCode::NOT_FOUND, Json(serde_json::json!({
                    "success": false,
                    "error": "Plugin not found"
                }))).into_response()
            } else {
                error!("Failed to enable plugin '{}': {:?}", name, e);
                (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({
                    "success": false,
                    "error": format!("Failed to enable plugin: {}", e)
                }))).into_response()
            }
        }
    }
}

/// POST /api/plugins/:name/disable — set status to 'disabled'.
async fn disable_plugin_handler(
    Path(name): Path<String>,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    match plugin::update_plugin_status(&state.pool, &name, "disabled").await {
        Ok(row) => {
            let detail = plugin::enrich_plugin(&row);
            info!("Disabled plugin '{}'", name);
            (StatusCode::OK, Json(serde_json::json!({
                "success": true,
                "data": detail
            }))).into_response()
        }
        Err(e) => {
            if e.to_string().contains("no rows") {
                (StatusCode::NOT_FOUND, Json(serde_json::json!({
                    "success": false,
                    "error": "Plugin not found"
                }))).into_response()
            } else {
                error!("Failed to disable plugin '{}': {:?}", name, e);
                (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({
                    "success": false,
                    "error": format!("Failed to disable plugin: {}", e)
                }))).into_response()
            }
        }
    }
}

/// POST /api/plugins/:name/reinstall — re-scan from disk and reload.
async fn reinstall_plugin_handler(
    Path(name): Path<String>,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    // Run a full sync from disk (will update this plugin if found)
    match plugin::sync_plugins_from_disk(&state.pool, &state.data_dir).await {
        Ok(_) => {
            match plugin::get_plugin_by_name(&state.pool, &name).await {
                Ok(Some(row)) => {
                    let detail = plugin::enrich_plugin(&row);
                    info!("Reinstalled plugin '{}'", name);
                    (StatusCode::OK, Json(serde_json::json!({
                        "success": true,
                        "data": detail
                    }))).into_response()
                }
                Ok(None) => {
                    (StatusCode::NOT_FOUND, Json(serde_json::json!({
                        "success": false,
                        "error": format!("Plugin '{}' not found on disk after re-scan", name)
                    }))).into_response()
                }
                Err(e) => {
                    (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({
                        "success": false,
                        "error": format!("Error checking plugin after reinstall: {}", e)
                    }))).into_response()
                }
            }
        }
        Err(e) => {
            error!("Failed to re-scan plugins from disk: {:?}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({
                "success": false,
                "error": format!("Failed to re-scan plugins: {}", e)
            }))).into_response()
        }
    }
}

/// DELETE /api/plugins/:name — remove from registry and files.
async fn delete_plugin_handler(
    Path(name): Path<String>,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    match plugin::delete_plugin(&state.pool, &name).await {
        Ok(true) => {
            // Also remove from disk if it's an installed plugin
            let _ = plugin::installer::uninstall(&name, &state.data_dir);
            info!("Deleted plugin '{}'", name);
            (StatusCode::OK, Json(serde_json::json!({
                "success": true,
                "data": {"deleted": true}
            }))).into_response()
        }
        Ok(false) => {
            (StatusCode::NOT_FOUND, Json(serde_json::json!({
                "success": false,
                "error": "Plugin not found"
            }))).into_response()
        }
        Err(e) => {
            error!("Failed to delete plugin '{}': {:?}", name, e);
            (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({
                "success": false,
                "error": format!("Failed to delete plugin: {}", e)
            }))).into_response()
        }
    }
}

/// POST /api/plugins/install-url — install a plugin from a URL.
async fn install_url_handler(
    State(state): State<Arc<AppState>>,
    Json(body): Json<InstallUrlRequest>,
) -> impl IntoResponse {
    info!("Installing plugin from URL: {}", body.url);

    // Download and extract
    let manifest = match plugin::installer::install_from_url(&body.url, &state.data_dir) {
        Ok(m) => m,
        Err(e) => {
            error!("Failed to install plugin from {}: {:?}", body.url, e);
            return (StatusCode::BAD_REQUEST, Json(serde_json::json!({
                "success": false,
                "error": format!("Installation failed: {}", e)
            }))).into_response();
        }
    };

    // Register in database
    let manifest_json = serde_json::to_value(&manifest).unwrap_or_default();
    let plugin_type_str = match manifest.plugin_type {
        plugin::PluginType::Platform => "platform",
        plugin::PluginType::Mcp => "mcp",
    };

    match plugin::upsert_plugin(
        &state.pool,
        &manifest.name,
        plugin_type_str,
        &manifest.version,
        Some(&body.url),
        &manifest_json,
        &serde_json::json!({}),
    )
    .await
    {
        Ok(row) => {
            let detail = plugin::enrich_plugin(&row);
            info!(
                "Successfully installed plugin '{}' version {} from {}",
                manifest.name, manifest.version, body.url
            );
            (StatusCode::CREATED, Json(serde_json::json!({
                "success": true,
                "data": detail
            }))).into_response()
        }
        Err(e) => {
            error!("Installed plugin from disk but failed to register in DB: {:?}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({
                "success": false,
                "error": format!("Plugin extracted but DB registration failed: {}", e)
            }))).into_response()
        }
    }
}
