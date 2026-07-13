//! MCP server: hindsight memory recall, retain, and reflect.
//!
//! Provides three tools:
//! - `hindsight_recall` - search memories via the hindsight service
//! - `hindsight_retain` - store a memory
//! - `hindsight_reflect` - ask hindsight to synthesize an answer
//!
//! Configuration is read from environment variables with sensible defaults:
//! - HINDSIGHT_URL: service URL (default: http://hindsight:8888)
//! - HINDSIGHT_BANK: bank ID (default: omniagent)
//! - HINDSIGHT_LIMIT: max recall results (default: 5)
//! - HINDSIGHT_BUDGET: recall budget (default: low)
//! - HINDSIGHT_TAGS: comma-separated tags to filter (default: from_user)
//! - HINDSIGHT_TAGS_MATCH: tag matching mode (default: any)
//! - HINDSIGHT_TYPES: comma-separated fact types (default: world)
//! - HINDSIGHT_TIMEOUT: HTTP timeout in seconds (default: 15)

use anyhow::{Context, Result};
use mcp_server_util::*;
use serde_json::Value;
use std::sync::OnceLock;

// ── Config (read from env, cached once) ──

struct HindsightConfig {
    url: String,
    bank_id: String,
    limit: u32,
    budget: String,
    tags: String,
    tags_match: String,
    types: String,
    timeout_secs: u64,
}

static CONFIG: OnceLock<HindsightConfig> = OnceLock::new();

fn config() -> &'static HindsightConfig {
    CONFIG.get_or_init(|| HindsightConfig {
        url: std::env::var("HINDSIGHT_URL").unwrap_or_else(|_| "http://hindsight:8888".to_string()),
        bank_id: std::env::var("HINDSIGHT_BANK").unwrap_or_else(|_| "omniagent".to_string()),
        limit: std::env::var("HINDSIGHT_LIMIT")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(5),
        budget: std::env::var("HINDSIGHT_BUDGET").unwrap_or_else(|_| "low".to_string()),
        tags: std::env::var("HINDSIGHT_TAGS").unwrap_or_else(|_| "from_user".to_string()),
        tags_match: std::env::var("HINDSIGHT_TAGS_MATCH").unwrap_or_else(|_| "any".to_string()),
        types: std::env::var("HINDSIGHT_TYPES").unwrap_or_else(|_| "world".to_string()),
        timeout_secs: std::env::var("HINDSIGHT_TIMEOUT")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(15),
    })
}

// ── Helpers ──

fn recall_url() -> String {
    let c = config();
    format!(
        "{}/v1/default/banks/{}/memories/recall",
        c.url.trim_end_matches('/'),
        c.bank_id
    )
}

fn retain_url() -> String {
    let c = config();
    format!(
        "{}/v1/default/banks/{}/memories",
        c.url.trim_end_matches('/'),
        c.bank_id
    )
}

fn reflect_url() -> String {
    let c = config();
    format!(
        "{}/v1/default/banks/{}/reflect",
        c.url.trim_end_matches('/'),
        c.bank_id
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

/// Build an HTTP client with the configured timeout.
fn http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(config().timeout_secs))
        .build()
        .context("Failed to build HTTP client")
}

// ── Tool handlers ──

/// Handle hindsight_recall - search memories.
async fn handle_recall(args: Value) -> Result<(String, bool)> {
    let c = config();
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
        .unwrap_or_else(|| serde_json::json!(c.limit));
    payload["budget"] = args["budget"]
        .as_str()
        .map(|s| serde_json::json!(s))
        .unwrap_or_else(|| serde_json::json!(c.budget));

    // Tags: parse from args["tags"] if present, else from config
    let tags = args["tags"]
        .as_str()
        .and_then(|s| parse_comma_separated(s))
        .or_else(|| parse_comma_separated(&c.tags));
    if let Some(ref t) = tags {
        payload["tags"] = serde_json::json!(t);
        let tags_match = args["tags_match"].as_str().unwrap_or(&c.tags_match);
        payload["tags_match"] = serde_json::json!(tags_match);
    }

    // Types: parse from args["types"] if present, else from config
    let types = args["types"]
        .as_str()
        .and_then(|s| parse_comma_separated(s))
        .or_else(|| parse_comma_separated(&c.types));
    if let Some(ref t) = types {
        payload["types"] = serde_json::json!(t);
    }

    let client = match http_client() {
        Ok(c) => c,
        Err(e) => return Ok((format!("Failed to build HTTP client: {}", e), true)),
    };

    match client.post(&recall_url()).json(&payload).send().await {
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

/// Handle hindsight_retain - store a memory.
async fn handle_retain(args: Value) -> Result<(String, bool)> {
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

    let client = match http_client() {
        Ok(c) => c,
        Err(e) => return Ok((format!("Failed to build HTTP client: {}", e), true)),
    };

    match client.post(&retain_url()).json(&payload).send().await {
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

/// Handle hindsight_reflect - synthesize an answer from memories.
async fn handle_reflect(args: Value) -> Result<(String, bool)> {
    let query = args["query"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing required argument: 'query'"))?;

    let payload = serde_json::json!({
        "query": query,
        "budget": config().budget,
    });

    let client = match http_client() {
        Ok(c) => c,
        Err(e) => return Ok((format!("Failed to build HTTP client: {}", e), true)),
    };

    match client.post(&reflect_url()).json(&payload).send().await {
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
    // Pre-initialize config before starting the server
    config();

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
                            "description": "Recall budget - low is fastest, high is most thorough (default: from config)"
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
            handler: Box::new(|args| Box::pin(handle_recall(args))),
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
            handler: Box::new(|args| Box::pin(handle_retain(args))),
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
            handler: Box::new(|args| Box::pin(handle_reflect(args))),
        },
    ];

    let server_info = ServerInfo {
        name: "hindsight".to_string(),
        version: "0.1.0".to_string(),
    };

    run_server(server_info, tools).await
}
