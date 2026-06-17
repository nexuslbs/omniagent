use crate::mcp::{truncate_content, AppContext, McpTool, McpToolResult, DEFAULT_MAX_TOOL_OUTPUT_CHARS};
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
        description: "READ A LOCAL FILE from disk. Use this to read any file on the filesystem (markdown, text files, config files, code files, research documents). This is the ONLY tool for reading existing file content. Do NOT use search_messages for file reading.".to_string(),
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
            let path = args["path"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("Missing 'path' argument"))?;
            let safe_path = restrict_path(path, &ctx)?;
            let content = fs::read_to_string(&safe_path)
                .map_err(|e| anyhow::anyhow!("Failed to read file '{}': {}", safe_path, e))?;
            Ok(McpToolResult {
                call_id: String::new(),
                content: truncate_content(&content, DEFAULT_MAX_TOOL_OUTPUT_CHARS),
                is_error: false,
            })
        }),
    }
}

pub fn write_tool() -> McpTool {
    McpTool {
        name: "filesystem_write".to_string(),
        description: "WRITE/CREATE A LOCAL FILE on disk. Use this to save content to a new or existing file. Creates parent directories automatically. This is the ONLY tool for writing file content.".to_string(),
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

            // For write, canonicalize the parent dir (file may not exist yet)
            let path_obj = Path::new(path);
            let parent = path_obj.parent().ok_or_else(|| anyhow::anyhow!("Invalid path: no parent directory"))?;
            let parent_canon = parent.canonicalize().map_err(|e| anyhow::anyhow!("Parent directory does not exist: {} ({})", parent.display(), e))?;

            // Check parent is within data directory
            let data_dir = Path::new(&ctx.data_dir).canonicalize()?;
            if !parent_canon.starts_with(&data_dir) {
                anyhow::bail!("Access denied: path is outside the data directory");
            }

            let safe_path = parent_canon.join(
                path_obj.file_name().ok_or_else(|| anyhow::anyhow!("Invalid path: no file name"))?
            );
            let safe_path_str = safe_path.to_string_lossy().to_string();
            if let Some(parent) = safe_path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(&safe_path, content)
                .map_err(|e| anyhow::anyhow!("Failed to write file '{}': {}", safe_path_str, e))?;
            Ok(McpToolResult {
                call_id: String::new(),
                content: format!("Successfully wrote {} bytes to {}", content.len(), safe_path_str),
                is_error: false,
            })
        }),
    }
}

pub fn list_tool() -> McpTool {
    McpTool {
        name: "filesystem_list".to_string(),
        description: "LIST FILES AND DIRECTORIES at a given path. Use this to explore a directory and see what files exist before reading them. Returns names and types (file vs directory).".to_string(),
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
            let path = args["path"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("Missing 'path' argument"))?;
            let safe_path = restrict_path(path, &ctx)?;
            let entries = fs::read_dir(&safe_path)
                .map_err(|e| anyhow::anyhow!("Failed to list '{}': {}", safe_path, e))?;
            let mut results = Vec::new();
            for entry in entries {
                let entry = entry?;
                let name = entry.file_name().to_string_lossy().to_string();
                let typ = if entry.file_type()?.is_dir() {
                    "directory"
                } else {
                    "file"
                };
                results.push(format!("{} [{}]", name, typ));
            }
            results.sort();
            let max_entries = 2000;
            let output = if results.len() > max_entries {
                let truncated: Vec<&str> = results.iter().take(max_entries).map(|s| s.as_str()).collect();
                format!(
                    "{}\n[... truncated from {} to ~{} entries]",
                    truncated.join("\n"),
                    results.len(),
                    max_entries
                )
            } else {
                results.join("\n")
            };
            Ok(McpToolResult {
                call_id: String::new(),
                content: output,
                is_error: false,
            })
        }),
    }
}

pub fn search_tool() -> McpTool {
    McpTool {
        name: "filesystem_search".to_string(),
        description: "SEARCH FOR FILES BY NAME matching a glob pattern (e.g. '*.md', '**/*.rs'). Searches recursively from the given path. Use this when you need to find files with specific names or extensions.".to_string(),
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
            let path = args["path"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("Missing 'path' argument"))?;
            let pattern = args["pattern"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("Missing 'pattern' argument"))?;
            let safe_path = restrict_path(path, &ctx)?;
            let glob_pattern = format!("{}/{}", safe_path.trim_end_matches('/'), pattern);
            let entries = glob::glob(&glob_pattern)
                .map_err(|e| anyhow::anyhow!("Invalid glob pattern: {}", e))?;
            let mut results: Vec<String> = entries
                .filter_map(|e| e.ok())
                .map(|p| p.to_string_lossy().to_string())
                .collect();
            results.sort();
            let max_results = 1000;
            let output = if results.is_empty() {
                "No matches found".to_string()
            } else if results.len() > max_results {
                let truncated: Vec<&str> = results.iter().take(max_results).map(|s| s.as_str()).collect();
                format!(
                    "{}\n[... truncated from {} to ~{} results]",
                    truncated.join("\n"),
                    results.len(),
                    max_results
                )
            } else {
                results.join("\n")
            };
            Ok(McpToolResult {
                call_id: String::new(),
                content: output,
                is_error: false,
            })
        }),
    }
}

pub fn info_tool() -> McpTool {
    McpTool {
        name: "filesystem_info".to_string(),
        description: "GET FILE/DIRECTORY METADATA. Returns size, type (file or directory), modification time, and permissions. Use this to check if a path exists and get details about it before reading.".to_string(),
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
            let path = args["path"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("Missing 'path' argument"))?;
            let safe_path = restrict_path(path, &ctx)?;
            let metadata = fs::metadata(&safe_path)
                .map_err(|e| anyhow::anyhow!("Failed to stat '{}': {}", safe_path, e))?;
            let modified = metadata
                .modified()
                .map(|t| {
                    let dur = t.duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
                    chrono::DateTime::from_timestamp(dur.as_secs() as i64, 0)
                        .map(|dt| dt.to_rfc3339())
                        .unwrap_or_default()
                })
                .unwrap_or_default();
            let typ = if metadata.is_dir() {
                "directory"
            } else {
                "file"
            };
            Ok(McpToolResult {
                call_id: String::new(),
                content: serde_json::json!({
                    "path": safe_path,
                    "type": typ,
                    "size": metadata.len(),
                    "modified": modified,
                    "permissions": format!("{:o}", metadata.permissions().mode() & 0o777),
                })
                .to_string(),
                is_error: false,
            })
        }),
    }
}
