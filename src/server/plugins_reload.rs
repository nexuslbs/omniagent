//! Plugin hot-reload and environment refresh utilities.
//!
//! Extracted from `plugins.rs` for separation of concerns.
//! Contains functions for refreshing .env files, reloading platform/tool
//! plugins after config changes, and name sanitization.

use std::sync::atomic::Ordering;
use std::sync::Arc;

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
pub(crate) async fn reload_platform_plugin(state: &Arc<AppState>, name: &str) {
    tracing::info!("Reloading platform plugin '{}' after config update", name);

    let refreshed = refresh_env_from_file(&state.env_path);
    if refreshed > 0 {
        tracing::info!(
            "Refreshed {} env var(s) from .env for platform plugin reload",
            refreshed
        );
    }

    let signal = {
        let signals = state.platform_restart_signals.lock().await;
        signals.get(name).cloned()
    };

    if let Some((restart_count, restart_notify)) = signal {
        restart_count.fetch_add(1, Ordering::SeqCst);
        restart_notify.notify_one();
        tracing::info!(
            "Set restart counter for platform plugin '{}': subprocess will be respawned (count: {})",
            name,
            restart_count.load(Ordering::SeqCst)
        );
    } else {
        tracing::warn!(
            "Platform plugin '{}' is not currently registered: restart flag not found. \
             The new config will take effect on next omniagent start.",
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
