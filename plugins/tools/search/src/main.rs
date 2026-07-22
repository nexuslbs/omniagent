//! mcp-server-search: standalone MCP server for searching messages and wiki content.
//! Communicates via stdio JSON-RPC (MCP protocol).
//!
//! Tools: search_messages, search_wiki

use anyhow::{Context, Result};
use mcp_server_util::*;
use serde_json::Value;
use sql_forge::sql_forge;
use sqlx::{FromRow, PgPool};
use std::sync::Arc;

// ---------------------------------------------------------------------------
// Shared row type
// ---------------------------------------------------------------------------

#[derive(Debug, FromRow)]
struct SearchResult {
    id: i64,
    role: String,
    content: String,
}

// ---------------------------------------------------------------------------
// Tool: search_messages
// ---------------------------------------------------------------------------

async fn handle_search_messages(pool: &PgPool, args: &Value) -> Result<(String, bool)> {
    let query = args["query"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing required argument: 'query'"))?;
    let limit = args["limit"].as_i64().unwrap_or(10).min(50);
    let channel_id = args["channel_id"].as_i64();

    let query_owned = query.to_string();
    let pool_ref = pool.clone();

    let results: Vec<SearchResult> = if let Some(cid) = channel_id {
        sql_forge!(
            SearchResult,
            r#"
            SELECT m.id, m.role, m.content FROM messages m
            JOIN threads t ON t.id = m.thread_id
            WHERE t.channel_id = :channel_id
              AND m.content ILIKE '%' || :query || '%'
            ORDER BY m.created_at DESC
            LIMIT :limit
            "#,
            ( :channel_id = cid, :query = &query_owned, :limit = limit )
        )
        .fetch_all(&pool_ref)
        .await
        .map_err(|e: sqlx::Error| anyhow::anyhow!("Database query failed: {e}"))?
    } else {
        sql_forge!(
            SearchResult,
            r#"
            SELECT id, role, content FROM messages
            WHERE content ILIKE '%' || :query || '%'
            ORDER BY created_at DESC
            LIMIT :limit
            "#,
            ( :query = &query_owned, :limit = limit )
        )
        .fetch_all(&pool_ref)
        .await
        .map_err(|e: sqlx::Error| anyhow::anyhow!("Database query failed: {e}"))?
    };

    if results.is_empty() {
        return Ok(("No matching messages found.".to_string(), false));
    }

    let mut lines = Vec::new();
    for r in &results {
        let preview = if r.content.len() > 200 {
            let truncate_to = r
                .content
                .char_indices()
                .nth(200)
                .map(|(i, _)| i)
                .unwrap_or(r.content.len());
            format!("{}...", &r.content[..truncate_to])
        } else {
            r.content.clone()
        };
        lines.push(format!("#{} [{}]: {}", r.id, r.role, preview));
    }

    let output = format!("Found {} result(s):\n{}", results.len(), lines.join("\n\n"));
    Ok((output, false))
}

// ---------------------------------------------------------------------------
// Tool: search_wiki
// ---------------------------------------------------------------------------

fn handle_search_wiki(args: &Value) -> Result<(String, bool)> {
    let query = args["query"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing required argument: 'query'"))?;
    let limit = args["limit"].as_i64().unwrap_or(10).min(30) as usize;
    let default_profile = omniagent::profile::default_profile_name();
    let profile = args["profile"].as_str().unwrap_or(&default_profile);

    let data_dir = std::env::var("OMNI_DIR")
        .or_else(|_| std::env::var("HOME").map(|h| format!("{}/.omniagent", h)))
        .expect("OMNI_DIR must be set");

    let wiki_dir = format!("{}/profiles/{}/wiki", data_dir, profile);
    let wiki_dir_path = std::path::Path::new(&wiki_dir);

    if !wiki_dir_path.exists() {
        return Ok((
            format!(
                "Wiki directory not found: {}. Is the profile correct?",
                wiki_dir
            ),
            false,
        ));
    }

    let mut results: Vec<(String, String)> = Vec::new();
    let query_lower = query.to_lowercase();

    if let Ok(entries) = std::fs::read_dir(wiki_dir_path) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().map(|e| e == "md").unwrap_or(false) {
                if let Ok(content) = std::fs::read_to_string(&path) {
                    let lines: Vec<&str> = content.lines().collect();
                    let title_line = lines.first().unwrap_or(&"");
                    let title = title_line.trim_start_matches("# ").trim();
                    let preview_lines: Vec<&str> = lines
                        .iter()
                        .filter(|l| l.to_lowercase().contains(&query_lower))
                        .take(3)
                        .map(|l| l.trim())
                        .collect();
                    if !preview_lines.is_empty() || title.to_lowercase().contains(&query_lower) {
                        let filename = path
                            .file_stem()
                            .and_then(|s| s.to_str())
                            .unwrap_or("")
                            .to_string();
                        let preview = if preview_lines.is_empty() {
                            "".to_string()
                        } else {
                            let truncated: Vec<&str> = preview_lines
                                .iter()
                                .map(|l| {
                                    if l.len() > 100 {
                                        let trunc_to = l
                                            .char_indices()
                                            .nth(100)
                                            .map(|(i, _)| i)
                                            .unwrap_or(l.len());
                                        &l[..trunc_to]
                                    } else {
                                        *l
                                    }
                                })
                                .collect();
                            format!("...{}...", truncated.join(" ... "))
                        };
                        results.push((filename, preview));
                    }
                }
            }

            if results.len() >= limit {
                break;
            }
        }
    }

    if results.is_empty() {
        return Ok(("No matching wiki results found.".to_string(), false));
    }

    let output = results
        .iter()
        .map(|(name, preview)| {
            if preview.is_empty() {
                format!("[[{}]]", name)
            } else {
                format!("[[{}]]: {}", name, preview)
            }
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    Ok((output, false))
}

// ---------------------------------------------------------------------------
// Plugin config hook
// ---------------------------------------------------------------------------

/// Callback invoked when the host sends configuration via configure message.
fn on_configure(params: serde_json::Value) {
    tracing::info!("Search plugin configured");
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<()> {
    let database_url = std::env::var("SEARCH_DATABASE_URL")
        .or_else(|_| std::env::var("DATABASE_URL"))
        .context("SEARCH_DATABASE_URL or DATABASE_URL must be set")?;
    let pool = omniagent::db::connect(&database_url)
        .await
        .context("Failed to connect to database")?;
    let pool = Arc::new(pool);

    let p_search = pool.clone();

    let search_handler: ToolHandler = Box::new(move |args: Value, _meta: Option<McpMeta>| {
        let p = p_search.clone();
        Box::pin(async move { handle_search_messages(&p, &args).await })
    });

    let wiki_handler: ToolHandler =
        Box::new(move |args: Value, _meta: Option<McpMeta>| Box::pin(async move { handle_search_wiki(&args) }));

    let tools = vec![
        McpToolEntry {
            def: McpToolDef {
                name: "search_messages".to_string(),
                description: "Search message history across all channels. Use this tool when the LLM needs to find information from past conversations. Use specific keywords and narrow the scope with channel_id when possible. Does NOT search wiki pages: use search_wiki for that.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "query": {
                            "type": "string",
                            "description": "Search query to find in messages"
                        },
                        "limit": {
                            "type": "integer",
                            "description": "Max results (max 50)",
                            "default": 10
                        },
                        "channel_id": {
                            "type": "integer",
                            "description": "Optional channel ID filter"
                        }
                    },
                    "required": ["query"]
                }),
            },
            handler: search_handler,
        },
        McpToolEntry {
            def: McpToolDef {
                name: "search_wiki".to_string(),
                description: "Search wiki pages for relevant documentation. Use this to find documentation, guides, and notes. Does NOT search message history: use search_messages for that.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "query": {
                            "type": "string",
                            "description": "Search query to find in wiki content and filenames"
                        },
                        "limit": {
                            "type": "integer",
                            "description": "Max results (max 30)",
                            "default": 10
                        },
                        "profile": {
                            "type": "string",
                            "description": "Profile name (default: default)"
                        }
                    },
                    "required": ["query"]
                }),
            },
            handler: wiki_handler,
        },
    ];

    let server_info = ServerInfo {
        name: "mcp-server-search".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
    };

    run_server_with_config(server_info, tools, Some(on_configure)).await
}
