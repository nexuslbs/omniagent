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

    // Update config in YAML. Boolean config values are first normalized to
    // canonical JSON booleans (schema-driven): the dashboard checkbox sends
    // "on"/"off" form strings and other truthy spellings can arrive from any
    // client. Storing a canonical boolean keeps re-reads, sandbox decisions
    // and the generic boolean renderer consistent.
    let mut new_config = body.config.clone();
    canonicalize_config_booleans(&mut new_config, &state.data_dir, &name, &yaml_type);
    match plugins_yaml::update_config(&state.data_dir, &yaml_type, &name, new_config) {
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
            match plugins_yaml::get_plugin(&state.data_dir, &name, &yaml_type) {
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
    Path((p_type, _source, name)): Path<(String, String, String)>,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    let pt = plugins_yaml::PluginYamlType::from_type_str(&p_type);
    match plugins_yaml::refresh_plugin_models(&state.data_dir, &name, &pt, &state.pool).await {
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

// ---------------------------------------------------------------------------
// Boolean config normalization (schema-driven)
// ---------------------------------------------------------------------------

/// Normalize the boolean values of `config` to canonical JSON booleans for the
/// fields the plugin's config_schema declares as `type: boolean`. Accepts JSON
/// booleans, numbers, and every truthy/falsy string spelling a form/API/config
/// path can produce ("true"/"false"/"on"/"off"/"yes"/"no"/"1"/"0",
/// case-insensitive). Values for other field types and unknown keys are left
/// untouched.
fn canonicalize_config_booleans_with_schema(
    config: &mut serde_json::Value,
    schema: &[crate::plugin::ConfigSchemaField],
) {
    let Some(obj) = config.as_object_mut() else {
        return;
    };
    for field in schema {
        if field.field_type != crate::plugin::FieldType::Boolean {
            continue;
        }
        let Some(value) = obj.get_mut(&field.key) else {
            continue;
        };
        let parsed = match value {
            serde_json::Value::Bool(b) => Some(*b),
            serde_json::Value::String(s) => Some(parse_boolish_config(s)),
            serde_json::Value::Number(n) => Some(n.as_f64().is_some_and(|f| f != 0.0)),
            _ => None,
        };
        if let Some(b) = parsed {
            *value = serde_json::Value::Bool(b);
        }
    }
}

/// Fetch the plugin's declared schema and normalize its boolean config values.
/// Schema lookup failures are not fatal: values are left untouched and the
/// plugin's own parser decides.
fn canonicalize_config_booleans(
    config: &mut serde_json::Value,
    data_dir: &str,
    name: &str,
    yaml_type: &plugins_yaml::PluginYamlType,
) {
    let Ok(Some(detail)) = plugins_yaml::get_plugin(data_dir, name, yaml_type) else {
        return;
    };
    canonicalize_config_booleans_with_schema(config, &detail.config_schema);
}

/// Every truthy/falsy spelling a form/API/config path can produce.
fn parse_boolish_config(s: &str) -> bool {
    matches!(
        s.trim().to_ascii_lowercase().as_str(),
        "true" | "on" | "yes" | "1"
    )
}

#[cfg(test)]
mod boolean_normalize_tests {
    use super::*;

    #[test]
    fn parse_boolish_accepts_every_spelling() {
        for truthy in ["true", "TRUE", "on", "On", "yes", "YES", "1", " true "] {
            assert!(
                parse_boolish_config(truthy),
                "truthy spelling rejected: '{truthy}'"
            );
        }
        for falsy in ["false", "FALSE", "off", "Off", "no", "NO", "0", "", "maybe"] {
            assert!(
                !parse_boolish_config(falsy),
                "falsy spelling accepted: '{falsy}'"
            );
        }
    }

    #[test]
    fn canonicalize_turns_bool_schema_fields_into_booleans() {
        let schema: Vec<crate::plugin::ConfigSchemaField> =
            serde_json::from_value(serde_json::json!([
                {
                    "key": "allow_omni_dir",
                    "label": "Allow OMNI_DIR",
                    "type": "boolean",
                    "required": false,
                    "secret": false
                }
            ]))
            .expect("schema parses");
        let mut cfg = serde_json::json!({
            "allow_omni_dir": "on",
            "workspace_dir": "/opt/workspace",
            "count": 3
        });
        canonicalize_config_booleans_with_schema(&mut cfg, &schema);
        assert_eq!(cfg["allow_omni_dir"], serde_json::Value::Bool(true));
        // Non-boolean keys are untouched.
        assert_eq!(cfg["workspace_dir"], serde_json::json!("/opt/workspace"));
        assert_eq!(cfg["count"], serde_json::json!(3));

        for raw in [
            serde_json::json!("off"),
            serde_json::json!("0"),
            serde_json::json!(false),
        ] {
            let mut cfg = serde_json::json!({ "allow_omni_dir": raw });
            canonicalize_config_booleans_with_schema(&mut cfg, &schema);
            assert_eq!(
                cfg["allow_omni_dir"],
                serde_json::Value::Bool(false),
                "raw: {raw}"
            );
        }
        for raw in [
            serde_json::json!("yes"),
            serde_json::json!("1"),
            serde_json::json!(true),
        ] {
            let mut cfg = serde_json::json!({ "allow_omni_dir": raw });
            canonicalize_config_booleans_with_schema(&mut cfg, &schema);
            assert_eq!(
                cfg["allow_omni_dir"],
                serde_json::Value::Bool(true),
                "raw: {raw}"
            );
        }
    }
}
