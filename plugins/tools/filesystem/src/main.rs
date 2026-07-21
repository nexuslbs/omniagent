//! mcp-server-filesystem: standalone MCP server for local file operations.
//! Communicates via stdio JSON-RPC (MCP protocol).
//!
//! Tools: filesystem_read, filesystem_write, filesystem_list, filesystem_search, filesystem_info

use anyhow::Result;
use chrono::{DateTime, Utc};
use mcp_server_util::*;
use serde_json::Value;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

/// Resolve and validate a path is within allowed directories.
fn restrict_path(path: &str, data_dir: &str, workspace_dir: &str) -> Result<String> {
    let data_real = Path::new(data_dir).canonicalize()?;
    let ws_real = Path::new(workspace_dir).canonicalize()?;
    let requested = Path::new(path).canonicalize()?;
    if !requested.starts_with(&data_real) && !requested.starts_with(&ws_real) {
        anyhow::bail!("Access denied: path is outside the data or workspace directory");
    }
    Ok(requested.to_string_lossy().to_string())
}

/// Truncate content to max_chars with a note.
fn truncate_content(content: &str, max_chars: usize) -> String {
    if content.len() <= max_chars {
        return content.to_string();
    }
    format!(
        "{}\n\n[... truncated from {} to ~{} chars]",
        &content[..max_chars],
        content.len(),
        max_chars
    )
}

/// Return thresholds and labels for human-readable sizes.

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

fn handle_read(args: Value, data_dir: &str, workspace_dir: &str) -> Result<(String, bool)> {
    let path = args["path"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing 'path' argument"))?;
    let safe_path = restrict_path(path, data_dir, workspace_dir)?;
    let content = fs::read_to_string(&safe_path)
        .map_err(|e| anyhow::anyhow!("Failed to read file '{}': {}", safe_path, e))?;
    Ok((truncate_content(&content, 50_000), false))
}

// ---------------------------------------------------------------------------
// Tool: filesystem_write
// ---------------------------------------------------------------------------

fn handle_write(args: Value, data_dir: &str, workspace_dir: &str) -> Result<(String, bool)> {
    let path = args["path"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing 'path' argument"))?;
    let content = args["content"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing 'content' argument"))?;

    // Validate path is within allowed dirs
    let safe_path = Path::new(path);
    let safe_path_str = safe_path.to_string_lossy();
    let canonical = safe_path.canonicalize().unwrap_or_else(|_| safe_path.to_path_buf());
    let data_real = Path::new(data_dir).canonicalize().unwrap_or_else(|_| Path::new(data_dir).to_path_buf());
    let ws_real = Path::new(workspace_dir).canonicalize().unwrap_or_else(|_| Path::new(workspace_dir).to_path_buf());
    if !canonical.starts_with(&data_real) && !canonical.starts_with(&ws_real) {
        // For new files that don't exist yet, check prefix of the path string
        if !safe_path_str.starts_with(data_dir) && !safe_path_str.starts_with(workspace_dir) {
            anyhow::bail!("Access denied: path is outside the data or workspace directory");
        }
    }

    if let Some(parent) = safe_path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| anyhow::anyhow!("Failed to create parent directories: {}", e))?;
    }
    fs::write(safe_path, content)
        .map_err(|e| anyhow::anyhow!("Failed to write file '{}': {}", safe_path_str, e))?;
    Ok((format!("Successfully wrote {} bytes to {}", content.len(), safe_path_str), false))
}

// ---------------------------------------------------------------------------
// Tool: filesystem_list
// ---------------------------------------------------------------------------

fn handle_list(args: Value, data_dir: &str, workspace_dir: &str) -> Result<(String, bool)> {
    let path = args["path"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing 'path' argument"))?;
    let safe_path = restrict_path(path, data_dir, workspace_dir)?;

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
        format!("{}\n[... truncated from {} to ~{} entries]", joined, results.len(), max_entries)
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

fn handle_search(args: Value, data_dir: &str, workspace_dir: &str) -> Result<(String, bool)> {
    let pattern = args["pattern"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing 'pattern' argument"))?;
    let base_path = args["path"].as_str().unwrap_or(data_dir);
    let safe_base = restrict_path(base_path, data_dir, workspace_dir)?;

    let glob_pattern = format!("{}/{}", safe_base.trim_end_matches('/'), pattern);
    let entries = glob::glob(&glob_pattern)
        .map_err(|e| anyhow::anyhow!("Invalid glob pattern: {}", e))?;

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
        format!("{}\n[... truncated from {} to ~{} results]", joined, results.len(), max_results)
    } else {
        results.join("\n")
    };

    Ok((output, false))
}

// ---------------------------------------------------------------------------
// Tool: filesystem_info
// ---------------------------------------------------------------------------

fn handle_info(args: Value, data_dir: &str, workspace_dir: &str) -> Result<(String, bool)> {
    let path = args["path"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing 'path' argument"))?;
    let safe_path = restrict_path(path, data_dir, workspace_dir)?;

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

    let typ = if metadata.is_dir() { "directory" } else { "file" };

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
// Main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<()> {
    let data_dir = std::env::var("OMNI_DIR")
        .unwrap_or_else(|_| { eprintln!("FATAL: OMNI_DIR must be set"); std::process::exit(1); });
    let workspace_dir = std::env::var("WORKSPACE_DIR").unwrap_or_else(|_| "/opt/workspace".to_string());

    let d1 = data_dir.clone();
    let w1 = workspace_dir.clone();
    let read_handler = soft_error(move |args: Value| handle_read(args, &d1, &w1));

    let d2 = data_dir.clone();
    let w2 = workspace_dir.clone();
    let write_handler = soft_error(move |args: Value| handle_write(args, &d2, &w2));

    let d3 = data_dir.clone();
    let w3 = workspace_dir.clone();
    let list_handler = soft_error(move |args: Value| handle_list(args, &d3, &w3));

    let d4 = data_dir.clone();
    let w4 = workspace_dir.clone();
    let search_handler = soft_error(move |args: Value| handle_search(args, &d4, &w4));

    let d5 = data_dir;
    let w5 = workspace_dir;
    let info_handler = soft_error(move |args: Value| handle_info(args, &d5, &w5));

    let tools = vec![
        McpToolEntry {
            def: McpToolDef {
                name: "filesystem_read".to_string(),
                description:
                    "READ A LOCAL FILE from disk. Use this to read any file on the filesystem (markdown, text files, config files, code files, research documents). This is the ONLY tool for reading existing file content. Do NOT use search_messages for file reading."
                        .to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Absolute path to the file to read"
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
                    "WRITE/CREATE A LOCAL FILE on disk. Use this to save content to a new or existing file. Creates parent directories automatically. This is the ONLY tool for writing file content."
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
                    "LIST FILES AND DIRECTORIES at a given path. Use this to explore a directory and see what files exist before reading them. Returns names and types (file vs directory)."
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
                    "SEARCH FOR FILES BY NAME matching a glob pattern (e.g. '*.md', '**/*.rs'). Searches recursively from the given path. Use this when you need to find files with specific names or extensions."
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
                    "GET FILE/DIRECTORY METADATA. Returns size, type (file or directory), modification time, and permissions. Use this to check if a path exists and get details about it before reading."
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

    run_server(server_info, tools).await
}
