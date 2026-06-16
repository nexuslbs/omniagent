use crate::mcp::{AppContext, McpTool, McpToolResult};
use anyhow::Result;
use serde_json::Value;
use std::sync::Arc;

pub fn search_messages_tool(ctx: &AppContext) -> McpTool {
    let pool = ctx.pool.clone();
    McpTool {
        name: "search_messages".to_string(),
        description: "Search messages in the database by text content. Returns matching message IDs, roles, and content previews.".to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Text to search for in message content"
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum number of results (default: 10, max: 50)",
                    "default": 10
                },
                "channel_id": {
                    "type": "integer",
                    "description": "Optional channel ID to restrict search to"
                }
            },
            "required": ["query"]
        }),
        handler: Arc::new(move |args: Value, _ctx: AppContext| -> Result<McpToolResult> {
            let query = args["query"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("Missing 'query' argument"))?;
            let limit = args["limit"]
                .as_i64()
                .unwrap_or(10)
                .min(50) as i64;
            let channel_id = args["channel_id"].as_i64();

            let pool = pool.clone();
            let rt = tokio::runtime::Runtime::new()
                .map_err(|e| anyhow::anyhow!("Failed to create runtime: {}", e))?;

            let results: Vec<(i64, String, String)> = rt.block_on(async {
                if let Some(cid) = channel_id {
                    sqlx::query_as(
                        "SELECT id, role, content FROM messages WHERE channel_id = $1 AND content ILIKE '%' || $2 || '%' ORDER BY created_at DESC LIMIT $3"
                    )
                    .bind(cid)
                    .bind(query)
                    .bind(limit)
                    .fetch_all(&pool)
                    .await
                } else {
                    sqlx::query_as(
                        "SELECT id, role, content FROM messages WHERE content ILIKE '%' || $1 || '%' ORDER BY created_at DESC LIMIT $2"
                    )
                    .bind(query)
                    .bind(limit)
                    .fetch_all(&pool)
                    .await
                }
            })?;

            if results.is_empty() {
                return Ok(McpToolResult {
                    call_id: String::new(),
                    content: "No matching messages found.".to_string(),
                    is_error: false,
                });
            }

            let mut lines = Vec::new();
            for (id, role, content) in &results {
                let preview = if content.len() > 200 {
                    format!("{}...", &content[..200])
                } else {
                    content.clone()
                };
                lines.push(format!("#{} [{}]: {}", id, role, preview));
            }

            Ok(McpToolResult {
                call_id: String::new(),
                content: format!("Found {} result(s):\n{}", results.len(), lines.join("\n\n")),
                is_error: false,
            })
        }),
    }
}

pub fn search_wiki_tool(ctx: &AppContext) -> McpTool {
    let _ = ctx; // keep for future use (Qdrant integration)
    McpTool {
        name: "search_wiki".to_string(),
        description: "Search the wiki by text in wiki files. Searches the profiles/<profile>/wiki/ directory for matching content.".to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Text to search for in wiki files"
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum number of results (default: 10, max: 30)",
                    "default": 10
                },
                "profile": {
                    "type": "string",
                    "description": "Profile name whose wiki to search (default: 'default')"
                }
            },
            "required": ["query"]
        }),
        handler: Arc::new(move |args: Value, ctx: AppContext| -> Result<McpToolResult> {
            let query = args["query"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("Missing 'query' argument"))?;
            let limit = args["limit"]
                .as_i64()
                .unwrap_or(10)
                .min(30) as usize;
            let profile = args["profile"]
                .as_str()
                .unwrap_or("default");

            let wiki_dir = format!("{}/profiles/{}/wiki", ctx.data_dir, profile);
            let wiki_path = std::path::Path::new(&wiki_dir);

            if !wiki_path.exists() {
                return Ok(McpToolResult {
                    call_id: String::new(),
                    content: format!("Wiki directory not found for profile '{}': {}", profile, wiki_dir),
                    is_error: false,
                });
            }

            let query_lower = query.to_lowercase();
            let mut results: Vec<String> = Vec::new();

            let entries = walkdir::WalkDir::new(wiki_path)
                .max_depth(5)
                .into_iter()
                .filter_map(|e| e.ok())
                .filter(|e| e.file_type().is_file());

            for entry in entries {
                    let path = entry.path().to_path_buf();
                    if let Ok(content) = std::fs::read_to_string(&path) {
                        let content_lower = content.to_lowercase();
                        if content_lower.contains(&query_lower) {
                            let matching_lines: Vec<&str> = content
                                .lines()
                                .filter(|line| line.to_lowercase().contains(&query_lower))
                                .take(3)
                                .collect();
                            let preview = if matching_lines.is_empty() {
                                content.lines().next().unwrap_or("").to_string()
                            } else {
                                matching_lines.join(" | ")
                            };
                            let rel_path = path.strip_prefix(wiki_path)
                                .map(|p| p.to_string_lossy().to_string())
                                .unwrap_or_default();
                            results.push(format!("{}: {}", rel_path, preview));
                        }
                    }
                    if results.len() >= limit {
                        break;
                    }
            }

            results.sort();
            results.truncate(limit);

            if results.is_empty() {
                return Ok(McpToolResult {
                    call_id: String::new(),
                    content: "No matching wiki content found.".to_string(),
                    is_error: false,
                });
            }

            Ok(McpToolResult {
                call_id: String::new(),
                content: format!("Found {} wiki result(s):\n{}", results.len(), results.join("\n\n")),
                is_error: false,
            })
        }),
    }
}
