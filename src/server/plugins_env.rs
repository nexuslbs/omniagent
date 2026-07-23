//! Plugin environment reload handlers.
//!
//! Extracted from `plugins.rs` for separation of concerns.

use axum::{
    body::Body,
    extract::State,
    http::{Response, StatusCode},
    response::IntoResponse,
    Json,
};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use crate::server::AppState;

use super::plugins_reload::*;

pub(crate) fn reload_env_handler(
    State(state): State<Arc<AppState>>,
) -> Pin<Box<dyn Future<Output = Response<Body>> + Send>> {
    Box::pin(async move {
        tracing::info!("Reload: starting");
        let env_path = state.env_path.clone();
        tracing::info!("Reload: reading .env from {}", env_path);
        let env_refreshed = refresh_env_from_file(&env_path);
        tracing::info!(
            "Reload: .env read ({} vars), now reloading plugins",
            env_refreshed
        );

        // Run reload in a spawned task with a 10s timeout so a stuck
        // spawn_blocking or MCP init can't hang the endpoint forever.
        let state_clone = state.clone();
        let result = tokio::select! {
            result = tokio::spawn(async move { reload_plugins(state_clone).await }) => {
                match result {
                    Ok(r) => r,
                    Err(e) => Err(format!("Reload task panicked: {}", e)),
                }
            }
            _ = tokio::time::sleep(std::time::Duration::from_secs(10)) => {
                Err("Reload timed out after 10s".to_string())
            }
        };

        tracing::info!("Reload: plugins reloaded: {:?}", result.as_ref().ok());
        format_reload_response(env_refreshed, result)
    })
}

/// Format the reload response (extracted to reduce handler complexity for axum's trait solver).
pub(crate) fn format_reload_response(
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

/// Minimal plugin info needed for reload (name + type + status + optional entrypoint).
struct ReloadPluginInfo {
    name: String,
    plugin_type: String,
    status: String,
    /// Entrypoint command (loaded lazily from plugin.json for providers).
    entrypoint: Option<(String, Vec<String>)>,
}

/// Read plugin state directly from plugins.yml (no filesystem discovery).
/// This is ~1000x faster than list_plugins() and sufficient for reload.
fn read_plugins_from_yaml(data_dir: &str) -> Result<Vec<ReloadPluginInfo>, String> {
    use crate::plugins_yaml::PluginYamlEntry;

    let file = crate::plugins_yaml::load_all_sections(data_dir)
        .map_err(|e| format!("Failed to read plugins.yml: {}", e))?;

    let mut plugins = Vec::new();
    let sections: Vec<(
        &str,
        &str,
        Option<&std::collections::BTreeMap<String, PluginYamlEntry>>,
    )> = vec![
        ("platform", "platforms", file.platforms.as_ref()),
        ("tool", "tools", file.tools.as_ref()),
        ("provider", "providers", file.providers.as_ref()),
    ];

    for (type_name, type_dir, entries) in &sections {
        if let Some(entries) = entries {
            for (name, entry) in entries.iter() {
                let status = if entry.enabled { "enabled" } else { "disabled" };
                // Load entrypoint lazily only for providers (needed to start them)
                let entrypoint = if *type_name == "provider" {
                    let manifest_path =
                        format!("{}/plugins/{}/{}/plugin.json", data_dir, type_dir, name);
                    if let Ok(manifest) = crate::plugin::load_manifest(&manifest_path) {
                        manifest.entrypoint.map(|ep| (ep.command, ep.args))
                    } else {
                        None
                    }
                } else {
                    None
                };
                plugins.push(ReloadPluginInfo {
                    name: name.clone(),
                    plugin_type: type_name.to_string(),
                    status: status.to_string(),
                    entrypoint,
                });
            }
        }
    }

    Ok(plugins)
}

/// Helper: reload plugin runtime state from YAML on disk.
pub(crate) async fn reload_plugins(
    state: Arc<AppState>,
) -> Result<(u32, u32, Vec<String>), String> {
    let data_dir = state.data_dir.clone();
    let all_plugins = read_plugins_from_yaml(&data_dir)?;
    tracing::info!("Reload: listed {} plugins", all_plugins.len());

    // 2. Snapshot current runtime state (tokio locks, Send-safe)
    let active_mcp: std::collections::HashSet<String> = {
        state
            .plugin_manager
            .snapshot_registry()
            .await
            .all()
            .iter()
            .filter_map(|t| t.server_name.clone())
            .collect()
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
                    // Timeout to prevent a hanging MCP subprocess spawn
                    // from blocking the entire reload endpoint indefinitely.
                    match tokio::time::timeout(
                        std::time::Duration::from_secs(15),
                        state
                            .plugin_manager
                            .initialize_single_server(&state.data_dir, name),
                    )
                    .await
                    {
                        Ok(Ok(ts)) => {
                            state.plugin_manager.register_tools(ts).await;
                            started += 1;
                        }
                        Ok(Err(e)) => errors.push(format!("{} MCP: {}", name, e)),
                        Err(_) => {
                            errors.push(format!("{} MCP: initialization timed out (15s)", name))
                        }
                    }
                } else if !enabled && running {
                    state.plugin_manager.remove_server_tools(name).await;
                    stopped += 1;
                }
            }
            "provider" => {
                let running = crate::provider::registry::PROVIDER_REGISTRY
                    .read()
                    .expect("PROVIDER_REGISTRY lock poisoned")
                    .has_provider(name);
                if enabled && !running {
                    let provider = if let Some((cmd, args)) = &plugin.entrypoint {
                        crate::provider::registry::PROVIDER_REGISTRY
                            .write()
                            .expect("PROVIDER_REGISTRY lock poisoned")
                            .register(name, cmd, args);
                        crate::provider::registry::PROVIDER_REGISTRY
                            .read()
                            .expect("PROVIDER_REGISTRY lock poisoned")
                            .get_cloned(name)
                    } else {
                        None
                    };
                    if let Some(c) = provider {
                        match c.start().await {
                            Ok(()) => started += 1,
                            Err(e) => {
                                crate::provider::registry::PROVIDER_REGISTRY
                                    .write()
                                    .expect("PROVIDER_REGISTRY lock poisoned")
                                    .remove(name);
                                errors.push(format!("{} provider: {}", name, e));
                            }
                        }
                    }
                } else if !enabled && running {
                    crate::provider::registry::PROVIDER_REGISTRY
                        .write()
                        .expect("PROVIDER_REGISTRY lock poisoned")
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
                        flag.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
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
            state.plugin_manager.remove_server_tools(srv).await;
            stopped += 1;
        }
    }

    Ok((started, stopped, errors))
}
