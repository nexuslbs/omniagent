//! mcp-server-search: standalone MCP server for searching wiki content.
//! Communicates via stdio JSON-RPC (MCP protocol).
//!
//! Tools: search_wiki only (search_messages is handled by omniagent built-in)

use anyhow::Result;
use mcp_server_util::*;
use serde_json::Value;
use std::sync::{Arc, Mutex};

// ---------------------------------------------------------------------------
// Plugin config — received via MCP configure message, not from env vars
// ---------------------------------------------------------------------------

#[derive(Default, Clone)]
struct Config {
    omni_dir: String,
}

// ---------------------------------------------------------------------------
// Tool: search_wiki
// ---------------------------------------------------------------------------

fn handle_search_wiki(args: &Value, omni_dir: &str) -> Result<(String, bool)> {
    let query = args["query"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing required argument: 'query'"))?;
    let limit = args["limit"].as_i64().unwrap_or(10).min(30) as usize;
    let profile = args["profile"].as_str().unwrap_or("default");

    let wiki_dir = format!("{}/profiles/{}/wiki", omni_dir, profile);
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
// Main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<()> {
    // Plugin config — received via MCP configure message
    let config: Arc<Mutex<Config>> = Arc::new(Mutex::new(Config::default()));

    // on_configure: called when omniagent sends the resolved plugin config
    let on_configure = {
        let config = config.clone();
        Some(move |params: Value| {
            if let Ok(mut cfg) = config.lock() {
                if let Some(dir) = params.get("omni_dir").and_then(|v| v.as_str()) {
                    if !dir.is_empty() {
                        cfg.omni_dir = dir.to_string();
                    }
                }
            }
            tracing::info!("Search plugin configured");
        })
    };

    let default_omni_dir = "/opt/omni".to_string();

    let c1 = config.clone();
    let d1 = default_omni_dir.clone();
    let wiki_handler: ToolHandler = Box::new(move |args: Value, _meta: Option<McpMeta>| {
        let c = c1.clone();
        let d = d1.clone();
        Box::pin(async move {
            let cfg = c.lock().unwrap_or_else(|e| e.into_inner());
            let omni_dir = if cfg.omni_dir.is_empty() { &d } else { &cfg.omni_dir };
            handle_search_wiki(&args, omni_dir)
        })
    });

    let tools = vec![
        McpToolEntry {
            def: McpToolDef {
                name: "search_wiki".to_string(),
                description: "Search wiki pages for relevant documentation. Use this to find documentation, guides, and notes.".to_string(),
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

    run_server_with_config(server_info, tools, on_configure).await
}
