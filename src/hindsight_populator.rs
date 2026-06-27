//! Hindsight populator — queries recent messages from the DB and retains them
//! into the omniagent-hindsight persistent memory store.
//!
//! Tracks progress via a watermark file so each run only processes new messages.
//! Designed for cron scheduling via the scheduler's builtin action dispatch
//! or manual triggering via the MCP tool.

use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use std::fs;
use std::path::PathBuf;
use tracing::{info, warn};

use crate::error::{AppResult, ErrorContext};

/// Watermark file stores the last processed message ID so we don't re-process.
const WATERMARK_FILE: &str = "hindsight_watermark.json";
/// Default hindsight bank name.
const DEFAULT_BANK: &str = "omniagent";
/// Default hindsight URL (can be overridden by env var).
const DEFAULT_HINDSIGHT_URL: &str = "http://omniagent-hindsight:8888";
/// Max messages to process per run.
const BATCH_SIZE: usize = 200;
/// Max content length per message before truncation.
const MAX_CONTENT_CHARS: usize = 500;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Watermark {
    /// Last message_id that was successfully retained.
    last_message_id: i64,
    /// ISO timestamp of the last successful run.
    last_run_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct MessageRow {
    id: i64,
    role: String,
    content: String,
    msg_type: String,
    msg_subtype: Option<String>,
    created_at: Option<String>,
}

/// Run the hindsight populator: query new messages, retain them, update watermark.
pub async fn run_hindsight_populator(pool: &PgPool, data_dir: &str) -> AppResult<String> {
    info!("[hindsight-populator] Starting run");

    // ── Resolve hindsight URL ──
    let hindsight_url = std::env::var("HINDSIGHT_URL")
        .unwrap_or_else(|_| DEFAULT_HINDSIGHT_URL.to_string())
        .trim_end_matches('/')
        .to_string();

    // ── Configure the bank (retain scope settings) ──
    if let Err(e) = configure_bank(&hindsight_url).await {
        warn!(
            "[hindsight-populator] Bank config failed (non-fatal): {:?}",
            e
        );
    }

    // ── Read watermark to find where we left off ──
    let watermark_path = PathBuf::from(data_dir).join(WATERMARK_FILE);
    let last_processed_id = read_watermark(&watermark_path).unwrap_or(0);

    // ── Query new messages (excluding low-signal types) ──
    let messages = query_new_messages(pool, last_processed_id).await?;

    if messages.is_empty() {
        info!("[hindsight-populator] No new messages to process");
        update_watermark(&watermark_path, last_processed_id)?;
        return Ok("No new messages to process".to_string());
    }

    let count = messages.len();
    info!("[hindsight-populator] Processing {} new messages", count);

    // ── Prepare retain payload ──
    let items: Vec<serde_json::Value> = messages
        .iter()
        .map(|msg| {
            let content = format_hindsight_content(msg);
            let ts = msg.created_at.as_deref().unwrap_or("");
            let tags = build_tags(msg);
            let context = format!(
                "message from {} in conversation",
                if msg.msg_type == "tool" || msg.msg_type == "tool-result" {
                    format!("tool ({})", msg.msg_subtype.as_deref().unwrap_or("unknown"))
                } else {
                    msg.role.clone()
                }
            );

            serde_json::json!({
                "content": content,
                "context": context,
                "document_id": format!("msg:{}", msg.id),
                "tags": tags,
                "timestamp": if ts.is_empty() { serde_json::Value::Null } else { serde_json::json!(ts) },
                "update_mode": "replace",
                "strategy": "fast"
            })
        })
        .collect();

    // ── Retain in batches ──
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .ctx("Failed to build HTTP client")?;

    // Process in sub-batches of 50
    for chunk in items.chunks(50) {
        let retain_url = format!(
            "{}/v1/default/banks/{}/memories",
            hindsight_url, DEFAULT_BANK
        );

        let payload = serde_json::json!({
            "items": chunk,
            "async": false,  // wait for completion so we can track watermark
        });

        match client.post(&retain_url).json(&payload).send().await {
            Ok(resp) if resp.status().is_success() => {
                let batch_count = chunk.len();
                info!("[hindsight-populator] Retained {} memories", batch_count);
            }
            Ok(resp) => {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                warn!(
                    "[hindsight-populator] Retain returned HTTP {}: {}",
                    status,
                    &body[..body.len().min(200)]
                );
                // Continue processing — partial success still progresses watermark
            }
            Err(e) => {
                warn!("[hindsight-populator] Retain request failed: {:?}", e);
                // Continue processing — don't fail the whole run
            }
        }
    }

    // ── Get the highest message ID from this batch ──
    let max_id = messages.iter().map(|m| m.id).max().unwrap_or(0);
    let new_watermark = last_processed_id.max(max_id);

    // ── Update watermark ──
    update_watermark(&watermark_path, new_watermark)?;

    // ── Trigger consolidation (best-effort) ──
    if let Err(e) = trigger_consolidation(&hindsight_url).await {
        warn!(
            "[hindsight-populator] Consolidation trigger failed (non-fatal): {:?}",
            e
        );
    }

    let summary = format!(
        "Hindsight populator complete: retained {} messages (watermark: {} → {})",
        count, last_processed_id, new_watermark
    );
    info!("[hindsight-populator] {}", summary);

    Ok(summary)
}

/// Format message content for hindsight retention.
fn format_hindsight_content(msg: &MessageRow) -> String {
    match msg.msg_type.as_str() {
        "tool" | "tool-result" | "multi-tool" => {
            let tool_name = msg.msg_subtype.as_deref().unwrap_or("unknown");
            let preview = msg
                .content
                .chars()
                .take(MAX_CONTENT_CHARS)
                .collect::<String>();
            format!("[Tool: {}] {}", tool_name, preview)
        }
        _ => msg
            .content
            .chars()
            .take(MAX_CONTENT_CHARS)
            .collect::<String>(),
    }
}

/// Build tags for a message based on its type and role.
fn build_tags(msg: &MessageRow) -> Vec<String> {
    let mut tags = vec![msg.role.clone(), msg.msg_type.clone()];

    if let Some(ref subtype) = msg.msg_subtype {
        tags.push(subtype.clone());
    }

    // Add role-based tag
    match msg.role.as_str() {
        "cause" => tags.push("from_user".to_string()),
        "agent" => tags.push("from_agent".to_string()),
        "system" => tags.push("system".to_string()),
        "tool" => tags.push("tool_call".to_string()),
        _ => {}
    }

    tags
}

/// Query new messages from the DB after the given watermark ID.
async fn query_new_messages(pool: &PgPool, last_id: i64) -> AppResult<Vec<MessageRow>> {
    // We query with msg_type filtering — exclude low-signal types:
    // - 'cron' (cron job metadata)
    // - 'tool_call' (the LLM's tool call instructions, not results)
    // But include: 'message', 'reasoning', 'plan', 'error', 'cause', 'tool', 'tool-result', 'summary', 'system'
    let rows = sqlx::query_as::<_, (i64, String, String, String, Option<String>, Option<String>)>(
        r#"
        SELECT m.id, m.role, m.content, m.msg_type, m.msg_subtype,
               COALESCE(TO_CHAR(m.created_at, 'YYYY-MM-DD"T"HH24:MI:SS"Z"'), '') AS created_at
        FROM messages m
        WHERE m.id > $1
          AND m.msg_type IN ('message', 'reasoning', 'plan', 'error', 'cause', 'tool', 'tool-result', 'summary', 'system')
          AND COALESCE(m.content, '') != ''
        ORDER BY m.id ASC
        LIMIT $2
        "#,
    )
    .bind(last_id)
    .bind(BATCH_SIZE as i64)
    .fetch_all(pool)
    .await
    .ctx("Failed to query new messages")?;

    Ok(rows
        .into_iter()
        .map(
            |(id, role, content, msg_type, msg_subtype, created_at)| MessageRow {
                id,
                role,
                content,
                msg_type,
                msg_subtype,
                created_at,
            },
        )
        .collect())
}

/// Read the watermark file, returning the last processed message ID.
fn read_watermark(path: &PathBuf) -> Option<i64> {
    if !path.exists() {
        return None;
    }
    match fs::read_to_string(path) {
        Ok(content) => serde_json::from_str::<Watermark>(&content)
            .ok()
            .map(|w| w.last_message_id),
        Err(_) => None,
    }
}

/// Update the watermark file with the latest processed message ID.
fn update_watermark(path: &PathBuf, last_message_id: i64) -> AppResult<()> {
    let now_iso = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let watermark = Watermark {
        last_message_id,
        last_run_at: now_iso,
    };
    let content =
        serde_json::to_string_pretty(&watermark).ctx("Failed to serialize watermark")?;
    fs::write(path, &content)
        .ctx(format!("Failed to write watermark to {}", path.display()))?;
    Ok(())
}

/// Configure the hindsight bank: set retain scope, custom instructions, etc.
async fn configure_bank(hindsight_url: &str) -> AppResult<()> {
    let config_url = format!(
        "{}/v1/default/banks/{}/config",
        hindsight_url.trim_end_matches('/'),
        DEFAULT_BANK
    );

    // Read current config first
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .ctx("Failed to build HTTP client for config")?;

    // Set bank overrides — skip tool output dumps, use fast strategy
    let updates = serde_json::json!({
        "updates": {
            "retain_custom_instructions": "Skip tool output dumps and parameter dumps. Focus on user intent, decisions, outcomes, and factual information from conversations.",
            "retain_extraction_mode": "fast",
        }
    });

    match client.patch(&config_url).json(&updates).send().await {
        Ok(resp) if resp.status().is_success() => {
            info!("[hindsight-populator] Bank config updated successfully");
            Ok(())
        }
        Ok(resp) => {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            // 404 means the bank might not exist yet — still proceed
            if status == 404 {
                info!("[hindsight-populator] Bank not found, will be auto-created on first retain");
                return Ok(());
            }
            warn!(
                "[hindsight-populator] Bank config update returned HTTP {}: {}",
                status,
                &body[..body.len().min(200)]
            );
            Ok(()) // non-fatal
        }
        Err(e) => {
            warn!("[hindsight-populator] Bank config update failed: {:?}", e);
            Ok(()) // non-fatal
        }
    }
}

/// Trigger consolidation on the hindsight bank (best-effort).
async fn trigger_consolidation(hindsight_url: &str) -> AppResult<()> {
    let consolidate_url = format!(
        "{}/v1/default/banks/{}/consolidate",
        hindsight_url.trim_end_matches('/'),
        DEFAULT_BANK
    );

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .ctx("Failed to build HTTP client for consolidation")?;

    match client.post(&consolidate_url).send().await {
        Ok(resp) if resp.status().is_success() => {
            info!("[hindsight-populator] Consolidation triggered");
            Ok(())
        }
        Ok(resp) => {
            info!(
                "[hindsight-populator] Consolidation returned HTTP {} (expected if already running)",
                resp.status()
            );
            Ok(())
        }
        Err(e) => {
            warn!(
                "[hindsight-populator] Consolidation request failed: {:?}",
                e
            );
            Ok(())
        }
    }
}
