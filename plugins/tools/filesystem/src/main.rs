//! mcp-server-filesystem: standalone MCP server for local file operations.
//! Communicates via stdio JSON-RPC (MCP protocol).
//!
//! Tools: filesystem_read, filesystem_write, filesystem_list, filesystem_search, filesystem_info
//!
//! SANDBOX: only WRITE operations are confined to the configured
//! `workspace_dir` (default `/opt/workspace`) and its subdirectories.
//! Reads, lists, searches, and metadata lookups are allowed anywhere —
//! reading is side-effect free, and the agent legitimately needs to inspect
//! files outside the workspace (configs, wiki, credentials paths, ...).

use anyhow::Result;
use chrono::{DateTime, Utc};
use mcp_server_util::*;
use parking_lot::Mutex;
use serde_json::Value;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path};
use std::sync::Arc;

const DEFAULT_WORKSPACE_DIR: &str = "/opt/workspace";

/// Resolve the sandbox workspace dir: configured value or `/opt/workspace`.
fn resolve_workspace_dir(cfg_ws: &str) -> String {
    if cfg_ws.is_empty() {
        DEFAULT_WORKSPACE_DIR.to_string()
    } else {
        cfg_ws.to_string()
    }
}

/// Normalize a path, resolving `.` / `..` lexically (no filesystem access).
fn normalize_path(p: &Path) -> std::path::PathBuf {
    let mut out = std::path::PathBuf::new();
    for comp in p.components() {
        match comp {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// WS-6: resolve the OMNI_DIR root (config value -> env OMNI_DIR -> /opt/omni).
fn resolve_omni_dir(cfg_omni: &str) -> String {
    if !cfg_omni.is_empty() {
        cfg_omni.to_string()
    } else {
        std::env::var("OMNI_DIR").unwrap_or_else(|_| "/opt/omni".to_string())
    }
}

/// WS-6: allowed write roots — the workspace dir (always) plus the OMNI_DIR
/// subdirs enabled by config. `write_omni_all` supersedes the three subdir
/// toggles.
fn allowed_write_roots(cfg: &Config) -> Vec<String> {
    let mut roots = vec![resolve_workspace_dir(&cfg.workspace_dir)];
    let omni = resolve_omni_dir(&cfg.omni_dir);
    if cfg.write_omni_all {
        roots.push(omni);
    } else {
        if cfg.write_profiles {
            roots.push(format!("{omni}/profiles"));
        }
        if cfg.write_data {
            roots.push(format!("{omni}/data"));
        }
        if cfg.write_plugins {
            roots.push(format!("{omni}/plugins"));
        }
    }
    roots
}

/// WS-6: replaces the single-root `restrict_to_workspace` for writes. A write
/// path must normalize INSIDE at least one allowed root (workspace dir always;
/// OMNI_DIR or its profiles/data/plugins subdirs per config). Relative paths
/// resolve against the workspace root; `..` traversal that escapes every root
/// is rejected.
fn restrict_write_path(path: &str, cfg: &Config) -> Result<String, String> {
    if path.trim().is_empty() {
        return Err("path must not be empty".to_string());
    }
    let ws = resolve_workspace_dir(&cfg.workspace_dir);
    let candidate = if path.trim_start().starts_with('/') {
        path.to_string()
    } else {
        format!(
            "{}/{}",
            ws.trim_end_matches('/'),
            path.trim_start_matches('/')
        )
    };
    let normalized = normalize_path(std::path::Path::new(&candidate));
    let roots = allowed_write_roots(cfg);
    let allowed = roots.iter().any(|root| {
        let root = normalize_path(std::path::Path::new(root));
        normalized == root
            || normalized
                .starts_with(std::path::Path::new(&format!("{}/", root.display())))
    });
    if allowed {
        Ok(normalized.to_string_lossy().to_string())
    } else {
        Err(format!(
            "path outside allowed write roots; allowed roots: {} (got {})",
            roots.join(", "),
            normalized.display()
        ))
    }
}

/// Resolve a READ path: reads are unrestricted (anywhere on the filesystem),
/// so this only normalizes the path. Relative paths still resolve against the
/// workspace root for convenience, but absolute paths may point anywhere —
/// reading is side-effect free.
fn resolve_read_path(path: &str, workspace_dir: &str) -> String {
    let requested = Path::new(path);
    if requested.is_absolute() {
        normalize_path(requested).to_string_lossy().to_string()
    } else {
        normalize_path(&Path::new(workspace_dir).join(requested))
            .to_string_lossy()
            .to_string()
    }
}

/// Wrap a handler so any Err(e) becomes Ok((error_msg, true)).
/// This prevents access-denied and file-not-found errors from
/// triggering the circuit breaker on the MCP client side.
fn soft_error<F>(handler: F) -> ToolHandler
where
    F: Fn(Value) -> Result<(String, bool)> + Clone + Send + Sync + 'static,
{
    Box::new(move |args: Value, _meta: Option<McpMeta>| {
        let h = handler.clone();
        Box::pin(async move {
            match h(args) {
                Ok((text, is_error)) => Ok((text, is_error)),
                Err(e) => Ok((format!("{}", e), true)),
            }
        })
    })
}

fn format_size(size: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * 1024;
    if size < KB {
        format!("{} bytes", size)
    } else if size < MB {
        format!("{:.1} KB", size as f64 / KB as f64)
    } else {
        format!("{:.1} MB", size as f64 / MB as f64)
    }
}

// ---------------------------------------------------------------------------
// Tool: filesystem_read
// ---------------------------------------------------------------------------

fn handle_read(args: Value, workspace_dir: &str) -> Result<(String, bool)> {
    let path = args["path"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing 'path' argument"))?;
    // Reads are unrestricted — allowed anywhere on the filesystem.
    let safe_path = resolve_read_path(path, workspace_dir);
    let content = fs::read_to_string(&safe_path)
        .map_err(|e| anyhow::anyhow!("Failed to read file '{}': {}", safe_path, e))?;
    // Char-based paged reads: offset = starting char position (default 0),
    // limit = max chars returned (default 50_000, the legacy truncation).
    // The response reports the total file size and the returned slice so the
    // agent can page through a large file deterministically.
    let total_chars = content.chars().count();
    let offset = args["offset"].as_u64().unwrap_or(0) as usize;
    let limit = args["limit"].as_u64().unwrap_or(50_000) as usize;
    let start = offset.min(total_chars);
    let end = start.saturating_add(limit).min(total_chars);
    let slice: String = content.chars().skip(start).take(end - start).collect();
    let mut out = slice;
    if start > 0 || end < total_chars {
        let note = if end < total_chars {
            format!(
                "[... truncated: showing chars {}-{} of {} total chars]",
                start, end, total_chars
            )
        } else {
            format!(
                "[showing chars {}-{} of {} total chars]",
                start, end, total_chars
            )
        };
        out.push_str("\n\n");
        out.push_str(&note);
    }
    Ok((out, false))
}

// ---------------------------------------------------------------------------
// Tool: filesystem_write
// ---------------------------------------------------------------------------

fn handle_write(args: Value, cfg: &Config) -> Result<(String, bool)> {
    let path = args["path"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing 'path' argument"))?;
    let content = args["content"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing 'content' argument"))?;
    let append = args["append"].as_bool().unwrap_or(false);

    // Validate path is within the workspace sandbox (lexical — works for
    // files that don't exist yet).
    let safe_path_str = restrict_write_path(path, cfg).map_err(|e| anyhow::anyhow!("{e}"))?;
    let safe_path = Path::new(&safe_path_str);

    if let Some(parent) = safe_path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| anyhow::anyhow!("Failed to create parent directories: {}", e))?;
    }
    if append {
        use std::io::Write;
        let mut f = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(safe_path)
            .map_err(|e| {
                anyhow::anyhow!("Failed to open file '{}' for append: {}", safe_path_str, e)
            })?;
        f.write_all(content.as_bytes())
            .map_err(|e| anyhow::anyhow!("Failed to append to file '{}': {}", safe_path_str, e))?;
        Ok((
            format!(
                "Successfully appended {} bytes to {}",
                content.len(),
                safe_path_str
            ),
            false,
        ))
    } else {
        fs::write(safe_path, content)
            .map_err(|e| anyhow::anyhow!("Failed to write file '{}': {}", safe_path_str, e))?;
        Ok((
            format!(
                "Successfully wrote {} bytes to {}",
                content.len(),
                safe_path_str
            ),
            false,
        ))
    }
}

// ---------------------------------------------------------------------------
// Tool: filesystem_list
// ---------------------------------------------------------------------------

fn handle_list(args: Value, workspace_dir: &str) -> Result<(String, bool)> {
    let path = args["path"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing 'path' argument"))?;
    // Listing is a read — allowed anywhere on the filesystem.
    let safe_path = resolve_read_path(path, workspace_dir);

    let entries = fs::read_dir(&safe_path)
        .map_err(|e| anyhow::anyhow!("Failed to list '{}': {}", safe_path, e))?;

    let mut results: Vec<String> = Vec::new();
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_string();
        let typ = if entry.file_type()?.is_dir() {
            "directory"
        } else {
            "file"
        };
        results.push(format!("[{}] {}", typ.to_uppercase(), name));
    }
    results.sort();

    let max_entries = 2000;
    let output = if results.len() > max_entries {
        let joined = results[..max_entries].join("\n");
        format!(
            "{}\n[... truncated from {} to ~{} entries]",
            joined,
            results.len(),
            max_entries
        )
    } else if results.is_empty() {
        "(empty directory)".to_string()
    } else {
        results.join("\n")
    };

    Ok((output, false))
}

// ---------------------------------------------------------------------------
// Tool: filesystem_search
// ---------------------------------------------------------------------------

fn handle_search(args: Value, workspace_dir: &str) -> Result<(String, bool)> {
    let pattern = args["pattern"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing 'pattern' argument"))?;
    // Default the search base to the workspace root, but searches may point
    // anywhere — searching is a read.
    let base_path = args["path"].as_str().unwrap_or(workspace_dir);
    let safe_base = resolve_read_path(base_path, workspace_dir);

    let glob_pattern = format!("{}/{}", safe_base.trim_end_matches('/'), pattern);
    let entries =
        glob::glob(&glob_pattern).map_err(|e| anyhow::anyhow!("Invalid glob pattern: {}", e))?;

    let mut results: Vec<String> = entries
        .filter_map(|e| e.ok())
        .map(|p| p.to_string_lossy().to_string())
        .collect();
    results.sort();

    let max_results = 1000;
    let output = if results.is_empty() {
        format!("No files matching '{}' in {}", pattern, safe_base)
    } else if results.len() > max_results {
        let joined = results[..max_results].join("\n");
        format!(
            "{}\n[... truncated from {} to ~{} results]",
            joined,
            results.len(),
            max_results
        )
    } else {
        results.join("\n")
    };

    Ok((output, false))
}

// ---------------------------------------------------------------------------
// Tool: filesystem_info
// ---------------------------------------------------------------------------

fn handle_info(args: Value, workspace_dir: &str) -> Result<(String, bool)> {
    let path = args["path"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing 'path' argument"))?;
    // Metadata lookup is a read — allowed anywhere on the filesystem.
    let safe_path = resolve_read_path(path, workspace_dir);

    let metadata = fs::metadata(&safe_path)
        .map_err(|e| anyhow::anyhow!("Failed to stat '{}': {}", safe_path, e))?;

    let modified = metadata
        .modified()
        .map(|t| {
            let dt: DateTime<Utc> = t.into();
            dt.to_rfc3339()
        })
        .unwrap_or_default();

    let created = metadata
        .created()
        .map(|t| {
            let dt: DateTime<Utc> = t.into();
            dt.to_rfc3339()
        })
        .unwrap_or_default();

    let typ = if metadata.is_dir() {
        "directory"
    } else {
        "file"
    };

    let output = format!(
        "Path: {}\nType: {}\nSize: {}\nPermissions: {:o}\nCreated: {}\nModified: {}",
        safe_path,
        typ,
        format_size(metadata.len()),
        metadata.permissions().mode() & 0o777,
        created,
        modified,
    );

    Ok((output, false))
}

// ---------------------------------------------------------------------------
// Plugin config — received via MCP configure message, not from env vars
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct Config {
    workspace_dir: String,
    omni_dir: String,
    write_profiles: bool,
    write_data: bool,
    write_plugins: bool,
    write_omni_all: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            workspace_dir: String::new(),
            omni_dir: String::new(),
            write_profiles: true,
            write_data: true,
            write_plugins: true,
            write_omni_all: false,
        }
    }
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<()> {
    let config: Arc<Mutex<Config>> = Arc::new(Mutex::new(Config::default()));

    // on_configure: called when omniagent sends the resolved plugin config
    let on_configure = {
        let config = config.clone();
        Some(move |params: Value| {
            let mut cfg = config.lock();
            if let Some(dir) = params.get("workspace_dir").and_then(|v| v.as_str()) {
                if !dir.is_empty() {
                    cfg.workspace_dir = dir.to_string();
                }
            }
            if let Some(dir) = params.get("omni_dir").and_then(|v| v.as_str()) {
                if !dir.is_empty() {
                    cfg.omni_dir = dir.to_string();
                }
            }
            if let Some(v) = params.get("write_profiles").and_then(|v| v.as_bool()) {
                cfg.write_profiles = v;
            }
            if let Some(v) = params.get("write_data").and_then(|v| v.as_bool()) {
                cfg.write_data = v;
            }
            if let Some(v) = params.get("write_plugins").and_then(|v| v.as_bool()) {
                cfg.write_plugins = v;
            }
            if let Some(v) = params.get("write_omni_all").and_then(|v| v.as_bool()) {
                cfg.write_omni_all = v;
            }
        })
    };

    // Resolve the effective workspace dir from the (possibly empty) config.
    let c1 = config.clone();
    let read_handler = soft_error(move |args: Value| {
        let cfg = c1.lock();
        let wd = resolve_workspace_dir(&cfg.workspace_dir);
        handle_read(args, &wd)
    });

    let c2 = config.clone();
    let write_handler = soft_error(move |args: Value| {
        let cfg = c2.lock();
        let write_cfg = Config {
            workspace_dir: cfg.workspace_dir.clone(),
            omni_dir: cfg.omni_dir.clone(),
            write_profiles: cfg.write_profiles,
            write_data: cfg.write_data,
            write_plugins: cfg.write_plugins,
            write_omni_all: cfg.write_omni_all,
        };
        handle_write(args, &write_cfg)
    });

    let c3 = config.clone();
    let list_handler = soft_error(move |args: Value| {
        let cfg = c3.lock();
        let wd = resolve_workspace_dir(&cfg.workspace_dir);
        handle_list(args, &wd)
    });

    let c4 = config.clone();
    let search_handler = soft_error(move |args: Value| {
        let cfg = c4.lock();
        let wd = resolve_workspace_dir(&cfg.workspace_dir);
        handle_search(args, &wd)
    });

    let c5 = config.clone();
    let info_handler = soft_error(move |args: Value| {
        let cfg = c5.lock();
        let wd = resolve_workspace_dir(&cfg.workspace_dir);
        handle_info(args, &wd)
    });

    let tools = vec![
        McpToolEntry {
            def: McpToolDef {
                name: "filesystem_read".to_string(),
                description:
                    "READ A LOCAL FILE from disk. Use this to read any file on the filesystem (markdown, text files, config files, code files, research documents). This is the ONLY tool for reading existing file content. Do NOT use search_messages for file reading. \
                    READS ARE UNRESTRICTED: any path on the filesystem can be read (only WRITES are confined to the workspace dir). \
                    LARGE FILES: reads are CHAR-BASED SLICES. 'offset' (default 0) is the starting char position; 'limit' (default 50000) is the max chars returned. The response always reports the total file size and which slice was returned, e.g. \"[showing chars 50000-100000 of 250000 total chars]\", so you can page deterministically: call again with offset=50000, then offset=100000, ... until the note no longer says 'truncated'. No args = legacy behavior (first 50000 chars, with a truncation note when the file is bigger)."
                        .to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Absolute path to the file to read"
                        },
                        "offset": {
                            "type": "integer",
                            "description": "Starting char position, 0-based (default 0). Use together with 'limit' to page through large files in slices."
                        },
                        "limit": {
                            "type": "integer",
                            "description": "Maximum chars to return (default 50000)."
                        }
                    },
                    "required": ["path"]
                }),
            },
            handler: read_handler,
        },
        McpToolEntry {
            def: McpToolDef {
                name: "filesystem_write".to_string(),
                description:
                    "WRITE/CREATE A LOCAL FILE on disk. Use this to save content to a new or existing file. Creates parent directories automatically. This is the ONLY tool for writing file content. \
                    For very large files that exceed your output token limit, split the content across multiple calls: first call with append=false, then subsequent calls with append=true to add the rest. \
                    SANDBOX: writes are allowed inside the workspace dir (/opt/workspace by default) AND inside OMNI_DIR subdirectories per plugin config: omni_dir/profiles (write_profiles), omni_dir/data (write_data), omni_dir/plugins (write_plugins); write_omni_all=true allows the entire OMNI_DIR. Writes anywhere else are rejected."
                        .to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Absolute path to the file to write"
                        },
                        "content": {
                            "type": "string",
                            "description": "The content to write to the file"
                        },
                        "append": {
                            "type": "boolean",
                            "description": "If true, append content to the end of the file instead of overwriting it (default: false). Use for writing very large files in chunks."
                        }
                    },
                    "required": ["path", "content"]
                }),
            },
            handler: write_handler,
        },
        McpToolEntry {
            def: McpToolDef {
                name: "filesystem_list".to_string(),
                description:
                    "LIST FILES AND DIRECTORIES at a given path. Use this to explore a directory and see what files exist before reading them. Returns names and types (file vs directory). \
                    LISTS ARE UNRESTRICTED: any path on the filesystem can be listed (only WRITES are confined to the workspace dir)."
                        .to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Absolute path to list"
                        }
                    },
                    "required": ["path"]
                }),
            },
            handler: list_handler,
        },
        McpToolEntry {
            def: McpToolDef {
                name: "filesystem_search".to_string(),
                description:
                    "SEARCH FOR FILES BY NAME matching a glob pattern (e.g. '*.md', '**/*.rs'). Searches recursively from the given path. Use this when you need to find files with specific names or extensions. \
                    SEARCHES ARE UNRESTRICTED: any base path on the filesystem can be searched (defaults to the workspace root; only WRITES are confined to the workspace dir)."
                        .to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Base directory to search from"
                        },
                        "pattern": {
                            "type": "string",
                            "description": "Glob pattern to match (e.g. '*.md', '**/*.rs')"
                        }
                    },
                    "required": ["path", "pattern"]
                }),
            },
            handler: search_handler,
        },
        McpToolEntry {
            def: McpToolDef {
                name: "filesystem_info".to_string(),
                description:
                    "GET FILE/DIRECTORY METADATA. Returns size, type (file or directory), modification time, and permissions. Use this to check if a path exists and get details about it before reading. \
                    INFO IS UNRESTRICTED: any path on the filesystem can be inspected (only WRITES are confined to the workspace dir)."
                        .to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Absolute path to the file or directory"
                        }
                    },
                    "required": ["path"]
                }),
            },
            handler: info_handler,
        },
    ];

    let server_info = ServerInfo {
        name: "mcp-server-filesystem".to_string(),
        version: "0.1.0".to_string(),
    };

    run_server_with_config(server_info, tools, on_configure).await
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_policy_workspace_always_allowed() {
        let cfg = Config {
            workspace_dir: "/opt/workspace".into(),
            ..Config::default()
        };
        assert!(restrict_write_path("/opt/workspace/a.txt", &cfg).is_ok());
        assert!(restrict_write_path("/opt/workspace", &cfg).is_ok());
        assert!(restrict_write_path("/opt/workspace/sub/dir/f.txt", &cfg).is_ok());
        // relative paths resolve against the workspace root
        assert!(restrict_write_path("a.txt", &cfg).is_ok());
        let err = restrict_write_path("/etc/passwd", &cfg).unwrap_err();
        assert!(err.contains("allowed write roots"), "err: {err}");
    }

    #[test]
    fn write_policy_rejects_traversal() {
        let cfg = Config {
            workspace_dir: "/opt/workspace".into(),
            ..Config::default()
        };
        assert!(restrict_write_path("/opt/workspace/../etc/passwd", &cfg).is_err());
        assert!(restrict_write_path("../../etc/passwd", &cfg).is_err());
        assert!(restrict_write_path("/opt/workspace/../../etc/passwd", &cfg).is_err());
    }

    #[test]
    fn write_policy_omni_subdir_toggles() {
        let cfg = Config {
            workspace_dir: "/opt/workspace".into(),
            omni_dir: "/opt/omni".into(),
            ..Config::default()
        };
        // defaults: all three subdir toggles on
        assert!(restrict_write_path("/opt/omni/data/threads/5/notes.md", &cfg).is_ok());
        assert!(restrict_write_path("/opt/omni/profiles/omni/wiki/a.md", &cfg).is_ok());
        assert!(restrict_write_path("/opt/omni/plugins/x/main.rs", &cfg).is_ok());
        // but the omni root itself is NOT allowed without write_omni_all
        assert!(restrict_write_path("/opt/omni/other.txt", &cfg).is_err());
        // write_data off
        let mut c2 = cfg.clone();
        c2.write_data = false;
        assert!(restrict_write_path("/opt/omni/data/x", &c2).is_err());
        assert!(restrict_write_path("/opt/omni/profiles/x", &c2).is_ok());
        assert!(restrict_write_path("/opt/omni/plugins/x", &c2).is_ok());
        // write_profiles off
        let mut c3 = cfg.clone();
        c3.write_profiles = false;
        assert!(restrict_write_path("/opt/omni/profiles/x", &c3).is_err());
        assert!(restrict_write_path("/opt/omni/data/x", &c3).is_ok());
        // write_plugins off
        let mut c4 = cfg.clone();
        c4.write_plugins = false;
        assert!(restrict_write_path("/opt/omni/plugins/x", &c4).is_err());
        assert!(restrict_write_path("/opt/omni/data/x", &c4).is_ok());
    }

    #[test]
    fn write_policy_omni_all_overrides_subdir_toggles() {
        let mut cfg = Config {
            workspace_dir: "/opt/workspace".into(),
            omni_dir: "/opt/omni".into(),
            ..Config::default()
        };
        cfg.write_omni_all = true;
        cfg.write_data = false;
        cfg.write_profiles = false;
        cfg.write_plugins = false;
        assert!(restrict_write_path("/opt/omni/anywhere.txt", &cfg).is_ok());
        assert!(restrict_write_path("/opt/omni/data/x", &cfg).is_ok());
        // .. traversal still rejected even with omni_all
        assert!(restrict_write_path("/opt/omni/../etc/passwd", &cfg).is_err());
        assert!(restrict_write_path("/etc/passwd", &cfg).is_err());
        // workspace always allowed
        assert!(restrict_write_path("/opt/workspace/f.rs", &cfg).is_ok());
    }

    #[test]
    fn write_policy_all_disabled_still_allows_workspace() {
        let mut cfg = Config {
            workspace_dir: "/opt/workspace".into(),
            omni_dir: "/opt/omni".into(),
            ..Config::default()
        };
        cfg.write_data = false;
        cfg.write_profiles = false;
        cfg.write_plugins = false;
        assert!(restrict_write_path("/opt/workspace/a", &cfg).is_ok());
        let err = restrict_write_path("/opt/omni/data/a", &cfg).unwrap_err();
        assert!(err.contains("/opt/workspace"), "error must list allowed roots: {err}");
    }

    #[test]
    fn write_outside_sandbox_rejected() {
        // The raw handler returns Err; soft_error at the MCP boundary converts
        // it to Ok((msg, true)) so the circuit breaker never trips.
        let err = handle_write(
            serde_json::json!({
                "path": "/opt/omni/evil.txt",
                "content": "boom",
            }),
            &Config {
                workspace_dir: "/opt/workspace".to_string(),
                ..Config::default()
            },
        )
        .expect_err("write outside sandbox must be rejected");
        assert!(err
            .to_string()
            .contains("outside allowed write roots"));
    }

    #[tokio::test]
    async fn write_outside_sandbox_soft_error_does_not_trip() {
        // Through soft_error the rejection arrives as Ok((msg, true)) — NOT a
        // handler Err — so the MCP circuit breaker stays closed.
        let (msg, is_error) = soft_error(|args: Value| handle_write(args, &Config { workspace_dir: "/opt/workspace".to_string(), ..Config::default() }))(
            serde_json::json!({
                "path": "/opt/omni/evil.txt",
                "content": "boom",
            }),
            None,
        )
        .await
        .expect("soft_error always returns Ok");
        assert!(is_error);
        assert!(msg.contains("outside allowed write roots"));
    }

    #[test]
    fn write_inside_sandbox_succeeds() {
        let dir = std::env::temp_dir().join("fs-sandbox-test");
        let _ = fs::remove_dir_all(&dir);
        let (msg, is_error) = handle_write(
            serde_json::json!({
                "path": dir.join("sub/deep/file.txt").to_string_lossy(),
                "content": "hello",
            }),
            &Config {
                workspace_dir: dir.to_string_lossy().to_string(),
                ..Config::default()
            },
        )
        .expect("write inside sandbox succeeds");
        assert!(!is_error, "msg: {}", msg);
        let content = fs::read_to_string(dir.join("sub/deep/file.txt")).unwrap();
        assert_eq!(content, "hello");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_outside_workspace_allowed() {
        // Reads are UNRESTRICTED — only writes are sandboxed. Reading
        // /etc/hostname (outside /opt/workspace) must succeed.
        let (msg, is_error) = handle_read(
            serde_json::json!({"path": "/etc/hostname"}),
            "/opt/workspace",
        )
        .expect("read outside workspace must succeed");
        assert!(!is_error, "msg: {}", msg);
        assert!(msg.contains("hostname") || !msg.trim().is_empty());
    }

    #[test]
    fn read_relative_path_resolves_to_workspace() {
        // Relative reads still resolve against the workspace root. Uses a
        // temp dir (like the other sandbox tests) so it also passes inside
        // the Docker build context, where /opt/workspace does not exist.
        let dir = std::env::temp_dir().join("fs-sandbox-rel-test");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("sub")).unwrap();
        fs::write(dir.join("sub/file.txt"), "hello").unwrap();
        let (msg, is_error) = handle_read(
            serde_json::json!({"path": "sub/file.txt"}),
            &dir.to_string_lossy(),
        )
        .expect("relative read must succeed");
        assert!(!is_error, "msg: {msg}");
        assert!(msg.contains("hello"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn list_and_info_outside_workspace_allowed() {
        // Listing /etc is a read — allowed.
        let (msg, is_error) = handle_list(serde_json::json!({"path": "/etc"}), "/opt/workspace")
            .expect("list outside workspace must succeed");
        assert!(!is_error, "msg: {}", msg);
        // info on a file outside the workspace is allowed too.
        let (msg2, is_error2) = handle_info(
            serde_json::json!({"path": "/etc/hostname"}),
            "/opt/workspace",
        )
        .expect("info outside workspace must succeed");
        assert!(!is_error2, "msg: {}", msg2);
        assert!(msg2.contains("Type: file"));
    }

    #[test]
    fn search_outside_workspace_allowed() {
        // Searching /usr/share is a read — allowed.
        let (msg, is_error) = handle_search(
            serde_json::json!({"path": "/usr/share", "pattern": "*.md"}),
            "/opt/workspace",
        )
        .expect("search outside workspace must succeed");
        assert!(!is_error, "msg: {}", msg);
    }
}
