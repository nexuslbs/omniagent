//! MCP server: hindsight memory recall, retain, and reflect.
//!
//! Provides three tools:
//! - `hindsight_recall`: search memories via the hindsight service
//! - `hindsight_retain`: store a memory
//! - `hindsight_reflect`: ask hindsight to synthesize an answer
//!
//! Configuration is received from the omniagent via the `configure` message
//! at startup. Plugins never read env vars for config. Users can use $env:
//! notation in plugins.yaml if they want values from env vars.

use anyhow::{Context, Result};
use mcp_server_util::*;
use serde_json::Value;
use std::sync::Arc;
use tokio::sync::RwLock;

// ---------------------------------------------------------------------------
// Plugin config — received via configure message, never from env vars
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct PluginConfig {
    url: String,
    bank_id: String,
    limit: u32,
    budget: String,
    tags: String,
    tags_match: String,
    types: String,
    timeout_secs: u64,
}

impl PluginConfig {
    fn default() -> Self {
        Self {
            url: "http://hindsight:8888".to_string(),
            bank_id: "omniagent".to_string(),
            limit: 5,
            budget: "low".to_string(),
            tags: "from_user".to_string(),
            tags_match: "any".to_string(),
            types: "world".to_string(),
            timeout_secs: 15,
        }
    }

    fn from_json(json: &Value) -> Self {
        let mut cfg = Self::default();
        if let Some(obj) = json.as_object() {
            if let Some(v) = obj.get("hindsight_url").and_then(|v| v.as_str()) {
                cfg.url = v.to_string();
            }
            if let Some(v) = obj.get("hindsight_bank").and_then(|v| v.as_str()) {
                cfg.bank_id = v.to_string();
            }
            if let Some(v) = obj.get("hindsight_limit").and_then(|v| v.as_u64()) {
                cfg.limit = v as u32;
            }
            if let Some(v) = obj.get("hindsight_budget").and_then(|v| v.as_str()) {
                cfg.budget = v.to_string();
            }
            if let Some(v) = obj.get("hindsight_tags").and_then(|v| v.as_str()) {
                cfg.tags = v.to_string();
            }
            if let Some(v) = obj.get("hindsight_tags_match").and_then(|v| v.as_str()) {
                cfg.tags_match = v.to_string();
            }
            if let Some(v) = obj.get("hindsight_types").and_then(|v| v.as_str()) {
                cfg.types = v.to_string();
            }
            if let Some(v) = obj.get("hindsight_timeout").and_then(|v| v.as_u64()) {
                cfg.timeout_secs = v;
            }
        }
        cfg
    }
}

// ── Helpers ──

fn recall_url(cfg: &PluginConfig) -> String {
    format!(
        "{}/v1/default/banks/{}/memories/recall",
        cfg.url.trim_end_matches('/'),
        cfg.bank_id
    )
}

fn retain_url(cfg: &PluginConfig) -> String {
    format!(
        "{}/v1/default/banks/{}/memories",
        cfg.url.trim_end_matches('/'),
        cfg.bank_id
    )
}

fn reflect_url(cfg: &PluginConfig) -> String {
    format!(
        "{}/v1/default/banks/{}/reflect",
        cfg.url.trim_end_matches('/'),
        cfg.bank_id
    )
}

fn parse_comma_separated(s: &str) -> Option<Vec<String>> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return None;
    }
    let items: Vec<String> = trimmed
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if items.is_empty() {
        None
    } else {
        Some(items)
    }
}

fn http_client(cfg: &PluginConfig) -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(cfg.timeout_secs))
        .build()
        .context("Failed to build HTTP client")
}

// ── Tool handlers ──

async fn handle_recall(args: Value, cfg: &PluginConfig) -> Result<(String, bool)> {
    let query = args["query"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing required argument: 'query'"))?;

    let mut payload = serde_json::json!({
        "query": query,
    });

    // Per-call override or config default
    payload["limit"] = args["limit"]
        .as_u64()
        .map(|v| serde_json::json!(v))
        .unwrap_or_else(|| serde_json::json!(cfg.limit));
    payload["budget"] = args["budget"]
        .as_str()
        .map(|s| serde_json::json!(s))
        .unwrap_or_else(|| serde_json::json!(cfg.budget));

    // Tags: parse from args["tags"] if present, else from config
    let tags = args["tags"]
        .as_str()
        .and_then(|s| parse_comma_separated(s))
        .or_else(|| parse_comma_separated(&cfg.tags));
    if let Some(ref t) = tags {
        payload["tags"] = serde_json::json!(t);
        let tags_match = args["tags_match"].as_str().unwrap_or(&cfg.tags_match);
        payload["tags_match"] = serde_json::json!(tags_match);
    }

    // Types: parse from args["types"] if present, else from config
    let types = args["types"]
        .as_str()
        .and_then(|s| parse_comma_separated(s))
        .or_else(|| parse_comma_separated(&cfg.types));
    if let Some(ref t) = types {
        payload["types"] = serde_json::json!(t);
    }

    let client = match http_client(cfg) {
        Ok(c) => c,
        Err(e) => return Ok((format!("Failed to build HTTP client: {}", e), true)),
    };

    match client.post(&recall_url(cfg)).json(&payload).send().await {
        Ok(resp) if resp.status().is_success() => match resp.json::<Value>().await {
            Ok(data) => {
                let memories = data["results"].as_array().cloned().unwrap_or_default();
                if memories.is_empty() {
                    Ok(("No relevant memories found.".to_string(), false))
                } else {
                    let text: String = memories
                        .iter()
                        .filter_map(|m| {
                            let text = m["text"].as_str()?;
                            let tags = m["tags"]
                                .as_array()
                                .map(|a| {
                                    a.iter()
                                        .filter_map(|t| t.as_str())
                                        .collect::<Vec<_>>()
                                        .join(", ")
                                })
                                .unwrap_or_default();
                            Some(format!("[{}] {}", tags, text))
                        })
                        .collect::<Vec<_>>()
                        .join("\n---\n");
                    Ok((
                        format!(
                            "## Hindsight Memories ({} results):\n\n{}",
                            memories.len(),
                            text
                        ),
                        false,
                    ))
                }
            }
            Err(e) => Ok((format!("Failed to parse hindsight response: {}", e), true)),
        },
        Ok(resp) => Ok((
            format!(
                "Hindsight returned HTTP {}: {}",
                resp.status(),
                resp.text().await.unwrap_or_default()
            ),
            true,
        )),
        Err(e) => Ok((format!("Hindsight request failed: {}", e), true)),
    }
}

async fn handle_retain(args: Value, cfg: &PluginConfig) -> Result<(String, bool)> {
    let content = args["content"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing required argument: 'content'"))?;
    let context = args["context"].as_str().unwrap_or("memory retention");
    let tags_str = args["tags"].as_str();
    let document_id = args["document_id"].as_str().unwrap_or("");

    let mut item = serde_json::json!({
        "content": content,
        "context": context,
        "strategy": "fast",
    });

    if let Some(t) = tags_str {
        if let Some(parsed) = parse_comma_separated(t) {
            item["tags"] = serde_json::json!(parsed);
        }
    }

    if !document_id.is_empty() {
        item["document_id"] = serde_json::json!(document_id);
    }

    let payload = serde_json::json!({
        "items": [item],
        "async": false,
    });

    let client = match http_client(cfg) {
        Ok(c) => c,
        Err(e) => return Ok((format!("Failed to build HTTP client: {}", e), true)),
    };

    match client.post(&retain_url(cfg)).json(&payload).send().await {
        Ok(resp) if resp.status().is_success() => {
            Ok(("Memory retained successfully.".to_string(), false))
        }
        Ok(resp) => Ok((
            format!(
                "Retain returned HTTP {}: {}",
                resp.status(),
                resp.text().await.unwrap_or_default()
            ),
            true,
        )),
        Err(e) => Ok((format!("Retain request failed: {}", e), true)),
    }
}

async fn handle_reflect(args: Value, cfg: &PluginConfig) -> Result<(String, bool)> {
    let query = args["query"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing required argument: 'query'"))?;

    let payload = serde_json::json!({
        "query": query,
        "budget": cfg.budget,
    });

    let client = match http_client(cfg) {
        Ok(c) => c,
        Err(e) => return Ok((format!("Failed to build HTTP client: {}", e), true)),
    };

    match client.post(&reflect_url(cfg)).json(&payload).send().await {
        Ok(resp) if resp.status().is_success() => match resp.json::<Value>().await {
            Ok(data) => {
                let text = data["text"].as_str().unwrap_or("No reflection");
                Ok((format!("## Hindsight Reflection:\n\n{}", text), false))
            }
            Err(e) => Ok((format!("Failed to parse reflect response: {}", e), true)),
        },
        Ok(resp) => Ok((
            format!(
                "Reflect returned HTTP {}: {}",
                resp.status(),
                resp.text().await.unwrap_or_default()
            ),
            true,
        )),
        Err(e) => Ok((format!("Reflect request failed: {}", e), true)),
    }
}

// ── Main ──

#[tokio::main]
async fn main() -> Result<()> {
    let plugin_config = Arc::new(RwLock::new(PluginConfig::default()));

    // Hindsight_recall handler
    let cfg_recall = plugin_config.clone();
    let recall_handler: ToolHandler = Box::new(move |args: Value, _meta: Option<McpMeta>| {
        let cfg = cfg_recall.clone();
        Box::pin(async move {
            let config = cfg.read().await;
            handle_recall(args, &config).await
        })
    });

    // Hindsight_retain handler
    let cfg_retain = plugin_config.clone();
    let retain_handler: ToolHandler = Box::new(move |args: Value, _meta: Option<McpMeta>| {
        let cfg = cfg_retain.clone();
        Box::pin(async move {
            let config = cfg.read().await;
            handle_retain(args, &config).await
        })
    });

    // Hindsight_reflect handler
    let cfg_reflect = plugin_config.clone();
    let reflect_handler: ToolHandler = Box::new(move |args: Value, _meta: Option<McpMeta>| {
        let cfg = cfg_reflect.clone();
        Box::pin(async move {
            let config = cfg.read().await;
            handle_reflect(args, &config).await
        })
    });

    let tools = vec![
        McpToolEntry {
            def: McpToolDef {
                name: "hindsight_recall".to_string(),
                description: "Search hindsight persistent memory for relevant past memories. Returns text passages ranked by relevance to the query.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "query": {
                            "type": "string",
                            "description": "The search query to find relevant memories"
                        },
                        "limit": {
                            "type": "integer",
                            "description": "Max results to return (default: from config)"
                        },
                        "budget": {
                            "type": "string",
                            "enum": ["low", "mid", "high"],
                            "description": "Recall budget: low is fastest, high is most thorough (default: from config)"
                        },
                        "tags": {
                            "type": "string",
                            "description": "Comma-separated tags to filter (e.g. 'from_user,message'). Default from config."
                        },
                        "tags_match": {
                            "type": "string",
                            "enum": ["any", "all", "any_strict", "all_strict"],
                            "description": "Tag matching mode (default: from config)"
                        },
                        "types": {
                            "type": "string",
                            "description": "Comma-separated fact types (e.g. 'world,observation'). Default from config."
                        }
                    },
                    "required": ["query"]
                }),
            },
            handler: recall_handler,
        },
        McpToolEntry {
            def: McpToolDef {
                name: "hindsight_retain".to_string(),
                description: "Store a memory in hindsight persistent memory. Use for important facts, decisions, and user preferences that should be remembered across sessions.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "content": {
                            "type": "string",
                            "description": "The memory content to store"
                        },
                        "context": {
                            "type": "string",
                            "description": "Context label for the memory (e.g. 'user preference', 'project decision')"
                        },
                        "tags": {
                            "type": "string",
                            "description": "Comma-separated tags (e.g. 'from_user,preference')"
                        },
                        "document_id": {
                            "type": "string",
                            "description": "Optional document ID for deduplication"
                        }
                    },
                    "required": ["content"]
                }),
            },
            handler: retain_handler,
        },
        McpToolEntry {
            def: McpToolDef {
                name: "hindsight_reflect".to_string(),
                description: "Ask hindsight to synthesize an answer by reasoning across all stored memories. Use when you need a synthesized answer rather than raw recall results.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "query": {
                            "type": "string",
                            "description": "The question or topic to reflect on"
                        }
                    },
                    "required": ["query"]
                }),
            },
            handler: reflect_handler,
        },
    ];

    let server_info = ServerInfo {
        name: "hindsight".to_string(),
        version: "0.1.0".to_string(),
    };

    // Use run_server_with_config so the omniagent can pass plugin config
    // via the configure message instead of env vars.
    let on_configure = {
        let cfg = plugin_config.clone();
        Some(move |params: Value| {
            let new_config = PluginConfig::from_json(&params);
            let mut locked = cfg.blocking_write();
            *locked = new_config;
            tracing::info!(
                "Hindsight plugin config updated via configure message: url={:?}, bank={}, limit={}, budget={}",
                locked.url, locked.bank_id, locked.limit, locked.budget
            );
        })
    };

    run_server_with_config(server_info, tools, on_configure).await
}
