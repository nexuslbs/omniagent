//! Plugin compilation and category detection helpers.
//!
//! Extracted from `plugins.rs` for separation of concerns.

use tracing::{error, info};

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
            let workspace_dir = std::env::var("WORKSPACE_DIR")
                .unwrap_or_else(|_| "/opt/workspace/omni-stack".to_string());
            format!(
                "{}/plugins/{}/{}",
                workspace_dir,
                yaml_type.type_dir_name(),
                name
            )
        }
        PluginCategory::Remote => {
            format!(
                "{}/plugins/{}/.remote/{}",
                data_dir,
                yaml_type.type_dir_name(),
                name
            )
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
        return Some((tool_type.clone(), detect_plugin_category(data_dir, &tool_type, name)));
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
pub(crate) async fn compile_rust_crate(
    plugin_dir: &str,
    name: &str,
    source: &str,
) -> Result<bool, String> {
    info!(
        "[plugin/compile] Compiling plugin '{}' from {} (source: {})",
        name, plugin_dir, source
    );

    // Locate the Cargo.toml
    let cargo_path = format!("{}/Cargo.toml", plugin_dir);
    if !std::path::Path::new(&cargo_path).exists() {
        return Err(format!("No Cargo.toml found at {}", cargo_path));
    }

    // Determine the package name from Cargo.toml
    let pkg_name = read_cargo_package_name(&cargo_path)
        .ok_or_else(|| format!("Failed to read package name from {}", cargo_path))?;

    // Run cargo build
    let output = tokio::process::Command::new("cargo")
        .args(["build", "--release", "--manifest-path", &cargo_path])
        .output()
        .await
        .map_err(|e| format!("Failed to run cargo build for '{}': {}", name, e))?;

    if output.status.success() {
        info!(
            "[plugin/compile] Successfully compiled '{}' (pkg: {})",
            name, pkg_name
        );
        Ok(true)
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        error!(
            "[plugin/compile] Failed to compile '{}':\nstdout: {}\nstderr: {}",
            name, stdout, stderr
        );
        Err(format!(
            "Compilation failed for '{}'. Check logs for details.",
            name
        ))
    }
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
    source: Option<&str>,
) -> Result<ResolvedPlugin, (axum::http::StatusCode, axum::Json<serde_json::Value>)> {
    use axum::Json;

    // 1. Detect the plugin type by checking each YAML type
    let yaml_type = if let Ok(Some(entry)) = plugins_yaml::get_entry(
        data_dir,
        &plugins_yaml::PluginYamlType::Tool,
        name,
    ) {
        if source.map_or(true, |s| entry.source == s) {
            plugins_yaml::PluginYamlType::Tool
        } else {
            plugins_yaml::PluginYamlType::Tool
        }
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
