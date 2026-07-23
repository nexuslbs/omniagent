//! mcp-server-memory: standalone MCP server for memory promotion, listing,
//! review, and management.
//!
//! Communicates via stdio JSON-RPC (MCP protocol).
//!
//! Tools: promote_to_memory, list_memories, review_memories, manage_memory
//!
//! Memories are validated facts promoted from conversations to long-term wiki
//! storage. Each memory entry is a markdown file in:
//! `<data_dir>/profiles/<profile>/wiki/Memory/Promoted/<name>.md`
//!
//! Frontmatter fields:
//! - `type`: "memory"
//! - `confidence`: "high" | "medium" | "low"
//! - `source_message_ids`: [int]: message IDs that support this fact
//! - `source_tool_outputs`: [string]: tool call IDs that produced evidence
//! - `last_verified_at`: ISO timestamp
//! - `created_at`: ISO timestamp
//! - `expires_at`: ISO timestamp (default: 30 days)

use anyhow::{Context, Result};
use mcp_server_util::*;
use omniagent::db;
use omniagent::db::types as queries;
use serde_json::Value;
use sqlx::PgPool;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::RwLock;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Generate an ISO timestamp string from the given offset.
fn iso_timestamp(offset_days: i64) -> String {
    let now = chrono::Utc::now();
    let ts = if offset_days == 0 {
        now
    } else {
        now + chrono::Duration::days(offset_days)
    };
    ts.format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// Validate confidence level string.
fn valid_confidence(s: &str) -> bool {
    matches!(s, "high" | "medium" | "low")
}

/// Sanitize a string for use as a filename (alphanumeric, hyphens, underscores).
fn sanitize_filename(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Tool: promote_to_memory
// ---------------------------------------------------------------------------

async fn handle_promote(data_dir: &str, args: &Value) -> Result<(String, bool)> {
    let name = args["name"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing required argument: 'name'"))?;
    let content = args["content"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing required argument: 'content'"))?;
    let confidence = args["confidence"].as_str().unwrap_or("medium");

    if !valid_confidence(confidence) {
        anyhow::bail!(
            "Invalid confidence: '{}'. Must be one of: high, medium, low",
            confidence
        );
    }

    let source_message_ids: Vec<i64> = args["source_message_ids"]
        .as_array()
        .map(|arr| arr.iter().filter_map(|v| v.as_i64()).collect())
        .unwrap_or_default();

    let source_tool_outputs: Vec<String> = args["source_tool_outputs"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    let expires_in_days = args["expires_in_days"].as_i64().unwrap_or(30).max(1);
    let _default_profile = omniagent::profile::default_profile_name();
    let profile = args["profile"].as_str().unwrap_or(&_default_profile);

    // Build the wiki path
    let wiki_memories_dir = format!("{}/profiles/{}/wiki/Memory/Promoted", data_dir, profile);
    let dir_path = Path::new(&wiki_memories_dir);
    std::fs::create_dir_all(dir_path)
        .map_err(|e| anyhow::anyhow!("Failed to create memories directory: {}", e))?;

    let sanitized = sanitize_filename(name);
    if sanitized.is_empty() {
        anyhow::bail!("Name resulted in empty filename after sanitization");
    }

    let filepath = dir_path.join(format!("{}.md", sanitized));
    if filepath.exists() {
        anyhow::bail!(
            "Memory '{}' already exists at {}. Use a different name or review the existing entry.",
            name,
            filepath.display()
        );
    }

    let now_iso = iso_timestamp(0);
    let expires_iso = iso_timestamp(expires_in_days);

    let source_ids_json: String = source_message_ids
        .iter()
        .map(|id| id.to_string())
        .collect::<Vec<_>>()
        .join(", ");

    let source_tools_json: String = source_tool_outputs
        .iter()
        .map(|s| format!("\"{}\"", s))
        .collect::<Vec<_>>()
        .join(", ");

    let frontmatter = format!(
        r#"---
type: memory
confidence: {}
source_message_ids: [{}]
source_tool_outputs: [{}]
last_verified_at: {}
created_at: {}
expires_at: {}
---"#,
        confidence, source_ids_json, source_tools_json, now_iso, now_iso, expires_iso,
    );

    let full_content = format!("{}# Memory: {}\n\n{}", frontmatter, name, content);

    std::fs::write(&filepath, &full_content)
        .map_err(|e| anyhow::anyhow!("Failed to write memory file: {}", e))?;

    Ok((
        format!(
            "Memory '{}' promoted to wiki at {} (confidence: {}, expires: {})",
            name,
            filepath.display(),
            confidence,
            expires_iso
        ),
        false,
    ))
}

// ---------------------------------------------------------------------------
// Tool: list_memories
// ---------------------------------------------------------------------------

async fn handle_list(data_dir: &str, args: &Value) -> Result<(String, bool)> {
    let _default_profile = omniagent::profile::default_profile_name();
    let profile = args["profile"].as_str().unwrap_or(&_default_profile);
    let include_expired = args["include_expired"].as_bool().unwrap_or(false);

    let wiki_memories_dir = format!("{}/profiles/{}/wiki/Memory/Promoted", data_dir, profile);
    let dir_path = Path::new(&wiki_memories_dir);

    if !dir_path.exists() {
        return Ok(("No promoted memories found.".to_string(), false));
    }

    let mut entries = Vec::new();
    let now_iso = iso_timestamp(0);

    for entry in std::fs::read_dir(dir_path)
        .map_err(|e| anyhow::anyhow!("Failed to read memories directory: {}", e))?
    {
        let entry = entry.map_err(|e| anyhow::anyhow!("Failed to read entry: {}", e))?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }

        let content = std::fs::read_to_string(&path)
            .map_err(|e| anyhow::anyhow!("Failed to read {}: {}", path.display(), e))?;

        let filename = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown");

        let title = content
            .lines()
            .find(|l| l.starts_with("# Memory:"))
            .map(|l| l.trim_start_matches("# Memory:").trim())
            .unwrap_or(filename);

        let confidence = content
            .lines()
            .find(|l| l.starts_with("confidence:"))
            .map(|l| l.trim_start_matches("confidence:").trim())
            .unwrap_or("unknown");

        let expires_at = content
            .lines()
            .find(|l| l.starts_with("expires_at:"))
            .map(|l| l.trim_start_matches("expires_at:").trim())
            .unwrap_or("");

        let is_expired = !expires_at.is_empty() && expires_at < now_iso.as_str();

        if is_expired && !include_expired {
            continue;
        }

        let status = if is_expired { "EXPIRED" } else { "active" };
        entries.push(format!(
            "- **{}** (confidence: {}, status: **{}**, expires: {})",
            title, confidence, status, expires_at
        ));
    }

    if entries.is_empty() {
        Ok(("No active promoted memories found.".to_string(), false))
    } else {
        let result = format!(
            "## Promoted Memories ({})\n\n{}",
            entries.len(),
            entries.join("\n")
        );
        Ok((result, false))
    }
}

// ---------------------------------------------------------------------------
// Tool: review_memories
// ---------------------------------------------------------------------------

async fn handle_review(data_dir: &str, args: &Value) -> Result<(String, bool)> {
    let _default_profile = omniagent::profile::default_profile_name();
    let profile = args["profile"].as_str().unwrap_or(&_default_profile);
    let expiring_soon_days = args["expiring_soon_days"].as_i64().unwrap_or(7).max(1);

    let wiki_memories_dir = format!("{}/profiles/{}/wiki/Memory/Promoted", data_dir, profile);
    let dir_path = Path::new(&wiki_memories_dir);

    if !dir_path.exists() {
        return Ok(("No promoted memories to review.".to_string(), false));
    }

    let now_iso = iso_timestamp(0);
    let soon_iso = iso_timestamp(expiring_soon_days);

    let mut expired = Vec::new();
    let mut expiring_soon = Vec::new();
    let mut active = Vec::new();
    let mut total = 0u32;

    for entry in std::fs::read_dir(dir_path)
        .map_err(|e| anyhow::anyhow!("Failed to read memories directory: {}", e))?
    {
        let entry = entry.map_err(|e| anyhow::anyhow!("Failed to read entry: {}", e))?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }

        let content = std::fs::read_to_string(&path)
            .map_err(|e| anyhow::anyhow!("Failed to read {}: {}", path.display(), e))?;

        total += 1;

        let filename = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown");

        let title = content
            .lines()
            .find(|l| l.starts_with("# Memory:"))
            .map(|l| l.trim_start_matches("# Memory:").trim())
            .unwrap_or(filename);

        let confidence = content
            .lines()
            .find(|l| l.starts_with("confidence:"))
            .map(|l| l.trim_start_matches("confidence:").trim())
            .unwrap_or("unknown");

        let source_ids = content
            .lines()
            .find(|l| l.starts_with("source_message_ids:"))
            .map(|l| l.trim_start_matches("source_message_ids:").trim())
            .unwrap_or("[]");

        let expires_at = content
            .lines()
            .find(|l| l.starts_with("expires_at:"))
            .map(|l| l.trim_start_matches("expires_at:").trim())
            .unwrap_or("");

        let created_at = content
            .lines()
            .find(|l| l.starts_with("created_at:"))
            .map(|l| l.trim_start_matches("created_at:").trim())
            .unwrap_or("");

        let is_expired = !expires_at.is_empty() && expires_at < now_iso.as_str();
        let is_expiring_soon =
            !is_expired && !expires_at.is_empty() && expires_at < soon_iso.as_str();

        if is_expired {
            expired.push(format!(
                "- **{}** (confidence: {}, expired: {}, created: {}, sources: {})",
                title, confidence, expires_at, created_at, source_ids
            ));
        } else if is_expiring_soon {
            expiring_soon.push(format!(
                "- **{}** (confidence: {}, expires: {}, sources: {})",
                title, confidence, expires_at, source_ids
            ));
        } else {
            active.push(format!(
                "- **{}** (confidence: {}, expires: {})",
                title, confidence, expires_at
            ));
        }
    }

    let mut report = format!("# Memory Review Report\n\nTotal entries: **{}**\n\n", total);

    if !expired.is_empty() {
        report.push_str(&format!(
            "## ⚠️ Expired ({}):\n{}\n\n",
            expired.len(),
            expired.join("\n")
        ));
    }

    if !expiring_soon.is_empty() {
        report.push_str(&format!(
            "## ⏳ Expiring soon (within {} days) ({}):\n{}\n\n",
            expiring_soon_days,
            expiring_soon.len(),
            expiring_soon.join("\n")
        ));
    }

    if !active.is_empty() {
        report.push_str(&format!(
            "## ✅ Active ({}):\n{}\n\n",
            active.len(),
            active.join("\n")
        ));
    }

    report.push_str("### Recommended Actions:\n");
    if !expired.is_empty() {
        report.push_str("- **Renew**: Re-verify expired facts and call `promote_to_memory` with updated content\n");
    }
    if !expiring_soon.is_empty() {
        report.push_str("- **Review soon**: Check expiring memories for continued accuracy\n");
    }
    report.push_str("- **Keep**: Active memories are current and valid\n");

    Ok((report, false))
}

// ---------------------------------------------------------------------------
// Tool: manage_memory
// ---------------------------------------------------------------------------

const ENTRY_DELIMITER: &str = "\n§\n";

async fn handle_manage(data_dir: &str, args: &Value) -> Result<(String, bool)> {
    let target = args["target"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing required argument: 'target'"))?;
    let action = args["action"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing required argument: 'action'"))?;
    let content = args["content"].as_str().unwrap_or("");
    let _default_profile = omniagent::profile::default_profile_name();
    let profile = args["profile"].as_str().unwrap_or(&_default_profile);

    let memories_dir = format!("{}/profiles/{}/memories", data_dir, profile);
    let dir_path = std::path::Path::new(&memories_dir);
    std::fs::create_dir_all(dir_path)
        .map_err(|e| anyhow::anyhow!("Failed to create memories directory: {}", e))?;

    let filename = match target {
        "memory" => "MEMORY.md",
        "user" => "USER.md",
        _ => anyhow::bail!("Invalid target '{}'. Must be 'memory' or 'user'", target),
    };
    let filepath = dir_path.join(filename);

    match action {
        "add" => {
            if content.is_empty() {
                anyhow::bail!("Content is required for 'add' action");
            }
            let existing = if filepath.exists() {
                std::fs::read_to_string(&filepath)
                    .map_err(|e| anyhow::anyhow!("Failed to read {}: {}", filepath.display(), e))?
            } else {
                String::new()
            };
            let existing = existing.trim();
            let new_content = if existing.is_empty() {
                content.to_string()
            } else {
                format!("{}\n§\n{}", content, existing)
            };
            std::fs::write(&filepath, &new_content)
                .map_err(|e| anyhow::anyhow!("Failed to write {}: {}", filepath.display(), e))?;
            Ok((
                format!(
                    "Entry added to {} (profile: {}). {} total chars.",
                    filename,
                    profile,
                    new_content.len()
                ),
                false,
            ))
        }
        "remove" => {
            if content.is_empty() {
                anyhow::bail!("Substring is required for 'remove' action to match entries");
            }
            if !filepath.exists() {
                return Ok((
                    format!("No {} file found: nothing to remove.", filename),
                    false,
                ));
            }
            let existing = std::fs::read_to_string(&filepath)
                .map_err(|e| anyhow::anyhow!("Failed to read {}: {}", filepath.display(), e))?;
            let entries: Vec<String> = existing
                .split(ENTRY_DELIMITER)
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            let before = entries.len();
            let removed: Vec<&String> = entries.iter().filter(|e| e.contains(content)).collect();
            let removed_count = removed.len();
            let kept: Vec<&str> = entries
                .iter()
                .filter(|e| !e.contains(content))
                .map(|s| s.as_str())
                .collect();
            if kept.is_empty() {
                std::fs::remove_file(&filepath).map_err(|e| {
                    anyhow::anyhow!("Failed to remove {}: {}", filepath.display(), e)
                })?;
            } else {
                let new_content = kept.join(ENTRY_DELIMITER);
                std::fs::write(&filepath, &new_content).map_err(|e| {
                    anyhow::anyhow!("Failed to write {}: {}", filepath.display(), e)
                })?;
            }
            Ok((
                format!(
                    "Removed {}/{} entries from {} matching '{}'. {} remaining.",
                    removed_count,
                    before,
                    filename,
                    content,
                    kept.len()
                ),
                false,
            ))
        }
        "clean" => {
            if filepath.exists() {
                std::fs::remove_file(&filepath).map_err(|e| {
                    anyhow::anyhow!("Failed to remove {}: {}", filepath.display(), e)
                })?;
            }
            Ok((
                format!(
                    "{} cleared: all entries removed (profile: {}).",
                    filename, profile
                ),
                false,
            ))
        }
        _ => anyhow::bail!(
            "Invalid action '{}'. Must be 'add', 'remove', or 'clean'",
            action
        ),
    }
}

// ---------------------------------------------------------------------------
// Plugin config: received via configure message
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
struct PluginConfig {
    pub database_url: String,
    pub omni_dir: String,
    summarize_after_days: i64,
    channel_summary_tokens: u32,
    summary_provider: Option<String>,
    summary_model: Option<String>,
}

impl PluginConfig {
    fn from_value(v: &serde_json::Value) -> Self {
        Self {
            database_url: v.get("database_url")
                .and_then(|v| v.as_str())
                .map(String::from)
                .unwrap_or_else(|| {
                    eprintln!("FATAL: database_url not in configure message");
                    std::process::exit(1);
                }),
            omni_dir: v.get("omni_dir")
                .and_then(|v| v.as_str())
                .map(String::from)
                .unwrap_or_else(|| std::env::var("HOME").map(|h| format!("{}/.omniagent", h)).unwrap_or_default()),
            summarize_after_days: v.get("summarize_after_days").and_then(|v| v.as_i64()).unwrap_or(7),
            channel_summary_tokens: v.get("channel_summary_tokens").and_then(|v| v.as_u64()).map(|v| v as u32).unwrap_or(500),
            summary_provider: v.get("summary_provider").and_then(|v| v.as_str()).map(String::from),
            summary_model: v.get("summary_model").and_then(|v| v.as_str()).map(String::from),
        }
    }
}

// ---------------------------------------------------------------------------
// Tool: generate_summary
// ---------------------------------------------------------------------------

/// Call the LLM via omniagent's provider proxy.
/// The plugin knows only the provider name and model name: no API keys or URLs.
async fn call_proxy_llm(
    agent_url: &str,
    provider: &str,
    model: &str,
    max_tokens: u32,
    system_prompt: &str,
    user_prompt: &str,
) -> Result<String> {
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{}/api/llm/chat", agent_url))
        .json(&serde_json::json!({
            "provider": provider,
            "model": model,
            "messages": [
                {"role": "system", "content": system_prompt},
                {"role": "user", "content": user_prompt}
            ],
            "max_tokens": max_tokens,
            "temperature": 0.2,
        }))
        .send()
        .await
        .context("LLM proxy request failed")?;
    let body: Value = resp.json().await.context("Failed to parse LLM proxy response")?;
    body["content"]
        .as_str()
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow::anyhow!("LLM proxy response missing content: {}", body))
}

/// Generate a cross-thread summary for a channel.
///
/// Called by the omniagent executor after a thread completes. Queries completed
/// threads, builds a summarization prompt, calls the LLM, and saves the result
/// to the database.
///
/// Reads config from plugin config (plugins_yaml::get_plugin): not from env vars.
async fn handle_generate_summary(
    pool: &PgPool,
    config: &PluginConfig,
    args: &Value,
) -> Result<(String, bool)> {
    let window = config.summarize_after_days.max(1);
    let summary_tokens = config.channel_summary_tokens.max(256);

    let (Some(ref provider_name), Some(ref model_name)) = (config.summary_provider.as_ref(), config.summary_model.as_ref()) else {
        return Ok(("Summarization not configured: set summary_provider and summary_model in memory plugin config".to_string(), false));
    };

    if window == 0 {
        return Ok(("Summaries disabled (window=0)".to_string(), false));
    }

    let channel_id = args["channel_id"]
        .as_i64()
        .ok_or_else(|| anyhow::anyhow!("Missing required argument: 'channel_id'"))?;
    let trigger_count = window * 2;

    // 1. Get latest summary's next_thread_id
    let since_id = match queries::get_latest_summary(pool, channel_id).await {
        Ok(Some(summary)) => summary.next_thread_id,
        _ => 0i64,
    };

    // 2. Fetch completed threads since last summary
    let completed_threads = match queries::get_completed_seq0_threads_since(
        pool,
        channel_id,
        since_id,
        trigger_count,
        None,
    )
    .await
    {
        Ok(threads) => threads,
        Err(e) => {
            tracing::warn!(
                "[generate_summary] Failed to fetch completed threads for channel {}: {:?}",
                channel_id, e
            );
            return Ok((
                format!("Failed to fetch threads: {}", e),
                true,
            ));
        }
    };

    if (completed_threads.len() as i64) < trigger_count {
        return Ok((
            format!(
                "Not enough threads: {} < {}",
                completed_threads.len(),
                trigger_count
            ),
            false,
        ));
    }

    let pivot_thread_id = completed_threads[(window - 1) as usize].id;
    let _first_thread_id = completed_threads[0].id;
    let _last_thread_id = completed_threads[(trigger_count - 1) as usize].id;

    // 3. Fetch all messages for each thread
    let mut all_thread_content = String::new();
    for thread_db in &completed_threads {
        match queries::get_thread_messages(pool, thread_db.id).await {
            Ok(thread_msgs) => {
                all_thread_content.push_str(&format!(
                    "\n=== Thread #{} (cause: {} at {}) ===\n",
                    thread_db.id,
                    thread_db.cause,
                    thread_db.created_at.as_deref().unwrap_or("?"),
                ));
                for m in &thread_msgs {
                    let role_display = match m.role.as_str() {
                        "cause" => "User",
                        "agent" => "Assistant",
                        "system" => "System",
                        _ => &m.role,
                    };
                    if m.msg_type == "tool-result" || m.msg_type == "tool" {
                        continue;
                    }
                    all_thread_content.push_str(&format!(
                        "[{}]: {}\n",
                        role_display,
                        m.content.chars().take(1000).collect::<String>()
                    ));
                }
            }
            Err(e) => {
                tracing::warn!(
                    "[generate_summary] Failed to fetch messages for thread {}: {:?}",
                    thread_db.id,
                    e
                );
            }
        }
    }

    // 4. Fetch the last summary for context
    let previous_summary_text = match queries::get_latest_summary(pool, channel_id).await {
        Ok(Some(s)) => s.content,
        _ => String::new(),
    };

    // 5. Build summarization prompt
    let system_summarizer_prompt =
        "You are a conversation summarizer for an autonomous agent system. \
         Produce a structured summary in the exact format below. \
         Be specific: include file paths, config keys, exact numbers, and command names. \
         Do NOT repeat information covered in the previous summary (if provided). \
         Every claim must be grounded in the provided conversation content.\n\n\
         ## Format:\n\
         ### Topics\n\
         - topic: <topic_name> | detail: <one sentence with specifics>\n\n\
         ### Key Decisions\n\
         - decision: <what was decided> | context: <why> | files: <affected files, if any>\n\n\
         ### Action Items\n\
         - status: <done|pending|failed> | task: <what> | details: <specifics>\n\n\
         ### Entities Referenced\n\
         - <entity_name> (<type>): <relation to conversation>\n\n\
         ### Thread Count\n\
         - total: <number> | first: <id> | last: <id>\n\n\
         Keep each entry on a single line. Use | as field separator.";

    let summary_prompt = if previous_summary_text.is_empty() {
        format!(
            "Summarize the following conversations from a single channel.\n\n{}",
            all_thread_content
        )
    } else {
        format!(
            "PREVIOUS SUMMARY (do NOT repeat):\n{}\n\n---\n\n\
             Now summarize the following new conversations, \
             connecting to the previous summary if relevant.\n\n{}",
            previous_summary_text, all_thread_content
        )
    };

    // 6. Call LLM for summary via omniagent proxy
    let agent_url = "http://localhost:8080";
    let summary_content = match call_proxy_llm(
        agent_url,
        provider_name,
        model_name,
        summary_tokens,
        system_summarizer_prompt,
        &summary_prompt,
    )
    .await
    {
        Ok(content) => {
            tracing::info!(
                "[generate_summary] Generated summary for channel {} ({} chars)",
                channel_id,
                content.len()
            );
            content
        }
        Err(e) => {
            tracing::error!(
                "[generate_summary] LLM call failed for channel {}: {:?}",
                channel_id,
                e
            );
            return Ok((format!("LLM call failed: {}", e), true));
        }
    };

    // 7. Save summary to database
    match sqlx::query(
        "INSERT INTO summaries (channel_id, thread_id_start, thread_id_end, next_thread_id, content) \
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(channel_id)
    .bind(completed_threads[0].id)
    .bind(completed_threads[(trigger_count - 1) as usize].id)
    .bind(pivot_thread_id)
    .bind(&summary_content)
    .execute(pool)
    .await
    {
        Ok(_) => {
            tracing::info!(
                "[generate_summary] Saved summary for channel {} (pivot={})",
                channel_id,
                pivot_thread_id
            );
        }
        Err(e) => {
            tracing::error!(
                "[generate_summary] Failed to save summary for channel {}: {:?}",
                channel_id,
                e
            );
            return Ok((format!("Failed to save summary: {}", e), true));
        }
    }

    Ok((
        format!(
            "Summary generated for channel {}: {} threads (pivot={})",
            channel_id, trigger_count, pivot_thread_id
        ),
        false,
    ))
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<()> {
    // Shared state — populated by configure callback before any tool call
    let pool: Arc<RwLock<Option<PgPool>>> = Arc::new(RwLock::new(None));
    let data_dir: Arc<RwLock<Option<String>>> = Arc::new(RwLock::new(None));

    // Wrap each handler to capture shared data_dir
    let dd_promote = data_dir.clone();
    let promote_handler: ToolHandler = Box::new(move |args: Value, _meta: Option<McpMeta>| {
        Box::pin({
            let dd = dd_promote.clone();
            async move {
                let guard = dd.read().await;
                let value = guard.as_ref().expect("data_dir not initialized").clone();
                handle_promote(&value, &args).await
            }
        })
    });

    let dd_list = data_dir.clone();
    let list_handler: ToolHandler = Box::new(move |args: Value, _meta: Option<McpMeta>| {
        Box::pin({
            let dd = dd_list.clone();
            async move {
                let guard = dd.read().await;
                let value = guard.as_ref().expect("data_dir not initialized").clone();
                handle_list(&value, &args).await
            }
        })
    });

    let dd_review = data_dir.clone();
    let review_handler: ToolHandler = Box::new(move |args: Value, _meta: Option<McpMeta>| {
        Box::pin({
            let dd = dd_review.clone();
            async move {
                let guard = dd.read().await;
                let value = guard.as_ref().expect("data_dir not initialized").clone();
                handle_review(&value, &args).await
            }
        })
    });

    let dd_manage = data_dir.clone();
    let manage_handler: ToolHandler = Box::new(move |args: Value, _meta: Option<McpMeta>| {
        Box::pin({
            let dd = dd_manage.clone();
            async move {
                let guard = dd.read().await;
                let value = guard.as_ref().expect("data_dir not initialized").clone();
                handle_manage(&value, &args).await
            }
        })
    });

    // Shared config: populated via configure message from omniagent
    let plugin_config: std::sync::Arc<std::sync::Mutex<PluginConfig>> =
        std::sync::Arc::new(std::sync::Mutex::new(PluginConfig::default()));

    // generate_summary handler: captures pool and config
    let p_summary = pool.clone();
    let cfg_gen = plugin_config.clone();
    let generate_handler: ToolHandler = Box::new(move |args: Value, _meta: Option<McpMeta>| {
        let p = p_summary.clone();
        let cfg = cfg_gen.clone();
        Box::pin(async move {
            let guard = p.read().await;
            let pool = guard.as_ref().expect("Pool not initialized").clone();
            let config = cfg.lock().unwrap().clone();
            handle_generate_summary(&pool, &config, &args).await
        })
    });

    let tools = vec![
        McpToolEntry {
            def: McpToolDef {
                name: "promote_to_memory".to_string(),
                description:
                    "Promote a validated fact to long-term memory by writing it to the wiki. \
                     Memories are stored as markdown files under Memory/Promoted/ with frontmatter \
                     containing provenance, confidence, and expiry information. \
                     Only promote facts that have been directly validated through conversation or tool output. \
                     The memory becomes available for future retrieval via wiki search."
                        .to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "name": {
                            "type": "string",
                            "description": "Short, descriptive name for the memory (used as filename)"
                        },
                        "content": {
                            "type": "string",
                            "description": "The validated fact(s) to store as memory. Be precise and concise."
                        },
                        "confidence": {
                            "type": "string",
                            "enum": ["high", "medium", "low"],
                            "description": "Confidence in the fact's accuracy"
                        },
                        "source_message_ids": {
                            "type": "array",
                            "items": {"type": "integer"},
                            "description": "Message IDs that support this fact from the conversation"
                        },
                        "source_tool_outputs": {
                            "type": "array",
                            "items": {"type": "string"},
                            "description": "Tool call IDs whose outputs provide evidence"
                        },
                        "expires_in_days": {
                            "type": "integer",
                            "description": "Days until this memory expires and needs review (default: 30)"
                        },
                        "profile": {
                            "type": "string",
                            "description": "Profile name for the wiki (default: 'default')"
                        }
                    },
                    "required": ["name", "content", "confidence"]
                }),
            },
            handler: promote_handler,
        },
        McpToolEntry {
            def: McpToolDef {
                name: "list_memories".to_string(),
                description:
                    "List all promoted memory entries in the wiki. Returns filenames, titles, \
                     confidence levels, and expiry dates for each memory."
                        .to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "profile": {
                            "type": "string",
                            "description": "Profile name (default: 'default')"
                        },
                        "include_expired": {
                            "type": "boolean",
                            "description": "Whether to include expired memories (default: false)"
                        }
                    }
                }),
            },
            handler: list_handler,
        },
        McpToolEntry {
            def: McpToolDef {
                name: "review_memories".to_string(),
                description:
                    "Review promoted memory entries for expiry, verifying factual accuracy. \
                     Returns a report of expired or soon-to-expire memories that need \
                     re-validation or renewal."
                        .to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "profile": {
                            "type": "string",
                            "description": "Profile name (default: 'default')"
                        },
                        "expiring_soon_days": {
                            "type": "integer",
                            "description": "Days threshold for 'expiring soon' warning (default: 7)"
                        }
                    }
                }),
            },
            handler: review_handler,
        },
        McpToolEntry {
            def: McpToolDef {
                name: "manage_memory".to_string(),
                description:
                    "Manage profile memory files (MEMORY.md and USER.md). Supports add, remove, and clean \
                     operations on the agent's persistent memory entries. Use on explicit user request only. \
                     'add' prepends a new entry, 'remove' deletes entries matching a substring, \
                     'clean' clears all entries."
                        .to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "target": {
                            "type": "string",
                            "enum": ["memory", "user"],
                            "description": "Which file: 'memory' for MEMORY.md, 'user' for USER.md"
                        },
                        "action": {
                            "type": "string",
                            "enum": ["add", "remove", "clean"],
                            "description": "Operation: 'add' prepends a new entry, 'remove' deletes entries matching substring, 'clean' clears all entries"
                        },
                        "content": {
                            "type": "string",
                            "description": "Content for 'add' action. For 'remove', a substring to match against entries."
                        },
                        "profile": {
                            "type": "string",
                            "description": "Profile name (default: 'default')"
                        }
                    },
                    "required": ["target", "action"]
                }),
            },
            handler: manage_handler,
        },
        McpToolEntry {
            def: McpToolDef {
                name: "generate_summary".to_string(),
                description:
                    "Generate a cross-thread summary for a channel. \
                     Queries completed threads since the last summary, fetches messages, \
                     calls the LLM for structured summarization, and persists the result. \
                     Called automatically by the executor after each thread completes."
                        .to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "channel_id": {
                            "type": "integer",
                            "description": "Channel ID to generate summary for"
                        }
                    },
                    "required": ["channel_id"]
                }),
            },
            handler: generate_handler,
        },
    ];

    let server_info = ServerInfo {
        name: "mcp-server-memory".to_string(),
        version: "0.1.0".to_string(),
    };

    // Set up configure callback that populates plugin config + connects DB
    let cfg_callback = plugin_config.clone();
    let p_pool = pool.clone();
    let dd_dir = data_dir.clone();
    let on_configure = Some(move |params: serde_json::Value| {
        let config = PluginConfig::from_value(&params);
        // Connect to database using config's database_url
        tokio::task::block_in_place(|| {
            let rt = tokio::runtime::Handle::current();
            let new_pool = rt.block_on(db::connect(&config.database_url))
                .expect("Failed to connect to database");
            *p_pool.blocking_write() = Some(new_pool);
            *dd_dir.blocking_write() = Some(config.omni_dir.clone());
        });
        // Store plugin config
        let mut locked = cfg_callback.lock().unwrap();
        *locked = config;
        tracing::info!("Memory plugin configured");
    });

    run_server_with_config(server_info, tools, on_configure).await
}
