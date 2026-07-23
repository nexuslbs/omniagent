//! Filesystem helpers for plugin installation, uninstallation, and discovery.
//!
//! Supports:
//! - Installing from URL (.tar.gz, .tgz, .zip)
//! - Installing from Git (clones to data_dir/plugins/<type>/.remote/<name>/)
//! - Uninstalling (removing plugin directory / YAML caller handles removal)
//! - Discovering plugins from disk

use crate::err_msg;
use crate::error::{AppResult, Error, ErrorContext};
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
    let response =
        reqwest::blocking::get(url).ctx(format!("Failed to download plugin from {}", url))?;

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
    std::fs::create_dir_all(&temp_path).ctx(format!(
        "Failed to create temp directory at {:?}",
        temp_path
    ))?;

    // Cleanup on drop (if still present)
    let temp_path_clone = temp_path.clone();
    let cleanup = std::sync::Mutex::new(Some(temp_path_clone));

    let result = install_from_url_inner(url, &bytes, &temp_path, data_dir);

    // Clean up install path
    if let Ok(mut path_opt) = cleanup.lock() {
        if let Some(path) = path_opt.take() {
            if let Err(e) = std::fs::remove_dir_all(&path) {
                tracing::warn!(
                    "[installer] Failed to clean up temp dir {:?}: {:?}",
                    path,
                    e
                );
            }
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
        std::fs::write(&archive_path, bytes)
            .ctx("Failed to write downloaded archive to temp dir")?;

        // Extract using tar
        let status = std::process::Command::new("tar")
            .arg("-xzf")
            .arg(&archive_path)
            .arg("-C")
            .arg(temp_path)
            .status()
            .ctx("Failed to execute tar command")?;

        if !status.success() {
            err_msg!("tar extraction failed with status: {}", status);
        }
    } else if url_lower.ends_with(".zip") {
        archive_path = temp_path.join("plugin.zip");
        std::fs::write(&archive_path, bytes)
            .ctx("Failed to write downloaded archive to temp dir")?;

        // Extract using unzip
        let status = std::process::Command::new("unzip")
            .arg("-o")
            .arg(&archive_path)
            .arg("-d")
            .arg(temp_path)
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
    let plugin_json = find_plugin_json(temp_path)?;
    let manifest = load_manifest(&plugin_json)?;

    // Install to the correct type directory under data_dir
    let type_dir = match manifest.plugin_type {
        PluginType::Platform => "platforms",
        PluginType::Mcp => "mcp",
        PluginType::Provider => "providers",
    };

    let install_dir = format!("{}/plugins/{}/{}", data_dir, type_dir, manifest.name);
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
    std::fs::create_dir_all(parent).ctx(format!(
        "Failed to create parent directories for: {}",
        install_dir
    ))?;

    // Copy the extracted plugin directory to the install location
    let extracted_dir = Path::new(&plugin_json).parent().ok_or_else(|| {
        Error::Message(format!("Plugin JSON path has no parent: {}", plugin_json))
    })?;
    let copy_result = copy_dir_recursive(extracted_dir, install_path);
    if let Err(e) = copy_result {
        // Clean up on failure
        if let Err(re) = std::fs::remove_dir_all(install_path) {
            tracing::warn!("[installer] Failed to clean up install path: {:?}", re);
        }
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
        std::fs::create_dir_all(dst).ctx(format!("Failed to create directory: {:?}", dst))?;
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

/// Clone a plugin from a git repository directly into
/// `<data_dir>/plugins/<type_dir>/.remote/<name>/`: NO source copying.
///
/// Uses a shared bare-mirror cache at `{workspace_dir}/.git-cache/<sha256(url)>/`
/// so that multiple plugins from the same repo share a single object store.
/// Fresh clones use `git clone --reference <cache>`: instant, zero network.
///
/// The type directory (mcp, platforms, providers) is determined automatically
/// from the `type` field in the plugin's plugin.json manifest after cloning.
///
/// Steps:
/// 1. Ensure git-cache is up to date (bare mirror at workspace_dir/.git-cache/)
/// 2. If plugin already cloned: fetch+reset in the plugin dir (preserves cargo target/)
/// 3. If first-time: reference clone from cache (instant, no network)
/// 4. Find plugin.json, read manifest, rename type dir if needed
/// 5. Return (PluginManifest, content_changed): caller decides whether to compile
pub fn install_from_git(
    url: &str,
    name: &str,
    git_ref: Option<&str>,
    workspace_dir: &str,
    data_dir: &str,
    repo_path: Option<&str>,
) -> AppResult<(PluginManifest, bool)> {
    // ── Git-cache: shared bare mirror for all plugins from the same URL ──
    use sha2::{Digest, Sha256};
    let cache_key = format!("{:x}", Sha256::digest(url.as_bytes()));
    let cache_dir = format!("{}/.git-cache/{}", workspace_dir, cache_key);
    let cache_path = std::path::Path::new(&cache_dir);

    // Ensure the cache exists and is up to date
    if cache_path.join("HEAD").exists() {
        // Update existing bare mirror: delta-only fetch
        tracing::info!("Updating git-cache for '{}' at {}", url, cache_dir);
        let fetch_status = std::process::Command::new("git")
            .args(["-C", &cache_dir, "remote", "update", "--prune"])
            .status()
            .ctx(format!("Failed to update git-cache at {}", cache_dir))?;
        if !fetch_status.success() {
            tracing::warn!(
                "git-cache update failed for {}, falling back to traditional clone",
                url
            );
            // Fall through: the cache exists and will still be used for
            // object references; the fetch failure just means stale objects.
        } else {
            tracing::info!("git-cache for '{}' updated successfully", url);
        }
    } else {
        // First time for this URL: create the bare mirror
        if cache_path.exists() {
            std::fs::remove_dir_all(cache_path)
                .ctx(format!("Failed to remove existing cache at {}", cache_dir))?;
        }
        if let Some(parent) = cache_path.parent() {
            std::fs::create_dir_all(parent).ctx(format!(
                "Failed to create parent dirs for git-cache at {}",
                cache_dir
            ))?;
        }
        tracing::info!("Creating git-cache for '{}' at {}", url, cache_dir);
        let clone_status = std::process::Command::new("git")
            .args(["clone", "--mirror", url, &cache_dir])
            .status()
            .ctx(format!(
                "Failed to clone git mirror for '{}' into cache at {}",
                url, cache_dir
            ))?;
        if !clone_status.success() {
            if let Err(e) = std::fs::remove_dir_all(cache_path) {
                tracing::warn!("[installer] Failed to clean up cache dir: {:?}", e);
            }
            err_msg!(
                "git mirror clone failed for '{}' with status: {}",
                url,
                clone_status
            );
        }
    }

    // ── Clone / update in data_dir/plugins/tools/.remote/<name> first ──
    // We clone into tools first because that's the most common type. If the
    // manifest says otherwise, we rename to the correct type dir afterwards.
    let initial_remote_dir = format!("{}/plugins/tools/.remote/{}", data_dir, name);
    let initial_remote_path = std::path::Path::new(&initial_remote_dir);

    // Track whether the git content actually changed (for callers to decide
    // whether a recompile is needed).
    let content_changed: bool;

    // If the directory already exists and has a .git dir, do a fetch+reset
    // instead of rm -rf + fresh clone. This avoids re-downloading the entire
    // repo and preserves the incremental cargo build cache.
    if initial_remote_path.join(".git").exists() {
        tracing::info!(
            "Updating existing git plugin '{}' from {} (ref: {:?})",
            name,
            url,
            git_ref
        );

        // Record pre-fetch HEAD
        let pre_fetch = std::process::Command::new("git")
            .args(["-C", &initial_remote_dir, "rev-parse", "HEAD"])
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_string());

        // Fetch and reset to latest
        let fetch_status = std::process::Command::new("git")
            .args([
                "-C",
                &initial_remote_dir,
                "fetch",
                "--depth",
                "1",
                "origin",
                "HEAD",
            ])
            .status()
            .ctx(format!(
                "Failed to git fetch for '{}' in {}",
                name, initial_remote_dir
            ))?;
        if !fetch_status.success() {
            // Fetch failed: fall back to fresh clone from cache
            tracing::warn!(
                "git fetch failed for '{}', falling back to reference clone from cache",
                name
            );
            std::fs::remove_dir_all(initial_remote_path).ctx(format!(
                "Failed to remove existing clone at {}",
                initial_remote_dir
            ))?;
            content_changed = true;
            // Fall through to clone below
        } else {
            let reset_status = std::process::Command::new("git")
                .args(["-C", &initial_remote_dir, "reset", "--hard", "FETCH_HEAD"])
                .status()
                .ctx(format!(
                    "Failed to git reset for '{}' in {}",
                    name, initial_remote_dir
                ))?;
            if !reset_status.success() {
                err_msg!(
                    "git reset failed for '{}' with status: {}",
                    name,
                    reset_status
                );
            }

            // Check if anything changed
            let post_fetch = std::process::Command::new("git")
                .args(["-C", &initial_remote_dir, "rev-parse", "HEAD"])
                .output()
                .ok()
                .and_then(|o| String::from_utf8(o.stdout).ok())
                .map(|s| s.trim().to_string());

            content_changed = pre_fetch != post_fetch;
            if !content_changed {
                tracing::info!(
                    "git plugin '{}' is already up to date (no new commits)",
                    name
                );
            } else {
                tracing::info!(
                    "git plugin '{}' updated: {} → {}",
                    name,
                    pre_fetch.as_deref().unwrap_or("?"),
                    post_fetch.as_deref().unwrap_or("?")
                );
            }

            // Checkout specific ref if specified (skip if not set)
            if let Some(ref_str) = git_ref {
                if !ref_str.is_empty() {
                    let checkout_status = std::process::Command::new("git")
                        .args(["-C", &initial_remote_dir, "checkout", ref_str])
                        .status()
                        .ctx(format!("Failed to git checkout {} for '{}'", ref_str, name))?;
                    if !checkout_status.success() {
                        if let Err(e) = std::fs::remove_dir_all(&initial_remote_dir) {
                            tracing::warn!("[installer] Failed to clean up remote dir after update checkout failure: {:?}", e);
                        }
                        err_msg!(
                            "git checkout '{}' failed for '{}' with status: {}",
                            ref_str,
                            name,
                            checkout_status
                        );
                    }
                }
            }
        }
    } else {
        // First time: reference clone from cache (instant, no network)
        content_changed = true;

        if initial_remote_path.exists() {
            std::fs::remove_dir_all(initial_remote_path).ctx(format!(
                "Failed to remove existing clone at {}",
                initial_remote_dir
            ))?;
        }
        if let Some(parent) = initial_remote_path.parent() {
            std::fs::create_dir_all(parent).ctx(format!(
                "Failed to create parent dirs for {}",
                initial_remote_dir
            ))?;
        }

        tracing::info!(
            "Reference-cloning git plugin '{}' from cache {} (ref: {:?})",
            name,
            cache_dir,
            git_ref
        );

        // Reference clone from local cache: instant, hardlinks objects
        let mut cmd = std::process::Command::new("git");
        cmd.arg("clone")
            .arg("--reference")
            .arg(&cache_dir)
            .arg("--depth")
            .arg("1")
            .arg(url)
            .arg(&initial_remote_dir);
        tracing::info!(
            "Running: git clone --reference {} --depth 1 {} {}",
            cache_dir,
            url,
            initial_remote_dir
        );
        let output = cmd.output().ctx(format!(
            "Failed to execute git clone for '{}' from {}",
            name, url
        ))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            tracing::error!(
                "git clone failed for '{}' from {}. stderr: {}",
                name,
                url,
                stderr
            );
            if let Err(e) = std::fs::remove_dir_all(&initial_remote_dir) {
                tracing::warn!(
                    "[installer] Failed to clean up remote dir after clone failure: {:?}",
                    e
                );
            }
            err_msg!(
                "git clone failed for '{}' from {} with status: {}. stderr: {}",
                name,
                url,
                output.status,
                stderr.trim()
            );
        }

        // Checkout specific ref if specified
        if let Some(ref_str) = git_ref {
            if !ref_str.is_empty() {
                tracing::info!("Checking out ref '{}' for plugin '{}'", ref_str, name);
                let checkout_status = std::process::Command::new("git")
                    .arg("-C")
                    .arg(&initial_remote_dir)
                    .arg("checkout")
                    .arg(ref_str)
                    .status()
                    .ctx(format!("Failed to git checkout {} for '{}'", ref_str, name))?;
                if !checkout_status.success() {
                    if let Err(e) = std::fs::remove_dir_all(&initial_remote_dir) {
                        tracing::warn!("[installer] Failed to clean up remote dir after checkout failure: {:?}", e);
                    }
                    err_msg!(
                        "git checkout '{}' failed for '{}' with status: {}",
                        ref_str,
                        name,
                        checkout_status
                    );
                }
            }
        }
    }

    // Find plugin.json in the cloned directory (respecting repo_path)
    let search_dir = match repo_path {
        Some(p) if !p.is_empty() => initial_remote_path.join(p),
        _ => initial_remote_path.to_path_buf(),
    };
    let plugin_json_path = find_plugin_json(&search_dir)?;
    let manifest = load_manifest(&plugin_json_path)?;

    // Determine the correct type directory from the manifest
    let type_dir = match manifest.plugin_type {
        PluginType::Platform => "platforms",
        PluginType::Mcp => "tools",
        PluginType::Provider => "providers",
    };

    // If the type is not tools, rename the .remote/ dir to the correct type directory
    if type_dir != "tools" {
        let final_remote_dir = format!("{}/plugins/{}/.remote/{}", data_dir, type_dir, name);
        let final_remote_path = std::path::Path::new(&final_remote_dir);
        if final_remote_path.exists() {
            std::fs::remove_dir_all(final_remote_path).ctx(format!(
                "Failed to remove existing clone at {}",
                final_remote_dir
            ))?;
        }
        if let Some(parent) = final_remote_path.parent() {
            std::fs::create_dir_all(parent).ctx(format!(
                "Failed to create parent dirs for {}",
                final_remote_dir
            ))?;
        }
        // Rename works here because both are under the same data_dir mount
        std::fs::rename(initial_remote_path, final_remote_path).ctx(format!(
            "Failed to rename clone from {} to {}",
            initial_remote_dir, final_remote_dir
        ))?;
    }

    // Verify the manifest from the .remote/ location
    // After the type rename above, the plugin is at {data_dir}/plugins/{type_dir}/.remote/{name}/
    let final_type_dir = type_dir; // Already correct after rename
    let final_remote_dir = format!("{}/plugins/{}/.remote/{}", data_dir, final_type_dir, name);
    let check_search_dir = match repo_path {
        Some(p) if !p.is_empty() => std::path::Path::new(&final_remote_dir).join(p),
        _ => std::path::PathBuf::from(&final_remote_dir),
    };
    let final_manifest_path = check_search_dir.join("plugin.json");
    let manifest = load_manifest(&final_manifest_path.to_string_lossy()).ctx(format!(
        "Failed to verify installed plugin manifest at: {}",
        final_manifest_path.display()
    ))?;

    tracing::info!(
        "Installed git plugin '{}' version {} from {} (ref: {:?}): in-place at .remote/",
        manifest.name,
        manifest.version,
        url,
        git_ref
    );

    Ok((manifest, content_changed))
}

/// Uninstall a plugin from disk.
///
/// - For remote plugins: removes `.remote/` directory under data_dir
/// - For builtin/omni-stack plugins: no-op on filesystem (YAML removal is caller's job)
pub fn uninstall(name: &str, data_dir: &str, type_dir: &str, is_remote: bool) -> AppResult<()> {
    if is_remote {
        // Remote plugin: remove the .remote/ directory
        let remote_dir = format!("{}/plugins/{}/.remote/{}", data_dir, type_dir, name);
        let path = Path::new(&remote_dir);
        if path.exists() && path.is_dir() {
            std::fs::remove_dir_all(path).ctx(format!(
                "Failed to remove remote plugin directory: {}",
                remote_dir
            ))?;
            tracing::info!("Removed remote plugin directory '{}'", name);
            return Ok(());
        } else {
            // Try all type dirs if type not known
            for t in &["mcp", "platforms", "providers"] {
                let alt_dir = format!("{}/plugins/{}/.remote/{}", data_dir, t, name);
                let alt_path = Path::new(&alt_dir);
                if alt_path.exists() && alt_path.is_dir() {
                    std::fs::remove_dir_all(alt_path).ctx(format!(
                        "Failed to remove remote plugin directory: {}",
                        alt_dir
                    ))?;
                    tracing::info!(
                        "Removed remote plugin directory '{}' from {}",
                        name,
                        alt_dir
                    );
                    return Ok(());
                }
            }
            err_msg!(
                "Remote plugin '{}' has no .remote/ directory at any known type path",
                name
            );
        }
    }

    // For builtin and omni-stack plugins: no filesystem removal
    // YAML entry removal is the caller's responsibility
    tracing::info!(
        "Uninstall: no filesystem removal for builtin/omni-stack plugin '{}' (type={})",
        name,
        type_dir
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Discover plugins
// ---------------------------------------------------------------------------

/// Helper to extract directory key from base_path of a discovered plugin.
/// For "bundled" and "remote" sources, the key is the parent directory name of plugin.json.
/// For "built-in" synthetic sources, the key is the directory name.
pub(crate) fn extract_plugin_key_from_path(base_path: &str) -> String {
    if let Some(parent) = std::path::Path::new(base_path).parent() {
        if let Some(dir_name) = parent.file_name().and_then(|n| n.to_str()) {
            return dir_name.to_string();
        }
    }
    String::new()
}

/// Discover all plugins from disk by scanning all canonical locations.
///
/// Returns a vector of `(PluginManifest, source_type, base_path)` tuples.
/// May contain multiple entries with the same key (directory name) from different sources
/// (e.g., bundled + built-in). Callers must resolve duplicates via YAML configuration.
///
/// Scans:
/// - `<data_dir>/plugins/<type>/<name>/plugin.json`: source: "bundled" (data level)
/// - `<workspace_dir>/plugins/<type>/<name>/plugin.json`: source: "bundled" (workspace - deduped against data_dir)
/// - `<data_dir>/plugins/<type>/.remote/<name>/plugin.json`: source: "remote"
/// - `/app/plugins/<type>/<name>/plugin.json or Cargo.toml`: source: "built-in"
pub fn discover_plugins(data_dir: &str) -> Vec<(PluginManifest, String, String)> {
    let mut results = Vec::new();

    // A. Scan data_dir plugins: <data_dir>/plugins/<type>/<name>/plugin.json
    // This covers providers, platforms, and MCP tools that live in the plugins/ directory.
    let data_plugins_base = format!("{}/plugins", data_dir);
    if let Ok(data_plugin_entries) = std::fs::read_dir(&data_plugins_base) {
        for entry in data_plugin_entries.flatten() {
            let type_path = entry.path();
            if !type_path.is_dir() {
                continue;
            }
            // Skip .remote/ directories at the type level: handled separately (section C)
            if type_path
                .file_name()
                .and_then(|n| n.to_str())
                .map(|s| s.starts_with('.'))
                .unwrap_or(false)
            {
                continue;
            }
            // type_path is like plugins/providers, plugins/platforms, plugins/mcp
            if let Ok(plugin_entries) = std::fs::read_dir(&type_path) {
                for plugin_entry in plugin_entries.flatten() {
                    let plugin_path = plugin_entry.path();
                    if !plugin_path.is_dir() {
                        continue;
                    }
                    // Skip .remote/ hidden directories
                    if plugin_path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .map(|s| s.starts_with('.'))
                        .unwrap_or(false)
                    {
                        continue;
                    }
                    let manifest_path = plugin_path.join("plugin.json");
                    if manifest_path.exists() {
                        let path_str = manifest_path.to_string_lossy().to_string();
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
                        let _key = extract_plugin_key_from_path(&path_str);
                        results.push((manifest, "bundled".to_string(), path_str));
                    }
                }
            }
        }
    }

    // B. Scan /app/plugins/ for built-in plugin config files (plugin.json, mcp-config.json).
    // This is the omniagent repo's plugins directory, mounted at /app in the container.
    // Built-in plugins keep their config with their source code, not in the OMNI_DIR.
    let app_plugins_base = "/app/plugins";
    if let Ok(app_plugin_entries) = std::fs::read_dir(app_plugins_base) {
        for entry in app_plugin_entries.flatten() {
            let type_path = entry.path();
            if !type_path.is_dir() {
                continue;
            }
            if type_path
                .file_name()
                .and_then(|n| n.to_str())
                .map(|s| s.starts_with('.'))
                .unwrap_or(false)
            {
                continue;
            }
            if let Ok(plugin_entries) = std::fs::read_dir(&type_path) {
                for plugin_entry in plugin_entries.flatten() {
                    let plugin_path = plugin_entry.path();
                    if !plugin_path.is_dir() {
                        continue;
                    }
                    if plugin_path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .map(|s| s.starts_with('.'))
                        .unwrap_or(false)
                    {
                        continue;
                    }
                    let manifest_path = plugin_path.join("plugin.json");
                    if manifest_path.exists() {
                        let path_str = manifest_path.to_string_lossy().to_string();
                        let manifest = match load_manifest(&path_str) {
                            Ok(m) => m,
                            Err(e) => {
                                tracing::warn!(
                                    "Failed to load /app/plugins plugin manifest at {}: {:?}",
                                    path_str,
                                    e
                                );
                                continue;
                            }
                        };
                        results.push((manifest, "built-in".to_string(), path_str));
                    }
                }
            }
        }
    }

    // C. Scan remote plugins using remote.yml for exact path resolution.
    // C1: remote.yml-driven using exact manifest paths
    // C2: fallback directory scan for orphan .remote/ dirs
    let _remote_plugins = crate::plugins_yaml::load_remote_plugins(data_dir);
    // C. Scan remote plugins using remote.yml for exact path resolution.
    // C1: remote.yml-driven using exact manifest paths
    // C2: fallback directory scan for orphan .remote/ dirs
    let remote_plugins = crate::plugins_yaml::load_remote_plugins(data_dir);
    let mut remote_seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    // Helper to process entries for a given type
    macro_rules! process_remote_entries {
        ($type_name:expr, $map:expr) => {
            if let Some(ref entries) = $map {
                for (name, remote_info) in entries {
                    let subpath = remote_info.path.as_deref().unwrap_or("");
                    let manifest_path = format!(
                        "{}/plugins/{}/.remote/{}/{}/plugin.json",
                        data_dir, $type_name, name, subpath
                    );
                    if std::path::Path::new(&manifest_path).exists() {
                        match load_manifest(&manifest_path) {
                            Ok(manifest) => {
                                // Key is the remote.yml key (repo name), NOT the subdirectory name.
                                // e.g., for remote.yml key "cron" with path "tools/cron-echo",
                                // the key is "cron" so it groups with built-in/bundled cron.
                                let key = name.clone();
                                remote_seen.insert(key.clone());
                                // Always add remote sources: they provide the manifest for
                                // grouping in plugins_yaml.rs. Dedup is handled there.
                                results.push((manifest, "remote".to_string(), manifest_path));
                            }
                            Err(e) => {
                                tracing::warn!(
                                    "Failed to load remote plugin manifest at {}: {:?}",
                                    manifest_path,
                                    e
                                );
                            }
                        }
                    }
                }
            }
        };
    }

    process_remote_entries!("tools", remote_plugins.tools);
    process_remote_entries!("platforms", remote_plugins.platforms);
    process_remote_entries!("providers", remote_plugins.providers);

    // C2. Fallback: scan .remote/ directories for orphan plugins not listed in remote.yml.
    // A plugin.json under .remote/<name>/ is treated as remote source even without a
    // remote.yml entry (e.g. during manual setup or testing).
    for type_name in &["mcp", "platforms", "providers"] {
        let remote_dir = format!("{}/plugins/{}/.remote", data_dir, type_name);
        if let Ok(entries) = std::fs::read_dir(&remote_dir) {
            for entry in entries.flatten() {
                let plugin_path = entry.path();
                if !plugin_path.is_dir() {
                    continue;
                }
                let manifest_path = plugin_path.join("plugin.json");
                if !manifest_path.exists() {
                    continue;
                }
                let name = plugin_path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("")
                    .to_string();
                // Skip if already discovered via remote.yml (C1)
                if remote_seen.contains(&name) {
                    continue;
                }
                match load_manifest(&manifest_path.to_string_lossy()) {
                    Ok(manifest) => {
                        results.push((
                            manifest,
                            "remote".to_string(),
                            manifest_path.to_string_lossy().to_string(),
                        ));
                    }
                    Err(e) => {
                        tracing::warn!(
                            "Failed to load orphan remote plugin manifest at {}: {:?}",
                            manifest_path.display(),
                            e
                        );
                    }
                }
            }
        }
    }

    // D. Scan builtin plugins: /app/plugins/<type>/<name>/
    // These are workspace member crates that have Cargo.toml (and optionally mcp-config.json
    // or plugin.json). All builtin sources are added; callers handle dedup.
    for type_name in &["tools", "platforms", "providers"] {
        let app_plugins_dir = format!("/app/plugins/{}", type_name);
        if let Ok(plugin_entries) = std::fs::read_dir(&app_plugins_dir) {
            for entry in plugin_entries.flatten() {
                let plugin_path = entry.path();
                if !plugin_path.is_dir() {
                    continue;
                }
                let has_cargo_toml = plugin_path.join("Cargo.toml").exists();
                let has_plugin_json = plugin_path.join("plugin.json").exists();
                let has_mcp_config = plugin_path.join("mcp-config.json").exists();
                // Require at least plugin.json or mcp-config.json to be a plugin;
                // having only Cargo.toml (e.g. a utility library like util) isn't enough
                if !has_cargo_toml && !has_plugin_json {
                    continue;
                }
                if !has_plugin_json && !has_mcp_config {
                    // Has Cargo.toml but no plugin.json or mcp-config.json: not a plugin
                    continue;
                }

                let dir_name = plugin_path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("")
                    .to_string();

                if has_plugin_json {
                    // Already discovered in Section B above.
                    // Skip — plugin.json entries are pushed there with the same
                    // source type. Only Cargo.toml-only plugins need this section.
                } else if has_cargo_toml {
                    // Synthetic manifest for Rust workspace member crates
                    let _pkg_name = std::fs::read_to_string(plugin_path.join("Cargo.toml"))
                        .ok()
                        .and_then(|content| {
                            content.lines().find_map(|line| {
                                let trimmed = line.trim();
                                if let Some(name) = trimmed.strip_prefix("name = \"") {
                                    name.strip_suffix('\"').map(|s| s.to_string())
                                } else {
                                    None
                                }
                            })
                        })
                        .unwrap_or_else(|| format!("mcp-server-{}", dir_name));

                    let manifest = PluginManifest {
                        name: dir_name.clone(),
                        version: "0.1.0".to_string(),
                        plugin_type: match *type_name {
                            "platforms" => PluginType::Platform,
                            "providers" => PluginType::Provider,
                            _ => PluginType::Mcp,
                        },
                        description: Some(format!("Builtin {} plugin", type_name)),
                        entrypoint: Some(crate::plugin::PluginEntrypoint {
                            command: format!("mcp-server-{}", dir_name),
                            args: vec![],
                            transport: "stdio".to_string(),
                            url: None,
                        }),
                        capabilities: None,
                        config_schema: vec![],
                        env: std::collections::HashMap::new(),
                        default_base_url: None,
                        api_mode: None,
                        api_modes: None,
                    };
                    let path_str = plugin_path.join("Cargo.toml").to_string_lossy().to_string();
                    results.push((manifest, "built-in".to_string(), path_str));
                }
            }
        }
    }

    // After all discovery, also scan mcp-config.json files for MCP server entries
    // that aren't covered by bundled/remote/builtin plugin.json files.
    // These get synthetic manifests but won't have config_schema unless
    // a plugin.json is also present.
    let mcp_plugin_servers = crate::mcp::external::config::discover_plugin_servers(data_dir);
    for srv in &mcp_plugin_servers {
        let already_exists =
            results
                .iter()
                .any(|(m, _, base_path): &(PluginManifest, String, String)| {
                    if m.name == srv.name {
                        return true;
                    }
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
                api_modes: None,
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
        let _workspace_dir = tempfile::tempdir().unwrap();

        // No plugin dirs exist yet
        let plugins = discover_plugins(data_dir.path().to_str().unwrap());
        // Builtin plugins may be found from /app/plugins/ but none from temp dirs
        // mcp_config entries also come from the CWD's plugins/ directory
        let from_temp: Vec<_> = plugins
            .iter()
            .filter(|(_, s, _)| s != "built-in" && s != "mcp_config")
            .collect();
        assert!(
            from_temp.is_empty(),
            "Expected no plugins from temp dirs, got {:?}",
            from_temp
        );
    }

    #[test]
    fn test_discover_installed_plugin() {
        let data_dir = tempfile::tempdir().unwrap();
        let _workspace_dir = tempfile::tempdir().unwrap();

        // Create a data directory plugin
        let plugin_dir = data_dir
            .path()
            .join("plugins")
            .join("mcp")
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

        let plugins = discover_plugins(data_dir.path().to_str().unwrap());
        assert!(!plugins.is_empty());
        let found = plugins
            .iter()
            .any(|(m, s, _)| m.name == "my-plugin" && s == "bundled");
        assert!(found, "Expected my-plugin to be discovered");
    }

    #[test]
    fn test_discover_remote_plugin() {
        let data_dir = tempfile::tempdir().unwrap();
        let _workspace_dir = tempfile::tempdir().unwrap();

        // Create a remote plugin under .remote/
        let plugin_dir = data_dir
            .path()
            .join("plugins")
            .join("mcp")
            .join(".remote")
            .join("my-remote-plugin");
        std::fs::create_dir_all(&plugin_dir).unwrap();
        let manifest_content = r#"{
            "name": "my-remote-plugin",
            "version": "1.0.0",
            "type": "mcp",
            "entrypoint": {
                "command": "python3",
                "args": ["server.py"]
            }
        }"#;
        std::fs::write(plugin_dir.join("plugin.json"), manifest_content).unwrap();

        let plugins = discover_plugins(data_dir.path().to_str().unwrap());
        assert!(!plugins.is_empty());
        let found = plugins
            .iter()
            .any(|(m, s, _)| m.name == "my-remote-plugin" && s == "remote");
        assert!(
            found,
            "Expected my-remote-plugin to be discovered as remote"
        );
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
    fn test_uninstall_remote_not_found() {
        let data_dir = tempfile::tempdir().unwrap();
        let result = uninstall(
            "nonexistent-plugin",
            data_dir.path().to_str().unwrap(),
            "mcp",
            true,
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_uninstall_non_remote_is_noop() {
        let data_dir = tempfile::tempdir().unwrap();
        let result = uninstall(
            "some-plugin",
            data_dir.path().to_str().unwrap(),
            "mcp",
            false,
        );
        assert!(result.is_ok()); // non-remote uninstall is a no-op
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
