//! Plugin enable / disable / restart handlers.
//!
//! Concurrency contract (bulk lifecycle bursts, 2026-09-11):
//!
//! Every handler below runs its WHOLE operation (config write + process apply)
//! through [`crate::plugin::lifecycle::PluginLifecycle`], so that
//!   * only one operation per plugin runs at a time (FIFO): concurrent enable,
//!     disable and restart calls for the same plugin are queued, and the last
//!     queued operation decides the end state. A disable issued while a restart
//!     is in flight is therefore applied after the restart and the plugin ends
//!     STOPPED;
//!   * at most `MAX_CONCURRENT_LIFECYCLE_OPS` subprocess operations run
//!     concurrently ACROSS plugins, so a 10-plugin burst cannot become an
//!     unbounded process storm;
//!   * duplicated concurrent enable calls for one plugin perform exactly ONE
//!     start (queue + the idempotent "already enabled" check below);
//!   * slow steps (MCP spawn + handshake) are bounded by
//!     `LIFECYCLE_STEP_TIMEOUT` and reported per plugin (HTTP status + reason)
//!     without deleting the plugin config or failing other plugins.
//!
//! Read paths (GET /api/plugins, /api/tools, prompt building, the executor
//! registry snapshot) never take these gates and stay responsive while
//! restarts are in flight.

use super::plugins_reload::*;
use super::plugins_types::*;
use crate::plugin::lifecycle::{self, PluginLifecycle};
use crate::plugins_yaml;
use crate::server::AppState;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use std::sync::Arc;
use tracing::{error, warn};

/// Lifecycle gate key for a plugin: type-scoped so a tool and a platform with
/// the same name never share a gate.
fn lifecycle_key(yaml_type: &plugins_yaml::PluginYamlType, name: &str) -> String {
    format!("{}/{}", yaml_type.to_type_str(), name)
}

fn error_response(status: StatusCode, message: String) -> Response {
    (
        status,
        Json(serde_json::json!({"success": false, "error": message})),
    )
        .into_response()
}

/// The per-plugin errors reported by a `reload_plugins` sweep.
///
/// The sweep is a global operation, so its error list may mention unrelated
/// plugins that were already failing. Only the entries naming THIS plugin are
/// this call's business: one broken plugin must never fail another plugin's
/// lifecycle call.
fn reload_errors_for(errors: &[String], name: &str) -> Vec<String> {
    let prefix = format!("{} ", name);
    errors
        .iter()
        .filter(|e| e.starts_with(&prefix))
        .cloned()
        .collect()
}

pub(crate) async fn enable_plugin_handler(
    Path((p_type, source, name)): Path<(String, String, String)>,
    State(state): State<Arc<AppState>>,
) -> Response {
    if let Err(e) = validate_plugin_type(&p_type) {
        return e.into_response();
    }
    if let Err(e) = validate_source(&source) {
        return e.into_response();
    }
    let yaml_type = plugins_yaml::PluginYamlType::from_type_str(&p_type);
    let key = lifecycle_key(&yaml_type, &name);
    PluginLifecycle::global()
        .run(&key, apply_enable(state, yaml_type, source, name))
        .await
}

/// Serialized body of `enable`: idempotency check, config write, process apply.
async fn apply_enable(
    state: Arc<AppState>,
    yaml_type: plugins_yaml::PluginYamlType,
    source: String,
    name: String,
) -> Response {
    if let Ok(Some(entry)) = plugins_yaml::get_entry(&state.data_dir, &yaml_type, &name) {
        if entry.enabled && entry.source == source {
            // Already enabled - idempotent no-op: just return the plugin detail.
            // (Previously this branch force-restarted the plugin, which is the
            // job of the dedicated /restart endpoint, not /enable.) The
            // lifecycle queue guarantees this check reads the state left by the
            // previous operation for this plugin, so a burst of duplicate
            // enables still performs exactly one start.
            //
            // EXCEPT for providers: reload_plugins is the ONLY place that
            // spawns the provider subprocess, and on a cold stack (fresh
            // deploy, container restart) the subprocess has not been started
            // yet - nothing triggers the startup reload. If we return here
            // without reloading, an enabled provider stays subprocess-less
            // until some unrelated API call happens to run reload_plugins, and
            // the first LLM completion falls back to HTTP and fails. Reload is
            // idempotent for already-running providers (entrypoint unchanged ->
            // no restart), so it is safe to run it in the idempotent branch.
            if yaml_type == plugins_yaml::PluginYamlType::Provider {
                crate::llm::refresh_provider_metadata();
                if let Err(e) = super::plugins_env::reload_plugins(state.clone()).await {
                    warn!("Provider reload for '{}' failed: {}", name, e);
                }
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
                let what = format!("tool '{}' MCP init", name);
                match lifecycle::step_timeout(
                    &what,
                    state
                        .plugin_manager
                        .initialize_single_server(&state.data_dir, &name),
                )
                .await
                {
                    Ok(tools) => state.plugin_manager.register_tools(tools).await,
                    Err(reason) => {
                        // Keep the entry (and its config) but reflect reality:
                        // the plugin is not running, so it is disabled. The old
                        // code DELETED the entry here, which lost the user's
                        // configuration and enabled intent whenever a start
                        // failed (observed during concurrent lifecycle bursts).
                        let _ =
                            plugins_yaml::set_enabled(&state.data_dir, &yaml_type, &name, false);
                        state.plugin_manager.remove_client(&name);
                        error!("Failed to start tool plugin '{}': {}", name, reason);
                        return error_response(
                            StatusCode::BAD_GATEWAY,
                            format!("MCP server for '{}' failed to start: {}", name, reason),
                        );
                    }
                }
            }
            if yaml_type == plugins_yaml::PluginYamlType::Platform {
                // Platform starts are signal based; the bound only guards an
                // unresponsive platform plugin from holding its lifecycle gate
                // (and one of the bounded slots) forever.
                if tokio::time::timeout(
                    lifecycle::LIFECYCLE_STEP_TIMEOUT,
                    reload_platform_plugin(&state, &name),
                )
                .await
                .is_err()
                {
                    return error_response(
                        StatusCode::GATEWAY_TIMEOUT,
                        format!(
                            "platform plugin '{}' did not start within {}s",
                            name,
                            lifecycle::LIFECYCLE_STEP_TIMEOUT.as_secs()
                        ),
                    );
                }
            }
            if yaml_type == plugins_yaml::PluginYamlType::Provider {
                crate::llm::refresh_provider_metadata();
                // Trigger plugin reload to start/stop external provider subprocess
                match super::plugins_env::reload_plugins(state.clone()).await {
                    Ok((_started, _stopped, errors)) => {
                        let mine = reload_errors_for(&errors, &name);
                        if !mine.is_empty() {
                            return error_response(
                                StatusCode::BAD_GATEWAY,
                                format!("provider '{}' failed to start: {}", name, mine.join("; ")),
                            );
                        }
                    }
                    Err(e) => {
                        return error_response(
                            StatusCode::INTERNAL_SERVER_ERROR,
                            format!("provider '{}' reload failed: {}", name, e),
                        );
                    }
                }
            }
            match plugins_yaml::get_plugin(&state.data_dir, &name, &yaml_type) {
                Ok(Some(detail)) => (StatusCode::OK, Json(serde_json::json!({"success": true, "data": detail}))).into_response(),
                _ => (StatusCode::OK, Json(serde_json::json!({"success": true, "data": {"name": name, "status": "enabled"}}))).into_response(),
            }
        }
        Err(e) => {
            error!("Failed to enable plugin '{}': {:?}", name, e);
            error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to enable plugin: {}", e),
            )
        }
    }
}

pub(crate) async fn disable_plugin_handler(
    Path((p_type, source, name)): Path<(String, String, String)>,
    State(state): State<Arc<AppState>>,
) -> Response {
    if let Err(e) = validate_plugin_type(&p_type) {
        return e.into_response();
    }
    if let Err(e) = validate_source(&source) {
        return e.into_response();
    }
    let yaml_type = plugins_yaml::PluginYamlType::from_type_str(&p_type);
    let key = lifecycle_key(&yaml_type, &name);
    PluginLifecycle::global()
        .run(&key, apply_disable(state, yaml_type, source, name))
        .await
}

/// Serialized body of `disable`: config write first, process teardown second.
///
/// Because the queue serializes per plugin, a disable that arrives while a
/// restart is in flight runs after it, so the plugin ends STOPPED.
async fn apply_disable(
    state: Arc<AppState>,
    yaml_type: plugins_yaml::PluginYamlType,
    source: String,
    name: String,
) -> Response {
    // Preserve existing config when disabling - only toggle the enabled flag.
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
                match super::plugins_env::reload_plugins(state.clone()).await {
                    Ok((_started, _stopped, errors)) => {
                        // The config change (authoritative state) is applied; the
                        // sweep only tears the subprocess down, so its errors are
                        // logged (an unrelated broken plugin must not fail this
                        // disable call).
                        let mine = reload_errors_for(&errors, &name);
                        if !mine.is_empty() {
                            warn!(
                                "Provider reload reported for '{}': {}",
                                name,
                                mine.join("; ")
                            );
                        }
                    }
                    Err(e) => warn!("Provider reload for '{}' failed: {}", name, e),
                }
            }
            match plugins_yaml::get_plugin(&state.data_dir, &name, &yaml_type) {
                Ok(Some(detail)) => (StatusCode::OK, Json(serde_json::json!({"success": true, "data": detail}))).into_response(),
                _ => (StatusCode::OK, Json(serde_json::json!({"success": true, "data": {"name": name, "status": "disabled"}}))).into_response(),
            }
        }
        Err(e) => {
            error!("Failed to disable plugin '{}': {:?}", name, e);
            error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to disable plugin: {}", e),
            )
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
) -> Response {
    if let Err(e) = validate_plugin_type(&p_type) {
        return e.into_response();
    }
    let yaml_type = plugins_yaml::PluginYamlType::from_type_str(&p_type);
    let key = lifecycle_key(&yaml_type, &name);
    PluginLifecycle::global()
        .run(&key, apply_restart(state, yaml_type, name))
        .await
}

/// Serialized body of `restart`: exactly one restart per plugin at a time, with
/// a bounded, reported outcome.
async fn apply_restart(
    state: Arc<AppState>,
    yaml_type: plugins_yaml::PluginYamlType,
    name: String,
) -> Response {
    match restart_action_for(&yaml_type) {
        RestartAction::Tool => {
            match restart_tool_plugin(&state, &name).await {
                Ok(count) => (
                    StatusCode::OK,
                    Json(serde_json::json!({"success": true, "data": {"name": name, "status": "restarted", "tools": count}})),
                )
                    .into_response(),
                Err(reason) => {
                    error!("Restart of tool plugin '{}' failed: {}", name, reason);
                    error_response(
                        StatusCode::BAD_GATEWAY,
                        format!("restart of '{}' failed: {}", name, reason),
                    )
                }
            }
        }
        RestartAction::Platform => {
            reload_platform_plugin(&state, &name).await;
            (StatusCode::OK, Json(serde_json::json!({"success": true}))).into_response()
        }
        RestartAction::Provider => {
            crate::llm::refresh_provider_metadata();
            match super::plugins_env::reload_plugins(state.clone()).await {
                Ok((_started, _stopped, errors)) => {
                    let mine = reload_errors_for(&errors, &name);
                    if mine.is_empty() {
                        (StatusCode::OK, Json(serde_json::json!({"success": true}))).into_response()
                    } else {
                        error_response(
                            StatusCode::BAD_GATEWAY,
                            format!("restart of '{}' failed: {}", name, mine.join("; ")),
                        )
                    }
                }
                Err(e) => error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("restart of '{}' failed: {}", name, e),
                ),
            }
        }
    }
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

    #[test]
    fn lifecycle_key_is_type_scoped() {
        assert_eq!(
            lifecycle_key(&plugins_yaml::PluginYamlType::Tool, "git"),
            "tool/git"
        );
        assert_eq!(
            lifecycle_key(&plugins_yaml::PluginYamlType::Platform, "git"),
            "platform/git"
        );
        assert_ne!(
            lifecycle_key(&plugins_yaml::PluginYamlType::Tool, "git"),
            lifecycle_key(&plugins_yaml::PluginYamlType::Platform, "git")
        );
    }

    #[test]
    fn reload_errors_are_scoped_to_the_named_plugin() {
        let errors = vec![
            "web MCP: initialization timed out (15s)".to_string(),
            "webhook MCP: boom".to_string(),
            "engram provider: start timed out (15s)".to_string(),
        ];
        assert_eq!(
            reload_errors_for(&errors, "web"),
            vec!["web MCP: initialization timed out (15s)".to_string()]
        );
        // A prefix must match the whole name: "webhook" is a different plugin.
        assert_eq!(reload_errors_for(&errors, "web").len(), 1);
        assert!(reload_errors_for(&errors, "web")
            .iter()
            .all(|e| !e.contains("webhook")));
        assert_eq!(reload_errors_for(&errors, "engram").len(), 1);
        assert!(reload_errors_for(&errors, "unrelated").is_empty());
    }
}
