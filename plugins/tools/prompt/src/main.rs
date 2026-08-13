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

#![allow(dead_code, unused_imports)]

use anyhow::{Context, Result};
use sql_forge::sql_forge;
mod chat_message;
mod compact;
mod dump;
mod memory_store;
mod notes;
mod prompt_builder;

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
    // Compact excerpts (plugin config, no hardcoded limits)
    pub tool_excerpt_chars: usize,
    pub total_excerpt_cap: usize,
    pub read_excerpt_chars: usize,
    pub compact_keep_recent: usize,
    pub compact_max_passes: usize,
    pub compact_keep_step: usize,
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
            char_budget_soft: 100000,
            char_budget_hard: 200000,
            token_budget_soft: 100000,
            token_budget_hard: 200000,
            old_msg_budget: 100000,
            condense_keep_turns: 4,
            tool_excerpt_chars: 800,
            total_excerpt_cap: 4000,
            read_excerpt_chars: 2000,
            compact_keep_recent: 3,
            compact_max_passes: 3,
            compact_keep_step: 1,
            memory_max_chars: 5000,
            soul_max_chars: 1000,
        }
    }

    /// Parse config from the JSON value sent by the configure message.
    /// Unknown keys are silently ignored; missing keys keep defaults.
    ///
    /// The omniagent sends ALL config values as JSON strings (it builds the
    /// configure payload from a HashMap<String,String>), so numeric fields
    /// must be parsed leniently: accept both a real JSON number and a
    /// numeric string ("200000"). Previously `as_i64()` was used, which
    /// returns None for strings — silently dropping every configured budget
    /// and forcing the plugin to run on defaults forever.
    fn from_json(json: &Value) -> Self {
        let mut cfg = Self::default();
        let as_usize = |v: &Value| {
            v.as_u64()
                .or_else(|| v.as_str().and_then(|s| s.parse::<u64>().ok()))
                .map(|n| n as usize)
        };
        if let Some(obj) = json.as_object() {
            if let Some(v) = obj.get("database_url").and_then(|v| v.as_str()) {
                cfg.database_url = v.to_string();
            }
            if let Some(v) = obj.get("omni_dir").and_then(|v| v.as_str()) {
                cfg.omni_dir = v.to_string();
            }
            if let Some(v) = obj.get("planning_complexity_max_chars").and_then(&as_usize) {
                cfg.planning_complexity_max_chars = v;
            }
            if let Some(v) = obj
                .get("planning_complexity_keywords")
                .and_then(|v| v.as_str())
            {
                cfg.planning_complexity_keywords = v.to_string();
            }
            if let Some(v) = obj.get("prompt_plan_max_tokens").and_then(&as_usize) {
                cfg.prompt_plan_max_tokens = v;
            }
            if let Some(v) = obj.get("tokenizer_encoding").and_then(|v| v.as_str()) {
                cfg.tokenizer_encoding = v.to_string();
            }
            if let Some(v) = obj.get("char_budget_soft").and_then(&as_usize) {
                cfg.char_budget_soft = v;
            }
            if let Some(v) = obj.get("char_budget_hard").and_then(&as_usize) {
                cfg.char_budget_hard = v;
            }
            if let Some(v) = obj.get("tool_excerpt_chars").and_then(&as_usize) {
                cfg.tool_excerpt_chars = v;
            }
            if let Some(v) = obj.get("total_excerpt_cap").and_then(&as_usize) {
                cfg.total_excerpt_cap = v;
            }
            if let Some(v) = obj.get("read_excerpt_chars").and_then(&as_usize) {
                cfg.read_excerpt_chars = v;
            }
            if let Some(v) = obj.get("compact_keep_recent").and_then(&as_usize) {
                cfg.compact_keep_recent = v;
            }
            if let Some(v) = obj.get("compact_max_passes").and_then(&as_usize) {
                cfg.compact_max_passes = v.max(1);
            }
            if let Some(v) = obj.get("compact_keep_step").and_then(&as_usize) {
                cfg.compact_keep_step = v.max(1);
            }
            if let Some(v) = obj.get("token_budget_soft").and_then(&as_usize) {
                cfg.token_budget_soft = v;
            }
            if let Some(v) = obj.get("token_budget_hard").and_then(&as_usize) {
                cfg.token_budget_hard = v;
            }
            if let Some(v) = obj.get("old_message_char_budget").and_then(&as_usize) {
                cfg.old_msg_budget = v;
            }
            if let Some(v) = obj.get("condense_keep_turns").and_then(&as_usize) {
                cfg.condense_keep_turns = v.max(1);
            }
            if let Some(v) = obj.get("memory_max_chars").and_then(&as_usize) {
                cfg.memory_max_chars = v;
            }
            if let Some(v) = obj.get("soul_max_chars").and_then(&as_usize) {
                cfg.soul_max_chars = v;
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
            // channel ids are strings now (channel NAMES).
            "channel_id" => None,
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
    channel_id: String,
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
    let rows = sql_forge!(
        MessageRow,
        r#"
        SELECT id, thread_id, role, content, msg_type, msg_subtype,
               COALESCE(TO_CHAR(created_at, 'YYYY-MM-DD"T"HH24' || CHR(58) || 'MI' || CHR(58) || 'SS.US"Z"'), '') AS created_at
        FROM messages
        WHERE thread_id = :thread_id
          AND (role = 'cause' OR msg_type IN ('message', 'reasoning'))
        ORDER BY created_at DESC
        LIMIT :limit
        "#,
        (
            :thread_id = thread_id,
            :limit = limit,
        )
    )
    .fetch_all(pool)
    .await
    .context("Failed to fetch thread messages")?;

    Ok(rows)
}

async fn get_latest_summary(pool: &PgPool, channel_id: &str) -> Result<Option<SummaryRow>> {
    let row = sql_forge!(
        SummaryRow,
        r#"
        SELECT id, channel_id, next_thread_id, content
        FROM summaries
        WHERE channel_id = :channel_id
        ORDER BY id DESC
        LIMIT 1
        "#,
        ( :channel_id = &channel_id )
    )
    .fetch_optional(pool)
    .await
    .context("Failed to fetch latest summary")?;

    Ok(row)
}

async fn get_threads_since(
    pool: &PgPool,
    channel_id: &str,
    since_id: i64,
    limit: i64,
) -> Result<Vec<ThreadRow>> {
    let rows = sql_forge!(
        ThreadRow,
        r#"
        SELECT id, status, cause
        FROM threads
        WHERE channel_id = :channel_id
          AND status = 'completed'
          AND id > :since_id
        ORDER BY id ASC
        LIMIT :limit
        "#,
        (
            :channel_id = &channel_id,
            :since_id = since_id,
            :limit = limit,
        )
    )
    .fetch_all(pool)
    .await
    .context("Failed to fetch completed threads")?;

    Ok(rows)
}

// ---------------------------------------------------------------------------
// R8-J: prior attempts of the SAME task — the prompt must pass past context.
// Earlier threads of this kanban task (ALL statuses, not just completed)
// with their final summaries, so a successor knows what prior attempts did
// and why they died. Threads 113/138/140/155 each burned 100-120 calls
// re-deriving the same harness knowledge because this context never reached
// them — 6 budget-deaths total.
// ---------------------------------------------------------------------------

const PRIOR_ATTEMPTS_MAX_ENTRIES: usize = 5;
const PRIOR_ATTEMPTS_MAX_SUMMARY_CHARS: usize = 800;

#[derive(Debug, FromRow)]
struct PriorAttemptRow {
    id: i64,
    status: String,
    iterations: Option<i32>,
    ended_at: Option<String>,
}

/// Prior threads of the SAME kanban task (R8-J): ALL statuses (completed /
/// interrupted / failed / ...), newest first, capped at `limit`. Earlier
/// attempts hold exactly the knowledge a successor needs — what was done,
/// what died, why — but `get_threads_since` filters status='completed' and
/// is therefore deliberately NOT reused here.
async fn get_prior_threads_by_task(
    pool: &PgPool,
    task_id: &str,
    current_thread_id: i64,
    limit: i64,
) -> Result<Vec<PriorAttemptRow>> {
    let rows = sql_forge!(
        PriorAttemptRow,
        r#"
        SELECT id, status, iterations,
               TO_CHAR(ended_at, 'YYYY-MM-DD HH24' || CHR(58) || 'MI') AS ended_at
        FROM threads
        WHERE task_id = :task_id
          AND (task_type = 'kanban' OR task_type IS NULL)
          AND id < :current_thread_id
        ORDER BY id DESC
        LIMIT :limit
        "#,
        (
            :task_id = task_id,
            :current_thread_id = current_thread_id,
            :limit = limit,
        )
    )
    .fetch_all(pool)
    .await
    .context("Failed to fetch prior threads of task")?;

    Ok(rows)
}

/// The thread's final summary message (msg_type='summary') if it produced
/// one — this is the report a thread leaves behind for its successor.
async fn get_thread_summary(pool: &PgPool, thread_id: i64) -> Result<Option<String>> {
    let row = sql_forge!(
        scalar String,
        "SELECT content FROM messages WHERE thread_id = :thread_id AND msg_type = 'summary' ORDER BY id DESC LIMIT 1",
        ( :thread_id = thread_id )
    )
    .fetch_optional(pool)
    .await
    .context("Failed to fetch thread summary")?;

    Ok(row)
}

/// Pure renderer for the prior-attempts block (R8-J). Every prior thread is
/// listed with id/status/iterations/ended_at plus its summary excerpt when
/// one exists — a thread with NO summary is still listed (its very existence
/// warns the successor). Returns None when there are no prior threads.
fn render_prior_attempts_block(
    rows: Vec<PriorAttemptRow>,
    summaries: &std::collections::HashMap<i64, Option<String>>,
) -> Option<String> {
    let mut rows = rows;
    rows.sort_by_key(|r| std::cmp::Reverse(r.id));
    rows.truncate(PRIOR_ATTEMPTS_MAX_ENTRIES);
    if rows.is_empty() {
        return None;
    }
    let mut lines = vec![
        "=== Previous attempts of this task (READ — do NOT repeat what they did) ===".to_string(),
    ];
    for r in &rows {
        let iterations = r
            .iterations
            .map(|i| i.to_string())
            .unwrap_or_else(|| "?".to_string());
        let ended_at = r.ended_at.as_deref().unwrap_or("?");
        let summary = summaries.get(&r.id).and_then(|s| s.as_deref());
        match summary {
            Some(s) => lines.push(format!(
                "- thread {} | status {} | iterations {} | ended_at {} | summary: {}",
                r.id,
                r.status,
                iterations,
                ended_at,
                truncate_str(s, PRIOR_ATTEMPTS_MAX_SUMMARY_CHARS)
            )),
            None => lines.push(format!(
                "- thread {} | status {} | iterations {} | ended_at {} | (no summary message)",
                r.id, r.status, iterations, ended_at
            )),
        }
    }
    Some(lines.join("\n"))
}

/// Build the prior-attempts block for the current thread (R8-J): earlier
/// threads of the SAME kanban task, all statuses, with their summaries.
/// Plain (non-task) threads get no block; any failure degrades gracefully
/// to no block (the prompt must never fail because history is unavailable).
async fn build_prior_attempts_block(pool: &PgPool, thread_id: i64) -> Result<Option<String>> {
    let task_id = sql_forge!(
        scalar Option<String>,
        "SELECT task_id FROM threads WHERE id = :thread_id",
        ( :thread_id = thread_id )
    )
    .fetch_optional(pool)
    .await
    .context("Failed to fetch thread task_id")?;
    let Some(Some(task_id)) = task_id else {
        return Ok(None); // plain thread — no same-task history
    };

    let rows =
        get_prior_threads_by_task(pool, &task_id, thread_id, PRIOR_ATTEMPTS_MAX_ENTRIES as i64)
            .await?;
    if rows.is_empty() {
        return Ok(None);
    }
    let mut summaries = std::collections::HashMap::new();
    for r in &rows {
        summaries.insert(
            r.id,
            get_thread_summary(pool, r.id).await.unwrap_or_default(),
        );
    }
    Ok(render_prior_attempts_block(rows, &summaries))
}
// ---------------------------------------------------------------------------
// R8-K: learned knowledge - read side of the learning loop. Write side:
// memory_promote-to-memory writes <data_dir>/profiles/<profile>/wiki/Memory/
// Promoted/*.md; WITHOUT this read-back the loop is write-only and every
// successor thread re-derives the same knowledge (6 threads died doing that).

const LEARNED_KNOWLEDGE_MAX_ENTRY_CHARS: usize = 600;
const LEARNED_KNOWLEDGE_MAX_TOTAL_CHARS: usize = 3000;

/// A single promoted memory: title (filename minus .md) + body (frontmatter
/// stripped, truncated).
struct LearnedMemory {
    title: String,
    body: String,
    mtime: Option<std::time::SystemTime>,
}

/// Strip YAML frontmatter: everything between a leading `---` line and the
/// closing `---` line is metadata; the durable body is what follows.
fn strip_frontmatter(content: &str) -> &str {
    let trimmed = content.trim_start();
    if let Some(after_open) = trimmed.strip_prefix("---") {
        if let Some(idx) = after_open.find("\n---") {
            return after_open[idx + 4..].trim_start();
        }
        return trimmed; // unterminated fence - treat whole doc as body
    }
    trimmed
}

/// Read promoted memories from <data_dir>/profiles/<profile>/wiki/Memory/
/// Promoted/*.md, newest first by mtime. Missing dir / unreadable files are
/// skipped silently - the learning loop must never fail the prompt.
fn load_promoted_memories(data_dir: &str, profile_name: &str) -> Vec<LearnedMemory> {
    let dir = std::path::Path::new(data_dir)
        .join("profiles")
        .join(profile_name)
        .join("wiki")
        .join("Memory")
        .join("Promoted");
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };
    let mut memories = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let name = entry.file_name().to_string_lossy().to_string();
        let title = name.strip_suffix(".md").unwrap_or(&name).to_string();
        let body = truncate_str(
            strip_frontmatter(&content).trim(),
            LEARNED_KNOWLEDGE_MAX_ENTRY_CHARS,
        );
        let mtime = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
        memories.push(LearnedMemory { title, body, mtime });
    }
    memories.sort_by_key(|b| std::cmp::Reverse(b.mtime));
    memories
}

/// Render the Learned Knowledge context block. Total body chars capped at
/// LEARNED_KNOWLEDGE_MAX_TOTAL_CHARS; entries beyond the cap are dropped.
fn render_learned_knowledge_block(memories: &[LearnedMemory]) -> String {
    let header = "=== Learned Knowledge (promoted memories from prior threads - READ before acting; these are validated facts, do not re-derive them) ===";
    let mut parts = vec![header.to_string()];
    let mut total = 0usize;
    for m in memories {
        let entry = format!("- **{}**: {}", m.title, m.body);
        if total + entry.len() > LEARNED_KNOWLEDGE_MAX_TOTAL_CHARS && total > 0 {
            break;
        }
        total += entry.len();
        parts.push(entry);
    }
    parts.join("\n")
}

/// Build the Learned Knowledge block for this profile. When no promoted
/// memories exist yet, emit a short hint that teaches the agent the loop
/// exists (write side: memory_promote-to-memory). Never fails the prompt.
fn build_learned_knowledge_block(data_dir: &str, profile_name: &str) -> Option<String> {
    let memories = load_promoted_memories(data_dir, profile_name);
    if memories.is_empty() {
        return Some(
            "=== Learned Knowledge === (none yet - after completing this task, promote what you learned via memory_promote-to-memory so future threads benefit)"
                .to_string(),
        );
    }
    Some(render_learned_knowledge_block(&memories))
}

/// Count prior threads of THIS task that ended 'interrupted'. Returns None
/// for non-task threads (no task_id) or when the lookups fail.
async fn count_interrupted_attempts(
    pool: &PgPool,
    thread_id: i64,
) -> Result<Option<i64>, anyhow::Error> {
    let task_id = sql_forge!(
        scalar Option<String>,
        "SELECT task_id FROM threads WHERE id = :thread_id",
        ( :thread_id = thread_id )
    )
    .fetch_optional(pool)
    .await?;
    let task_id = match task_id {
        Some(Some(t)) => t,
        _ => return Ok(None),
    };
    let count: i64 = sql_forge!(
        scalar i64,
        "SELECT COUNT(*) FROM threads WHERE task_id = :task_id AND status = 'interrupted'",
        ( :task_id = task_id )
    )
    .fetch_one(pool)
    .await?;
    Ok(Some(count))
}

// ---------------------------------------------------------------------------
async fn get_subtasks(pool: &PgPool, thread_id: i64) -> Result<Vec<SubtaskRow>> {
    let rows = sql_forge!(
        SubtaskRow,
        r#"
        SELECT id, description, status, thread_id
        FROM thread_subtasks
        WHERE thread_id = :thread_id
        ORDER BY id ASC
        "#,
        ( :thread_id = thread_id )
    )
    .fetch_all(pool)
    .await
    .context("Failed to fetch subtasks")?;

    Ok(rows)
}

// ---------------------------------------------------------------------------
// Continuation self-orientation: prior threads of the same task, kanban
// history, and resume-ledger pointer. Built only for threads linked to a
// kanban task (task_id) or cron schedule (schedule_task_id).
// ---------------------------------------------------------------------------

#[derive(Debug, FromRow)]
struct ThreadTaskRef {
    task_id: Option<String>,
    schedule_task_id: Option<String>,
}

#[derive(Debug, FromRow)]
struct PriorThreadRow {
    id: i64,
    status: String,
    workflow_step: Option<String>,
}

#[derive(Debug, FromRow)]
struct KanbanHistoryRow {
    action: String,
    initial_board: Option<String>,
    final_board: Option<String>,
    comment: Option<String>,
    created_at: Option<String>,
}

#[derive(Debug, FromRow)]
struct KanbanBodyRow {
    body: Option<String>,
}

#[derive(Debug, FromRow)]
struct LastMessageRow {
    content: String,
    msg_type: Option<String>,
}

/// Last message of a thread (spec: last_message = LAST message in thread; type =
/// messages.msg_type). For a successful step-thread this is normally the thread
/// summary; for a failed one it is the fail message (Error type).
async fn last_message_info(pool: &PgPool, thread_id: i64) -> anyhow::Result<LastMessageRow> {
    Ok(    sql_forge!(
        LastMessageRow,
        "SELECT content, msg_type FROM messages WHERE thread_id = :thread_id ORDER BY id DESC LIMIT 1",
        ( :thread_id = thread_id )
    )
    .fetch_optional(pool)
    .await?
    .unwrap_or(LastMessageRow {
        content: "<no messages>".to_string(),
        msg_type: None,
    }))
}

/// Pull a resume-ledger path (e.g. /opt/omni/data/tasks/<name>.md) out of a
/// task body if one is referenced.
// ---------------------------------------------------------------------------
// Cross-task channel context (Phase 3b-ext): the most recent TERMINAL threads
// of OTHER tasks that ran on the SAME channel as the current thread.
// Automates the manual ORIENT step 4 of the dev template - a multi-phase
// project implements each phase as its own kanban task on the same channel,
// and a prior phase's thread often already solved the exact problem being
// investigated (canonical build commands, error signatures, root causes,
// what changed where). This block lets the agent trust those results instead
// of re-deriving them. Task-linked threads only; omitted when nothing
// qualifies.
//
// SELECTION RULE (documented; kept simple and testable):
//   - same channel_id as the current thread
//   - exclude the current thread and every thread whose task_id equals the
//     current thread's own task_id OR schedule_task_id (own-task history is
//     already covered by the Phase 3b continuation block)
//   - terminal statuses only: completed / review / failed / interrupted /
//     skipped, final message = highest thread_sequence row
//   - recency ordering (most recent thread first), hard cap of 3 entries,
//     400-char cap on the final-message excerpt, 60-char cap on task titles
// ---------------------------------------------------------------------------
const CROSS_TASK_MAX_ENTRIES: usize = 3;
const CROSS_TASK_MAX_MESSAGE_CHARS: usize = 400;
const CROSS_TASK_MAX_TITLE_CHARS: usize = 60;

/// The current thread's own task linkage (kanban `task_id` and/or cron
/// `schedule_task_id`) - used to exclude own-task threads from the block.
#[derive(Debug, Clone, PartialEq, Eq)]
struct OwnTaskLink {
    task_id: Option<String>,
    schedule_task_id: Option<String>,
}

impl OwnTaskLink {
    /// A thread is task-linked when it belongs to a kanban task or a cron
    /// schedule. Plain threads get no cross-task block (existing behavior).
    fn is_task_linked(&self) -> bool {
        self.task_id.is_some() || self.schedule_task_id.is_some()
    }

    /// True when `other_task_id` identifies the current thread's own task.
    fn excludes(&self, other_task_id: &str) -> bool {
        self.task_id.as_deref() == Some(other_task_id)
            || self.schedule_task_id.as_deref() == Some(other_task_id)
    }
}

#[derive(Debug, FromRow)]
struct CrossTaskThreadRef {
    task_id: Option<String>,
    schedule_task_id: Option<String>,
    channel_id: Option<String>,
}

#[derive(Debug, FromRow)]
struct CrossTaskThreadRow {
    id: i64,
    task_id: Option<String>,
    task_title: Option<String>,
    workflow_step: Option<String>,
    status: String,
    last_content: Option<String>,
    last_msg_type: Option<String>,
}

/// Pure renderer for the cross-task block: drops own-task threads, orders by
/// most recent thread first, caps the entry count, truncates titles and
/// messages, and emits the labeled section. Returns `None` when nothing
/// qualifies. Kept free of I/O so the selection logic is unit-testable
/// in-process (tests a-e below).
fn render_cross_task_block(own: &OwnTaskLink, rows: Vec<CrossTaskThreadRow>) -> Option<String> {
    let mut entries: Vec<CrossTaskThreadRow> = rows
        .into_iter()
        .filter(|r| {
            r.task_id
                .as_deref()
                .map(|tid| !own.excludes(tid))
                .unwrap_or(false)
        })
        .collect();
    entries.sort_by_key(|r| std::cmp::Reverse(r.id));
    entries.truncate(CROSS_TASK_MAX_ENTRIES);
    if entries.is_empty() {
        return None;
    }
    let mut lines = vec![
        "Recent threads from other tasks on this channel (background context from sibling tasks, not your own history): a prior phase on this channel often already solved the exact problem you are investigating - trust its final result instead of re-deriving it."
            .to_string(),
    ];
    for r in &entries {
        let task = match (&r.task_id, &r.task_title) {
            (Some(tid), Some(title)) => format!(
                "task {} \"{}\"",
                tid,
                truncate_str(title, CROSS_TASK_MAX_TITLE_CHARS)
            ),
            (Some(tid), None) => format!("task {}", tid),
            (None, _) => "plain thread".to_string(),
        };
        let step = r.workflow_step.as_deref().unwrap_or("-");
        let msg_type = r.last_msg_type.as_deref().unwrap_or("text");
        let content = r.last_content.as_deref().unwrap_or("<no messages>");
        lines.push(format!(
            "- thread {} | {} | step {} | status {} | last message ({}): {}",
            r.id,
            task,
            step,
            r.status,
            msg_type,
            truncate_str(content, CROSS_TASK_MAX_MESSAGE_CHARS)
        ));
    }
    Some(lines.join("\n"))
}

/// Build the cross-task channel context block for the given thread. Returns
/// `None` for plain (non-task) threads and when the channel has no other
/// terminal task threads. One bounded query; never fails the prompt.
async fn build_cross_task_block(pool: &PgPool, thread_id: i64) -> anyhow::Result<Option<String>> {
    let thread_ref: Option<CrossTaskThreadRef> = sql_forge!(
        CrossTaskThreadRef,
        "SELECT task_id, schedule_task_id, channel_id FROM threads WHERE id = :thread_id",
        ( :thread_id = thread_id )
    )
    .fetch_optional(pool)
    .await?;
    let thread_ref = match thread_ref {
        Some(tr) => tr,
        None => return Ok(None),
    };
    let own = OwnTaskLink {
        task_id: thread_ref.task_id,
        schedule_task_id: thread_ref.schedule_task_id,
    };
    if !own.is_task_linked() {
        return Ok(None); // plain thread - cross-task context does not apply
    }
    let channel_id = match thread_ref.channel_id {
        Some(cid) => cid,
        None => return Ok(None),
    };

    let rows: Vec<CrossTaskThreadRow> = sql_forge!(
        CrossTaskThreadRow,
        r#"
        SELECT t.id,
               t.task_id,
               k.title        AS task_title,
               t.workflow_step,
               t.status,
               m.content      AS last_content,
               m.msg_type     AS last_msg_type
        FROM threads t
        LEFT JOIN kanban_tasks k ON k.id = t.task_id
        LEFT JOIN LATERAL (
            SELECT content, msg_type
            FROM messages
            WHERE thread_id = t.id
            ORDER BY thread_sequence DESC NULLS LAST,
                     iteration_number DESC NULLS LAST,
                     id DESC
            LIMIT 1
        ) m ON true
        WHERE t.channel_id = :channel_id
          AND t.id != :thread_id
          AND t.task_id IS NOT NULL
          AND (NULLIF(:task_id, '')::text IS NULL OR t.task_id != NULLIF(:task_id, '')::text)
          AND (NULLIF(:schedule_task_id, '')::text IS NULL OR t.task_id != NULLIF(:schedule_task_id, '')::text)
          AND t.status IN ('completed', 'review', 'failed', 'interrupted', 'skipped')
        ORDER BY t.id DESC
        LIMIT :i64
        "#,
        (
            :channel_id = &channel_id,
            :thread_id = thread_id,
            :task_id = own.task_id.as_deref().unwrap_or(""),
            :schedule_task_id = own.schedule_task_id.as_deref().unwrap_or(""),
            :i64 = CROSS_TASK_MAX_ENTRIES as i64,
        )
    )
    .fetch_all(pool)
    .await?;

    Ok(render_cross_task_block(&own, rows))
}

fn extract_tracking_path(body: &str) -> Option<String> {
    let pos = body.find("data/tasks/")?;
    let start = body[..pos]
        .rfind(|c: char| c.is_whitespace() || matches!(c, '`' | '"' | '\'' | '(' | '[' | ','))
        .map(|i| i + 1)
        .unwrap_or(0);
    let after = &body[pos + "data/tasks/".len()..];
    let end = after
        .find(|c: char| c.is_whitespace() || matches!(c, '`' | '"' | '\'' | ')' | ']' | ','))
        .unwrap_or(after.len());
    if end == 0 {
        return None;
    }
    Some(format!(
        "{}{}",
        &body[start..pos + "data/tasks/".len()],
        &after[..end]
    ))
}

/// Build the continuation self-orientation block for a thread. Returns None
/// (no-op) for threads without a task linkage; a failing sub-query degrades
/// gracefully by simply omitting that sub-part — never fails the prompt.
async fn build_continuation_block(pool: &PgPool, thread_id: i64) -> anyhow::Result<Option<String>> {
    let thread_ref: Option<ThreadTaskRef> = sql_forge!(
        ThreadTaskRef,
        "SELECT task_id, schedule_task_id FROM threads WHERE id = :thread_id",
        ( :thread_id = thread_id )
    )
    .fetch_optional(pool)
    .await?;
    let Some(thread_ref) = thread_ref else {
        return Ok(None);
    };

    let mut blocks: Vec<String> = Vec::new();

    // Role instructions for the CURRENT thread's workflow step (executor /
    // tester / reviewer) plus thread-access rules (R11).
    let step_row: Option<WorkflowStepRow> = sql_forge!(
        WorkflowStepRow,
        "SELECT workflow_id, workflow_step FROM threads WHERE id = :thread_id",
        ( :thread_id = thread_id )
    )
    .fetch_optional(pool)
    .await?;
    if let Some(step) = step_row.and_then(|r| r.workflow_step) {
        if let Some(role_block) = build_role_block(&step) {
            blocks.push(role_block);
        }
    }

    // Prior step-threads of this task (chronological): thread, workflow_step,
    // terminal status, last message + its type. Legacy threads created before
    // the task_type backfill are treated as kanban (task_type IS NULL).
    let mut prior_threads: Vec<PriorThreadRow> = Vec::new();
    if let Some(task_id) = &thread_ref.task_id {
        let rows =         sql_forge!(
            PriorThreadRow,
            "SELECT id, status, workflow_step FROM threads WHERE task_id = :task_id AND (task_type = 'kanban' OR task_type IS NULL) AND id != :thread_id ORDER BY id DESC LIMIT 8",
            (
                :task_id = task_id,
                :thread_id = thread_id,
            )
        )
        .fetch_all(pool)
        .await?;
        prior_threads.extend(rows);
    }
    if let Some(schedule_task_id) = &thread_ref.schedule_task_id {
        // Legacy cron threads carry the schedule id in schedule_task_id.
        let rows =         sql_forge!(
            PriorThreadRow,
            "SELECT id, status, workflow_step FROM threads WHERE schedule_task_id = :schedule_task_id AND id != :thread_id ORDER BY id DESC LIMIT 8",
            (
                :schedule_task_id = schedule_task_id,
                :thread_id = thread_id,
            )
        )
        .fetch_all(pool)
        .await?;
        prior_threads.extend(rows);
        // Cron threads created after the migration carry task_id + task_type='cron'.
        let rows =         sql_forge!(
            PriorThreadRow,
            "SELECT id, status, workflow_step FROM threads WHERE task_id = :schedule_task_id AND task_type = 'cron' AND id != :thread_id ORDER BY id DESC LIMIT 8",
            (
                :schedule_task_id = schedule_task_id,
                :thread_id = thread_id,
            )
        )
        .fetch_all(pool)
        .await?;
        prior_threads.extend(rows);
    }
    prior_threads.sort_by_key(|t| t.id);
    if !prior_threads.is_empty() {
        let mut parts = vec![format!(
            "Prior step-threads of this task (thread, step, terminal status, last message) - resume from where the previous attempt ended; do not re-do completed work or repeat its mistakes:"
        )];
        for t in &prior_threads {
            let step = t.workflow_step.as_deref().unwrap_or("-");
            let (content, msg_type) = match last_message_info(pool, t.id).await {
                Ok(last) => (last.content, last.msg_type),
                Err(e) => {
                    tracing::warn!(thread_id = t.id, "last-message lookup failed: {}", e);
                    ("<unavailable>".to_string(), None)
                }
            };
            parts.push(format!(
                "thread {} [step {}] status {} | last message ({}): {}",
                t.id,
                step,
                t.status,
                msg_type.as_deref().unwrap_or("text"),
                truncate_str(&content, 180)
            ));
        }
        blocks.push(parts.join("\n"));
    }

    // Recent kanban history: last status changes + comments (why this task is
    // being run again).
    if let Some(task_id) = &thread_ref.task_id {
        let history =         sql_forge!(
            KanbanHistoryRow,
            "SELECT action, initial_board, final_board, comment, TO_CHAR(created_at, 'YYYY-MM-DD HH24' || CHR(58) || 'MI') AS created_at FROM kanban_history WHERE kanban_task_id = :task_id ORDER BY id DESC LIMIT 5",
            ( :task_id = task_id )
        )
        .fetch_all(pool)
        .await?;
        if !history.is_empty() {
            let mut parts =
                vec!["Recent kanban history (why this task is being run again):".to_string()];
            for h in &history {
                let comment = h
                    .comment
                    .as_deref()
                    .map(|c| format!(" - \"{}\"", truncate_str(c, 120)))
                    .unwrap_or_default();
                parts.push(format!(
                    "{} -> {}: {} ({}){}",
                    h.initial_board.as_deref().unwrap_or("?"),
                    h.final_board.as_deref().unwrap_or("?"),
                    h.action,
                    h.created_at.as_deref().unwrap_or("?"),
                    comment
                ));
            }
            blocks.push(parts.join("\n"));
        }
    }

    // Resume ledger: tracking file referenced by the task body.
    if let Some(task_id) = &thread_ref.task_id {
        let body: Option<KanbanBodyRow> = sql_forge!(
            KanbanBodyRow,
            "SELECT body FROM kanban_tasks WHERE id = :task_id",
            ( :task_id = task_id )
        )
        .fetch_optional(pool)
        .await?;
        if let Some(b) = body {
            if let Some(ledger) = b.body.as_deref().and_then(extract_tracking_path) {
                blocks.push(format!(
                    "Task tracking file (resume ledger): read {} first - it records what has been done, verified, or failed across attempts of this task.",
                    ledger
                ));
            }
        }
    }

    if blocks.is_empty() {
        Ok(None)
    } else {
        Ok(Some(blocks.join("\n\n")))
    }
}

#[derive(Debug, FromRow)]
struct WorkflowStepRow {
    workflow_id: Option<String>,
    workflow_step: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
struct WorkflowsYaml {
    #[serde(default)]
    workflows: std::collections::HashMap<String, WorkflowYamlEntry>,
}

#[derive(Debug, serde::Deserialize)]
struct WorkflowYamlEntry {
    #[serde(default)]
    roles: std::collections::HashMap<String, WorkflowRoleYaml>,
}

#[derive(Debug, serde::Deserialize)]
struct WorkflowRoleYaml {
    #[serde(default)]
    template: Option<String>,
}

/// Map a workflow step key to its role key (workflows.yml role names):
/// running -> executor, testing -> tester, review -> reviewer.
fn step_to_role(workflow_step: &str) -> Option<&'static str> {
    match workflow_step {
        "running" => Some("executor"),
        "testing" => Some("tester"),
        "review" => Some("reviewer"),
        _ => None,
    }
}

/// Role instructions for the current thread's workflow step (R11). Both tester
/// and reviewer must NOT implement the task; thread-access rules are included.
fn build_role_block(workflow_step: &str) -> Option<String> {
    match workflow_step {
        "running" => Some(
            "You are the EXECUTOR of this workflow step: implement/execute the task described in the task description. Before acting, read the prior step-threads of this task listed below - resume from where the previous attempt ended, avoid repeating work that already succeeded, and fix the work if the last testing/review step failed or requested changes."
                .to_string(),
        ),
        "testing" => Some(
            "You are the TESTER of this workflow step: run the tests for the executed task (you may create automated tests), but you must NOT implement the task itself. Read the executor's thread and all recent threads of this task before testing."
                .to_string(),
        ),
        "review" => Some(
            "You are the REVIEWER of this workflow step: perform a comprehensive review of the execution AND the tests. You must NOT implement the task. Read the executor and tester threads plus all recent threads of this task. If everything passes: report a successful status with a normal summary. If you find issues: call the fail tool with workflow_step 'running', 'testing', or 'blocked' (never 'review') so the right role re-runs."
                .to_string(),
        ),
        _ => None,
    }
}

/// Load the role template CONTENT for a workflow.
///
/// The role's `template` field in workflows.yml is a FILE NAME (e.g.
/// `dev-development`), NOT the template text itself. The content is loaded
/// from `<omni_dir>/profiles/<profile>/templates/<name>.md` (with `.md`
/// appended when the name has no extension) and returned. Returns None when
/// the workflow/role is absent, the template field is empty, or the template
/// file is missing — callers decide whether to degrade (executor) or fall
/// back (tester/reviewer).
fn load_role_template(
    data_dir: &str,
    profile_name: &str,
    workflow_id: &str,
    role: &str,
) -> Option<String> {
    let path = std::path::Path::new(data_dir).join("workflows.yml");
    let text = std::fs::read_to_string(path).ok()?;
    let file: WorkflowsYaml = serde_yaml::from_str(&text).ok()?;
    let template_name = file
        .workflows
        .get(workflow_id)?
        .roles
        .get(role)?
        .template
        .clone()?;
    if template_name.trim().is_empty() {
        return None;
    }
    let loaded = crate::memory_store::load_template(data_dir, profile_name, &template_name);
    if loaded.is_none() {
        tracing::warn!(
            workflow_id,
            role,
            profile_name,
            template_name,
            "workflow role template file not found in profiles/<profile>/templates — no template applied"
        );
    }
    loaded
}

/// Inverse prompt mapping for workflow steps (Phase 3b):
/// - executor: task description = USER prompt (unchanged); template (optional) = SYSTEM message.
/// - tester/reviewer: INVERSE - template = USER prompt (drives the role); task description = SYSTEM prompt.
fn apply_workflow_mapping(
    system: &mut String,
    user: &mut String,
    user_message: &str,
    workflow_step: &str,
    template: Option<&str>,
) {
    let Some(role) = step_to_role(workflow_step) else {
        return;
    };
    let Some(template) = template else {
        if workflow_step == "testing" || workflow_step == "review" {
            tracing::warn!(
                workflow_step,
                role,
                "workflow template required for this step but missing in workflows.yml"
            );
        }
        return;
    };
    match workflow_step {
        "running" => {
            system.push_str(&format!(
                "\n\n## Workflow instructions ({})\n{}",
                role, template
            ));
        }
        "testing" | "review" => {
            *user = template.to_string();
            let description = user_message.trim();
            if !description.is_empty() {
                system.push_str(&format!(
                    "\n\n## Task under {} - context only, do not implement\n{}",
                    role, description
                ));
            }
        }
        _ => {}
    }
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
    let channel_id = args["channel_id"]
        .as_str()
        .map(String::from)
        .or_else(|| meta.as_ref().and_then(|m| m.channel_id.clone()));
    let data_dir = &cfg.omni_dir;

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
            memory_text.push_str(part);
            memory_text.push('\n');
        } else if system_message.is_some()
            && !system_message.unwrap().is_empty()
            && part == system_message.unwrap()
        {
            soul_text = part.clone();
        } else {
            system_parts.push(part.clone());
        }
    }

    let mut system = system_parts.join("\n\n");
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
        match get_latest_summary(pool, &cid).await {
            Ok(Some(summary)) => {
                context_blocks.push(format!(
                    "Previous channel summary (covers threads up to id={}):\n{}",
                    summary.next_thread_id,
                    truncate_str(&summary.content, 4000)
                ));

                match get_threads_since(pool, &cid, summary.next_thread_id, 5).await {
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
    let skills = get_skills(data_dir, profile_name);
    if !skills.is_empty() {
        context_blocks.push(format!(
            "Available skills (read one with view_skill before acting when it matches the task):\n{}",
            skills.join("\n")
        ));
    }

    // 2c-ext2. Previous attempts of the SAME task (R8-J) — earlier threads
    // of this kanban task, ALL statuses, with their final summaries. Placed
    // right after the template so the agent reads "what to do" and "what was
    // already tried (and died)" together — a successor must never re-derive
    // harness knowledge a prior thread already documented (6 budget-deaths:
    // threads 113/138/140/155 burned 100-120 calls each re-exploring the
    // same ground because this context never reached them).
    if let Some(tid) = thread_id {
        match build_prior_attempts_block(pool, tid).await {
            Ok(Some(block)) => context_blocks.push(block),
            Ok(None) => {}
            Err(e) => tracing::warn!("Prior attempts context unavailable: {}", e),
        }
    }

    // 2c-ext3. Learned Knowledge (R8-K) - promoted memories written by prior
    // threads via memory_promote-to-memory live under
    // <data_dir>/profiles/<profile>/wiki/Memory/Promoted/*.md. Without this
    // read-back the learning loop is write-only: facts get promoted but never
    // reach future prompts, so every successor re-derives the same knowledge.
    // Inject newest-first, truncated; when none exist yet, the block itself
    // teaches the agent that the loop exists.
    if let Some(block) = build_learned_knowledge_block(data_dir, profile_name) {
        context_blocks.push(block);
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

    // 2e. Continuation self-orientation — prior threads of the same task,
    // kanban history, and resume-ledger pointer. Skipped entirely for plain
    // (non-task) threads; never fails the prompt.
    if let Some(tid) = thread_id {
        match build_continuation_block(pool, tid).await {
            Ok(Some(block)) => context_blocks.push(block),
            Ok(None) => {}
            Err(e) => tracing::warn!("continuation context unavailable: {}", e),
        }
    }

    // 2e-ext0. Interrupted-attempt warning (R8-K) - if prior attempts of
    // this task died at the iteration limit, say so loudly right next to the
    // continuation block so a successor does not walk into the same trap.
    if let Some(tid) = thread_id {
        match count_interrupted_attempts(pool, tid).await {
            Ok(Some(n)) if n > 0 => context_blocks.push(format!(
                "WARNING: {} prior attempt(s) of this task were INTERRUPTED (iteration limit). Read their summaries above. If you are about to do what they did, you are repeating a mistake.",
                n
            )),
            _ => {}
        }
    }

    // 2e-ext. Cross-task channel context - recent terminal threads of OTHER
    // tasks on this same channel (Phase 3b-ext). Automates the manual ORIENT
    // step 4 of the dev template: prior phases on this channel often solved
    // the exact problem being investigated; trust their documented results
    // instead of re-deriving them. Task-linked threads only; omitted when no
    // other terminal task threads exist on the channel.
    if let Some(tid) = thread_id {
        match build_cross_task_block(pool, tid).await {
            Ok(Some(block)) => context_blocks.push(block),
            Ok(None) => {}
            Err(e) => tracing::warn!("cross-task context unavailable: {}", e),
        }
    }

    let context = context_blocks.join("\n\n---\n\n");
    let original_user = user_message.to_string();
    let mut user = original_user.clone();

    // 2f. Workflow step prompt mapping (inverse for tester/reviewer):
    // executor = task description as USER prompt + template as SYSTEM message
    // (template optional); tester/reviewer = template as USER prompt + task
    // description as SYSTEM prompt (template required).
    if let Some(tid) = thread_id {
        match sql_forge!(
            WorkflowStepRow,
            "SELECT workflow_id, workflow_step FROM threads WHERE id = :tid",
            ( :tid = tid )
        )
        .fetch_optional(pool)
        .await
        {
            Ok(Some(wf)) => {
                if let (Some(wf_id), Some(step)) = (&wf.workflow_id, &wf.workflow_step) {
                    let template = step_to_role(step)
                        .and_then(|role| load_role_template(data_dir, profile_name, wf_id, role));
                    apply_workflow_mapping(
                        &mut system,
                        &mut user,
                        user_message,
                        step,
                        template.as_deref(),
                    );
                }
            }
            Ok(None) => {}
            Err(e) => tracing::warn!("workflow step lookup unavailable: {}", e),
        }
    }

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
                let lower = original_user.to_lowercase();
                keywords.iter().any(|k| lower.contains(k))
            };
            original_user.chars().count() > max_chars || has_keyword
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
// Real size measurement for the compaction gate (tiktoken BPE).
// ---------------------------------------------------------------------------

/// Unit of the size measurement returned by [`measure_size`]. The compaction
/// gate compares against the TOKEN budgets only when the measurement is in
/// real tokens; on tokenizer failure the measurement falls back to chars and
/// the gate must use the CHAR budgets — never chars/4 against token budgets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SizeUnit {
    Tokens,
    Chars,
}

/// Measure the size of a message list for the compaction gate.
///
/// When `tokenizer_encoding` is configured (e.g. "gpt-4" -> cl100k_base,
/// "o200k_base"), the size is the REAL tiktoken BPE token count of the
/// JSON-serialized message array — the same proven counter the core uses in
/// src/agent/helpers.rs::count_tokens. This replaces the old chars/4 proxy,
/// which made the token budgets 4x too lenient in chars: real long threads
/// (thread 87: 845K chars peak, 25.5M input tokens) never crossed the hard
/// budget, so the compaction gate stayed dead and context grew unbounded.
///
/// On tokenizer failure (bad encoding, tiktoken load error, serialize error)
/// the measurement falls back to the CHAR size; the caller then compares
/// against the CHAR budgets. Empty `tokenizer_encoding` always measures
/// chars (the deployment does not use token budgets).
fn measure_size(
    items: &[crate::chat_message::ChatMessage],
    tokenizer_encoding: &str,
) -> (usize, SizeUnit) {
    // Char measurement (the tokenizer-free path and the fallback).
    let chars: usize = items
        .iter()
        .map(|m| {
            let calls = m
                .tool_calls
                .as_ref()
                .map(|calls| {
                    calls
                        .iter()
                        .map(|call| {
                            call.function.name.chars().count()
                                + call.function.arguments.chars().count()
                        })
                        .sum::<usize>()
                })
                .unwrap_or(0);
            m.content.chars().count() + calls
        })
        .sum();

    if tokenizer_encoding.is_empty() {
        return (chars, SizeUnit::Chars);
    }

    // Real token count: serialize the array exactly as the API receives it,
    // then run tiktoken BPE with special tokens (mirrors
    // src/agent/helpers.rs::count_tokens).
    let json = match serde_json::to_string(items) {
        Ok(j) => j,
        Err(e) => {
            tracing::warn!(
                "[prompt] Failed to serialize messages for token counting: {}",
                e
            );
            return (chars, SizeUnit::Chars);
        }
    };
    let bpe = match tiktoken_rs::get_bpe_from_model(tokenizer_encoding) {
        Ok(bpe) => bpe,
        Err(e) => {
            tracing::warn!(
                "[prompt] Failed to load BPE encoding '{}': {}: falling back to char budget",
                tokenizer_encoding,
                e
            );
            return (chars, SizeUnit::Chars);
        }
    };
    (
        bpe.encode_with_special_tokens(&json).len(),
        SizeUnit::Tokens,
    )
}

// ---------------------------------------------------------------------------
// Tool: prompt_compact_messages
// ---------------------------------------------------------------------------

async fn handle_compact_messages(args: &Value, cfg: &PluginConfig) -> Result<(String, bool)> {
    let messages_arr = match args["messages"].as_array() {
        Some(arr) => arr,
        None => {
            return Ok((
                "Missing required argument: 'messages' (array of ChatMessage)".to_string(),
                true,
            ))
        }
    };

    let keep_recent = args["keep_recent"]
        .as_u64()
        .map(|v| v as usize)
        .unwrap_or(cfg.compact_keep_recent);

    let mut messages: Vec<crate::chat_message::ChatMessage> =
        match serde_json::from_value(serde_json::Value::Array(messages_arr.clone())) {
            Ok(msgs) => msgs,
            Err(e) => return Ok((format!("Failed to parse messages: {}", e), true)),
        };

    // Threshold gate: compact ONLY when the hard budget is exceeded.
    // The main loop calls this tool before EVERY LLM call, and the noop
    // test-tool-caller relies on the assistant tool_calls history to count
    // script steps and resolve ${step.field} placeholders. Compacting a tiny
    // 4-step script nulls the oldest tool_calls, corrupting the step counter
    // and causing an infinite re-emit loop (deploy Groups 13/14 regression).
    // The hard budget protects against that: short conversations never exceed
    // it, so they are never compacted. Real long threads do exceed it, and
    // then compaction runs on EVERY call (no cadence) until the size drops
    // back to the soft budget — the soft budget is the REDUCTION TARGET, not
    // a trigger. When the tool decides not to compact, it returns null.
    //
    // Compaction itself runs at most 3 passes with a progressively smaller
    // keep_recent; if the size is still over the soft budget after that, the
    // tool returns the PARTIAL result (the reduction achieved so far) instead
    // of erroring — the caller applies it, which gets the size under the HARD
    // trigger budget, so later iterations stop re-triggering compaction.
    // Erroring would discard the partial reduction and make every later
    // iteration repeat the same failed compaction forever.
    // Real size measurement: tiktoken BPE tokens when a tokenizer encoding
    // is configured, chars otherwise (and on tokenizer failure). The budget
    // compared against follows the measurement unit: token budgets for real
    // tokens, char budgets for chars — never chars/4 against token budgets.
    let (current_size, size_unit) = measure_size(&messages, &cfg.tokenizer_encoding);
    let hard_budget = match size_unit {
        SizeUnit::Tokens => cfg.token_budget_hard,
        SizeUnit::Chars => cfg.char_budget_hard,
    };
    let soft_budget = match size_unit {
        SizeUnit::Tokens => cfg.token_budget_soft,
        SizeUnit::Chars => cfg.char_budget_soft,
    };

    let before = messages.len();
    // WS-2/WS-3: durable context dump + compaction event plumbing.
    let thread_dir = args["thread_dir"].as_str().map(std::path::PathBuf::from);
    let current_iteration = args["current_iteration"].as_u64().unwrap_or(0) as u32;
    let mut entries = 0usize;
    let mut dump_file: Option<String> = None;

    if current_size > hard_budget {
        // Reduce to the soft budget: compact, and if still over soft, keep
        // compacting with a progressively smaller keep_recent. Compaction
        // stops when size <= soft or there is nothing more to compact
        // (keep_recent would hit 0 — compact_old_assistant_messages then
        // compacts every assistant tool-call message).
        //
        // At most 3 passes: if the size is still over the soft budget after
        // 3 progressively more aggressive compactions and there is material
        // left to compact (keep_recent has not yet reached 0), raise an
        // error instead of looping forever.
        let mut keep = keep_recent;
        for pass in 0..cfg.compact_max_passes {
            let outcome = crate::compact::compact_old_assistant_messages(
                &mut messages,
                keep,
                thread_dir.as_deref(),
                current_iteration,
                &crate::compact::CompactSettings {
                    tool_excerpt_chars: cfg.tool_excerpt_chars,
                    total_excerpt_cap: cfg.total_excerpt_cap,
                    read_excerpt_chars: cfg.read_excerpt_chars,
                },
            );
            if let Some(df) = outcome.dump_file {
                dump_file = Some(df);
            }
            entries += outcome.dump_entries;
            let after_size = measure_size(&messages, &cfg.tokenizer_encoding).0;
            if after_size <= soft_budget || keep == 0 {
                break;
            }
            if pass + 1 == cfg.compact_max_passes {
                // Maximum configured passes reached. Return the partial result;
                // discarding it would make every later iteration repeat the same
                // failed compaction forever.
                break;
            }
            keep = keep.saturating_sub(cfg.compact_keep_step);
        }
    }
    let after = messages.len();

    // Contract: return the compacted messages array when something changed,
    // or null when nothing was compacted. No boolean flags — the caller
    // applies the result iff it receives an array.
    let result = serde_json::json!({
        "messages": if before != after {
            serde_json::Value::Array(
                messages.iter().map(|m| serde_json::to_value(m).unwrap_or(serde_json::Value::Null)).collect()
            )
        } else {
            serde_json::Value::Null
        },
        "was_compacted": before != after,
        "iteration": current_iteration,
        "dump_file": dump_file,
        "entries": entries,
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
    let (current_size, size_unit) = measure_size(&messages, &cfg.tokenizer_encoding);
    let soft_budget = match size_unit {
        SizeUnit::Tokens => cfg.token_budget_soft,
        SizeUnit::Chars => cfg.char_budget_soft,
    };
    let hard_budget = match size_unit {
        SizeUnit::Tokens => cfg.token_budget_hard,
        SizeUnit::Chars => cfg.char_budget_hard,
    };
    let target_budget = soft_budget.min(hard_budget);

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
        crate::compact::compact_old_assistant_messages(
            &mut messages,
            condense_keep_turns,
            None,
            current_iteration as u32,
            &crate::compact::CompactSettings {
                tool_excerpt_chars: cfg.tool_excerpt_chars,
                total_excerpt_cap: cfg.total_excerpt_cap,
                read_excerpt_chars: cfg.read_excerpt_chars,
            },
        );

        let after_size: usize = measure_size(&messages, &cfg.tokenizer_encoding).0;

        if after_size > target_budget {
            let aggressive_keep = condense_keep_turns.saturating_sub(1);
            crate::compact::compact_old_assistant_messages(
                &mut messages,
                aggressive_keep,
                None,
                current_iteration as u32,
                &crate::compact::CompactSettings {
                    tool_excerpt_chars: cfg.tool_excerpt_chars,
                    total_excerpt_cap: cfg.total_excerpt_cap,
                    read_excerpt_chars: cfg.read_excerpt_chars,
                },
            );
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
    if s.chars().count() <= max_chars {
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
    let pool_ready_gen = pool_ready_rx;
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
            let pool = match guard.as_ref() {
                Some(pool) => pool.clone(),
                None => {
                    return Ok((
                        "Prompt database is unavailable; prompt generation cannot load context"
                            .to_string(),
                        true,
                    ));
                }
            };
            let config = cfg.read().await.clone();
            handle_generate_full(&pool, &args, meta, &config).await
        })
    });

    // Compact messages handler
    let cfg_compact = plugin_config.clone();
    let compact_handler: ToolHandler = Box::new(move |args: Value, _meta: Option<McpMeta>| {
        let cfg = cfg_compact.clone();
        Box::pin(async move {
            let config = cfg.read().await.clone();
            handle_compact_messages(&args, &config).await
        })
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_tracking_path_parses_resume_ledger() {
        let body =
            "Run me. Resume ledger: /opt/omni/data/tasks/WorkflowImplementation.md — append, don't overwrite.";
        assert_eq!(
            extract_tracking_path(body).as_deref(),
            Some("/opt/omni/data/tasks/WorkflowImplementation.md")
        );
        assert_eq!(extract_tracking_path("no tracking path here"), None);
        assert_eq!(extract_tracking_path("see data/tasks/"), None);
    }

    #[tokio::test]
    #[ignore = "requires a live DATABASE_URL"]
    async fn continuation_block_skipped_for_plain_thread() {
        let url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set");
        let pool = connect_db(&url).await.expect("connect_db");
        let row: i64 = sql_forge!(
            scalar i64,
            "SELECT id FROM threads WHERE task_id IS NULL AND schedule_task_id IS NULL ORDER BY id DESC LIMIT 1",
        )
        .fetch_one(&pool)
        .await
        .expect("a plain thread exists");
        assert!(
            build_continuation_block(&pool, row)
                .await
                .expect("build_continuation_block")
                .is_none(),
            "plain thread must not get a continuation block"
        );
    }

    #[tokio::test]
    #[ignore = "requires a live DATABASE_URL"]
    async fn continuation_block_lists_prior_step_threads_for_kanban_task() {
        let url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set");
        let pool = connect_db(&url).await.expect("connect_db");
        // task_18c909688609da2f: thread 72 is the newest attempt;
        // 71/70 skipped, 69 interrupted (legacy rows, task_type IS NULL).
        let block = build_continuation_block(&pool, 72)
            .await
            .expect("build_continuation_block")
            .expect("a block is produced for a task thread");
        println!("=== CONTINUATION BLOCK ===\n{}\n=== END ===", block);
        assert!(
            block.contains("Prior step-threads of this task"),
            "prior-threads header missing:\n{block}"
        );
        assert!(
            block.contains("thread 71 [step -] status skipped | last message"),
            "thread 71 entry missing:\n{block}"
        );
        assert!(
            block.contains("thread 70 [step -] status skipped | last message"),
            "thread 70 entry missing:\n{block}"
        );
        assert!(
            block.contains("thread 69 [step -] status interrupted | last message"),
            "thread 69 entry missing:\n{block}"
        );
        assert!(
            block.contains("Recent kanban history (why this task is being run again):"),
            "kanban history header missing:\n{block}"
        );
        assert!(
            block.contains("Task tracking file (resume ledger):"),
            "resume ledger line missing:\n{block}"
        );
    }

    #[test]
    fn role_block_matches_step() {
        let executor = build_role_block("running").unwrap();
        assert!(executor.contains("EXECUTOR"));
        let tester = build_role_block("testing").unwrap();
        assert!(tester.contains("TESTER"));
        assert!(tester.contains("must NOT implement"));
        let reviewer = build_role_block("review").unwrap();
        assert!(reviewer.contains("REVIEWER"));
        assert!(reviewer.contains("never 'review'"));
        assert!(build_role_block("bogus").is_none());
    }

    #[test]
    fn workflow_mapping_executor_keeps_task_description_as_user() {
        let mut system = String::new();
        let mut user = "implement the weekly report".to_string();
        apply_workflow_mapping(
            &mut system,
            &mut user,
            "implement the weekly report",
            "running",
            Some("You are the executor for this workflow."),
        );
        assert_eq!(user, "implement the weekly report");
        assert!(system.contains("Workflow instructions (executor)"));
        assert!(system.contains("You are the executor for this workflow."));
    }

    #[test]
    fn workflow_mapping_tester_reviewer_is_inverse() {
        let mut system = String::new();
        let mut user = "implement the weekly report".to_string();
        apply_workflow_mapping(
            &mut system,
            &mut user,
            "implement the weekly report",
            "testing",
            Some("Run the test suite against the implementation."),
        );
        assert_eq!(user, "Run the test suite against the implementation.");
        assert!(system.contains("Task under tester"));
        assert!(system.contains("implement the weekly report"));

        let mut system = String::new();
        let mut user = "x".to_string();
        apply_workflow_mapping(
            &mut system,
            &mut user,
            "implement the weekly report",
            "review",
            Some("Review the implementation and the tests."),
        );
        assert_eq!(user, "Review the implementation and the tests.");
        assert!(system.contains("Task under reviewer"));
    }

    #[test]
    fn workflow_mapping_missing_template_falls_back_for_testing() {
        let mut system = String::new();
        let mut user = "implement the weekly report".to_string();
        apply_workflow_mapping(
            &mut system,
            &mut user,
            "implement the weekly report",
            "testing",
            None,
        );
        assert_eq!(user, "implement the weekly report");
        assert!(!system.contains("Task under tester"));
    }

    #[test]
    fn role_template_loads_content_from_profile_templates_dir() {
        // The workflow role `template` field is a FILE NAME resolved against
        // <data_dir>/profiles/<profile>/templates/<name>.md — the content is
        // loaded, never the raw name.
        let dir = tempdir_uniq("role-template-test");
        let data_dir = dir.as_path().to_str().unwrap();
        let templates_dir = dir
            .as_path()
            .join("profiles")
            .join("omni")
            .join("templates");
        std::fs::create_dir_all(&templates_dir).expect("create templates dir");
        std::fs::write(
            templates_dir.join("dev-executor.md"),
            "EXECUTOR CONTENT FROM FILE",
        )
        .expect("write template file");
        std::fs::write(
            dir.as_path().join("workflows.yml"),
            "workflows:\n  wf:\n    roles:\n      executor:\n        template: dev-executor\n      tester:\n        template: missing-template\n",
        )
        .expect("write workflows.yml");

        // File name -> content loaded from the profile templates directory.
        let content = load_role_template(data_dir, "omni", "wf", "executor")
            .expect("template should load from file");
        assert_eq!(content, "EXECUTOR CONTENT FROM FILE");

        // Missing template file -> None (never the raw name).
        assert!(
            load_role_template(data_dir, "omni", "wf", "tester").is_none(),
            "missing template file must yield None, not the raw name"
        );
        // Unknown role -> None.
        assert!(load_role_template(data_dir, "omni", "wf", "nope").is_none());
        // Unknown workflow -> None.
        assert!(load_role_template(data_dir, "omni", "nope", "executor").is_none());
    }

    /// Unique temp dir under the system temp dir (no tempfile dev-dep).
    fn tempdir_uniq(tag: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let path = std::env::temp_dir().join(format!(
            "prompt-plugin-{}-{}-{}",
            tag,
            std::process::id(),
            nanos
        ));
        std::fs::create_dir_all(&path).expect("create temp dir");
        path
    }
}

#[cfg(test)]
mod cross_task_tests {
    use super::*;

    fn ct_row(
        id: i64,
        task_id: Option<&str>,
        title: Option<&str>,
        step: Option<&str>,
        status: &str,
        content: Option<&str>,
        msg_type: Option<&str>,
    ) -> CrossTaskThreadRow {
        CrossTaskThreadRow {
            id,
            task_id: task_id.map(str::to_string),
            task_title: title.map(str::to_string),
            workflow_step: step.map(str::to_string),
            status: status.to_string(),
            last_content: content.map(str::to_string),
            last_msg_type: msg_type.map(str::to_string),
        }
    }

    #[test]
    fn cross_task_block_lists_other_tasks_terminal_threads_most_recent_first_capped() {
        // (a) task-linked thread on a channel with other tasks' terminal
        // threads -> block lists them with id/step/status/final-message/type,
        // most recent first, capped at CROSS_TASK_MAX_ENTRIES.
        let own = OwnTaskLink {
            task_id: Some("task-own".to_string()),
            schedule_task_id: None,
        };
        let rows = vec![
            ct_row(
                90,
                Some("task-a"),
                Some("Phase A"),
                Some("build"),
                "completed",
                Some("build done"),
                Some("thread_summary"),
            ),
            ct_row(
                92,
                Some("task-b"),
                Some("Phase B"),
                Some("test"),
                "failed",
                Some("clippy errors here"),
                Some("text"),
            ),
            ct_row(
                91,
                Some("task-c"),
                Some("Phase C"),
                Some("review"),
                "interrupted",
                Some("flaky test"),
                Some("text"),
            ),
            ct_row(
                93,
                Some("task-d"),
                Some("Phase D"),
                Some("deploy"),
                "skipped",
                Some("not needed"),
                Some("thread_summary"),
            ),
            ct_row(
                94,
                Some("task-own"),
                Some("Own Phase"),
                Some("build"),
                "completed",
                Some("own history"),
                Some("text"),
            ),
        ];
        let block = render_cross_task_block(&own, rows).expect("block present");
        assert!(block.starts_with("Recent threads from other tasks on this channel"));
        let lines: Vec<&str> = block.lines().collect();
        assert_eq!(
            lines.len(),
            1 + CROSS_TASK_MAX_ENTRIES,
            "entry cap: {block}"
        );
        // most recent first (thread 94 is own task -> excluded, then 93, 92, 91)
        assert!(lines[1].contains("thread 93"));
        assert!(lines[2].contains("thread 92"));
        assert!(lines[3].contains("thread 91"));
        assert!(
            !block.contains("thread 90"),
            "oldest entry dropped by cap: {block}"
        );
        // per-entry fields: id, task id/title, step, status, final message, type
        assert!(lines[1].contains("task task-d \"Phase D\""));
        assert!(lines[1].contains("step deploy"));
        assert!(lines[1].contains("status skipped"));
        assert!(lines[1].contains("last message (thread_summary): not needed"));
        assert!(
            !block.contains("task-own"),
            "own task must not appear: {block}"
        );
    }

    #[test]
    fn cross_task_block_excludes_own_task_threads() {
        // (b) the current task's own threads are NOT duplicated in the
        // cross-task block (same-task history stays in the Phase 3b block).
        let own = OwnTaskLink {
            task_id: Some("task-own".to_string()),
            schedule_task_id: Some("cron-own".to_string()),
        };
        let rows = vec![
            ct_row(
                80,
                Some("task-own"),
                Some("Own"),
                Some("build"),
                "completed",
                Some("own msg"),
                Some("text"),
            ),
            ct_row(
                81,
                Some("cron-own"),
                Some("Own cron"),
                Some("run"),
                "completed",
                Some("cron msg"),
                Some("text"),
            ),
            ct_row(
                82,
                Some("task-x"),
                Some("Other"),
                Some("test"),
                "completed",
                Some("other msg"),
                Some("text"),
            ),
        ];
        let block = render_cross_task_block(&own, rows).expect("block present");
        assert!(
            !block.contains("task-own"),
            "own kanban task must be excluded: {block}"
        );
        assert!(
            !block.contains("cron-own"),
            "own cron schedule must be excluded: {block}"
        );
        assert!(block.contains("task-x"), "other task must remain: {block}");
    }

    #[test]
    fn cross_task_block_respects_entry_and_char_caps() {
        // (c) cap on entry count and on title/message length.
        let own = OwnTaskLink {
            task_id: Some("task-own".to_string()),
            schedule_task_id: None,
        };
        let long_msg = "x".repeat(600);
        let long_title = "y".repeat(80);
        let rows: Vec<CrossTaskThreadRow> = (1..=6i64)
            .map(|i| {
                ct_row(
                    100 + i,
                    Some(format!("task-{i}").as_str()),
                    Some(long_title.as_str()),
                    Some("build"),
                    "completed",
                    Some(long_msg.as_str()),
                    Some("text"),
                )
            })
            .collect();
        let block = render_cross_task_block(&own, rows).expect("block present");
        let lines: Vec<&str> = block.lines().collect();
        assert_eq!(lines.len(), 1 + CROSS_TASK_MAX_ENTRIES);
        for line in &lines[1..] {
            assert!(
                line.contains(&format!("{}...", "y".repeat(CROSS_TASK_MAX_TITLE_CHARS))),
                "title truncated to {} chars: {line}",
                CROSS_TASK_MAX_TITLE_CHARS
            );
            assert!(
                line.contains(&format!("{}...", "x".repeat(CROSS_TASK_MAX_MESSAGE_CHARS))),
                "message truncated to {} chars: {line}",
                CROSS_TASK_MAX_MESSAGE_CHARS
            );
            assert!(!line.contains(&"x".repeat(CROSS_TASK_MAX_MESSAGE_CHARS + 1)));
        }
    }

    #[test]
    fn cross_task_block_omitted_when_no_other_terminal_threads() {
        // (d) channel with no other terminal task threads -> section omitted
        // (no empty placeholder noise).
        let own = OwnTaskLink {
            task_id: Some("task-own".to_string()),
            schedule_task_id: None,
        };
        assert_eq!(render_cross_task_block(&own, vec![]), None);
        let own_only = vec![ct_row(
            70,
            Some("task-own"),
            Some("Own"),
            Some("build"),
            "completed",
            Some("m"),
            Some("text"),
        )];
        assert_eq!(render_cross_task_block(&own, own_only), None);
    }

    #[test]
    fn cross_task_block_skipped_for_plain_threads() {
        // (e) plain thread (no task_id / schedule_task_id) -> no cross-task
        // block (existing behavior preserved). build_cross_task_block gates on
        // is_task_linked() before any query; the renderer also drops rows
        // that are not task-linked.
        let plain = OwnTaskLink {
            task_id: None,
            schedule_task_id: None,
        };
        assert!(!plain.is_task_linked(), "plain thread is not task-linked");
        let kanban = OwnTaskLink {
            task_id: Some("task-1".to_string()),
            schedule_task_id: None,
        };
        assert!(kanban.is_task_linked());
        let cron = OwnTaskLink {
            task_id: None,
            schedule_task_id: Some("cron-1".to_string()),
        };
        assert!(cron.is_task_linked());
        // a plain-thread row is never task-linked -> renderer drops it
        assert_eq!(
            render_cross_task_block(
                &plain,
                vec![ct_row(
                    60,
                    None,
                    Some("A"),
                    None,
                    "completed",
                    Some("m"),
                    Some("text")
                )]
            ),
            None
        );
    }
}

#[cfg(test)]
mod prior_attempts_tests {
    use super::*;

    fn pa_row(
        id: i64,
        status: &str,
        iterations: Option<i32>,
        ended_at: Option<&str>,
    ) -> PriorAttemptRow {
        PriorAttemptRow {
            id,
            status: status.to_string(),
            iterations,
            ended_at: ended_at.map(str::to_string),
        }
    }

    #[test]
    fn prior_attempts_block_lists_all_statuses_with_summaries() {
        // (a) ALL statuses are listed (not just completed) — interrupted
        // threads hold the "what died and why" knowledge.
        let rows = vec![
            pa_row(140, "interrupted", Some(120), Some("2026-08-08 03:10")),
            pa_row(155, "interrupted", Some(100), Some("2026-08-08 04:40")),
            pa_row(156, "completed", Some(45), Some("2026-08-08 04:43")),
        ];
        let mut summaries = std::collections::HashMap::new();
        summaries.insert(
            140,
            Some("The harness is the dev stack under /opt/workspace/omniagent".to_string()),
        );
        // thread 155 left NO summary -> still listed with status + iterations
        summaries.insert(155, None);
        summaries.insert(
            156,
            Some("R8-H COMPLETE: commit 3abfca6 pushed to main".to_string()),
        );
        let block = render_prior_attempts_block(rows, &summaries).expect("block present");
        assert!(block.starts_with(
            "=== Previous attempts of this task (READ — do NOT repeat what they did) ==="
        ));
        // newest first
        assert!(block.contains(
            "thread 156 | status completed | iterations 45 | ended_at 2026-08-08 04:43 | summary: R8-H COMPLETE"
        ));
        assert!(block.contains(
            "thread 155 | status interrupted | iterations 100 | ended_at 2026-08-08 04:40 | (no summary message)"
        ));
        assert!(block.contains(
            "thread 140 | status interrupted | iterations 120 | ended_at 2026-08-08 03:10 | summary: The harness is the dev stack"
        ));
    }

    #[test]
    fn prior_attempts_block_truncates_summary_and_caps_entries() {
        let long = "x".repeat(1200);
        let rows: Vec<PriorAttemptRow> = (1..=7i64)
            .map(|i| pa_row(200 + i, "failed", Some(i as i32), None))
            .collect();
        let mut summaries = std::collections::HashMap::new();
        summaries.insert(207, Some(long.clone()));
        let block = render_prior_attempts_block(rows, &summaries).expect("block present");
        let block_lines: Vec<&str> = block.lines().collect();
        assert_eq!(block_lines.len(), 1 + PRIOR_ATTEMPTS_MAX_ENTRIES);
        assert!(block_lines[1].contains(&format!(
            "{}...",
            "x".repeat(PRIOR_ATTEMPTS_MAX_SUMMARY_CHARS)
        )));
        assert!(!block_lines[1].contains(&"x".repeat(PRIOR_ATTEMPTS_MAX_SUMMARY_CHARS + 1)));
        // newest first
        assert!(block_lines[1].contains("thread 207"));
    }

    #[test]
    fn prior_attempts_block_omitted_when_no_prior_threads() {
        assert_eq!(
            render_prior_attempts_block(vec![], &std::collections::HashMap::new()),
            None
        );
    }

    #[tokio::test]
    #[ignore = "requires a live DATABASE_URL"]
    async fn get_prior_threads_by_task_returns_interrupted_threads() {
        // task_18c9b88db4f55f65 (R7-D4): thread 155 interrupted, 156
        // completed. The query must return BOTH — completed-only filtering
        // is exactly the bug this change fixes.
        let url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set");
        let pool = connect_db(&url).await.expect("connect_db");
        let rows = get_prior_threads_by_task(&pool, "task_18c9b88db4f55f65", 999999, 10)
            .await
            .expect("query ok");
        assert!(!rows.is_empty(), "expected prior threads for task");
        let statuses: Vec<&str> = rows.iter().map(|r| r.status.as_str()).collect();
        assert!(
            statuses.contains(&"interrupted"),
            "prior threads must include interrupted ones: {rows:?}"
        );
    }

    // --- R8-K: learned knowledge ---

    #[test]
    fn strip_frontmatter_removes_yaml_metadata() {
        let raw = "---\ntype: memory\nconfidence: high\nexpires_at: 2026-09-08\n---\nThe dev-stack build command is `cd /app && cargo build --release -p mcp-server-prompt`.";
        let body = strip_frontmatter(raw);
        assert!(!body.contains("type: memory"), "frontmatter leaked: {body}");
        assert!(!body.contains("confidence"), "frontmatter leaked: {body}");
        assert!(body.contains("cargo build"), "body missing: {body}");
        assert_eq!(strip_frontmatter("plain body"), "plain body");
        let unterminated = strip_frontmatter("---\ntype: memory\nno closing fence");
        assert!(unterminated.contains("no closing fence"));
    }

    #[test]
    fn learned_knowledge_block_lists_memories_and_orders_newest_first() {
        let mut mems = vec![
            LearnedMemory {
                title: "older".into(),
                body: "older validated fact".into(),
                mtime: Some(std::time::SystemTime::UNIX_EPOCH),
            },
            LearnedMemory {
                title: "newer".into(),
                body: "newer validated fact".into(),
                mtime: Some(
                    std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(3600),
                ),
            },
        ];
        mems.sort_by_key(|b| std::cmp::Reverse(b.mtime));
        let block = render_learned_knowledge_block(&mems);
        assert!(block.starts_with("=== Learned Knowledge"));
        let newer_pos = block.find("**newer**").expect("newer memory missing");
        let older_pos = block.find("**older**").expect("older memory missing");
        assert!(newer_pos < older_pos, "newest memory must be listed first");
        assert!(block.contains("newer validated fact"));
        assert!(block.contains("older validated fact"));
    }

    #[test]
    fn learned_knowledge_block_caps_total_chars() {
        let mems: Vec<LearnedMemory> = (1..=20i64)
            .map(|i| LearnedMemory {
                title: format!("m{i}"),
                body: "y".repeat(400),
                mtime: None,
            })
            .collect();
        let block = render_learned_knowledge_block(&mems);
        assert!(
            block.len() <= LEARNED_KNOWLEDGE_MAX_TOTAL_CHARS + 600,
            "block too big: {}",
            block.len()
        );
        assert!(block.contains("**m1**"), "first memory missing from block");
        assert!(
            block.len() >= LEARNED_KNOWLEDGE_MAX_TOTAL_CHARS / 2,
            "cap logic dropped everything: {}",
            block.len()
        );
    }

    #[test]
    fn learned_knowledge_block_emits_hint_when_no_memories() {
        let block = build_learned_knowledge_block("/nonexistent/data_dir", "omni")
            .expect("hint block must be emitted even with no memories");
        assert!(block.contains("none yet"));
        assert!(block.contains("memory_promote-to-memory"));
    }
}

#[cfg(test)]
mod token_counting_tests {
    use super::*;
    use crate::chat_message::{ChatMessage, ToolCallData, ToolCallFunction};
    use serde_json::json;

    fn user_msg(content: &str) -> ChatMessage {
        ChatMessage {
            role: "user".to_string(),
            content: content.to_string(),
            tool_call_id: None,
            tool_calls: None,
            name: None,
        }
    }

    fn assistant_msg(content: &str) -> ChatMessage {
        ChatMessage {
            role: "assistant".to_string(),
            content: content.to_string(),
            tool_call_id: None,
            tool_calls: None,
            name: None,
        }
    }

    fn tool_call_msg(name: &str, args: &str, content: &str) -> ChatMessage {
        ChatMessage {
            role: "assistant".to_string(),
            content: content.to_string(),
            tool_call_id: None,
            tool_calls: Some(vec![ToolCallData {
                id: "call_1".to_string(),
                call_type: "function".to_string(),
                function: ToolCallFunction {
                    name: name.to_string(),
                    arguments: args.to_string(),
                },
            }]),
            name: None,
        }
    }

    fn tool_result(name: &str, content: &str) -> ChatMessage {
        ChatMessage {
            role: "tool".to_string(),
            content: content.to_string(),
            tool_call_id: Some("call_1".to_string()),
            tool_calls: None,
            name: Some(name.to_string()),
        }
    }

    fn compact_cfg(tokenizer: &str, hard: usize, soft: usize) -> PluginConfig {
        let mut cfg = PluginConfig::default();
        cfg.tokenizer_encoding = tokenizer.to_string();
        cfg.token_budget_hard = hard;
        cfg.token_budget_soft = soft;
        cfg
    }

    /// Ground-truth real token count of the JSON-serialized message array
    /// via tiktoken — what the plugin measurement must match.
    fn real_tokens(messages: &[ChatMessage]) -> usize {
        let json = serde_json::to_string(messages).unwrap();
        tiktoken_rs::get_bpe_from_model("gpt-4")
            .unwrap()
            .encode_with_special_tokens(&json)
            .len()
    }

    /// The OLD chars/4 proxy this task replaces (kept here to prove it is
    /// dead: the gate must fire on the real count even when chars/4 would
    /// stay under the budget).
    fn chars_4_proxy(messages: &[ChatMessage]) -> usize {
        chars_of(messages) / 4
    }

    fn chars_of(messages: &[ChatMessage]) -> usize {
        messages
            .iter()
            .map(|m| {
                let calls = m
                    .tool_calls
                    .as_ref()
                    .map(|calls| {
                        calls
                            .iter()
                            .map(|call| {
                                call.function.name.chars().count()
                                    + call.function.arguments.chars().count()
                            })
                            .sum::<usize>()
                    })
                    .unwrap_or(0);
                m.content.chars().count() + calls
            })
            .sum()
    }

    async fn run_compact(
        messages: &[ChatMessage],
        cfg: &PluginConfig,
        keep_recent: usize,
        thread_dir: Option<&str>,
    ) -> serde_json::Value {
        let arr: Vec<serde_json::Value> = messages
            .iter()
            .map(|m| serde_json::to_value(m).unwrap())
            .collect();
        let mut args = json!({
            "messages": arr,
            "keep_recent": keep_recent,
        });
        if let Some(dir) = thread_dir {
            args["thread_dir"] = json!(dir);
            args["current_iteration"] = json!(7);
        }
        let (out, is_error) = handle_compact_messages(&args, cfg).await.unwrap();
        assert!(!is_error, "compact-messages must not error: {out}");
        serde_json::from_str(&out).unwrap()
    }

    // (a) The measurement uses REAL tiktoken tokens, not the chars/4 proxy.
    #[test]
    fn measure_uses_real_tiktoken_tokens_not_chars_proxy() {
        let msgs = vec![
            user_msg("hello world"),
            tool_call_msg(
                "filesystem_read",
                r#"{"path":"/opt/workspace/omniagent/src/main.rs","offset":0,"limit":50000}"#,
                "reading the file",
            ),
            tool_result("filesystem_read", &"x".repeat(2000)),
        ];

        // Configured encoding -> real tokens, exactly matching tiktoken.
        let (size, unit) = measure_size(&msgs, "gpt-4");
        assert_eq!(unit, SizeUnit::Tokens);
        assert_eq!(size, real_tokens(&msgs), "must be the real tiktoken count");
        assert!(size > 0);

        // Meaningfully different from the chars/4 proxy (dense JSON args
        // tokenize far denser than 4 chars per token).
        assert!(
            (size as i64 - chars_4_proxy(&msgs) as i64).abs() > 10,
            "real token count must differ meaningfully from chars/4"
        );

        // No encoding configured -> chars (token budgets not in use).
        let (chars_size, chars_unit) = measure_size(&msgs, "");
        assert_eq!(chars_unit, SizeUnit::Chars);
        assert_eq!(chars_size, chars_of(&msgs));

        // Tokenizer failure -> char fallback, reported as Chars so the gate
        // compares against the CHAR budgets (never chars/4 vs token budgets).
        let (fallback_size, fallback_unit) = measure_size(&msgs, "nonexistent_encoding_xyz");
        assert_eq!(fallback_unit, SizeUnit::Chars);
        assert_eq!(fallback_size, chars_size);
    }

    // (b) Compaction triggers on the REAL token count — a case where the old
    // chars/4 proxy would stay under the hard budget and never fire.
    #[tokio::test]
    async fn compaction_triggers_on_real_token_count_where_chars_proxy_stays_dead() {
        // Digit-dense tool args/results tokenize ~1 token/char in cl100k_base,
        // so the REAL count dwarfs the chars/4 proxy.
        let dense = "1234567890".repeat(20_000); // 200,000 chars of digits
        let mut msgs = vec![user_msg("run the analysis")];
        for i in 0..4 {
            msgs.push(tool_call_msg(
                "query_database",
                &dense,
                &format!("query {i}"),
            ));
            msgs.push(tool_result("query_database", &dense));
        }
        msgs.push(assistant_msg("done"));

        let real = real_tokens(&msgs);
        let proxy = chars_4_proxy(&msgs);
        assert!(
            real > proxy,
            "dense digits must tokenize denser than chars/4"
        );

        // A hard budget the REAL count exceeds but the proxy stays under:
        // the old gate would never fire; the new one must.
        let hard = (real + proxy) / 2;
        assert!(real > hard, "REAL token count must exceed the hard budget");
        assert!(
            proxy < hard,
            "chars/4 proxy must stay under the hard budget"
        );

        // Positive: real tokens over hard -> compaction fires and drains old
        // tool-result turns.
        let cfg = compact_cfg("gpt-4", hard, hard / 2);
        let out = run_compact(&msgs, &cfg, 2, None).await;
        assert_eq!(out["was_compacted"], true);
        let arr = out["messages"].as_array().expect("compacted array");
        assert!(
            arr.len() < msgs.len(),
            "old tool-result turns must be drained"
        );

        // Negative control: budget above the real count -> no compaction.
        let cfg2 = compact_cfg("gpt-4", real + 1000, real + 1000);
        let out2 = run_compact(&msgs, &cfg2, 2, None).await;
        assert_eq!(out2["was_compacted"], false);
        assert!(
            out2["messages"].is_null(),
            "no compaction -> messages must be null"
        );
    }

    // (c) keep_recent turns survive verbatim through compaction.
    #[tokio::test]
    async fn keep_recent_turns_survive_verbatim() {
        let mut msgs = vec![user_msg("start")];
        for i in 0..5 {
            msgs.push(tool_call_msg(
                "filesystem_read",
                r#"{"path":"/etc/x"}"#,
                &format!("reading {i}"),
            ));
            msgs.push(tool_result("filesystem_read", &format!("result {i}")));
        }
        msgs.push(assistant_msg("final answer"));

        // Hard budget triggers; soft budget is HIGH so the kept recent
        // turns fit after the first pass (the point: keep_recent survives).
        let cfg = compact_cfg("gpt-4", 100, 5000);
        let out = run_compact(&msgs, &cfg, 2, None).await;
        assert_eq!(out["was_compacted"], true);
        let arr = out["messages"].as_array().expect("compacted array");

        // Only the 2 most recent tool-result messages survive, verbatim.
        let tool_msgs: Vec<&serde_json::Value> =
            arr.iter().filter(|m| m["role"] == "tool").collect();
        assert_eq!(
            tool_msgs.len(),
            2,
            "only the 2 most recent tool results survive"
        );
        assert_eq!(tool_msgs[0]["content"], "result 3");
        assert_eq!(tool_msgs[1]["content"], "result 4");

        // Their assistant tool-call turns survive too, with tool_calls intact.
        let call_msgs: Vec<&serde_json::Value> = arr
            .iter()
            .filter(|m| m["role"] == "assistant" && m["tool_calls"].is_array())
            .collect();
        assert_eq!(call_msgs.len(), 2);
        assert_eq!(call_msgs[0]["content"], "reading 3");
        assert_eq!(call_msgs[1]["content"], "reading 4");

        // Final answer kept; oldest drained turns marked as compacted.
        let joined = serde_json::to_string(&arr).unwrap();
        assert!(joined.contains("final answer"));
        assert!(joined.contains("[context compacted: filesystem_read"));
    }

    // (d) Retention channels stay intact: read-type tool results are
    // excerpted into auto-notes.md AND dumped to context-<iter>.json.
    #[tokio::test]
    async fn read_type_results_excerpted_into_auto_notes_and_dump() {
        let tmp = std::env::temp_dir().join(format!(
            "prompt-compact-retention-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let dir = tmp.to_str().unwrap().to_string();

        let mut msgs = vec![user_msg("read the files")];
        for i in 0..4 {
            msgs.push(tool_call_msg(
                "filesystem_read",
                r#"{"path":"/etc/x"}"#,
                &format!("reading {i}"),
            ));
            msgs.push(tool_result(
                "filesystem_read",
                &format!("FILE CONTENT {i} ").repeat(50),
            ));
        }
        msgs.push(assistant_msg("done"));

        let cfg = compact_cfg("gpt-4", 100, 50);
        let out = run_compact(&msgs, &cfg, 2, Some(&dir)).await;
        assert_eq!(out["was_compacted"], true);

        // WS-2/WS-3: durable context dump written with the drained read results.
        let dump_path = tmp.join("context-7.json");
        let dump_text = std::fs::read_to_string(&dump_path)
            .unwrap_or_else(|_| panic!("context dump missing: {}", dump_path.display()));
        assert!(dump_text.contains("filesystem_read"));

        // Auto-notes: read-type results excerpted into auto-notes.md.
        let notes_path = tmp.join("auto-notes.md");
        let notes_text = std::fs::read_to_string(&notes_path)
            .unwrap_or_else(|_| panic!("auto-notes missing: {}", notes_path.display()));
        assert!(notes_text.contains("[engine:auto-note filesystem_read]"));
        assert!(notes_text.contains("FILE CONTENT"));

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
