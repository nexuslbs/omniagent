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
        // Platform is running — signal a restart
        restart_count.fetch_add(1, Ordering::SeqCst);
        restart_notify.notify_one();
        tracing::info!(
            "Set restart counter for platform plugin '{}': subprocess will be respawned (count: {})",
            name,
            restart_count.load(Ordering::SeqCst)
        );
    } else {
        // Platform is NOT running — start it dynamically
        tracing::info!(
            "Platform plugin '{}' is not currently running — starting dynamically",
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
    // stays plugin-agnostic — no knowledge of field names like access_token.
    // Just store the Arc<dyn Platform> in AppContext for the MCP tool to use.
    state
        .app_context
        .platforms
        .write()
        .await
        .insert(name.to_string(), client.clone() as Arc<dyn crate::platform::Platform>);

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
            "Platform plugin '{}' was not registered — already stopped or never started",
            name
        );
    }
}

/// Trigger a hot-reload of a tool (MCP) plugin after its config has been updated.
pub(crate) async fn reload_tool_plugin(state: &Arc<AppState>, name: &str) {
    tracing::info!("Reloading tool plugin '{}' after config update", name);

    let refreshed = refresh_env_from_file(&state.env_path);
    if refreshed > 0 {
        tracing::info!(
            "Refreshed {} env var(s) from .env for tool plugin reload",
            refreshed
        );
    }

    state.plugin_manager.remove_client(name);

    match state
        .plugin_manager
        .initialize_single_server(&state.data_dir, name)
        .await
    {
        Ok(tools) => {
            let count = tools.len();
            state.plugin_manager.remove_server_tools(name).await;
            state.plugin_manager.register_tools(tools).await;
            tracing::info!(
                "Hot-reloaded {} tool(s) from MCP server '{}' after config update (no restart needed)",
                count,
                name
            );
        }
        Err(e) => {
            tracing::warn!(
                "Hot-reload of MCP server '{}' after config update failed (config saved, will retry on next restart): {}",
                name,
                e
            );
        }
    }
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
