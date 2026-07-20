//! Plugin setup handler.
//!
//! Extracted from `plugins.rs` for separation of concerns.

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use std::sync::Arc;

use crate::db::{channels, types::CreateChannelParams};
use crate::err_str;
use crate::plugin;
use crate::plugins_yaml;
use crate::server::AppState;

use super::plugins_reload::*;

pub(crate) async fn setup_plugin_handler(
    Path((_p_type, _source, name)): Path<(String, String, String)>,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    // 1. Get plugin detail from disk
    let data_dir = state.data_dir.clone();
    let data_dir_for_blocking = data_dir.clone();
    let name_clone = name.clone();
    let detail = match tokio::task::spawn_blocking(move || {
        plugins_yaml::get_plugin(&data_dir_for_blocking, &name_clone)
    })
    .await
    .unwrap_or_else(|e| Err(err_str!("Task join error: {}", e)))
    {
        Ok(Some(d)) => d,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({
                    "success": false,
                    "error": format!("Plugin '{}' not found", name)
                })),
            )
                .into_response();
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "success": false,
                    "error": format!("Failed to look up plugin '{}': {}", name, e)
                })),
            )
                .into_response();
        }
    };

    // 2. Check that this platform supports setup
    let manifest: plugin::PluginManifest = match serde_json::from_value(detail.manifest.clone()) {
        Ok(m) => m,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "success": false,
                    "error": format!("Failed to parse manifest for '{}': {}", name, e)
                })),
            )
                .into_response();
        }
    };

    let has_setup = manifest
        .capabilities
        .as_ref()
        .map(|c| c.setup)
        .unwrap_or(false);

    if !has_setup {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "success": false,
                "error": format!("Plugin '{}' does not support setup", name)
            })),
        )
            .into_response();
    }

    // 3. Find the plugin entrypoint
    let entrypoint = match &manifest.entrypoint {
        Some(ep) => ep,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "success": false,
                    "error": format!("Plugin '{}' has no entrypoint defined", name)
                })),
            )
                .into_response();
        }
    };

    let cmd = std::path::Path::new(&entrypoint.command);
    let binary_path = if cmd.is_absolute() {
        cmd.to_path_buf()
    } else {
        // Try relative to plugin directory: scan possible locations
        let plugin_dirs = [
            format!("{}/plugins/platforms/{}", data_dir, name),
            format!("{}/plugins/tools/{}", data_dir, name),
            format!("{}/plugins/providers/{}", data_dir, name),
            format!("/app/plugins/platforms/{}", name),
            format!("/app/plugins/tools/{}", name),
            format!("/app/plugins/providers/{}", name),
        ];
        let mut found = None;
        for dir in &plugin_dirs {
            let candidate = std::path::Path::new(dir).join(&entrypoint.command);
            if candidate.exists() {
                found = Some(candidate);
                break;
            }
        }
        match found {
            Some(p) => p,
            None => {
                // Try get_bin_path() for builtin plugins
                if let Some(bin_path) =
                    crate::mcp::external::config::get_bin_path(&entrypoint.command)
                {
                    if std::path::Path::new(&bin_path).exists() {
                        return (
                            StatusCode::OK,
                            Json(serde_json::json!({
                                "success": true,
                                "data": { "binary": bin_path }
                            })),
                        )
                            .into_response();
                    }
                }
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({
                        "success": false,
                        "error": format!("Plugin binary not found for '{}': {}", name, entrypoint.command)
                    })),
                )
                    .into_response();
            }
        }
    };

    // 4-8. Same setup logic as before (unchanged)
    let mut setup_env: std::collections::HashMap<String, String> = std::collections::HashMap::new();

    // Copy the resolved_env from the plugin detail
    for (k, v) in &detail.resolved_env {
        setup_env.insert(k.clone(), v.clone());
    }

    // Also set env vars from the env block in the manifest
    if let Ok(manifest) = serde_json::from_value::<plugin::PluginManifest>(detail.manifest.clone())
    {
        for (env_key, env_val) in &manifest.env {
            let resolved = if let Some(var_name) =
                env_val.strip_prefix("${").and_then(|s| s.strip_suffix('}'))
            {
                std::env::var(var_name).unwrap_or_default()
            } else if let Some(var_name) = env_val.strip_prefix("$env:") {
                std::env::var(var_name).unwrap_or_default()
            } else {
                env_val.clone()
            };
            setup_env.insert(env_key.clone(), resolved);
        }
    }

    // Inject ALL config values into setup_env so they're forwarded to the
    // plugin binary via the configure message. This avoids maintaining a
    // hardcoded list of what each plugin needs : the plugin knows its own
    // config schema. $secret: and $env: references are resolved below.
    let config = &detail.config;
    if let Some(config_map) = config.as_object() {
        for (key, value) in config_map {
            if !setup_env.contains_key(key) {
                if let Some(raw) = value.as_str().filter(|s| !s.is_empty()) {
                    setup_env.insert(key.clone(), raw.to_string());
                }
            }
        }
    }

    // Resolve $secret: references in setup_env
    crate::plugins_yaml::resolve_config_refs(&mut setup_env, &state.pool).await;

    // Build the setup params
    let setup_val = |key: &str| -> String {
        if let Some(v) = setup_env.get(key) {
            if !v.is_empty() {
                return v.clone();
            }
        }
        if let Some(raw) = config
            .get(key)
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
        {
            if raw.starts_with("$env:") {
                std::env::var(raw.strip_prefix("$env:").unwrap_or(raw)).unwrap_or_default()
            } else if raw.starts_with("$secret:") {
                // Already resolved above via setup_env + resolve_config_refs.
                // This branch is a fallback : see the injection block above.
                String::new()
            } else {
                raw.to_string()
            }
        } else {
            String::new()
        }
    };
    let setup_params = serde_json::json!({
        "setup_team": setup_val("setup_team"),
        "setup_channel": setup_val("setup_channel"),
        "bot_user": setup_val("bot_user"),
        "admin_user": setup_val("admin_user"),
        "admin_password": setup_val("admin_password"),
        "test_user": setup_val("test_user"),
        "test_password": setup_val("test_password"),
        "bot_password": setup_val("bot_password"),
    });

    let request_body = serde_json::json!({
        "method": "setup",
        "id": 1,
        "params": setup_params,
    });

    let request_str = serde_json::to_string(&request_body).unwrap_or_else(|_| "{}".to_string());

    tracing::info!(
        "Spawning plugin '{}' for setup: {}",
        name,
        binary_path.display()
    );

    let mut child = match std::process::Command::new(&binary_path)
        .arg("setup")
        .args(&entrypoint.args)
        .env_clear()
        .env("RUST_LOG", "info")
        .env("OMNI_DIR", &state.data_dir)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "success": false,
                    "error": format!("Failed to spawn plugin '{}' for setup: {}", name, e)
                })),
            )
                .into_response();
        }
    };

    let mut stdin = match child.stdin.take() {
        Some(s) => s,
        None => {
            if let Err(ke) = child.kill() {
                tracing::warn!("[plugins] Failed to kill child after stdin failure: {:?}", ke);
            }
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "success": false,
                    "error": format!("Failed to capture stdin for plugin '{}'", name)
                })),
            )
                .into_response();
        }
    };
    let mut stdout = match child.stdout.take() {
        Some(s) => s,
        None => {
            if let Err(ke) = child.kill() {
                tracing::warn!("[plugins] Failed to kill child after stdout failure: {:?}", ke);
            }
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "success": false,
                    "error": format!("Failed to capture stdout for plugin '{}'", name)
                })),
            )
                .into_response();
        }
    };
    let mut reader = std::io::BufReader::new(&mut stdout);

    // Send initialize request
    let init_req = serde_json::json!({"method": "initialize", "id": 1, "params": {}});
    {
        use std::io::Write;
        if let Err(e) = writeln!(
            stdin,
            "{}",
            serde_json::to_string(&init_req).unwrap_or_default()
        ) {
            if let Err(ke) = child.kill() {
                tracing::warn!("[plugins] Failed to kill child after init send failure: {:?}", ke);
            }
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "success": false,
                    "error": format!("Failed to send initialize request to '{}': {}", name, e)
                })),
            )
                .into_response();
        }
    }
    // Read initialize response
    {
        use std::io::BufRead;
        let mut line = String::new();
        if let Err(e) = reader.read_line(&mut line) {
            if let Err(ke) = child.kill() {
                tracing::warn!("[plugins] Failed to kill child after init read failure: {:?}", ke);
            }
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "success": false,
                    "error": format!("Failed to read initialize response from '{}': {}", name, e)
                })),
            )
                .into_response();
        }
        tracing::debug!("Plugin '{}' initialize response: {}", name, line.trim());
    }

    // Send configure request with config values
    let configure_req = serde_json::json!({"method": "configure", "id": 2, "params": &setup_env});
    {
        use std::io::Write;
        if let Err(e) = writeln!(
            stdin,
            "{}",
            serde_json::to_string(&configure_req).unwrap_or_default()
        ) {
            if let Err(ke) = child.kill() {
                tracing::warn!("[plugins] Failed to kill child after configure send failure: {:?}", ke);
            }
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "success": false,
                    "error": format!("Failed to send configure request to '{}': {}", name, e)
                })),
            )
                .into_response();
        }
    }
    // Read configure response
    {
        use std::io::BufRead;
        let mut line = String::new();
        if let Err(e) = reader.read_line(&mut line) {
            if let Err(ke) = child.kill() {
                tracing::warn!("[plugins] Failed to kill child after configure read failure: {:?}", ke);
            }
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "success": false,
                    "error": format!("Failed to read configure response from '{}': {}", name, e)
                })),
            )
                .into_response();
        }
        tracing::debug!("Plugin '{}' configure response: {}", name, line.trim());
    }

    // Write setup request to stdin
    {
        use std::io::Write;
        if let Err(e) = writeln!(stdin, "{}", request_str) {
            if let Err(ke) = child.kill() {
                tracing::warn!("[plugins] Failed to kill child after setup send failure: {:?}", ke);
            }
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "success": false,
                    "error": format!("Failed to send setup request to '{}': {}", name, e)
                })),
            )
                .into_response();
        }
    }
    // Close stdin (signals EOF to the plugin so it knows to exit after setup)
    drop(stdin);

    // Read all stdout output with timeout (120 seconds)
    let start = std::time::Instant::now();
    let max_wait = std::time::Duration::from_secs(120);

    let mut stdout_output = String::new();
    loop {
        if start.elapsed() >= max_wait {
            if let Err(ke) = child.kill() {
                tracing::warn!("[plugins] Failed to kill child after setup timeout: {:?}", ke);
            }
            return (
                StatusCode::REQUEST_TIMEOUT,
                Json(serde_json::json!({
                    "success": false,
                    "error": format!("Setup for '{}' timed out after 120 seconds", name)
                })),
            )
                .into_response();
        }

        match child.try_wait() {
            Ok(Some(status)) => {
                {
                    use std::io::Read;
                    if let Err(e) = reader.read_to_string(&mut stdout_output) {
                        tracing::warn!("[plugins] Failed to read stdout: {:?}", e);
                    }
                }
                let stderr_output = child
                    .stderr
                    .take()
                    .map(|mut s| {
                        let mut buf = String::new();
                        use std::io::Read;
                        if let Err(e) = s.read_to_string(&mut buf) {
                            tracing::warn!("[plugins] Failed to read stderr: {:?}", e);
                        }
                        buf
                    })
                    .unwrap_or_default();

                if !status.success() {
                    let err_detail = if stderr_output.is_empty() {
                        stdout_output.clone()
                    } else {
                        stderr_output.clone()
                    };
                    let truncated = if err_detail.len() > 500 {
                        format!("{}...", &err_detail[..500])
                    } else {
                        err_detail
                    };

                    tracing::error!(
                        "Setup for '{}' failed (exit: {}): {}",
                        name,
                        status,
                        truncated
                    );

                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(serde_json::json!({
                            "success": false,
                            "error": format!("Setup failed: {}", truncated)
                        })),
                    )
                        .into_response();
                }

                break;
            }
            Ok(None) => {
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                continue;
            }
            Err(e) => {
                if let Err(ke) = child.kill() {
                    tracing::warn!("[plugins] Failed to kill child after wait error: {:?}", ke);
                }
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({
                        "success": false,
                        "error": format!("Error waiting for setup process '{}': {}", name, e)
                    })),
                )
                    .into_response();
            }
        }
    }

    // Parse response
    let first_line = stdout_output.lines().next().unwrap_or("");

    match serde_json::from_str::<serde_json::Value>(first_line) {
        Ok(val) => {
            if let Some(error) = val.get("error") {
                let msg = error
                    .get("message")
                    .and_then(|m| m.as_str())
                    .unwrap_or("Setup failed with unknown error");

                return (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({
                        "success": false,
                        "error": msg.to_string()
                    })),
                )
                    .into_response();
            }

            let result = val.get("result").cloned().unwrap_or(val);

            tracing::info!("Setup completed for plugin '{}'", name);

            // Create omniagent channels for any channel_id returned by setup
            if let Some(channel_id) = result
                .get("channel_id")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
            {
                let channel_name = result
                    .get("channel_name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("setup");
                let omni_channel_name = format!("{}-{}", name, channel_name);
                match channels::create_channel(
                    &state.pool,
                    CreateChannelParams {
                        name: omni_channel_name.clone(),
                        platform: name.clone(),
                        external_id: channel_id.to_string(),
                        resource_identifier: channel_id.to_string(),
                        cause: "setup".to_string(),
                    },
                )
                .await
                {
                    Ok(ch) => tracing::info!(
                        "Setup: created channel '{}' (id={}) for plugin '{}' channel '{}'",
                        ch.name,
                        ch.id,
                        name,
                        channel_name
                    ),
                    Err(e) => tracing::warn!(
                        "Setup: failed to create channel for plugin '{}' channel '{}': {:?}",
                        name,
                        channel_name,
                        e
                    ),
                }
            }

            // Register file reader for any bot_token returned by setup
            if let Some(bot_token) = result
                .get("bot_token")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
            {
                let reader = crate::platform::external::HttpBearerFileReader::new(bot_token.to_string());
                state
                    .app_context
                    .platform_file_readers
                    .write()
                    .await
                    .insert(name.clone(), Arc::new(reader));
                tracing::info!(
                    "Registered file reader for plugin '{}' (from setup bot_token)",
                    name
                );

                // Persist bot_token to .env so the subprocess can authenticate
                // after restart. The config uses $env:MATTERMOST_ACCESS_TOKEN.
                let env_path = &state.env_path;
                let env_var_name = format!("{}_ACCESS_TOKEN", name.to_uppercase());
                let existing = std::fs::read_to_string(env_path).unwrap_or_default();
                let updated = {
                    let mut result = String::new();
                    let mut replaced = false;
                    for line in existing.lines() {
                        if line.starts_with(&format!("{}=", env_var_name)) {
                            result.push_str(&format!("{}={}\n", env_var_name, bot_token));
                            replaced = true;
                        } else {
                            result.push_str(line);
                            result.push('\n');
                        }
                    }
                    if !replaced {
                        result.push_str(&format!("{}={}\n", env_var_name, bot_token));
                    }
                    result
                };
                if let Err(e) = std::fs::write(env_path, &updated) {
                    tracing::error!("[plugins] Failed to write bot_token to .env for '{}': {:?}", name, e);
                }
                std::env::set_var(&env_var_name, bot_token);

                reload_platform_plugin(&state, &name).await;
            }

            (
                StatusCode::OK,
                Json(serde_json::json!({
                    "success": true,
                    "data": result
                })),
            )
                .into_response()
        }
        Err(e) => {
            let msg = if stdout_output.trim().is_empty() {
                format!("Setup completed but returned no data for '{}'", name)
            } else {
                format!("Setup for '{}' returned unexpected output: {}", name, e)
            };

            tracing::warn!("{}", msg);

            (
                StatusCode::OK,
                Json(serde_json::json!({
                    "success": true,
                    "data": {
                        "raw_output": stdout_output.trim()
                    }
                })),
            )
                .into_response()
        }
    }
}

