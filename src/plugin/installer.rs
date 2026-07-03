//! Filesystem helpers for plugin installation, uninstallation, and discovery.
//!
//! Supports:
//! - Installing from URL (.tar.gz, .tgz, .zip)
//! - Uninstalling (removing plugin directory)
//! - Discovering plugins from disk

use crate::error::{Error, AppResult, ErrorContext};
use crate::err_msg;
use std::path::Path;

use crate::plugin::{load_manifest, PluginManifest, PluginType};

// ---------------------------------------------------------------------------
// Install from URL
// ---------------------------------------------------------------------------

/// Download a tarball/zip from a URL and extract it to `<data_dir>/plugins/installed/<name>/`.
///
/// Uses `reqwest::blocking::get` to download and shell commands (tar, unzip) to extract.
/// Returns the parsed PluginManifest from the extracted plugin.json.
pub fn install_from_url(url: &str, data_dir: &str) -> AppResult<PluginManifest> {
    let response = reqwest::blocking::get(url)
        .ctx(format!("Failed to download plugin from {}", url))?;

    if !response.status().is_success() {
        err_msg!(
            "Failed to download plugin from {}: HTTP {}",
            url,
            response.status()
        );
    }

    let bytes = response
        .bytes()
        .ctx(format!("Failed to read response body from {}", url))?;

    // Create a temp directory for extraction using a unique name under /tmp
    let temp_id = format!(
        "omniagent-plugin-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    );
    let temp_path = std::path::PathBuf::from("/tmp").join(&temp_id);
    std::fs::create_dir_all(&temp_path)
        .ctx(format!("Failed to create temp directory at {:?}", temp_path))?;

    // Cleanup on drop (if still present)
    let temp_path_clone = temp_path.clone();
    let cleanup = std::sync::Mutex::new(Some(temp_path_clone));

    let result = install_from_url_inner(url, &bytes, &temp_path, data_dir);

    // Clean up temp directory
    if let Ok(mut path_opt) = cleanup.lock() {
        if let Some(path) = path_opt.take() {
            let _ = std::fs::remove_dir_all(&path);
        }
    }

    result
}

fn install_from_url_inner(
    url: &str,
    bytes: &[u8],
    temp_path: &std::path::Path,
    data_dir: &str,
) -> AppResult<PluginManifest> {
    // Determine file extension to choose extraction method
    let url_lower = url.to_lowercase();
    let archive_path;

    if url_lower.ends_with(".tar.gz") || url_lower.ends_with(".tgz") {
        archive_path = temp_path.join("plugin.tar.gz");
        std::fs::write(&archive_path, &bytes)
            .ctx("Failed to write downloaded archive to temp dir")?;

        // Extract using tar
        let status = std::process::Command::new("tar")
            .arg("-xzf")
            .arg(&archive_path)
            .arg("-C")
            .arg(&temp_path)
            .status()
            .ctx("Failed to execute tar command")?;

        if !status.success() {
            err_msg!("tar extraction failed with status: {}", status);
        }
    } else if url_lower.ends_with(".zip") {
        archive_path = temp_path.join("plugin.zip");
        std::fs::write(&archive_path, &bytes)
            .ctx("Failed to write downloaded archive to temp dir")?;

        // Extract using unzip
        let status = std::process::Command::new("unzip")
            .arg("-o")
            .arg(&archive_path)
            .arg("-d")
            .arg(&temp_path)
            .status()
            .ctx("Failed to execute unzip command")?;

        if !status.success() {
            err_msg!("unzip extraction failed with status: {}", status);
        }
    } else {
        err_msg!(
            "Unsupported archive format in URL '{}'. Supported: .tar.gz, .tgz, .zip",
            url
        );
    }

    // Find plugin.json in the extracted directory
    let plugin_json = find_plugin_json(&temp_path)?;
    let manifest = load_manifest(&plugin_json)?;

    // Copy to the installed plugins directory
    let install_dir = format!("{}/plugins/installed/{}", data_dir, manifest.name);
    let install_path = Path::new(&install_dir);

    // Remove existing if present
    if install_path.exists() {
        std::fs::remove_dir_all(install_path).ctx(format!(
            "Failed to remove existing plugin directory: {}",
            install_dir
        ))?;
    }

    // Create parent directories
    let parent = install_path
        .parent()
        .ok_or_else(|| Error::Message(format!("Install path has no parent: {}", install_dir)))?;
    std::fs::create_dir_all(parent)
        .ctx(format!("Failed to create parent directories for: {}", install_dir))?;

    // Copy the extracted plugin directory to the install location
    let extracted_dir = Path::new(&plugin_json)
        .parent()
        .ok_or_else(|| Error::Message(format!("Plugin JSON path has no parent: {}", plugin_json)))?;
    let copy_result = copy_dir_recursive(extracted_dir, install_path);
    if let Err(e) = copy_result {
        // Clean up on failure
        let _ = std::fs::remove_dir_all(install_path);
        return Err(e).ctx(format!(
            "Failed to copy plugin to install directory: {}",
            install_dir
        ));
    }

    // Verify the installed manifest
    let installed_manifest_path = format!("{}/plugin.json", install_dir);
    let manifest = load_manifest(&installed_manifest_path).ctx(format!(
        "Failed to verify installed plugin manifest at: {}",
        installed_manifest_path
    ))?;

    tracing::info!(
        "Installed plugin '{}' version {} from {}",
        manifest.name,
        manifest.version,
        url
    );

    Ok(manifest)
}

/// Recursively copy a directory.
fn copy_dir_recursive(src: &Path, dst: &Path) -> AppResult<()> {
    if !src.is_dir() {
        err_msg!("Source is not a directory: {:?}", src);
    }
    if !dst.exists() {
        std::fs::create_dir_all(dst)
            .ctx(format!("Failed to create directory: {:?}", dst))?;
    }

    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let entry_type = entry.file_type()?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());

        if entry_type.is_dir() {
            copy_dir_recursive(&src_path, &dst_path)?;
        } else {
            std::fs::copy(&src_path, &dst_path)
                .ctx(format!("Failed to copy {:?} to {:?}", src_path, dst_path))?;
        }
    }
    Ok(())
}

/// Find the first plugin.json in a directory tree (searches top-level and one level deep).
fn find_plugin_json(dir: &Path) -> AppResult<String> {
    // Check if plugin.json exists at the top level
    let top_level = dir.join("plugin.json");
    if top_level.exists() {
        return Ok(top_level.to_string_lossy().to_string());
    }

    // Search one level deep
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let candidate = path.join("plugin.json");
                if candidate.exists() {
                    return Ok(candidate.to_string_lossy().to_string());
                }
            }
        }
    }

    err_msg!("No plugin.json found in extracted archive at: {:?}", dir);
}

// ---------------------------------------------------------------------------
// Install from Git
// ---------------------------------------------------------------------------

/// Clone a plugin from a git repository into `<workspace_dir>/plugins/<type_dir>/external/<name>/`,
/// then copy it to `<data_dir>/plugins/<type_dir>/<name>/` and return the manifest.
///
/// Steps:
/// 1. Shallow clone (`--depth 1`) the repo into the external dir
/// 2. If a git_ref is specified, check it out after clone
/// 3. Find plugin.json in the cloned directory
/// 4. Copy to data_dir (removes existing if present)
/// 5. Return the parsed PluginManifest
pub fn install_from_git(
    url: &str,
    name: &str,
    git_ref: Option<&str>,
    remote_type: &str,
    workspace_dir: &str,
    data_dir: &str,
    repo_path: Option<&str>,
) -> AppResult<PluginManifest> {
    let external_dir = format!("{}/plugins/{}/external/{}", workspace_dir, remote_type, name);
    let external_path = std::path::Path::new(&external_dir);

    // Remove existing clone if present
    if external_path.exists() {
        std::fs::remove_dir_all(external_path)
            .ctx(format!("Failed to remove existing clone at {}", external_dir))?;
    }

    // Create parent directories
    if let Some(parent) = external_path.parent() {
        std::fs::create_dir_all(parent)
            .ctx(format!("Failed to create parent dirs for {}", external_dir))?;
    }

    tracing::info!(
        "Cloning git plugin '{}' from {} (ref: {:?}) to {}",
        name, url, git_ref, external_dir
    );

    // Shallow clone
    let mut cmd = std::process::Command::new("git");
    cmd.arg("clone").arg("--depth").arg("1").arg(url).arg(&external_dir);

    let status = cmd.status().ctx(format!(
        "Failed to execute git clone for '{}' from {}",
        name, url
    ))?;

    if !status.success() {
        err_msg!("git clone failed for '{}' from {} with status: {}", name, url, status);
    }

    // Checkout specific ref if specified
    if let Some(ref_str) = git_ref {
        if !ref_str.is_empty() {
            tracing::info!("Checking out ref '{}' for plugin '{}'", ref_str, name);
            let checkout_status = std::process::Command::new("git")
                .arg("-C")
                .arg(&external_dir)
                .arg("checkout")
                .arg(ref_str)
                .status()
                .ctx(format!("Failed to git checkout {} for '{}'", ref_str, name))?;

            if !checkout_status.success() {
                err_msg!("git checkout '{}' failed for '{}' with status: {}", ref_str, name, checkout_status);
            }
        }
    }

    // Find plugin.json in the cloned directory
    let search_dir = match repo_path {
        Some(p) if !p.is_empty() => external_path.join(p),
        _ => external_path.clone(),
    };
    let plugin_json_path = find_plugin_json(&search_dir)?;
    let manifest = load_manifest(&plugin_json_path)?;

    // Copy to data_dir
    let install_dir = format!("{}/plugins/{}/{}", data_dir, remote_type, name);
    let install_path = std::path::Path::new(&install_dir);

    if install_path.exists() {
        std::fs::remove_dir_all(install_path).ctx(format!(
            "Failed to remove existing plugin directory: {}",
            install_dir
        ))?;
    }

    let parent = install_path
        .parent()
        .ok_or_else(|| Error::Message(format!("Install path has no parent: {}", install_dir)))?;
    std::fs::create_dir_all(parent)
        .ctx(format!("Failed to create parent directories for: {}", install_dir))?;

    let extracted_dir = std::path::Path::new(&plugin_json_path)
        .parent()
        .ok_or_else(|| Error::Message(format!("Plugin JSON path has no parent: {}", plugin_json_path)))?;
    copy_dir_recursive(extracted_dir, install_path)?;

    // Verify the installed manifest
    let installed_manifest_path = format!("{}/plugin.json", install_dir);
    let manifest = load_manifest(&installed_manifest_path).ctx(format!(
        "Failed to verify installed plugin manifest at: {}",
        installed_manifest_path
    ))?;

    tracing::info!(
        "Installed git plugin '{}' version {} from {} (ref: {:?})",
        manifest.name,
        manifest.version,
        url,
        git_ref
    );

    Ok(manifest)
}

/// Remove a plugin directory from `<data_dir>/plugins/installed/<name>/`.
pub fn uninstall(name: &str, data_dir: &str) -> AppResult<()> {
    let install_dir = format!("{}/plugins/installed/{}", data_dir, name);
    let path = Path::new(&install_dir);

    if !path.exists() {
        err_msg!("Plugin '{}' is not installed at: {}", name, install_dir);
    }

    if !path.is_dir() {
        err_msg!("Plugin path is not a directory: {}", install_dir);
    }

    std::fs::remove_dir_all(path)
        .ctx(format!("Failed to remove plugin directory: {}", install_dir))?;

    tracing::info!("Uninstalled plugin '{}'", name);
    Ok(())
}

// ---------------------------------------------------------------------------
// Discover plugins
// ---------------------------------------------------------------------------

/// Discover all plugins from disk by scanning installed and bundled directories.
///
/// Returns a vector of `(PluginManifest, source_type, base_path)` tuples.
///
/// Scans:
/// - `<data_dir>/plugins/installed/<name>/plugin.json` — source: "installed"
/// - `<workspace_dir>/plugins/<type>/<name>/plugin.json` — source: "bundled"
///   where `<type>` is one of: platforms, mcp, providers
pub fn discover_plugins(
    data_dir: &str,
    workspace_dir: &str,
) -> Vec<(PluginManifest, String, String)> {
    let mut results = Vec::new();

    // A. Scan installed plugins: <data_dir>/plugins/installed/<name>/plugin.json
    let installed_base = format!("{}/plugins/installed", data_dir);
    if let Ok(entries) = std::fs::read_dir(&installed_base) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let manifest_path = path.join("plugin.json");
            if manifest_path.exists() {
                let path_str = manifest_path.to_string_lossy().to_string();
                match load_manifest(&path_str) {
                    Ok(manifest) => {
                        results.push((manifest, "installed".to_string(), path_str));
                    }
                    Err(e) => {
                        tracing::warn!(
                            "Failed to load installed plugin manifest at {}: {:?}",
                            path_str,
                            e
                        );
                    }
                }
            }
        }
    }

    // B. Scan bundled plugins: <workspace_dir>/plugins/<type>/<name>/plugin.json
    let bundled_base = format!("{}/plugins", workspace_dir);
    if let Ok(bundled_entries) = std::fs::read_dir(&bundled_base) {
        for entry in bundled_entries.flatten() {
            let type_path = entry.path();
            if !type_path.is_dir() {
                continue;
            }
            // type_path is like plugins/mcp or plugins/platform
            if let Ok(plugin_entries) = std::fs::read_dir(&type_path) {
                for plugin_entry in plugin_entries.flatten() {
                    let plugin_path = plugin_entry.path();
                    if !plugin_path.is_dir() {
                        continue;
                    }
                    let manifest_path = plugin_path.join("plugin.json");
                    if manifest_path.exists() {
                        let path_str = manifest_path.to_string_lossy().to_string();
                        match load_manifest(&path_str) {
                            Ok(manifest) => {
                                results.push((manifest, "bundled".to_string(), path_str));
                            }
                            Err(e) => {
                                tracing::warn!(
                                    "Failed to load bundled plugin manifest at {}: {:?}",
                                    path_str,
                                    e
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    // C. Scan data_dir plugins: <data_dir>/plugins/<type>/<name>/plugin.json
    // This covers providers, platforms, and MCP tools that live in the plugins/ directory.
    let data_plugins_base = format!("{}/plugins", data_dir);
    if let Ok(data_plugin_entries) = std::fs::read_dir(&data_plugins_base) {
        for entry in data_plugin_entries.flatten() {
            let type_path = entry.path();
            if !type_path.is_dir() {
                continue;
            }
            // Skip the 'installed' subdirectory — handled by section A
            if type_path.file_name().and_then(|n| n.to_str()) == Some("installed") {
                continue;
            }
            // type_path is like plugins/providers, plugins/platforms, plugins/mcp
            if let Ok(plugin_entries) = std::fs::read_dir(&type_path) {
                for plugin_entry in plugin_entries.flatten() {
                    let plugin_path = plugin_entry.path();
                    if !plugin_path.is_dir() {
                        continue;
                    }
                    let manifest_path = plugin_path.join("plugin.json");
                    if manifest_path.exists() {
                        let path_str = manifest_path.to_string_lossy().to_string();
                        // Dedup against already-discovered plugins (by name)
                        let manifest = match load_manifest(&path_str) {
                            Ok(m) => m,
                            Err(e) => {
                                tracing::warn!(
                                    "Failed to load data_dir plugin manifest at {}: {:?}",
                                    path_str,
                                    e
                                );
                                continue;
                            }
                        };
                        if !results.iter().any(|(m, _, _)| m.name == manifest.name) {
                            results.push((manifest, "bundled".to_string(), path_str));
                        }
                    }
                }
            }
        }
    }

    // After all discovery, also scan mcp-config.json files for MCP server entries
    // that aren't covered by bundled/installed plugin.json files.
    // These get synthetic manifests but won't have config_schema unless
    // a plugin.json is also present under the installed/bundled paths.
    let mcp_plugin_servers = crate::mcp::external::config::discover_plugin_servers(data_dir, workspace_dir);
    for srv in &mcp_plugin_servers {
        // Dedup: check if a plugin with the same name OR from the same source directory
        // (matching plugin.json directory name) already exists in results.
        let already_exists = results.iter().any(|(m, _, base_path)| {
            if m.name == srv.name {
                return true;
            }
            // The mcp-config.json server name typically matches the directory name
            // (e.g. directory "filesystem" → server name "filesystem").
            // Check if any existing plugin's base_path directory name matches.
            // Normalize both sides by replacing hyphens with underscores for comparison
            // (some directories use hyphens while server names use underscores or different names).
            if let Some(parent_dir) = std::path::Path::new(base_path).parent() {
                if let Some(dir_name) = parent_dir.file_name().and_then(|n| n.to_str()) {
                    let dir_normalized = dir_name.replace('-', "_");
                    let srv_normalized = srv.name.replace('-', "_");
                    if dir_normalized == srv_normalized || dir_name == srv.name {
                        return true;
                    }
                }
            }
            false
        });
        if !already_exists {
            let transport_str = match srv.transport {
                crate::mcp::external::config::McpTransport::Stdio => "stdio",
                crate::mcp::external::config::McpTransport::Http => "http",
            };
            let manifest = PluginManifest {
                name: srv.name.clone(),
                version: "0.1.0".to_string(),
                plugin_type: PluginType::Mcp,
                description: Some(format!("MCP server '{}' (from mcp-config.json)", srv.name)),
                entrypoint: Some(crate::plugin::PluginEntrypoint {
                    command: srv.command.clone().unwrap_or_default(),
                    args: srv.args.clone(),
                    transport: transport_str.to_string(),
                    url: srv.url.clone(),
                }),
                capabilities: None,
                config_schema: vec![],
                env: std::collections::HashMap::new(),
                default_base_url: None,
                api_mode: None,
            };
            results.push((manifest, "mcp_config".to_string(), String::new()));
        }
    }

    results
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_discover_plugins_empty_dirs() {
        let data_dir = tempfile::tempdir().unwrap();
        let workspace_dir = tempfile::tempdir().unwrap();

        // No plugin dirs exist yet
        let plugins = discover_plugins(
            data_dir.path().to_str().unwrap(),
            workspace_dir.path().to_str().unwrap(),
        );
        assert!(plugins.is_empty());
    }

    #[test]
    fn test_discover_installed_plugin() {
        let data_dir = tempfile::tempdir().unwrap();
        let workspace_dir = tempfile::tempdir().unwrap();

        // Create an installed plugin
        let plugin_dir = data_dir
            .path()
            .join("plugins")
            .join("installed")
            .join("my-plugin");
        std::fs::create_dir_all(&plugin_dir).unwrap();
        let manifest_content = r#"{
            "name": "my-plugin",
            "version": "1.0.0",
            "type": "mcp",
            "entrypoint": {
                "command": "python3",
                "args": ["server.py"]
            }
        }"#;
        std::fs::write(plugin_dir.join("plugin.json"), manifest_content).unwrap();

        let plugins = discover_plugins(
            data_dir.path().to_str().unwrap(),
            workspace_dir.path().to_str().unwrap(),
        );
        assert_eq!(plugins.len(), 1);
        assert_eq!(plugins[0].0.name, "my-plugin");
        assert_eq!(plugins[0].1, "installed");
    }

    #[test]
    fn test_find_plugin_json_top_level() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("plugin.json"),
            r#"{"name":"test","type":"mcp","entrypoint":{"command":"test"}}"#,
        )
        .unwrap();
        let found = find_plugin_json(dir.path()).unwrap();
        assert_eq!(
            found,
            dir.path().join("plugin.json").to_string_lossy().to_string()
        );
    }

    #[test]
    fn test_find_plugin_json_nested() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("plugin-dir");
        std::fs::create_dir(&nested).unwrap();
        std::fs::write(
            nested.join("plugin.json"),
            r#"{"name":"test","type":"mcp","entrypoint":{"command":"test"}}"#,
        )
        .unwrap();
        let found = find_plugin_json(dir.path()).unwrap();
        assert_eq!(
            found,
            nested.join("plugin.json").to_string_lossy().to_string()
        );
    }

    #[test]
    fn test_find_plugin_json_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let result = find_plugin_json(dir.path());
        assert!(result.is_err());
    }

    #[test]
    fn test_uninstall_nonexistent() {
        let data_dir = tempfile::tempdir().unwrap();
        let result = uninstall("nonexistent-plugin", data_dir.path().to_str().unwrap());
        assert!(result.is_err());
    }

    #[test]
    fn test_copy_dir_recursive() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();

        // Create a nested structure
        std::fs::create_dir(src.path().join("subdir")).unwrap();
        std::fs::write(src.path().join("file1.txt"), "content1").unwrap();
        std::fs::write(src.path().join("subdir").join("file2.txt"), "content2").unwrap();

        let dst_path = dst.path().join("copied");
        copy_dir_recursive(src.path(), &dst_path).unwrap();

        assert!(dst_path.join("file1.txt").exists());
        assert!(dst_path.join("subdir").join("file2.txt").exists());
        assert_eq!(
            std::fs::read_to_string(dst_path.join("file1.txt")).unwrap(),
            "content1"
        );
    }
}
