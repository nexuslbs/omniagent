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
) -> Result<String, String> {
    match category {
        PluginCategory::Builtin => {
            Ok(format!("/app/plugins/{}/{}", yaml_type.type_dir_name(), name))
        }
        PluginCategory::OmniStack => {
            Ok(format!(
                "{}/plugins/{}/{}",
                data_dir,
                yaml_type.type_dir_name(),
                name
            ))
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
            let remote = crate::plugins_yaml::get_remote_plugin(data_dir, yaml_type, name)
                .ok_or_else(|| format!(
                    "Remote plugin '{}' (type={:?}) has no entry in remote.yml. Expected entry under '{}' section. Re-register the plugin or update remote.yml manually.",
                    name,
                    yaml_type,
                    yaml_type.type_dir_name()
                ))?;
            tracing::debug!(
                "[compile] Found remote plugin: path={:?}, url={}",
                remote.path,
                remote.url
            );
            if let Some(ref sub_path) = remote.path {
                if !sub_path.is_empty() {
                    let resolved = format!("{}/{}", base, sub_path);
                    tracing::debug!("[compile] Resolved plugin_dir: {}", resolved);
                    return Ok(resolved);
                }
            }
            Ok(base)
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
/// When `force_rebuild` is true, removes the existing binary before building
/// to force cargo to actually recompile (not just return "up to date" from cache).
///
/// Retries once on failure for remote plugins, since transient network timeouts
/// (crates.io index update, dependency download) are the most common cause of
/// first-attempt failures.
pub(crate) async fn compile_rust_crate(
    plugin_dir: &str,
    name: &str,
    source: &str,
    force_rebuild: bool,
) -> Result<bool, String> {
    info!(
        "[plugin/compile] Compiling plugin '{}' from {} (source: {}, force_rebuild: {})",
        name, plugin_dir, source, force_rebuild
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

    // If force_rebuild, remove the existing binary so cargo actually recompiles
    // (cargo build --release would otherwise return "up to date" from cache).
    if force_rebuild {
        let binary_path = format!("{}/target/release/{}", plugin_dir, pkg_name);
        let bin_path = std::path::Path::new(&binary_path);
        if bin_path.exists() {
            info!(
                "[plugin/compile] Force rebuild: removing existing binary at {}",
                binary_path
            );
            if let Err(e) = std::fs::remove_file(bin_path) {
                warn!(
                    "[plugin/compile] Failed to remove existing binary for '{}': {}",
                    name, e
                );
            }
        }
    }

    // Remote plugins get one retry for transient network failures (e.g. crates.io timeout)
    let max_attempts = if source == "remote" { 2 } else { 1 };

    let label = format!("{} (pkg: {})", name, pkg_name);
    for attempt in 1..=max_attempts {
        let output = tokio::process::Command::new("cargo")
            .args(["build", "--release", "--manifest-path", &cargo_path])
            .env_remove("CARGO_TARGET_DIR")
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
/// Uses type and source from the URL path deterministically — no guessing, no fallbacks.
///
/// - `source = "remote"` → reads remote.yml for sub-path, dir is `.remote/{name}/{sub_path}`
/// - `source = "bundled"` → dir is `{data_dir}/plugins/{type_dir}/{name}`
/// - Other sources → BAD_REQUEST error
///
/// Returns an HTTP response error tuple on failure.
pub(crate) async fn resolve_plugin_for_compile(
    data_dir: &str,
    plugin_type: &str,
    source: &str,
    name: &str,
    handler_name: &str,
) -> Result<ResolvedPlugin, (axum::http::StatusCode, axum::Json<serde_json::Value>)> {
    use axum::{http::StatusCode, Json};

    // 1. Parse YAML type from the URL path type string
    let yaml_type = plugins_yaml::PluginYamlType::from_type_str(plugin_type);
    let type_dir = yaml_type.type_dir_name();

    // 2. Resolve category and directory based on source from URL path
    let (category, plugin_dir) = match source {
        "remote" => {
            // Remote: read remote.yml to get the sub-path (deterministic lookup)
            // Error if no remote.yml entry — the Add/install-git action must create it first.
            let remote = crate::plugins_yaml::get_remote_plugin(data_dir, &yaml_type, name)
                .ok_or_else(|| {
                    (
                        axum::http::StatusCode::NOT_FOUND,
                        axum::Json(serde_json::json!({
                            "success": false,
                            "error": format!(
                                "{}: Remote plugin '{}' (type={}) not found in remote.yml",
                                handler_name, name, plugin_type
                            )
                        })),
                    )
                })?;
            let base = format!("{}/plugins/{}/.remote/{}", data_dir, type_dir, name);
            let dir = if let Some(ref sub_path) = remote.path {
                if !sub_path.is_empty() {
                    format!("{}/{}", base, sub_path)
                } else {
                    base
                }
            } else {
                base
            };
            (PluginCategory::Remote, dir)
        }
        "bundled" => {
            let dir = format!("{}/plugins/{}/{}", data_dir, type_dir, name);
            (PluginCategory::OmniStack, dir)
        }
        _ => {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "success": false,
                    "error": format!(
                        "{}: Invalid source '{}': must be 'remote' or 'bundled'",
                        handler_name, source
                    )
                })),
            ));
        }
    };

    // 3. Verify the directory exists on disk
    if !std::path::Path::new(&plugin_dir).exists() {
        return Err((
            StatusCode::NOT_FOUND,
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
