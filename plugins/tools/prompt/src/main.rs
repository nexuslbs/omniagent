//! mcp-server-prompt: standalone MCP server that generates the full LLM prompt.
//!
//! Tools:
//! - `generate`: generates the complete prompt including system prompt,
//!   thread context, recent messages, summaries, skills, and planning instructions.
//! - `compact-messages`: compacts a conversation by removing old assistant
//!   tool-call pairs, preserving the most recent messages.
//!
//! Standalone binary: pure prompt assembly inlined from prompt-tools.

mod chat_message;
mod compact;
mod memory_store;
mod prompt_builder;

use anyhow::{Context, Result};
use mcp_server_util::*;
use serde_json::Value;
use sqlx::{FromRow, PgPool};
use std::sync::Arc;

// ---------------------------------------------------------------------------
// Helpers to extract values from args or _meta (args take priority for standalone calls)
// ---------------------------------------------------------------------------

/// Extract a value from args first, then fall back to _meta.
fn extract_i64(args: &Value, meta: &Option<McpMeta>, key: &str) -> Option<i64> {
    args[key].as_i64().or_else(|| {
        meta.as_ref().and_then(|m| match key {
            "channel_id" => m.channel_id,
            "thread_id" => m.thread_id,
            _ => None,
        })
    })
}

fn extract_str<'a>(args: &'a Value, meta: &'a Option<McpMeta>, key: &str) -> Option<&'a str> {
    args[key].as_str().or_else(|| {
        meta.as_ref().and_then(|m| match key {
            "channel_name" => m.channel_name.as_deref(),
            "profile_name" => m.profile_name.as_deref(),
            "platform" => m.platform.as_deref(),
            _ => None,
        })
    })
}

// ---------------------------------------------------------------------------
// DB row types
// ---------------------------------------------------------------------------

#[derive(Debug, FromRow)]
struct MessageRow {
    id: i64,
    thread_id: i64,
    role: String,
    content: String,
    msg_type: String,
    #[allow(dead_code)]
    msg_subtype: Option<String>,
    created_at: Option<String>,
}

#[derive(Debug, FromRow)]
struct SummaryRow {
    id: i64,
    channel_id: i64,
    next_thread_id: i64,
    content: String,
}

#[derive(Debug, FromRow)]
struct ThreadRow {
    id: i64,
    status: String,
    cause: String,
}

#[derive(Debug, FromRow)]
struct SubtaskRow {
    id: i64,
    description: String,
    status: String,
    #[allow(dead_code)]
    thread_id: i64,
}

// ---------------------------------------------------------------------------
// DB helpers
// ---------------------------------------------------------------------------

/// Connect to the Postgres database using DATABASE_URL.
async fn connect_db() -> Result<PgPool> {
    let database_url = std::env::var("DATABASE_URL").context("DATABASE_URL must be set")?;
    let pool = PgPool::connect(&database_url)
        .await
        .context("Failed to connect to database")?;
    Ok(pool)
}

/// Get recent messages from a thread.
async fn get_thread_messages(pool: &PgPool, thread_id: i64, limit: i64) -> Result<Vec<MessageRow>> {
    let rows = sqlx::query_as::<_, MessageRow>(
        r#"
        SELECT id, thread_id, role, content, msg_type, msg_subtype,
               COALESCE(TO_CHAR(created_at, 'YYYY-MM-DD"T"HH24:MI:SS.US"Z"'), '') AS created_at
        FROM messages
        WHERE thread_id = $1
          AND role IN ('cause', 'agent')
          AND msg_type IN ('message', 'reasoning')
        ORDER BY created_at DESC
        LIMIT $2
        "#
    )
    .bind(thread_id)
    .bind(limit)
    .fetch_all(pool)
    .await
    .context("Failed to fetch thread messages")?;

    Ok(rows)
}

/// Get the latest summary for a channel.
async fn get_latest_summary(pool: &PgPool, channel_id: i64) -> Result<Option<SummaryRow>> {
    let row = sqlx::query_as::<_, SummaryRow>(
        r#"
        SELECT id, channel_id, next_thread_id, content
        FROM summaries
        WHERE channel_id = $1
        ORDER BY id DESC
        LIMIT 1
        "#
    )
    .bind(channel_id)
    .fetch_optional(pool)
    .await
    .context("Failed to fetch latest summary")?;

    Ok(row)
}

/// Get completed threads since a given thread ID (for summary context).
async fn get_threads_since(
    pool: &PgPool,
    channel_id: i64,
    since_id: i64,
    limit: i64,
) -> Result<Vec<ThreadRow>> {
    let rows = sqlx::query_as::<_, ThreadRow>(
        r#"
        SELECT id, status, cause
        FROM threads
        WHERE channel_id = $1
          AND status = 'completed'
          AND id > $2
        ORDER BY id ASC
        LIMIT $3
        "#
    )
    .bind(channel_id)
    .bind(since_id)
    .bind(limit)
    .fetch_all(pool)
    .await
    .context("Failed to fetch completed threads")?;

    Ok(rows)
}

/// Get subtasks for a thread.
async fn get_subtasks(pool: &PgPool, thread_id: i64) -> Result<Vec<SubtaskRow>> {
    let rows = sqlx::query_as::<_, SubtaskRow>(
        r#"
        SELECT id, description, status, thread_id
        FROM thread_subtasks
        WHERE thread_id = $1
        ORDER BY id ASC
        "#
    )
    .bind(thread_id)
    .fetch_all(pool)
    .await
    .context("Failed to fetch subtasks")?;

    Ok(rows)
}

/// Get list of skills from the filesystem. Returns formatted descriptions.
fn get_skills(data_dir: &str, profile_name: &str) -> Vec<String> {
    let skills_dir = format!("{}/profiles/{}/skills", data_dir, profile_name);
    let mut skills = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&skills_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("md") {
                if let Ok(content) = std::fs::read_to_string(&path) {
                    let name = path.file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or("unknown");
                    let first_line = content.lines().next().unwrap_or("").trim();
                    let desc = if first_line.starts_with('#') {
                        first_line.trim_start_matches('#').trim()
                    } else {
                        first_line
                    };
                    skills.push(format!("- {}: {}", name, desc));
                }
            }
        }
    }
    skills.sort();
    skills
}

// ---------------------------------------------------------------------------
// Tool: prompt_generate_full
// ---------------------------------------------------------------------------

async fn handle_generate_full(pool: &PgPool, args: &Value, meta: Option<McpMeta>) -> Result<(String, bool)> {
    let profile_name = extract_str(args, &meta, "profile_name").unwrap_or("default");
    let platform = extract_str(args, &meta, "platform").unwrap_or("");
    let system_message = args["system_message"].as_str();
    let user_message = args["user_message"].as_str().unwrap_or("");
    let tool_names: Vec<String> = args["tool_names"]
        .as_array()
        .map(|arr| arr.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .unwrap_or_default();
    let thread_id = extract_i64(args, &meta, "thread_id");
    let channel_id = extract_i64(args, &meta, "channel_id");
    let data_dir = std::env::var("OMNI_DIR")
        .or_else(|_| std::env::var("HOME").map(|h| format!("{}/.omniagent", h)))
        .context("OMNI_DIR must be set")?;

    // 1. Build system prompt parts using the builder
    let base_path = format!("{}/profiles/{}", data_dir, profile_name);
    let mut memory_store = crate::memory_store::MemoryStore::new(&base_path);
    memory_store.load_from_disk();

    // Use build_system_prompt_parts to get separated tiers
    let all_parts = crate::prompt_builder::build_system_prompt_parts(
        &memory_store,
        platform,
        system_message,
        profile_name,
        &tool_names,
    );

    // all_parts contains: [identity, tool_guidance, profile_hint, (system_message?), platform_hint?, memory_section, user_profile_section]
    // We need to split into: system (identity + guidance + profile), memory, soul (system_message)
    let mut system_parts = Vec::new();
    let mut memory_text = String::new();
    let mut soul_text = String::new();

    for part in &all_parts {
        if part.starts_with("## MEMORY") || part.starts_with("## USER PROFILE") {
            // This is the memory or user profile section: extract as memory
            if part.starts_with("## USER PROFILE") {
                memory_text.push_str(part);
                memory_text.push('\n');
            } else {
                memory_text.push_str(part);
                memory_text.push('\n');
            }
        } else if system_message.is_some() && !system_message.unwrap().is_empty() && part == system_message.unwrap() {
            // This is the optional system_message override → soul
            soul_text = part.clone();
        } else {
            system_parts.push(part.clone());
        }
    }

    let system = system_parts.join("\n\n");
    let memory = memory_text.trim().to_string();
    let soul = if soul_text.is_empty() {
        String::new()
    } else {
        soul_text
    };

    // 2. Build context blocks (thread messages, summaries, skills)
    let mut context_blocks: Vec<String> = Vec::new();

    // 2a. Recent thread messages
    if let Some(tid) = thread_id {
        match get_thread_messages(pool, tid, 10).await {
            Ok(msgs) if !msgs.is_empty() => {
                let formatted: Vec<String> = msgs.iter().rev().map(|m| {
                    format!("[{}]: {}", m.role, truncate_str(&m.content, 500))
                }).collect();
                context_blocks.push(format!(
                    "Recent conversation history (current thread):\n{}",
                    formatted.join("\n")
                ));
            }
            Ok(_) => {}
            Err(e) => tracing::warn!("Failed to get thread messages: {}", e),
        }
    }

    // 2b. Latest summary and threads since
    if let Some(cid) = channel_id {
        match get_latest_summary(pool, cid).await {
            Ok(Some(summary)) => {
                context_blocks.push(format!(
                    "Previous channel summary (covers threads up to id={}):\n{}",
                    summary.next_thread_id, truncate_str(&summary.content, 4000)
                ));

                // Threads completed after the summary
                match get_threads_since(pool, cid, summary.next_thread_id, 5).await {
                    Ok(threads) if !threads.is_empty() => {
                        let thread_info: Vec<String> = threads.iter().map(|t| {
                            format!("[Thread #{} by {}]: completed", t.id, t.cause)
                        }).collect();
                        context_blocks.push(format!(
                            "Recent threads (after last summary):\n{}",
                            thread_info.join("\n---\n")
                        ));
                    }
                    _ => {}
                }
            }
            Ok(None) => { /* no summary yet */ }
            Err(e) => tracing::warn!("Failed to get summary: {}", e),
        }
    }

    // 2c. Skills
    let skills = get_skills(&data_dir, profile_name);
    if !skills.is_empty() {
        context_blocks.push(format!(
            "Available skills:\n{}",
            skills.join("\n")
        ));
    }

    // 2d. Subtasks
    if let Some(tid) = thread_id {
        match get_subtasks(pool, tid).await {
            Ok(subtasks) if !subtasks.is_empty() => {
                let mut lines = vec![format!("## Subtasks (Thread #{})", tid)];
                for (i, s) in subtasks.iter().enumerate() {
                    let icon = match s.status.as_str() {
                        "completed" => "✅",
                        "cancelled" => "❌",
                        "error" => "⚠️",
                        _ => "⬜",
                    };
                    lines.push(format!("{}. {} {}", i + 1, icon, s.description));
                }
                context_blocks.push(lines.join("\n"));
            }
            _ => {}
        }
    }

    let context = context_blocks.join("\n\n---\n\n");
    let user = user_message.to_string();

    // ── Plan resolution ──
    // Plan input: true=plan, false=no plan, null/absent=let plugin config decide
    let plan_input: Option<bool> = args.get("plan").and_then(|v| {
        if v.is_null() { None } else { v.as_bool() }
    });

    // When plan is null/absent, use plugin-level config to decide
    let plan = match plan_input {
        Some(val) => val,
        None => {
            // Plugin-level config: complexity thresholds
            // When plan is null/None, the plugin decides based on message complexity
            let max_chars = std::env::var("PROMPT_PLANNING_COMPLEXITY_MAX_CHARS")
                .ok()
                .and_then(|v| v.parse::<usize>().ok())
                .unwrap_or(60);
            let keywords_str = std::env::var("PROMPT_PLANNING_COMPLEXITY_KEYWORDS")
                .unwrap_or_default();
            let keywords: Vec<&str> = keywords_str
                .split(',')
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .collect();
            // Plan if message is complex (long) or contains keywords
            let has_keyword = if keywords.is_empty() {
                false
            } else {
                let lower = user.to_lowercase();
                keywords.iter().any(|k| lower.contains(k))
            };
            user.len() > max_chars || has_keyword
        }
    };

    let result = serde_json::json!({
        "system": system,
        "memory": memory,
        "soul": soul,
        "context": context,
        "user": user,
        "plan": plan,
    });

    Ok((serde_json::to_string_pretty(&result).unwrap_or_else(|_| "Serialization error".to_string()), false))
}

// ---------------------------------------------------------------------------
// Tool: prompt_compact_messages
// ---------------------------------------------------------------------------

async fn handle_compact_messages(args: &Value) -> Result<(String, bool)> {
    let messages_arr = match args["messages"].as_array() {
        Some(arr) => arr,
        None => return Ok(("Missing required argument: 'messages' (array of ChatMessage)".to_string(), true)),
    };

    let keep_recent = args["keep_recent"].as_u64().unwrap_or(3) as usize;

    let mut messages: Vec<crate::chat_message::ChatMessage> =
        match serde_json::from_value(serde_json::Value::Array(messages_arr.clone())) {
            Ok(msgs) => msgs,
            Err(e) => return Ok((format!("Failed to parse messages: {}", e), true)),
        };

    let before = messages.len();
    crate::compact::compact_old_assistant_messages(&mut messages, keep_recent);
    let after = messages.len();

    let result = serde_json::json!({
        "messages": messages,
        "was_compacted": before != after,
        "before_count": before,
        "after_count": after,
    });

    Ok((serde_json::to_string_pretty(&result).unwrap_or_else(|_| "Serialization error".to_string()), false))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn truncate_str(s: &str, max_chars: usize) -> String {
    if s.len() <= max_chars {
        return s.to_string();
    }
    let trunc_at = s.char_indices()
        .nth(max_chars)
        .map(|(i, _)| i)
        .unwrap_or(s.len());
    format!("{}...", &s[..trunc_at])
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<()> {
    let pool = connect_db().await?;
    let pool = Arc::new(pool);

    // Generate full prompt handler
    let p_gen = pool.clone();
    let gen_handler: ToolHandler = Box::new(move |args: Value, meta: Option<McpMeta>| {
        let p = p_gen.clone();
        Box::pin(async move { handle_generate_full(&p, &args, meta).await })
    });

    // Compact messages handler
    let compact_handler: ToolHandler =
        Box::new(move |args: Value, _meta: Option<McpMeta>| Box::pin(async move { handle_compact_messages(&args).await }));

    let tools = vec![
        McpToolEntry {
            def: McpToolDef {
                name: "prompt_generate".to_string(),
                description:
                    "Generate the complete LLM prompt for a conversation, including system prompt \
                     (identity, tool guidance, memory, user profile), thread context (recent messages, \
                     summaries, skills, subtasks), and optional planning instructions. Returns the full \
                     prompt as a JSON string. This is the single source of truth for prompt building: \
                     no other prompt assembly is needed."
                    .to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "profile_name": {
                            "type": "string",
                            "description": "Profile name (default: default)"
                        },
                        "platform": {
                            "type": "string",
                            "description": "Platform identifier (e.g. 'telegram', 'mattermost')"
                        },
                        "system_message": {
                            "type": "string",
                            "description": "Optional system message override"
                        },
                        "user_message": {
                            "type": "string",
                            "description": "User's message to include in the prompt"
                        },
                        "tool_names": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "List of available tool names"
                        },
                        "thread_id": {
                            "type": "integer",
                            "description": "Thread ID for context assembly (recent messages, subtasks)"
                        },
                        "channel_id": {
                            "type": "integer",
                            "description": "Channel ID for context assembly (summaries)"
                        },
                        "plan": {
                            "type": "boolean",
                            "description": "Plan mode suggestion: true=plan, false=no plan, null=let plugin decide based on config"
                        }
                    },
                    "required": []
                }),
            },
            handler: gen_handler,
        },
        McpToolEntry {
            def: McpToolDef {
                name: "compact-messages".to_string(),
                description:
                    "Compact old assistant messages in a conversation to save tokens. \
                     Removes redundant assistant tool-call pairs from the middle of the \
                     conversation while preserving system messages, the most recent messages, \
                     and tool results. Returns the compacted message array."
                    .to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "messages": {
                            "type": "array",
                            "description": "Array of ChatMessage objects to compact"
                        },
                        "keep_recent": {
                            "type": "integer",
                            "description": "Number of most recent messages to always keep (default: 3)"
                        }
                    },
                    "required": ["messages"]
                }),
            },
            handler: compact_handler,
        },
    ];

    let server_info = ServerInfo {
        name: "mcp-server-prompt".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
    };

    run_server(server_info, tools).await
}
