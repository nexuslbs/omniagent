//! Plugin hot-reload and environment refresh utilities.
//!
//! Extracted from `plugins.rs` for separation of concerns.
//! Contains functions for refreshing .env files, reloading platform/tool
//! plugins after config changes, and name sanitization.

use std::sync::atomic::Ordering;
use std::sync::Arc;

use crate::platform::Platform;
use crate::server::AppState;

/// Read a `.env` file and set all key=value pairs as environment variables.
/// Returns the number of variables that were refreshed.
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
/// If the platform is already running, a restart is signalled.
/// If the platform is NOT running (e.g. enabled after boot), it is started dynamically.
pub(crate) async fn reload_platform_plugin(state: &Arc<AppState>, name: &str) {
    tracing::info!("Reloading platform plugin '{}' after config update", name);

    let refreshed = refresh_env_from_file(&state.env_path);
    if refreshed > 0 {
        tracing::info!(
            "Refreshed {} env var(s) from .env for platform plugin reload",
            refreshed
        );
    }

    // Check if the platform is already running (has registered restart signals)
    let signal = {
        let signals = state.platform_restart_signals.lock().await;
        signals.get(name).cloned()
    };

    if let Some((restart_count, _stopped, restart_notify)) = signal {
        // Platform is running - signal a restart
        restart_count.fetch_add(1, Ordering::SeqCst);
        restart_notify.notify_one();
        tracing::info!(
            "Set restart counter for platform plugin '{}': subprocess will be respawned (count: {})",
            name,
            restart_count.load(Ordering::SeqCst)
        );
    } else {
        // Platform is NOT running - start it dynamically
        tracing::info!(
            "Platform plugin '{}' is not currently running - starting dynamically",
            name
        );
        if let Err(e) = start_platform_plugin(state, name).await {
            tracing::error!("Failed to start platform plugin '{}': {}", name, e);
        }
    }
}

/// Start a platform plugin dynamically (after boot, when enabled via API).
///
/// Creates a new `ExternalPlatformClient`, registers its restart signals,
/// sets up the outbound message channel, adds the sender to the shared
/// platform senders map, and spawns the client's main loop in a tokio task.
pub(crate) async fn start_platform_plugin(state: &Arc<AppState>, name: &str) -> Result<(), String> {
    tracing::info!("Starting platform plugin '{}' dynamically", name);

    // 1. Load platform config from disk
    let configs = crate::platform::external::load_plugins_config(&state.data_dir);
    let plugin_config = match configs.into_iter().find(|c| c.name == name) {
        Some(c) => {
            if !c.enabled {
                return Err(format!("Platform plugin '{}' is disabled in config", name));
            }
            c
        }
        None => {
            return Err(format!(
                "Platform plugin '{}' not found in plugin config",
                name
            ));
        }
    };

    // 2. Create the ExternalPlatformClient
    //    This automatically registers restart/stop signals in the shared map.
    let client = Arc::new(
        crate::platform::external::client::ExternalPlatformClient::new(
            plugin_config.clone(),
            &state.data_dir,
            state.platform_restart_signals.clone(),
        )
        .await,
    );

    // 3. Create an outbound delivery channel (sender + receiver)
    let (tx, rx) = crate::platform::queue::outbound_channel(1024);

    // 4. Add the sender to the shared platform senders map
    //    This must be done BEFORE spawning the client, so the agent can start
    //    delivering messages immediately when the platform is ready.
    {
        let mut senders = state.app_context.platform_senders.write().await;
        senders.insert(name.to_string(), tx);
        tracing::info!("Registered outbound sender for platform plugin '{}'", name);
    }

    // 5. Register platform client for the read_attached_file MCP tool
    // The platform plugin implements read_file internally, so the core
    // stays plugin-agnostic - no knowledge of field names like access_token.
    // Just store the Arc<dyn Platform> in AppContext for the MCP tool to use.
    state.app_context.platforms.write().await.insert(
        name.to_string(),
        client.clone() as Arc<dyn crate::platform::Platform>,
    );

    // 6. Spawn the client's start loop in a background task
    let pool = state.pool.clone();
    let name_for_spawn = name.to_string();
    tokio::spawn(async move {
        tracing::info!(
            "Starting dynamically-enabled platform plugin: {}",
            name_for_spawn
        );
        if let Err(e) = client.start(pool, rx).await {
            tracing::error!(
                "Platform plugin '{}' exited with error: {:?}",
                name_for_spawn,
                e
            );
        } else {
            tracing::info!("Platform plugin '{}' stopped cleanly", name_for_spawn);
        }
    });

    tracing::info!(
        "Platform plugin '{}' started dynamically (task spawned)",
        name
    );
    Ok(())
}

/// Stop a running platform plugin.
///
/// Sets the stopped flag in the shared restart signals, notifies the
/// platform's outer loop, and removes the sender from the shared map
/// so no further outbound messages are sent to this platform.
pub(crate) async fn stop_platform_plugin(state: &Arc<AppState>, name: &str) {
    tracing::info!("Stopping platform plugin '{}'", name);

    // 1. Remove sender from the shared map so no more outbound messages
    //    are sent to this platform.
    {
        let mut senders = state.app_context.platform_senders.write().await;
        senders.remove(name);
        tracing::info!("Removed outbound sender for platform plugin '{}'", name);
    }

    // 2. Remove platform from the shared platforms map (for read_attached_file)
    {
        let mut platforms = state.app_context.platforms.write().await;
        platforms.remove(name);
    }

    // 3. Signal the running client to stop (set stopped flag + notify)
    let signal = {
        let mut signals = state.platform_restart_signals.lock().await;
        signals.remove(name)
    };

    if let Some((_restart_count, stopped, restart_notify)) = signal {
        stopped.store(true, Ordering::SeqCst);
        restart_notify.notify_one();
        tracing::info!(
            "Set stop flag for platform plugin '{}': subprocess will exit",
            name
        );
    } else {
        tracing::warn!(
            "Platform plugin '{}' was not registered - already stopped or never started",
            name
        );
    }
}

/// Trigger a hot-reload of a tool (MCP) plugin after its config has been updated.
/// Failures are logged (the config is already saved and the next reload retries);
/// callers that must report the outcome use [`restart_tool_plugin`].
pub(crate) async fn reload_tool_plugin(state: &Arc<AppState>, name: &str) {
    if let Err(e) = restart_tool_plugin(state, name).await {
        tracing::warn!(
            "Hot-reload of MCP server '{}' after config update failed (config saved, will retry on next restart): {}",
            name,
            e
        );
    }
}

/// Restart one tool (MCP) plugin: drop the existing client, spawn a fresh one
/// and register its tools. The spawn+handshake step is bounded by
/// `lifecycle::LIFECYCLE_STEP_TIMEOUT`, so an unresponsive plugin can neither
/// wedge the API nor hold its lifecycle gate forever; the failure is returned so
/// the lifecycle handler can report it per plugin. Returns the registered tool
/// count.
pub(crate) async fn restart_tool_plugin(
    state: &Arc<AppState>,
    name: &str,
) -> Result<usize, String> {
    tracing::info!("Reloading tool plugin '{}' after config update", name);

    let refreshed = refresh_env_from_file(&state.env_path);
    if refreshed > 0 {
        tracing::info!(
            "Refreshed {} env var(s) from .env for tool plugin reload",
            refreshed
        );
    }

    state.plugin_manager.remove_client(name);

    let what = format!("tool '{}' MCP init", name);
    let tools = crate::plugin::lifecycle::step_timeout(
        &what,
        state
            .plugin_manager
            .initialize_single_server(&state.data_dir, name),
    )
    .await?;

    let count = tools.len();
    state.plugin_manager.remove_server_tools(name).await;
    state.plugin_manager.register_tools(tools).await;
    tracing::info!(
        "Hot-reloaded {} tool(s) from MCP server '{}' after config update (no restart needed)",
        count,
        name
    );
    Ok(count)
}

/// Sanitize a plugin name for use as a YAML key and directory path.
/// - Trims whitespace
/// - NFD-normalizes to decompose diacritics
/// - Converts to lowercase
/// - Replaces runs of whitespace with a single hyphen
/// - Strips any character that isn't alphanumeric, hyphen, or underscore
pub(crate) fn sanitize_plugin_name(name: &str) -> String {
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

// ═══════════════════════════════════════════════════════════════════════════
// Runtime status: is an ENABLED plugin actually RUNNING?
// ═══════════════════════════════════════════════════════════════════════════
//
// `plugins.yml` records the operator's INTENT (enabled: true); it is NOT proof
// that the plugin process exists. The remote source may have been installed
// after boot, the subprocess may have died, or the MCP handshake may have
// failed. The lifecycle handlers (enable/restart) and BOTH read endpoints
// (GET /api/plugins and GET /api/plugins/{type}/{source}/{name}) go through the
// helpers below, so the API can never report a broken plugin as "enabled"
// (2026-09-18: the production memory plugin was listed as enabled while its
// detail said error, and enabling it returned a green success without starting
// anything).

/// True when the MCP registry holds at least one tool for `server_name`.
///
/// The registry is the runtime source of truth for tool plugins: tools only
/// appear after a successful spawn + JSON-RPC handshake.
pub(crate) async fn tool_server_running(state: &Arc<AppState>, server_name: &str) -> bool {
    let registry = state.plugin_manager.snapshot_registry().await;
    let all_tools = registry.all();
    all_tools
        .iter()
        .any(|t| t.server_name.as_deref() == Some(server_name))
}

/// True when a platform plugin has a running client registered.
pub(crate) async fn platform_plugin_running(state: &Arc<AppState>, name: &str) -> bool {
    if state.app_context.platforms.read().await.contains_key(name) {
        return true;
    }
    state
        .platform_restart_signals
        .lock()
        .await
        .contains_key(name)
}

/// A TRUTHFUL explanation for an enabled tool plugin that registered no tools.
///
/// The previous message ("binary may not have compiled successfully") was a
/// hardcoded guess that was also shown for Python/JS script plugins whose
/// source was simply not installed yet. Check what is actually on disk.
pub(crate) async fn mcp_start_failure_reason(data_dir: &str, name: &str) -> String {
    let dir = data_dir.to_string();
    let plugin = name.to_string();
    let has_config = tokio::task::spawn_blocking(move || {
        crate::mcp::external::config::server_config_exists(&dir, &plugin)
    })
    .await
    .unwrap_or(false);

    if has_config {
        format!(
            "the MCP server config for '{}' was found but the process did not initialize (check the omniagent log for the MCP server's own error output)",
            name
        )
    } else {
        format!(
            "no startable MCP server config found for '{}': its source is not installed at the configured path (a remote plugin must be downloaded/installed first), or it provides no mcp-config.json/entrypoint",
            name
        )
    }
}

/// Refresh the live status of tool plugins: fill `tool_names` from the MCP
/// registry and turn "enabled but nothing running" into a real error carrying a
/// truthful reason. Shared by the list AND the detail endpoint so both always
/// agree on a plugin's status.
pub(crate) async fn apply_tool_runtime_status_all(
    state: &Arc<AppState>,
    details: &mut [crate::plugins_yaml::PluginDetail],
) {
    // Snapshot the registry once (one cheap clone) and index tools per server.
    let registry = state.plugin_manager.snapshot_registry().await;
    let all_tools = registry.all();
    let mut server_tools: std::collections::HashMap<String, Vec<String>> =
        std::collections::HashMap::new();
    for tool in all_tools.iter() {
        if let Some(ref server) = tool.server_name {
            server_tools
                .entry(server.clone())
                .or_default()
                .push(tool.name.clone());
        }
    }

    for detail in details.iter_mut() {
        if detail.plugin_type != "tool" {
            continue;
        }
        if let Some(names) = server_tools.get(&detail.name) {
            let mut names = names.clone();
            names.sort();
            names.dedup();
            detail.tool_names = names;
            continue;
        }
        if detail.status == "enabled" {
            detail.status = "error".to_string();
            detail.tool_names.clear();
            detail.status_message = format!(
                "MCP server failed to start: {}",
                mcp_start_failure_reason(&state.data_dir, &detail.name).await
            );
        }
    }

    // Platform plugins SELF-REPORT their runtime status (the `plugin_status`
    // notification): e.g. inbound DEGRADED after a failed startup auth while the
    // background self-heal keeps retrying, or `ok` once inbound is enabled.
    // Surface it so `GET /api/plugins` tells the truth about a degraded
    // capability instead of showing a plain "enabled".
    for detail in details.iter_mut() {
        if detail.plugin_type != "platform" || detail.status != "enabled" {
            continue;
        }
        let Some(runtime) = crate::platform::external::platform_runtime_status(&detail.name) else {
            continue;
        };
        let status = runtime.get("status").and_then(|v| v.as_str()).unwrap_or("");
        if status.is_empty() || status == "ok" {
            continue;
        }
        let message = runtime
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let updated_at = runtime
            .get("updated_at")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        detail.status = "error".to_string();
        detail.status_message = format!("{} (reported '{}' at {})", message, status, updated_at);
    }
}

/// Single-plugin form of [`apply_tool_runtime_status_all`].
pub(crate) async fn apply_tool_runtime_status(
    state: &Arc<AppState>,
    detail: &mut crate::plugins_yaml::PluginDetail,
) {
    apply_tool_runtime_status_all(state, std::slice::from_mut(detail)).await;
}

/// Verify that a plugin is ACTUALLY running. `Err` carries a real reason.
pub(crate) async fn verify_plugin_started(
    state: &Arc<AppState>,
    yaml_type: &crate::plugins_yaml::PluginYamlType,
    name: &str,
) -> Result<(), String> {
    match yaml_type {
        crate::plugins_yaml::PluginYamlType::Tool => {
            if tool_server_running(state, name).await {
                Ok(())
            } else {
                Err(format!(
                    "MCP server '{}' is not running: {}",
                    name,
                    mcp_start_failure_reason(&state.data_dir, name).await
                ))
            }
        }
        crate::plugins_yaml::PluginYamlType::Platform => {
            if platform_plugin_running(state, name).await {
                Ok(())
            } else {
                Err(format!(
                    "platform plugin '{}' is enabled but no running client is registered",
                    name
                ))
            }
        }
        // Providers are started by the reload sweep below, whose per-plugin
        // error list is the verification.
        crate::plugins_yaml::PluginYamlType::Provider => Ok(()),
    }
}

/// Really START a plugin and return a real error string when it did not start.
pub(crate) async fn start_plugin_now(
    state: &Arc<AppState>,
    yaml_type: &crate::plugins_yaml::PluginYamlType,
    name: &str,
) -> Result<(), String> {
    match yaml_type {
        crate::plugins_yaml::PluginYamlType::Tool => {
            restart_tool_plugin(state, name).await.map(|_| ())
        }
        crate::plugins_yaml::PluginYamlType::Platform => {
            let what = format!("platform plugin '{}' start", name);
            crate::plugin::lifecycle::step_timeout(&what, start_platform_plugin(state, name))
                .await
                .map(|_| ())
        }
        crate::plugins_yaml::PluginYamlType::Provider => {
            crate::llm::refresh_provider_metadata();
            match super::plugins_env::reload_plugins(state.clone()).await {
                Ok((_started, _stopped, errors)) => {
                    let prefix = format!("{} ", name);
                    let mine: Vec<String> = errors
                        .iter()
                        .filter(|e| e.starts_with(&prefix))
                        .cloned()
                        .collect();
                    if mine.is_empty() {
                        Ok(())
                    } else {
                        Err(format!(
                            "provider '{}' failed to start: {}",
                            name,
                            mine.join("; ")
                        ))
                    }
                }
                Err(e) => Err(format!("provider '{}' reload failed: {}", name, e)),
            }
        }
    }
}

/// Ensure `name` is running: verify first (a healthy plugin is never needlessly
/// restarted, so a burst of duplicate enables still performs exactly one start)
/// and start it for real when it is not running. Providers always take the
/// start path because the reload sweep is the only thing that spawns them and
/// that sweep is itself idempotent.
pub(crate) async fn ensure_plugin_running(
    state: &Arc<AppState>,
    yaml_type: &crate::plugins_yaml::PluginYamlType,
    name: &str,
) -> Result<(), String> {
    if *yaml_type != crate::plugins_yaml::PluginYamlType::Provider
        && verify_plugin_started(state, yaml_type, name).await.is_ok()
    {
        return Ok(());
    }
    start_plugin_now(state, yaml_type, name).await?;
    verify_plugin_started(state, yaml_type, name).await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression (task_omnidev_memory_plugin_remote_python_fix_the, 2026-09-18):
    /// the reason reported for an enabled tool plugin that registered no tools
    /// must be TRUTHFUL. The old hardcoded guess ("binary may not have compiled
    /// successfully") was wrong for Python/JS script plugins whose remote source
    /// was simply not installed yet, and it sent operators down the wrong path
    /// in production.
    #[tokio::test]
    async fn missing_mcp_config_reason_is_truthful_and_never_a_compile_guess() {
        let dir = tempfile::tempdir().expect("tempdir");
        let data_dir = dir.path().to_string_lossy().to_string();

        let reason = mcp_start_failure_reason(&data_dir, "tester-missing-server").await;

        assert!(
            reason.contains("no startable MCP server config found for 'tester-missing-server'"),
            "must say the source/config is not installed, got: {reason}"
        );
        assert!(
            reason.contains("must be downloaded/installed first"),
            "must point at the real remote-plugin remedy, got: {reason}"
        );
        assert!(
            !reason.contains("binary may not have compiled"),
            "the old compilation guess must be gone, got: {reason}"
        );
    }
}
