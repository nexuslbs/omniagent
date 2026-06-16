use crate::mcp::{AppContext, McpTool, McpToolResult};
use anyhow::Result;
use serde_json::Value;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;

fn restrict_path(path: &str, ctx: &AppContext) -> Result<String> {
    let data_dir = Path::new(&ctx.data_dir).canonicalize()?;
    let requested = Path::new(path).canonicalize()?;
    if !requested.starts_with(&data_dir) {
        anyhow::bail!("Access denied: path is outside the data directory");
    }
    Ok(requested.to_string_lossy().to_string())
}

pub fn read_tool() -> McpTool {
    McpTool {
        name: "filesystem_read".to_string(),
        description: "Read the contents of a file. Returns the file content as text.".to_string(),
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
        handler: Arc::new(|args: Value, ctx: AppContext| -> Result<McpToolResult> {
            let path = args["path"].as_str().ok_or_else(|| anyhow::anyhow!("Missing 'path' argument"))?;
            let safe_path = restrict_path(path, &ctx)?;
            let content = fs::read_to_string(&safe_path)
                .map_err(|e| anyhow::anyhow!("Failed to read file '{}': {}", safe_path, e))?;
            Ok(McpToolResult {
                call_id: String::new(),
                content,
                is_error: false,
            })
        }),
    }
}

pub fn write_tool() -> McpTool {
    McpTool {
        name: "filesystem_write".to_string(),
        description: "Write content to a file. Creates parent directories if needed. Overwrites existing content.".to_string(),
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
        handler: Arc::new(|args: Value, ctx: AppContext| -> Result<McpToolResult> {
            let path = args["path"].as_str().ok_or_else(|| anyhow::anyhow!("Missing 'path' argument"))?;
            let content = args["content"].as_str().ok_or_else(|| anyhow::anyhow!("Missing 'content' argument"))?;
            let safe_path = restrict_path(path, &ctx)?;
            if let Some(parent) = Path::new(&safe_path).parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(&safe_path, content)
                .map_err(|e| anyhow::anyhow!("Failed to write file '{}': {}", safe_path, e))?;
            Ok(McpToolResult {
                call_id: String::new(),
                content: format!("Successfully wrote {} bytes to {}", content.len(), safe_path),
                is_error: false,
            })
        }),
    }
}

pub fn list_tool() -> McpTool {
    McpTool {
        name: "filesystem_list".to_string(),
        description: "List files and directories at a given path. Returns names and types.".to_string(),
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
        handler: Arc::new(|args: Value, ctx: AppContext| -> Result<McpToolResult> {
            let path = args["path"].as_str().ok_or_else(|| anyhow::anyhow!("Missing 'path' argument"))?;
            let safe_path = restrict_path(path, &ctx)?;
            let entries = fs::read_dir(&safe_path)
                .map_err(|e| anyhow::anyhow!("Failed to list '{}': {}", safe_path, e))?;
            let mut results = Vec::new();
            for entry in entries {
                let entry = entry?;
                let name = entry.file_name().to_string_lossy().to_string();
                let typ = if entry.file_type()?.is_dir() { "directory" } else { "file" };
                results.push(format!("{} [{}]", name, typ));
            }
            results.sort();
            Ok(McpToolResult {
                call_id: String::new(),
                content: results.join("\n"),
                is_error: false,
            })
        }),
    }
}

pub fn search_tool() -> McpTool {
    McpTool {
        name: "filesystem_search".to_string(),
        description: "Search for files matching a glob pattern. Searches recursively from the given path.".to_string(),
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
        handler: Arc::new(|args: Value, ctx: AppContext| -> Result<McpToolResult> {
            let path = args["path"].as_str().ok_or_else(|| anyhow::anyhow!("Missing 'path' argument"))?;
            let pattern = args["pattern"].as_str().ok_or_else(|| anyhow::anyhow!("Missing 'pattern' argument"))?;
            let safe_path = restrict_path(path, &ctx)?;
            let glob_pattern = format!("{}/{}", safe_path.trim_end_matches('/'), pattern);
            let entries = glob::glob(&glob_pattern)
                .map_err(|e| anyhow::anyhow!("Invalid glob pattern: {}", e))?;
            let mut results: Vec<String> = entries
                .filter_map(|e| e.ok())
                .map(|p| p.to_string_lossy().to_string())
                .collect();
            results.sort();
            Ok(McpToolResult {
                call_id: String::new(),
                content: if results.is_empty() {
                    "No matches found".to_string()
                } else {
                    results.join("\n")
                },
                is_error: false,
            })
        }),
    }
}

pub fn info_tool() -> McpTool {
    McpTool {
        name: "filesystem_info".to_string(),
        description: "Get metadata about a file or directory (size, modified time, type).".to_string(),
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
        handler: Arc::new(|args: Value, ctx: AppContext| -> Result<McpToolResult> {
            let path = args["path"].as_str().ok_or_else(|| anyhow::anyhow!("Missing 'path' argument"))?;
            let safe_path = restrict_path(path, &ctx)?;
            let metadata = fs::metadata(&safe_path)
                .map_err(|e| anyhow::anyhow!("Failed to stat '{}': {}", safe_path, e))?;
            let modified = metadata.modified()
                .map(|t| {
                    let dur = t.duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
                    chrono::DateTime::from_timestamp(dur.as_secs() as i64, 0)
                        .map(|dt| dt.to_rfc3339())
                        .unwrap_or_default()
                })
                .unwrap_or_default();
            let typ = if metadata.is_dir() { "directory" } else { "file" };
            Ok(McpToolResult {
                call_id: String::new(),
                content: serde_json::json!({
                    "path": safe_path,
                    "type": typ,
                    "size": metadata.len(),
                    "modified": modified,
                    "permissions": format!("{:o}", metadata.permissions().mode() & 0o777),
                }).to_string(),
                is_error: false,
            })
        }),
    }
}
