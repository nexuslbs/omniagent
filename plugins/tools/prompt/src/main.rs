//! mcp-server-prompt: standalone MCP server that generates the full LLM prompt.
//!
//! Tools:
//! - `generate`: generates the complete prompt including system prompt,
//!   thread context, recent messages, summaries, skills, and planning instructions.
//! - `compact-messages`: compacts a conversation by removing old assistant
//!   tool-call pairs, preserving the most recent messages.
//! - `condense`: condenses conversation context based on configured thresholds.
//!
//! Config is received from the omniagent via the `configure` message at startup.
//! Plugins never read env vars for config. Users can use $env: notation in
//! plugins.yaml if they want values from env vars.

mod chat_message;
mod compact;
mod memory_store;
mod prompt_builder;

use anyhow::{Context, Result};
use mcp_server_util::*;
use serde_json::Value;
use sqlx::{FromRow, PgPool};
use std::sync::Arc;
use tokio::sync::{watch, RwLock};
// ---------------------------------------------------------------------------
// Plugin config — received via configure message, never from env vars
// ---------------------------------------------------------------------------

/// Plugin-level config with defaults matching the original settings values.
#[derive(Debug, Clone)]
pub struct PluginConfig {
    // Database
    pub database_url: String,
    pub omni_dir: String,
    // Planning
    pub planning_complexity_max_chars: usize,
    pub planning_complexity_keywords: String,
    pub prompt_plan_max_tokens: usize,
    // Condense
    pub tokenizer_encoding: String,
    pub char_budget_soft: usize,
    pub char_budget_hard: usize,
    pub token_budget_soft: usize,
    pub token_budget_hard: usize,
    pub old_msg_budget: usize,
    pub condense_keep_turns: usize,
    // Prompt builder
    pub memory_max_chars: usize,
    pub soul_max_chars: usize,
}

impl PluginConfig {
    fn default() -> Self {
        Self {
            database_url: String::new(),
            omni_dir: String::new(),
            planning_complexity_max_chars: 60,
            planning_complexity_keywords:
                "implement,refactor,redesign,architecture,create,build,design,develop,\
                 migrate,restructure,overhaul,rewrite,configure,set up,deploy,integrate,\
                 add feature,fix bug,resolve issue,multi-step,complex"
                    .to_string(),
            prompt_plan_max_tokens: 2048,
            tokenizer_encoding: String::new(),
            char_budget_soft: 350000,
            char_budget_hard: 500000,
            token_budget_soft: 200000,
            token_budget_hard: 350000,
            old_msg_budget: 100000,
            condense_keep_turns: 4,
            memory_max_chars: 5000,
            soul_max_chars: 1000,
        }
    }

    /// Parse config from the JSON value sent by the configure message.
    /// Unknown keys are silently ignored; missing keys keep defaults.
    fn from_json(json: &Value) -> Self {
        let mut cfg = Self::default();
        if let Some(obj) = json.as_object() {
            if let Some(v) = obj.get("database_url").and_then(|v| v.as_str()) {
                cfg.database_url = v.to_string();
            }
            if let Some(v) = obj.get("omni_dir").and_then(|v| v.as_str()) {
                cfg.omni_dir = v.to_string();
            }
            if let Some(v) = obj
                .get("planning_complexity_max_chars")
                .and_then(|v| v.as_i64())
            {
                cfg.planning_complexity_max_chars = v as usize;
            }
            if let Some(v) = obj
                .get("planning_complexity_keywords")
                .and_then(|v| v.as_str())
            {
                cfg.planning_complexity_keywords = v.to_string();
            }
            if let Some(v) = obj.get("prompt_plan_max_tokens").and_then(|v| v.as_i64()) {
                cfg.prompt_plan_max_tokens = v as usize;
            }
            if let Some(v) = obj.get("tokenizer_encoding").and_then(|v| v.as_str()) {
                cfg.tokenizer_encoding = v.to_string();
            }
            if let Some(v) = obj.get("char_budget_soft").and_then(|v| v.as_i64()) {
                cfg.char_budget_soft = v as usize;
            }
            if let Some(v) = obj.get("char_budget_hard").and_then(|v| v.as_i64()) {
                cfg.char_budget_hard = v as usize;
            }
            if let Some(v) = obj.get("token_budget_soft").and_then(|v| v.as_i64()) {
                cfg.token_budget_soft = v as usize;
            }
            if let Some(v) = obj.get("token_budget_hard").and_then(|v| v.as_i64()) {
                cfg.token_budget_hard = v as usize;
            }
            if let Some(v) = obj.get("old_message_char_budget").and_then(|v| v.as_i64()) {
                cfg.old_msg_budget = v as usize;
            }
            if let Some(v) = obj.get("condense_keep_turns").and_then(|v| v.as_i64()) {
                cfg.condense_keep_turns = (v as usize).max(1);
            }
            if let Some(v) = obj.get("memory_max_chars").and_then(|v| v.as_i64()) {
                cfg.memory_max_chars = v as usize;
            }
            if let Some(v) = obj.get("soul_max_chars").and_then(|v| v.as_i64()) {
                cfg.soul_max_chars = v as usize;
            }
        }
        cfg
    }

    /// Build a PromptBuilderConfig from this plugin config.
    fn builder_config(&self) -> prompt_builder::PromptBuilderConfig {
        prompt_builder::PromptBuilderConfig {
            memory_max_chars: self.memory_max_chars,
            soul_max_chars: self.soul_max_chars,
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers to extract values from args or _meta (args take priority for standalone calls)
// ---------------------------------------------------------------------------

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

async fn connect_db(database_url: &str) -> Result<PgPool> {
    let pool = PgPool::connect(database_url)
        .await
        .context("Failed to connect to database")?;
    Ok(pool)
}

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
        "#,
    )
    .bind(thread_id)
    .bind(limit)
    .fetch_all(pool)
    .await
    .context("Failed to fetch thread messages")?;

    Ok(rows)
}

async fn get_latest_summary(pool: &PgPool, channel_id: i64) -> Result<Option<SummaryRow>> {
    let row = sqlx::query_as::<_, SummaryRow>(
        r#"
        SELECT id, channel_id, next_thread_id, content
        FROM summaries
        WHERE channel_id = $1
        ORDER BY id DESC
        LIMIT 1
        "#,
    )
    .bind(channel_id)
    .fetch_optional(pool)
    .await
    .context("Failed to fetch latest summary")?;

    Ok(row)
}

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
        "#,
    )
    .bind(channel_id)
    .bind(since_id)
    .bind(limit)
    .fetch_all(pool)
    .await
    .context("Failed to fetch completed threads")?;

    Ok(rows)
}

async fn get_subtasks(pool: &PgPool, thread_id: i64) -> Result<Vec<SubtaskRow>> {
    let rows = sqlx::query_as::<_, SubtaskRow>(
        r#"
        SELECT id, description, status, thread_id
        FROM thread_subtasks
        WHERE thread_id = $1
        ORDER BY id ASC
        "#,
    )
    .bind(thread_id)
    .fetch_all(pool)
    .await
    .context("Failed to fetch subtasks")?;

    Ok(rows)
}

fn get_skills(data_dir: &str, profile_name: &str) -> Vec<String> {
    let skills_dir = format!("{}/profiles/{}/skills", data_dir, profile_name);
    let mut skills = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&skills_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("md") {
                if let Ok(content) = std::fs::read_to_string(&path) {
                    let name = path
                        .file_stem()
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

async fn handle_generate_full(
    pool: &PgPool,
    args: &Value,
    meta: Option<McpMeta>,
    cfg: &PluginConfig,
) -> Result<(String, bool)> {
    let profile_name = extract_str(args, &meta, "profile_name").unwrap_or("omni");
    let platform = extract_str(args, &meta, "platform").unwrap_or("");
    let system_message = args["system_message"].as_str();
    let user_message = args["user_message"].as_str().unwrap_or("");
    let tool_names: Vec<String> = args["tool_names"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
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
        &cfg.builder_config(),
    );

    // all_parts contains: [identity, tool_guidance, profile_hint, (system_message?), platform_hint?, memory_section, user_profile_section]
    // We need to split into: system (identity + guidance + profile), memory, soul (system_message)
    let mut system_parts = Vec::new();
    let mut memory_text = String::new();
    let mut soul_text = String::new();

    for part in &all_parts {
        if part.starts_with("## MEMORY") || part.starts_with("## USER PROFILE") {
            if part.starts_with("## USER PROFILE") {
                memory_text.push_str(part);
                memory_text.push('\n');
            } else {
                memory_text.push_str(part);
                memory_text.push('\n');
            }
        } else if system_message.is_some()
            && !system_message.unwrap().is_empty()
            && part == system_message.unwrap()
        {
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
                let formatted: Vec<String> = msgs
                    .iter()
                    .rev()
                    .map(|m| format!("[{}]: {}", m.role, truncate_str(&m.content, 500)))
                    .collect();
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
                    summary.next_thread_id,
                    truncate_str(&summary.content, 4000)
                ));

                match get_threads_since(pool, cid, summary.next_thread_id, 5).await {
                    Ok(threads) if !threads.is_empty() => {
                        let thread_info: Vec<String> = threads
                            .iter()
                            .map(|t| format!("[Thread #{} by {}]: completed", t.id, t.cause))
                            .collect();
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
        context_blocks.push(format!("Available skills:\n{}", skills.join("\n")));
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
    let plan_input: Option<bool> =
        args.get("plan")
            .and_then(|v| if v.is_null() { None } else { v.as_bool() });

    // When plan is null/absent, use plugin-level config to decide
    let plan = match plan_input {
        Some(val) => val,
        None => {
            let max_chars = cfg.planning_complexity_max_chars;
            let keywords_str = &cfg.planning_complexity_keywords;
            let keywords: Vec<&str> = keywords_str
                .split(',')
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .collect();
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

    Ok((
        serde_json::to_string_pretty(&result).unwrap_or_else(|_| "Serialization error".to_string()),
        false,
    ))
}

// ---------------------------------------------------------------------------
// Tool: prompt_compact_messages
// ---------------------------------------------------------------------------

async fn handle_compact_messages(args: &Value) -> Result<(String, bool)> {
    let messages_arr = match args["messages"].as_array() {
        Some(arr) => arr,
        None => {
            return Ok((
                "Missing required argument: 'messages' (array of ChatMessage)".to_string(),
                true,
            ))
        }
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

    Ok((
        serde_json::to_string_pretty(&result).unwrap_or_else(|_| "Serialization error".to_string()),
        false,
    ))
}

// ---------------------------------------------------------------------------
// Tool: prompt_condense (threshold-based context condensation)
// ---------------------------------------------------------------------------

async fn handle_condense(args: &Value, cfg: &PluginConfig) -> Result<(String, bool)> {
    let messages_arr = match args["messages"].as_array() {
        Some(arr) => arr,
        None => {
            return Ok((
                "Missing required argument: 'messages' (array of ChatMessage)".to_string(),
                true,
            ))
        }
    };

    let mut messages: Vec<crate::chat_message::ChatMessage> =
        match serde_json::from_value(serde_json::Value::Array(messages_arr.clone())) {
            Ok(msgs) => msgs,
            Err(e) => return Ok((format!("Failed to parse messages: {}", e), true)),
        };

    let before = messages.len();

    // Read config from shared plugin config (set by configure message)
    let use_tokens = !cfg.tokenizer_encoding.is_empty();
    let soft_budget = if use_tokens {
        cfg.token_budget_soft.min(cfg.char_budget_soft)
    } else {
        cfg.char_budget_soft
    };
    let hard_budget = if use_tokens {
        cfg.token_budget_hard.min(cfg.char_budget_hard)
    } else {
        cfg.char_budget_hard
    };
    let target_budget = soft_budget.min(hard_budget);

    let current_size: usize = if use_tokens {
        messages.iter().map(|m| m.content.len()).sum::<usize>() / 4
    } else {
        messages.iter().map(|m| m.content.len()).sum::<usize>()
    };

    let current_iteration = args["current_iteration"].as_i64().unwrap_or(0);
    let last_condense_iteration = args["last_condense_iteration"].as_i64().unwrap_or(-1);
    let state_interval: i64 = 5;

    let needs_hard = current_size > hard_budget;
    let needs_soft = !needs_hard
        && current_size > soft_budget
        && state_interval > 0
        && (current_iteration - last_condense_iteration) >= state_interval;

    let was_condensed = if needs_hard || needs_soft {
        let condense_keep_turns = cfg.condense_keep_turns;
        crate::compact::compact_old_assistant_messages(&mut messages, condense_keep_turns);

        let after_size: usize = if use_tokens {
            messages.iter().map(|m| m.content.len()).sum::<usize>() / 4
        } else {
            messages.iter().map(|m| m.content.len()).sum::<usize>()
        };

        if after_size > target_budget {
            let aggressive_keep = if condense_keep_turns > 1 {
                condense_keep_turns - 1
            } else {
                0
            };
            crate::compact::compact_old_assistant_messages(&mut messages, aggressive_keep);
        }
        true
    } else {
        false
    };

    let after = messages.len();
    let result = serde_json::json!({
        "messages": messages,
        "was_condensed": was_condensed,
        "before_count": before,
        "after_count": after,
    });

    Ok((
        serde_json::to_string_pretty(&result).unwrap_or_else(|_| "Serialization error".to_string()),
        false,
    ))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn truncate_str(s: &str, max_chars: usize) -> String {
    if s.len() <= max_chars {
        return s.to_string();
    }
    let trunc_at = s
        .char_indices()
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
    // Shared pool — populated by configure callback before any tool call
    let pool: Arc<RwLock<Option<PgPool>>> = Arc::new(RwLock::new(None));
    let (pool_ready_tx, pool_ready_rx) = tokio::sync::watch::channel(false);

    // Shared config — updated by configure message at startup
    let plugin_config = Arc::new(RwLock::new(PluginConfig::default()));

    // Generate full prompt handler
    let p_gen = pool.clone();
    let cfg_gen = plugin_config.clone();
    let mut pool_ready_gen = pool_ready_rx;
    let gen_handler: ToolHandler = Box::new(move |args: Value, meta: Option<McpMeta>| {
        let p = p_gen.clone();
        let cfg = cfg_gen.clone();
        let mut rx = pool_ready_gen.clone();
        Box::pin(async move {
            // Wait until pool is configured (persistent state — already-true fires
            // immediately for latecomers, unlike Notify which misses them).
            while !*rx.borrow() {
                rx.changed().await.ok();
            }
            let guard = p.read().await;
            let pool = guard.as_ref().expect("Pool not initialized").clone();
            let config = cfg.read().await.clone();
            handle_generate_full(&pool, &args, meta, &config).await
        })
    });

    // Compact messages handler
    let compact_handler: ToolHandler = Box::new(move |args: Value, _meta: Option<McpMeta>| {
        Box::pin(async move { handle_compact_messages(&args).await })
    });

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
                            "description": "Profile name (default: omni)"
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
                name: "prompt_compact-messages".to_string(),
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

    // Use run_server_with_config so the omniagent can pass plugin config
    // via the configure message instead of env vars.
    let on_configure = {
        let cfg = plugin_config.clone();
        let p = pool.clone();
        let ready_tx = pool_ready_tx.clone();
        Some(move |params: Value| {
            let new_config = PluginConfig::from_json(&params);
            let db_url = new_config.database_url.clone();
            let pc = p.clone();
            let tx = ready_tx.clone();
            let cfg_c = cfg.clone();
            tracing::info!(
                "Prompt configure received: database_url present={}, omni_dir present={}",
                !new_config.database_url.is_empty(),
                !new_config.omni_dir.is_empty()
            );
            // Spawn async DB connection — runs in background while
            // the MCP loop continues. Handlers wait on pool_ready.
            tokio::spawn(async move {
                match connect_db(&db_url).await {
                    Ok(new_pool) => {
                        *pc.write().await = Some(new_pool);
                        tracing::info!("Prompt plugin DB connected");
                    }
                    Err(e) => {
                        tracing::error!("Failed to connect to database: {:?}", e);
                    }
                }
                tx.send(true).ok();
            });
            // Store config immediately (no DB needed for config values)
            tokio::spawn(async move {
                let mut locked = cfg_c.write().await;
                *locked = new_config.clone();
                tracing::info!(
                    "Prompt plugin configured: database_url set, tokenizer_encoding={:?}, char_budget_soft={}, char_budget_hard={}",
                    locked.tokenizer_encoding, locked.char_budget_soft, locked.char_budget_hard
                );
            });
        })
    };

    run_server_with_config(server_info, tools, on_configure).await
}
