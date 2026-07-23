//! Plugin compilation and category detection helpers.
//!
//! Extracted from `plugins.rs` for separation of concerns.

use tracing::{error, info, warn};

use crate::plugins_yaml;

// ---------------------------------------------------------------------------
// Plugin type detection helpers
// ---------------------------------------------------------------------------

/// The three plugin location types.
#[derive(Debug, Clone)]
pub(crate) enum PluginCategory {
    /// builtin: true in YAML, source at /app/plugins/
    Builtin,
    /// Workspace bundled (has plugin.json in workspace_dir/plugins/)
    OmniStack,
    /// Has `remote` field in YAML, source at data_dir/plugins/<type>/.remote/
    Remote,
}

/// Detect a plugin's category from its YAML entry and disk state.
pub(crate) fn detect_plugin_category(
    data_dir: &str,
    yaml_type: &plugins_yaml::PluginYamlType,
    name: &str,
) -> PluginCategory {
    // Check YAML entry first: source field is authoritative
    if let Ok(Some(entry)) = plugins_yaml::get_entry(data_dir, yaml_type, name) {
        match entry.source.as_str() {
            "built-in" => return PluginCategory::Builtin,
            "remote" => return PluginCategory::Remote,
            _ => return PluginCategory::OmniStack,
        }
    }

    // No YAML entry: check disk for builtin source directory
    if plugins_yaml::is_plugin_builtin(data_dir, name, yaml_type) {
        return PluginCategory::Builtin;
    }

    // Check if it's remote by looking for .remote/ directory
    let type_dir = yaml_type.type_dir_name();
    let remote_dir = format!("{}/plugins/{}/.remote/{}", data_dir, type_dir, name);
    if std::path::Path::new(&remote_dir).exists() {
        return PluginCategory::Remote;
    }

    PluginCategory::OmniStack
}

/// Get the source directory for a plugin based on its category.
pub(crate) fn get_plugin_dir_for_category(
    data_dir: &str,
    category: &PluginCategory,
    yaml_type: &plugins_yaml::PluginYamlType,
    name: &str,
) -> String {
    match category {
        PluginCategory::Builtin => {
            format!("/app/plugins/{}/{}", yaml_type.type_dir_name(), name)
        }
        PluginCategory::OmniStack => {
            format!(
                "{}/plugins/{}/{}",
                data_dir,
                yaml_type.type_dir_name(),
                name
            )
        }
        PluginCategory::Remote => {
            let base = format!(
                "{}/plugins/{}/.remote/{}",
                data_dir,
                yaml_type.type_dir_name(),
                name
            );
            tracing::debug!(
                "[compile] Remote plugin_dir base: {}, yaml_type: {:?}, name: {}",
                base,
                yaml_type,
                name
            );
            // Remote plugins may have a sub-path inside the cloned repo
            // (e.g. path: "tools/test-rust-tool" in remote.yml). Append it
            // so compile_rust_crate finds the actual Cargo.toml.
            if let Some(remote) = crate::plugins_yaml::get_remote_plugin(data_dir, yaml_type, name)
            {
                tracing::debug!(
                    "[compile] Found remote plugin: path={:?}, url={}",
                    remote.path,
                    remote.url
                );
                if let Some(ref sub_path) = remote.path {
                    if !sub_path.is_empty() {
                        let resolved = format!("{}/{}", base, sub_path);
                        tracing::debug!("[compile] Resolved plugin_dir: {}", resolved);
                        return resolved;
                    }
                }
            } else {
                tracing::warn!("[compile] get_remote_plugin returned None for '{}'", name);
            }
            base
        }
    }
}

/// Detect category but also cross-reference the type directory.
/// Returns None if no YAML entry exists for any plugin type.
pub(crate) fn detect_plugin_category_cross_type(
    data_dir: &str,
    name: &str,
) -> Option<(plugins_yaml::PluginYamlType, PluginCategory)> {
    // Try tool type first
    let tool_type = plugins_yaml::PluginYamlType::Tool;
    if plugins_yaml::get_entry(data_dir, &tool_type, name)
        .ok()
        .flatten()
        .is_some()
    {
        return Some((
            tool_type.clone(),
            detect_plugin_category(data_dir, &tool_type, name),
        ));
    }

    // Try provider type
    let provider_type = plugins_yaml::PluginYamlType::Provider;
    if plugins_yaml::get_entry(data_dir, &provider_type, name)
        .ok()
        .flatten()
        .is_some()
    {
        return Some((
            provider_type.clone(),
            detect_plugin_category(data_dir, &provider_type, name),
        ));
    }

    // Try platform type
    let platform_type = plugins_yaml::PluginYamlType::Platform;
    if plugins_yaml::get_entry(data_dir, &platform_type, name)
        .ok()
        .flatten()
        .is_some()
    {
        return Some((
            platform_type.clone(),
            detect_plugin_category(data_dir, &platform_type, name),
        ));
    }

    None
}

/// Read the package name from a Cargo.toml at the given path.
pub(crate) fn read_cargo_package_name(cargo_toml_path: &str) -> Option<String> {
    let content = std::fs::read_to_string(cargo_toml_path).ok()?;
    // Parse package.name from Cargo.toml using string scanning
    // (avoiding direct toml crate dependency in this module scope)
    for line in content.lines() {
        let trimmed = line.trim();
        if let Some(name_val) = trimmed.strip_prefix("name = ") {
            let stripped = name_val.trim().trim_matches('"').to_string();
            if !stripped.is_empty() {
                return Some(stripped);
            }
        }
    }
    None
}

/// Compile a Rust crate at the given path. Returns true if compilation succeeded.
///
/// Retries once on failure for remote plugins, since transient network timeouts
/// (crates.io index update, dependency download) are the most common cause of
/// first-attempt failures.
pub(crate) async fn compile_rust_crate(
    plugin_dir: &str,
    name: &str,
    source: &str,
) -> Result<bool, String> {
    info!(
        "[plugin/compile] Compiling plugin '{}' from {} (source: {})",
        name, plugin_dir, source
    );

    // Locate the Cargo.toml — if none exists, skip compilation
    // (non-Rust plugins like Python don't have Cargo.toml)
    let cargo_path = format!("{}/Cargo.toml", plugin_dir);
    if !std::path::Path::new(&cargo_path).exists() {
        info!(
            "[plugin/compile] No Cargo.toml at {}, skipping compilation for '{}'",
            cargo_path, name
        );
        return Ok(false);
    }

    // Determine the package name from Cargo.toml
    let pkg_name = read_cargo_package_name(&cargo_path)
        .ok_or_else(|| format!("Failed to read package name from {}", cargo_path))?;

    // Remote plugins get one retry for transient network failures (e.g. crates.io timeout)
    let max_attempts = if source == "remote" { 2 } else { 1 };

    let label = format!("{} (pkg: {})", name, pkg_name);
    for attempt in 1..=max_attempts {
        let output = tokio::process::Command::new("cargo")
            .args(["build", "--release", "--manifest-path", &cargo_path])
            .output()
            .await
            .map_err(|e| format!("Failed to run cargo build for '{}': {}", name, e))?;

        if output.status.success() {
            info!("[plugin/compile] Successfully compiled '{}'", label,);
            return Ok(true);
        }

        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);

        if attempt < max_attempts {
            warn!(
                "[plugin/compile] Attempt {}/{} failed for '{}' (will retry):\nstdout: {}\nstderr: {}",
                attempt, max_attempts, label, stdout, stderr
            );
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        } else {
            error!(
                "[plugin/compile] All {}/{} attempts failed for '{}':\nstdout: {}\nstderr: {}",
                attempt, max_attempts, label, stdout, stderr
            );
            return Err(format!(
                "Compilation failed for '{}'. Check logs for details.",
                name
            ));
        }
    }

    unreachable!()
}

/// Map a category string back to its source keyword for YAML.
pub(crate) fn category_to_source(category: &PluginCategory) -> &'static str {
    match category {
        PluginCategory::Builtin => "built-in",
        PluginCategory::OmniStack => "bundled",
        PluginCategory::Remote => "remote",
    }
}

// ── Shared preamble result ──

/// Result of resolving a plugin for compilation/installation.
/// Used by the install/reinstall handlers.
pub(crate) struct ResolvedPlugin {
    pub yaml_type: plugins_yaml::PluginYamlType,
    pub category: PluginCategory,
    pub plugin_dir: String,
}

/// Resolve the plugin source directory, type, and category.
/// This is the shared preamble used by Install and Reinstall handlers.
/// Returns an HTTP response error tuple on failure.
pub(crate) async fn resolve_plugin_for_compile(
    data_dir: &str,
    _state_data_dir: &str,
    name: &str,
    handler_name: &str,
    _source: Option<&str>,
) -> Result<ResolvedPlugin, (axum::http::StatusCode, axum::Json<serde_json::Value>)> {
    use axum::Json;

    // 1. Detect the plugin type by checking each YAML type
    let yaml_type = if let Ok(Some(_entry)) =
        plugins_yaml::get_entry(data_dir, &plugins_yaml::PluginYamlType::Tool, name)
    {
        plugins_yaml::PluginYamlType::Tool
    } else if let Ok(Some(_)) =
        plugins_yaml::get_entry(data_dir, &plugins_yaml::PluginYamlType::Provider, name)
    {
        plugins_yaml::PluginYamlType::Provider
    } else if let Ok(Some(_)) =
        plugins_yaml::get_entry(data_dir, &plugins_yaml::PluginYamlType::Platform, name)
    {
        plugins_yaml::PluginYamlType::Platform
    } else {
        return Err((
            axum::http::StatusCode::NOT_FOUND,
            Json(serde_json::json!({
                "success": false,
                "error": format!("Plugin '{}' not found in any plugin type registry", name)
            })),
        ));
    };

    // 2. Get plugin source directory with Builtin fallback
    let category = detect_plugin_category(data_dir, &yaml_type, name);
    let plugin_dir = get_plugin_dir_for_category(data_dir, &category, &yaml_type, name);

    // 3. Verify the directory exists
    if !std::path::Path::new(&plugin_dir).exists() {
        return Err((
            axum::http::StatusCode::NOT_FOUND,
            Json(serde_json::json!({
                "success": false,
                "error": format!(
                    "{}: Plugin source directory not found for '{}' at: {}",
                    handler_name, name, plugin_dir
                )
            })),
        ));
    }

    Ok(ResolvedPlugin {
        yaml_type,
        category,
        plugin_dir,
    })
}
