//! Plugin environment reload handlers.
//!
//! Extracted from `plugins.rs` for separation of concerns.

use axum::{
    extract::State,
    http::{StatusCode, Response},
    response::IntoResponse,
    body::Body,
    Json,
};
use std::pin::Pin;
use std::future::Future;
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
        tracing::info!("Reload: .env read ({} vars), now reloading plugins", env_refreshed);

        // Run reload in a spawned task with a 30s timeout so a stuck
        // spawn_blocking or MCP init can't hang the endpoint forever.
        let state_clone = state.clone();
        let result = tokio::select! {
            result = tokio::spawn(async move { reload_plugins(state_clone).await }) => {
                match result {
                    Ok(r) => r,
                    Err(e) => Err(format!("Reload task panicked: {}", e)),
                }
            }
            _ = tokio::time::sleep(std::time::Duration::from_secs(60)) => {
                Err("Reload timed out after 60s".to_string())
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


/// Helper: reload plugin runtime state from YAML on disk.
pub(crate) async fn reload_plugins(state: Arc<AppState>) -> Result<(u32, u32, Vec<String>), String> {
    let data_dir = state.data_dir.clone();
    let all_plugins = {
        // Use a dedicated OS thread (not tokio's shared blocking pool) so that
        // a timed-out reload can't saturate the blocking thread pool and hang
        // future reload requests. The oneshot channel lets this future complete
        // independently of the spawn_blocking thread-pool queue.
        let (tx, rx) = tokio::sync::oneshot::channel::<crate::error::AppResult<Vec<crate::plugins_yaml::PluginDetail>>>();
        std::thread::spawn(move || {
            let result = crate::plugins_yaml::list_plugins(&data_dir);
            if let Err(e) = tx.send(result) {
                tracing::warn!("[plugins] Reload: failed to send plugin list: {:?}", e);
            }
        });
        tokio::time::timeout(std::time::Duration::from_secs(25), rx)
            .await
            .map_err(|_| "list_plugins timed out after 25s")?
            .map_err(|_| "list_plugins thread dropped")?
            .map_err(|e| format!("list_plugins: {}", e))?
    };
    tracing::info!("Reload: listed {} plugins", all_plugins.len());

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
                    // Timeout to prevent a hanging MCP subprocess spawn
                    // from blocking the entire reload endpoint indefinitely.
                    match tokio::time::timeout(
                        std::time::Duration::from_secs(15),
                        crate::mcp::external::client::initialize_single_server_tools(
                            &state.data_dir,
                            name,
                        ),
                    )
                    .await
                    {
                        Ok(Ok(ts)) => {
                            state.tool_registry.write().await.register_all(ts);
                            started += 1;
                        }
                        Ok(Err(e)) => errors.push(format!("{} MCP: {}", name, e)),
                        Err(_) => errors.push(format!(
                            "{} MCP: initialization timed out (15s)",
                            name
                        )),
                    }
                } else if !enabled && running {
                    state.tool_registry.write().await.remove_by_server(name);
                    stopped += 1;
                }
            }
            "provider" => {
                let running = crate::provider::registry::PROVIDER_REGISTRY
                    .read()
                    .expect("PROVIDER_REGISTRY lock poisoned")
                    .has_provider(name);
                if enabled && !running {
                    let provider = {
                        let guard = crate::provider::registry::PROVIDER_REGISTRY
                            .read()
                            .expect("PROVIDER_REGISTRY lock poisoned");
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
                                    .expect("PROVIDER_REGISTRY lock poisoned")
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
            state.tool_registry.write().await.remove_by_server(srv);
            stopped += 1;
        }
    }

    Ok((started, stopped, errors))
}

