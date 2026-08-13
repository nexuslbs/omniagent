//! mcp-server-search: standalone MCP server for searching messages and wiki content.
//! Communicates via stdio JSON-RPC (MCP protocol).
//!
//! Tools: search_messages, search_wiki

use anyhow::Result;
use mcp_server_util::*;
use parking_lot::Mutex;
use serde_json::Value;
use sql_forge::sql_forge;
use sqlx::{FromRow, PgPool};
use std::sync::Arc;
use tokio::sync::RwLock;

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
// Plugin config — received via MCP configure message, not from env vars
// ---------------------------------------------------------------------------

#[derive(Default, Clone)]
struct Config {
    database_url: String,
    omni_dir: String,
}

// ---------------------------------------------------------------------------
// Tool: search_messages
// ---------------------------------------------------------------------------

async fn handle_search_messages(pool: &PgPool, args: &Value) -> Result<(String, bool)> {
    let query = args["query"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing required argument: 'query'"))?;
    let limit = args["limit"].as_i64().unwrap_or(10).min(50);
    let channel_id = args["channel_id"].as_str().map(|s| s.to_string());

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
            ( :channel_id = cid.as_str(), :query = &query_owned, :limit = limit )
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

fn handle_search_wiki(args: &Value, omni_dir: &str, profile_name: &str) -> Result<(String, bool)> {
    let query = args["query"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing required argument: 'query'"))?;
    let limit = args["limit"].as_i64().unwrap_or(10).min(30) as usize;
    // Profile comes from the AGENT's runtime context (_meta.profile_name,
    // injected by the MCP client on every tool call) — NOT from a tool
    // argument. The agent's profile is e.g. "omni"; a hardcoded "default"
    // made every no-profile search_wiki call return "Wiki directory not
    // found". Only fall back to the active profile when meta is absent
    // (e.g. manual testing outside the agent).
    let profile = if profile_name.trim().is_empty() {
        omniagent::profile::default_profile_name()
    } else {
        profile_name.trim().to_string()
    };

    let wiki_dir = format!("{}/profiles/{}/wiki", omni_dir, profile);
    let wiki_dir_path = std::path::Path::new(&wiki_dir);

    if !wiki_dir_path.exists() {
        return Ok((
            format!(
                "Wiki directory not found: {}. Is the profile correct? (active profile: {})",
                wiki_dir,
                omniagent::profile::default_profile_name()
            ),
            false,
        ));
    }

    let query_lower = query.to_lowercase();

    // Walk the wiki tree RECURSIVELY (top-level pages AND Reference/ etc.):
    // read_dir alone never descends into subdirectories, so the most
    // important pages (Reference/Container-Mount-Map.md, Budget-and-Context.md,
    // Deployment-Checklist.md, ...) were invisible to search_wiki.
    let mut results: Vec<(String, String)> = Vec::new();
    let mut stack: Vec<std::path::PathBuf> = vec![wiki_dir_path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            if results.len() >= limit {
                break;
            }
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
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
                        // Relative path from the wiki root, so nested pages are
                        // distinguishable (e.g. "Reference/Container-Mount-Map").
                        let rel = path.strip_prefix(wiki_dir_path).unwrap_or(&path);
                        let filename = rel.with_extension("").to_string_lossy().to_string();
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
// Main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<()> {
    // Plugin config — received via MCP configure message
    let config: Arc<Mutex<Config>> = Arc::new(Mutex::new(Config::default()));

    // Shared database pool — populated by configure callback before any tool call
    let pool: Arc<RwLock<Option<PgPool>>> = Arc::new(RwLock::new(None));

    // on_configure: called when omniagent sends the resolved plugin config
    let on_configure = {
        let config = config.clone();
        let pool = pool.clone();
        Some(move |params: Value| {
            let mut cfg = config.lock();
            if let Some(url) = params.get("database_url").and_then(|v| v.as_str()) {
                if !url.is_empty() {
                    cfg.database_url = url.to_string();

                    // Also initialize the database pool
                    let url_clone = url.to_string();
                    tokio::task::block_in_place(|| {
                        let rt = tokio::runtime::Handle::current();
                        let new_pool = rt
                            .block_on(omniagent::db::connect(&url_clone))
                            .expect("Failed to connect to database");
                        *pool.blocking_write() = Some(new_pool);
                    });
                }
            }
            if let Some(dir) = params.get("omni_dir").and_then(|v| v.as_str()) {
                if !dir.is_empty() {
                    cfg.omni_dir = dir.to_string();
                }
            }
            tracing::info!("Search plugin configured");
        })
    };

    let default_omni_dir = "/opt/omni".to_string();

    let p_search = pool.clone();
    let search_handler: ToolHandler = Box::new(move |args: Value, _meta: Option<McpMeta>| {
        let p = p_search.clone();
        Box::pin(async move {
            let guard = p.read().await;
            let pool = guard
                .as_ref()
                .ok_or_else(|| {
                    anyhow::anyhow!("Database pool not initialized. Configure plugin first.")
                })?
                .clone();
            handle_search_messages(&pool, &args).await
        })
    });

    let c1 = config.clone();
    let d1 = default_omni_dir.clone();
    let wiki_handler: ToolHandler = Box::new(move |args: Value, meta: Option<McpMeta>| {
        let c = c1.clone();
        let d = d1.clone();
        // Agent's profile from _meta (injected by the MCP client) — same
        // pattern as the skills plugin. Never requires a profile argument.
        let profile = meta
            .as_ref()
            .and_then(|m| m.profile_name.clone())
            .unwrap_or_default();
        Box::pin(async move {
            let cfg = c.lock();
            let omni_dir = if cfg.omni_dir.is_empty() {
                &d
            } else {
                &cfg.omni_dir
            };
            handle_search_wiki(&args, omni_dir, &profile)
        })
    });

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
                description: "Search wiki pages for relevant documentation. Use this to find documentation, guides, and notes. Searches the ACTIVE PROFILE's wiki automatically (the profile is injected by the runtime, no profile argument needed). Does NOT search message history: use search_messages for that.".to_string(),
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

    run_server_with_config(server_info, tools, on_configure).await
}
