//! DB-focused structs using only primitive types compatible with sql-forge's
//! compile-time validation. Each struct mirrors a domain model but stores
//! complex types (DateTime, JSON) as plain strings. Conversion to
//! domain types is done explicitly in Rust — no SQL type casting.

use chrono::{DateTime, Utc};
use sql_forge::sql_forge;
use sqlx::PgPool;

use crate::models::{Action, Channel, ChannelStop, Message, MessageNew, Thread};
use crate::agent::AgentConfig;

// ---------------------------------------------------------------------------
// Thread DB struct (for SELECT results)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ThreadDb {
    pub id: i64,
    pub status: String,
    pub cause: String,
    pub channel_id: i64,
    pub profile: String,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub input_tokens: Option<i32>,
    pub cached_tokens: Option<i32>,
    pub output_tokens: Option<i32>,
    pub duration_ms: Option<i32>,
    pub created_at: Option<String>,
    pub started_at: Option<String>,
    pub ended_at: Option<String>,
    pub terminal: bool,
    pub task_id: Option<String>,
    pub schedule_task_id: Option<String>,
    pub planning_mode: String,
}

impl TryFrom<ThreadDb> for Thread {
    type Error = anyhow::Error;

    fn try_from(db: ThreadDb) -> Result<Self, Self::Error> {
        Ok(Self {
            id: db.id,
            status: db.status,
            cause: db.cause,
            channel_id: db.channel_id,
            profile: db.profile,
            provider: db.provider,
            model: db.model,
            input_tokens: db.input_tokens.unwrap_or(0),
            cached_tokens: db.cached_tokens.unwrap_or(0),
            output_tokens: db.output_tokens.unwrap_or(0),
            duration_ms: db.duration_ms.unwrap_or(0),
            created_at: db
                .created_at
                .as_deref()
                .unwrap_or("")
                .parse::<DateTime<Utc>>()
                .map_err(|e| anyhow::anyhow!("Invalid timestamp '{}': {}", db.created_at.as_deref().unwrap_or("?"), e))?,
            started_at: if let Some(ref s) = db.started_at {
                if !s.is_empty() {
                    Some(s.parse::<DateTime<Utc>>().map_err(|e| anyhow::anyhow!("Invalid timestamp '{}': {}", s, e))?)
                } else {
                    None
                }
            } else {
                None
            },
            ended_at: if let Some(ref s) = db.ended_at {
                if !s.is_empty() {
                    Some(s.parse::<DateTime<Utc>>().map_err(|e| anyhow::anyhow!("Invalid timestamp '{}': {}", s, e))?)
                } else {
                    None
                }
            } else {
                None
            },
            terminal: db.terminal,
            task_id: db.task_id,
            schedule_task_id: db.schedule_task_id,
            planning_mode: db.planning_mode,
        })
    }
}

// ---------------------------------------------------------------------------
// Message DB struct (for SELECT results) — simplified without per-thread fields
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct MessageDb {
    pub id: i64,
    pub thread_id: i64,
    pub role: String,
    pub content: String,
    pub thread_sequence: i32,
    pub external_id: Option<String>,
    pub metadata: Option<String>,
    pub embedding: Option<String>,
    pub summary_text: Option<String>,
    pub is_summary: bool,
    pub msg_type: String,
    pub msg_subtype: Option<String>,
    pub created_at: Option<String>,
    pub token_usage: Option<String>,
    pub processing_time_ms: Option<i32>,
}

impl TryFrom<MessageDb> for Message {
    type Error = anyhow::Error;

    fn try_from(db: MessageDb) -> Result<Self, Self::Error> {
        Ok(Self {
            id: db.id,
            thread_id: db.thread_id,
            role: db.role,
            content: db.content,
            thread_sequence: db.thread_sequence,
            external_id: db.external_id,
            metadata: db.metadata.as_deref().map(|s| serde_json::from_str(s).unwrap_or_default()).unwrap_or_default(),
            embedding: db.embedding,
            summary_text: db.summary_text,
            is_summary: db.is_summary,
            msg_type: db.msg_type,
            msg_subtype: db.msg_subtype,
            created_at: db
                .created_at
                .as_deref()
                .unwrap_or("")
                .parse::<DateTime<Utc>>()
                .map_err(|e| anyhow::anyhow!("Invalid timestamp '{}': {}", db.created_at.as_deref().unwrap_or("?"), e))?,
            token_usage: db.token_usage.as_deref().map(|s| serde_json::from_str(s).unwrap_or_default()),
            processing_time_ms: db.processing_time_ms,
        })
    }
}

// ---------------------------------------------------------------------------
// Channel DB struct (for SELECT results) — unchanged
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ChannelDb {
    pub id: i64,
    pub name: String,
    pub platform: Option<String>,
    pub resource_identifier: Option<String>,
    pub external_id: Option<String>,
    pub cause: String,
    pub current_profile: String,
    pub current_model: Option<String>,
    pub current_provider: Option<String>,
    pub readonly: bool,
    pub closed: Option<bool>,
    pub metadata: Option<String>,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
}

impl TryFrom<ChannelDb> for Channel {
    type Error = anyhow::Error;

    fn try_from(db: ChannelDb) -> Result<Self, Self::Error> {
        Ok(Self {
            id: db.id,
            name: db.name,
            platform: db.platform,
            resource_identifier: db.resource_identifier,
            external_id: db.external_id,
            cause: db.cause,
            current_profile: db.current_profile,
            current_model: db.current_model,
            current_provider: db.current_provider,
            readonly: db.readonly,
            closed: db.closed.unwrap_or(false),
            metadata: db.metadata.as_deref().map(|s| serde_json::from_str(s).unwrap_or_default()).unwrap_or_default(),
            created_at: db
                .created_at
                .as_deref()
                .unwrap_or("")
                .parse::<DateTime<Utc>>()
                .map_err(|e| anyhow::anyhow!("Invalid timestamp '{}': {}", db.created_at.as_deref().unwrap_or("?"), e))?,
            updated_at: db
                .updated_at
                .as_deref()
                .unwrap_or("")
                .parse::<DateTime<Utc>>()
                .map_err(|e| anyhow::anyhow!("Invalid timestamp '{}': {}", db.updated_at.as_deref().unwrap_or("?"), e))?,
        })
    }
}

// ---------------------------------------------------------------------------
// ChannelStop DB struct (for SELECT results) — unchanged
// ---------------------------------------------------------------------------

#[allow(dead_code)]
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ChannelStopDb {
    pub id: i64,
    pub channel_id: i64,
    pub stopped_at: Option<String>,
}

impl TryFrom<ChannelStopDb> for ChannelStop {
    type Error = anyhow::Error;

    fn try_from(db: ChannelStopDb) -> Result<Self, Self::Error> {
        Ok(Self {
            id: db.id,
            channel_id: db.channel_id,
            stopped_at: db
                .stopped_at
                .as_deref()
                .unwrap_or("")
                .parse::<DateTime<Utc>>()
                .map_err(|e| anyhow::anyhow!("Invalid timestamp '{}': {}", db.stopped_at.as_deref().unwrap_or("?"), e))?,
        })
    }
}

// ---------------------------------------------------------------------------
// Summary DB struct (for SELECT results) — unchanged
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct SummaryDb {
    pub id: i64,
    #[allow(dead_code)]
    pub channel_id: i64,
    pub next_thread_id: i64,
    pub content: String,
    #[allow(dead_code)]
    pub created_at: Option<String>,
}

// ---------------------------------------------------------------------------
// Subscription DB struct
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct SubscriptionDb {
    pub id: i64,
    pub channel_id: i64,
    pub subscriber_platform: String,
    pub subscriber_resource: String,
    #[allow(dead_code)]
    pub created_at: Option<String>,
}

// ---------------------------------------------------------------------------
// Thread query functions
// ---------------------------------------------------------------------------

/// Create a new thread with status 'created'.
pub async fn create_thread(
    pool: &PgPool,
    cause: &str,
    channel_id: i64,
    profile: &str,
    provider: Option<&str>,
    model: Option<&str>,
    task_id: Option<&str>,
    schedule_task_id: Option<&str>,
    planning_mode: &str,
) -> anyhow::Result<Thread> {
    let row: ThreadDb = sql_forge!(
        ThreadDb,
        r#"
        INSERT INTO threads (status, cause, channel_id, profile, provider, model, task_id, schedule_task_id, planning_mode)
        VALUES ('created', :cause, :channel_id, :profile, NULLIF(:provider, '')::text, NULLIF(:model, '')::text, NULLIF(:task_id, '')::text, NULLIF(:schedule_task_id, '')::text, :planning_mode)
        RETURNING
            id, status, cause, channel_id, profile, provider, model, task_id, schedule_task_id,
            input_tokens, cached_tokens, output_tokens, duration_ms,
            COALESCE(TO_CHAR(created_at, 'YYYY-MM-DD"T"HH24' || CHR(58) || 'MI' || CHR(58) || 'SS.US"Z"'), '') AS "created_at",
            ''::text AS "started_at",
            ''::text AS "ended_at",
            terminal,
            planning_mode
        "#,
        ( :cause = cause, :channel_id = channel_id, :profile = profile, :provider = provider.unwrap_or(""), :model = model.unwrap_or(""), :task_id = task_id.unwrap_or(""), :schedule_task_id = schedule_task_id.unwrap_or(""), :planning_mode = planning_mode )
    )
    .fetch_one(pool)
    .await?;

    row.try_into()
}

/// Set a thread's status to 'system' (terminal — init messages like /start).
/// These threads should never be picked up by the executor.
pub async fn set_thread_system(pool: &PgPool, thread_id: i64) -> anyhow::Result<()> {
    sql_forge!(
        "UPDATE threads SET status = 'system', terminal = true WHERE id = :id",
        ( :id = thread_id )
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Set a thread's status to 'failed' (terminal — action execution failure).
/// These threads should never be picked up by the executor.
pub async fn set_thread_failed(pool: &PgPool, thread_id: i64) -> anyhow::Result<()> {
    sql_forge!(
        "UPDATE threads SET status = 'failed', terminal = true WHERE id = :id",
        ( :id = thread_id )
    )
    .execute(pool)
    .await?;
    Ok(())
}

// ── Planning mode resolution ──────────────────────────────────

/// Resolve what planning_mode to stamp on a thread at creation time.
///
/// Priority:
/// 1. Channel planning_mode (column on channels table) — overrides everything
/// 2. Task-level planning_mode (for cron tasks: "no_plan" -> "prompt_only",
///    "max_plan" -> max of global mode)
/// 3. Kanban tasks always get the max plan mode currently enabled
/// 4. Default (empty string) — runtime uses complexity-based logic
///
/// The resolved value is stored on the thread at creation time and is the
/// single source of truth during thread execution.
pub fn resolve_thread_planning_mode(
    channel_planning_mode: &str,
    task_planning_mode: &str,
    msg_type: &str,
    global_planning_mode: &str,
) -> String {
    // 1. Channel override (absolute — overrides everything)
    if !channel_planning_mode.is_empty() {
        return normalize_task_planning_mode(channel_planning_mode);
    }

    // 2. Cron task with explicit mode
    if msg_type == "cron" && !task_planning_mode.is_empty() {
        match task_planning_mode {
            "no_plan" => return "prompt_only".to_string(),
            "max_plan" => return resolve_max_plan(global_planning_mode),
            other => return normalize_task_planning_mode(other),
        }
    }

    // 3. Kanban — always use max plan mode currently enabled
    if msg_type == "kanban" {
        return resolve_max_plan(global_planning_mode);
    }

    // 4. Default: empty string — runtime does complexity-based resolution
    String::new()
}

/// Normalize a planning mode value to one of the canonical values.
fn normalize_task_planning_mode(mode: &str) -> String {
    match mode {
        "never" => "prompt_only".to_string(),
        "always" => "auto_subtasks".to_string(),
        other => other.to_string(),
    }
}

/// Calculate the maximum plan mode that should be used based on the
/// global PLANNING_MODE setting. Kanban tasks always use this.
fn resolve_max_plan(global_mode: &str) -> String {
    match global_mode {
        "auto_subtasks" | "always" => "auto_subtasks".to_string(),
        "auto_plan" => "auto_plan".to_string(),
        _ => "prompt_only".to_string(),
    }
}

/// Resolve the max tool-call iterations based on the thread's planning mode.
/// These replace the old single MAX_ITERATIONS setting.
pub fn max_iterations_for_planning_mode(config: &AgentConfig, planning_mode: &str) -> u32 {
    match planning_mode {
        "auto_subtasks" | "always" => config.max_iterations_complex_plan,
        "auto_plan" => config.max_iterations_simple_plan,
        _ => config.max_iterations_no_plan, // no plan or complexity-based
    }
}

#[allow(dead_code)]
/// Set a thread's status to 'pending' so the executor picks it up.
pub async fn set_thread_pending(pool: &PgPool, thread_id: i64) -> anyhow::Result<()> {
    sql_forge!(
        "UPDATE threads SET status = 'pending' WHERE id = :id AND NOT terminal",
        ( :id = thread_id )
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Create the seq-0 (cause) message and set the thread to pending in a single transaction.
pub async fn create_cause_and_set_pending(pool: &PgPool, msg: &MessageNew) -> anyhow::Result<Message> {
    let mut tx = pool.begin().await?;
    let metadata_val: serde_json::Value = serde_json::from_str(&msg.metadata.to_string()).unwrap_or_default();
    let token_usage_val: serde_json::Value = msg.token_usage.clone().unwrap_or(serde_json::Value::Null);
    let saved: MessageDb = sql_forge!(
        MessageDb,
        r#"
        INSERT INTO messages (
            thread_id, role, content, thread_sequence, external_id,
            metadata, embedding, summary_text, is_summary,
            msg_type, msg_subtype, processing_time_ms, token_usage
        )
        VALUES (:thread_id, :role, :content, :thread_sequence, NULLIF(:external_id, '')::text,
            :metadata, NULLIF(:embedding, '')::text, NULLIF(:summary_text, '')::text, :is_summary,
            :msg_type, NULLIF(:msg_subtype, '')::text, NULLIF(:processing_time_ms, -1)::int, NULLIF(:token_usage, 'null')::jsonb)
        RETURNING
            id, thread_id, role, content, thread_sequence, external_id,
            metadata::text AS "metadata", embedding, summary_text, is_summary,
            msg_type, msg_subtype,
            token_usage::text AS "token_usage", processing_time_ms,
            COALESCE(TO_CHAR(created_at, 'YYYY-MM-DD"T"HH24' || CHR(58) || 'MI' || CHR(58) || 'SS.US"Z"'), '') AS "created_at"
        "#,
        ( :thread_id = msg.thread_id, :role = &msg.role, :content = &msg.content, :thread_sequence = msg.thread_sequence, :external_id = msg.external_id.as_deref().unwrap_or(""), :metadata = &metadata_val, :embedding = msg.embedding.as_deref().unwrap_or(""), :summary_text = msg.summary_text.as_deref().unwrap_or(""), :is_summary = msg.is_summary, :msg_type = &msg.msg_type, :msg_subtype = msg.msg_subtype.as_deref().unwrap_or(""), :processing_time_ms = msg.processing_time_ms.unwrap_or(-1), :token_usage = &token_usage_val.to_string() )
    )
    .fetch_one(&mut *tx)
    .await?;

    // Determine thread status based on channel state
    // If the channel is closed, set to 'skipped' unless the message role is 'system' (for /open etc.)
    let thread_status = {
        let channel_closed: Option<bool> = sql_forge!(
            scalar Option<bool>,
            r#"
            SELECT ch.closed
            FROM channels ch
            JOIN threads t ON t.channel_id = ch.id
            WHERE t.id = :thread_id
            "#,
            ( :thread_id = msg.thread_id )
        )
        .fetch_one(&mut *tx)
        .await?;

        if channel_closed.unwrap_or(false) && msg.role != "system" {
            "skipped"
        } else {
            "pending"
        }
    };

    sql_forge!(
        "UPDATE threads SET status = :status WHERE id = :id AND NOT terminal",
        ( :status = thread_status, :id = msg.thread_id )
    )
    .execute(&mut *tx)
    .await?;

    // If the thread was skipped because the channel is closed, move the
    // associated kanban task back to 'todo' so it can be retried later.
    if thread_status == "skipped" {
        let _ = sql_forge!(
            "UPDATE kanban_tasks SET status = 'todo', updated_at = NOW() WHERE id = (
                SELECT task_id FROM threads WHERE id = :tid AND task_id IS NOT NULL
            )",
            ( :tid = msg.thread_id )
        )
        .execute(&mut *tx)
        .await;
    }

    tx.commit().await?;
    saved.try_into()
}

/// Find pending threads for a channel.
pub async fn find_pending_threads_by_channel(
    pool: &PgPool,
    channel_id: i64,
) -> anyhow::Result<Vec<Thread>> {
    let rows: Vec<ThreadDb> = sql_forge!(
        ThreadDb,
        r#"
        SELECT
            id, status, cause, channel_id, profile, provider, model, task_id, schedule_task_id,
            input_tokens, cached_tokens, output_tokens, duration_ms,
            COALESCE(TO_CHAR(created_at, 'YYYY-MM-DD"T"HH24' || CHR(58) || 'MI' || CHR(58) || 'SS.US"Z"'), '') AS "created_at",
            COALESCE(TO_CHAR(started_at, 'YYYY-MM-DD"T"HH24' || CHR(58) || 'MI' || CHR(58) || 'SS.US"Z"'), '') AS "started_at",
            COALESCE(TO_CHAR(ended_at, 'YYYY-MM-DD"T"HH24' || CHR(58) || 'MI' || CHR(58) || 'SS.US"Z"'), '') AS "ended_at",
            terminal,
            planning_mode
        FROM threads
        WHERE channel_id = :channel_id AND status = 'pending'
        ORDER BY created_at ASC
        "#,
        ( :channel_id = channel_id )
    )
    .fetch_all(pool)
    .await?;

    rows.into_iter().map(|r| r.try_into()).collect()
}

/// Atomically claim a thread by setting status to 'processing' and started_at to NOW().
/// Returns true if the thread was successfully claimed.
pub async fn claim_thread(pool: &PgPool, thread_id: i64) -> bool {
    let result = sql_forge!(
        "UPDATE threads SET status = 'processing', started_at = NOW() WHERE id = :id AND status = 'pending' AND NOT terminal",
        ( :id = thread_id )
    )
    .execute(pool)
    .await;

    match result {
        Ok(r) => r.rows_affected() > 0,
        Err(e) => {
            tracing::error!("Failed to claim thread {}: {:?}", thread_id, e);
            false
        }
    }
}

/// Complete a thread with final status and usage stats.
pub async fn complete_thread(
    pool: &PgPool,
    thread_id: i64,
    status: &str,
    _input_tokens: i32,
    _cached_tokens: i32,
    _output_tokens: i32,
    _duration_ms: i32,
) -> anyhow::Result<()> {
    sql_forge!(
        r#"
        UPDATE threads
        SET status = :status,
            input_tokens = COALESCE(
                (SELECT SUM((token_usage->>'prompt_tokens')::int)
                 FROM messages WHERE thread_id = :id AND token_usage IS NOT NULL),
                0
            ),
            cached_tokens = COALESCE(
                (SELECT SUM((token_usage->>'cached_tokens')::int)
                 FROM messages WHERE thread_id = :id AND token_usage IS NOT NULL),
                0
            ),
            output_tokens = COALESCE(
                (SELECT SUM((token_usage->>'completion_tokens')::int)
                 FROM messages WHERE thread_id = :id AND token_usage IS NOT NULL),
                0
            ),
            duration_ms = COALESCE(
                EXTRACT(EPOCH FROM (NOW() - COALESCE(started_at, NOW())))::int * 1000,
                0
            ),
            ended_at = NOW(),
            terminal = true
        WHERE id = :id AND NOT terminal
        "#,
        ( :status = status, :id = thread_id )
    )
    .execute(pool)
    .await?;

    Ok(())
}

/// Set all pending/processing threads for a channel to 'skipped'.
pub async fn skip_channel_threads(pool: &PgPool, channel_id: i64) -> anyhow::Result<u64> {
    // Mark all pending/processing threads as skipped
    let result = sql_forge!(
        "UPDATE threads SET status = 'skipped', ended_at = NOW(), terminal = true WHERE channel_id = :channel_id AND status IN ('pending', 'processing') AND NOT terminal",
        ( :channel_id = channel_id )
    )
    .execute(pool)
    .await?;

    // Update associated kanban tasks:
    // - pending (never started) → todo (can be retried)
    // - processing (was started) → blocked (needs investigation)
    let _ = sql_forge!(
        "UPDATE kanban_tasks SET status = 'todo', updated_at = NOW() WHERE id IN (
            SELECT task_id FROM threads
            WHERE channel_id = :ch AND task_id IS NOT NULL AND status = 'pending'
        ) AND status = 'ready'",
        ( :ch = channel_id )
    )
    .execute(pool)
    .await;

    let _ = sql_forge!(
        "UPDATE kanban_tasks SET status = 'blocked', updated_at = NOW() WHERE id IN (
            SELECT task_id FROM threads
            WHERE channel_id = :ch AND task_id IS NOT NULL AND status = 'processing'
        ) AND status = 'running'",
        ( :ch = channel_id )
    )
    .execute(pool)
    .await;

    Ok(result.rows_affected())
}

/// Count messages in a thread.
pub async fn count_thread_messages(pool: &PgPool, thread_id: i64) -> anyhow::Result<i32> {
    let count: Option<i64> = sql_forge!(
        scalar Option<i64>,
        "SELECT COUNT(*) FROM messages WHERE thread_id = :thread_id",
        ( :thread_id = thread_id )
    )
    .fetch_one(pool)
    .await?;

    Ok(count.unwrap_or(0) as i32)
}

/// Update a kanban task's status by task_id.
pub async fn update_kanban_status(pool: &PgPool, task_id: &str, status: &str) -> anyhow::Result<()> {
    sql_forge!(
        "UPDATE kanban_tasks SET status = :status, updated_at = NOW() WHERE id = :id",
        ( :status = status, :id = task_id )
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Skip all pending/processing threads on startup.
pub async fn skip_all_pending_threads(pool: &PgPool) -> anyhow::Result<u64> {
    let result = sql_forge!(
        r#"
        UPDATE threads
        SET status = 'skipped', ended_at = NOW(), terminal = true
        WHERE status IN ('pending', 'processing') AND NOT terminal
        "#
    )
    .execute(pool)
    .await?;

    Ok(result.rows_affected())
}

/// Get the cause message (first message, role='cause') for a thread.
pub async fn get_cause_message(pool: &PgPool, thread_id: i64) -> anyhow::Result<Option<Message>> {
    let row: Option<MessageDb> = sql_forge!(
        MessageDb,
        r#"
        SELECT
            id, thread_id, role, content, thread_sequence, external_id,
            metadata::text AS "metadata", embedding, summary_text, is_summary,
            msg_type, msg_subtype,
            token_usage::text AS "token_usage", processing_time_ms,
            COALESCE(TO_CHAR(created_at, 'YYYY-MM-DD"T"HH24' || CHR(58) || 'MI' || CHR(58) || 'SS.US"Z"'), '') AS "created_at"
        FROM messages
        WHERE thread_id = :thread_id AND role = 'cause'
        ORDER BY thread_sequence ASC, id ASC
        LIMIT 1
        "#,
        ( :thread_id = thread_id )
    )
    .fetch_optional(pool)
    .await?;

    row.map(|r| r.try_into()).transpose()
}

// ---------------------------------------------------------------------------
// Message query functions — simplified, no per-thread fields
// ---------------------------------------------------------------------------

/// Insert a new message (no status/channel/provider/model — those are on the thread).
pub async fn create_message(pool: &PgPool, msg: &MessageNew) -> anyhow::Result<Message> {
    let metadata_val: serde_json::Value = serde_json::from_str(&msg.metadata.to_string()).unwrap_or_default();
    let token_usage_val: serde_json::Value = msg.token_usage.clone().unwrap_or(serde_json::Value::Null);
    let row: MessageDb = sql_forge!(
        MessageDb,
        r#"
        INSERT INTO messages (
            thread_id, role, content, thread_sequence, external_id,
            metadata, embedding, summary_text, is_summary,
            msg_type, msg_subtype, processing_time_ms, token_usage
        )
        VALUES (:thread_id, :role, :content, :thread_sequence, NULLIF(:external_id, '')::text,
            :metadata, NULLIF(:embedding, '')::text, NULLIF(:summary_text, '')::text, :is_summary,
            :msg_type, NULLIF(:msg_subtype, '')::text, NULLIF(:processing_time_ms, -1)::int, NULLIF(:token_usage, 'null')::jsonb)
        RETURNING
            id, thread_id, role, content, thread_sequence, external_id,
            metadata::text AS "metadata", embedding, summary_text, is_summary,
            msg_type, msg_subtype,
            token_usage::text AS "token_usage", processing_time_ms,
            COALESCE(TO_CHAR(created_at, 'YYYY-MM-DD"T"HH24' || CHR(58) || 'MI' || CHR(58) || 'SS.US"Z"'), '') AS "created_at"
        "#,
        ( :thread_id = msg.thread_id, :role = &msg.role, :content = &msg.content, :thread_sequence = msg.thread_sequence, :external_id = msg.external_id.as_deref().unwrap_or(""), :metadata = &metadata_val, :embedding = msg.embedding.as_deref().unwrap_or(""), :summary_text = msg.summary_text.as_deref().unwrap_or(""), :is_summary = msg.is_summary, :msg_type = &msg.msg_type, :msg_subtype = msg.msg_subtype.as_deref().unwrap_or(""), :processing_time_ms = msg.processing_time_ms.unwrap_or(-1), :token_usage = &token_usage_val.to_string() )
    )
    .fetch_one(pool)
    .await?;

    row.try_into()
}

// ---------------------------------------------------------------------------
// Profile DB struct and queries
// ---------------------------------------------------------------------------

/// A profile as stored in the DB — uses string timestamps for sql-forge compatibility.
/// Claim a channel for a session by updating its resource_identifier.
/// Returns the old resource_identifier (if any) so the caller can notify the
/// previous session.
pub async fn claim_channel_resource(
    pool: &PgPool,
    channel_id: i64,
    session_id: &str,
) -> anyhow::Result<Option<String>> {
    // Get old resource_identifier first
    let old = find_channel_by_id(pool, channel_id).await?;
    let old_rid = old.and_then(|c| c.resource_identifier.filter(|r| !r.is_empty()));

    // Update resource_identifier and external_id to our session_id
    sql_forge!(
        r#"
        UPDATE channels
        SET resource_identifier = :session_id,
            external_id = :session_id,
            updated_at = NOW()
        WHERE id = :channel_id
        "#,
        ( :session_id = session_id, :channel_id = channel_id )
    )
    .execute(pool)
    .await?;

    Ok(old_rid)
}

/// Validate that a channel name contains only allowed characters.
pub fn validate_channel_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name
            .chars()
            .all(|c| c.is_alphanumeric() || c == '_' || c == '-')
}

// ---------------------------------------------------------------------------
// Channel query functions — mostly unchanged
// ---------------------------------------------------------------------------

pub async fn find_all_channels(pool: &PgPool) -> anyhow::Result<Vec<Channel>> {
    let rows: Vec<ChannelDb> = sql_forge!(
        ChannelDb,
        r#"
        SELECT
            id, name,
            COALESCE(platform, '') AS "platform",
            resource_identifier,
            COALESCE(external_id, '') AS "external_id",
            cause,
            current_profile, current_model, current_provider,
            readonly,
            COALESCE(closed, false) as "closed",
            '{}'::text AS "metadata",
            COALESCE(TO_CHAR(created_at, 'YYYY-MM-DD"T"HH24' || CHR(58) || 'MI' || CHR(58) || 'SS.US"Z"'), '') AS "created_at",
            COALESCE(TO_CHAR(updated_at, 'YYYY-MM-DD"T"HH24' || CHR(58) || 'MI' || CHR(58) || 'SS.US"Z"'), '') AS "updated_at"
        FROM channels
        ORDER BY name ASC
        "#
    )
    .fetch_all(pool)
    .await?;

    rows.into_iter().map(|r| r.try_into()).collect()
}

pub async fn get_channel_by_name(pool: &PgPool, name: &str) -> anyhow::Result<Option<Channel>> {
    let row: Option<ChannelDb> = sql_forge!(
        ChannelDb,
        r#"
        SELECT
            id, name,
            COALESCE(platform, '') AS "platform",
            resource_identifier,
            COALESCE(external_id, '') AS "external_id",
            cause,
            current_profile, current_model, current_provider,
            readonly,
            COALESCE(closed, false) as "closed",
            '{}'::text AS "metadata",
            COALESCE(TO_CHAR(created_at, 'YYYY-MM-DD"T"HH24' || CHR(58) || 'MI' || CHR(58) || 'SS.US"Z"'), '') AS "created_at",
            COALESCE(TO_CHAR(updated_at, 'YYYY-MM-DD"T"HH24' || CHR(58) || 'MI' || CHR(58) || 'SS.US"Z"'), '') AS "updated_at"
        FROM channels
        WHERE name = :name
        "#,
        ( :name = name )
    )
    .fetch_optional(pool)
    .await?;

    row.map(|r| r.try_into()).transpose()
}

pub async fn get_channel_by_platform_name(
    pool: &PgPool,
    platform: &str,
    name: &str,
) -> anyhow::Result<Option<Channel>> {
    let row: Option<ChannelDb> = sql_forge!(
        ChannelDb,
        r#"
        SELECT
            id, name,
            COALESCE(platform, '') AS "platform",
            resource_identifier,
            COALESCE(external_id, '') AS "external_id",
            cause,
            current_profile, current_model, current_provider,
            readonly,
            COALESCE(closed, false) as "closed",
            '{}'::text AS "metadata",
            COALESCE(TO_CHAR(created_at, 'YYYY-MM-DD"T"HH24' || CHR(58) || 'MI' || CHR(58) || 'SS.US"Z"'), '') AS "created_at",
            COALESCE(TO_CHAR(updated_at, 'YYYY-MM-DD"T"HH24' || CHR(58) || 'MI' || CHR(58) || 'SS.US"Z"'), '') AS "updated_at"
        FROM channels
        WHERE platform = :platform AND name = :name
        "#,
        ( :platform = platform, :name = name )
    )
    .fetch_optional(pool)
    .await?;

    row.map(|r| r.try_into()).transpose()
}

pub async fn find_channel_by_id(
    pool: &PgPool,
    channel_id: i64,
) -> anyhow::Result<Option<Channel>> {
    let row: Option<ChannelDb> = sql_forge!(
        ChannelDb,
        r#"
        SELECT
            id, name,
            COALESCE(platform, '') AS "platform",
            resource_identifier,
            COALESCE(external_id, '') AS "external_id",
            cause,
            current_profile, current_model, current_provider,
            readonly,
            COALESCE(closed, false) as "closed",
            '{}'::text AS "metadata",
            COALESCE(TO_CHAR(created_at, 'YYYY-MM-DD"T"HH24' || CHR(58) || 'MI' || CHR(58) || 'SS.US"Z"'), '') AS "created_at",
            COALESCE(TO_CHAR(updated_at, 'YYYY-MM-DD"T"HH24' || CHR(58) || 'MI' || CHR(58) || 'SS.US"Z"'), '') AS "updated_at"
        FROM channels
        WHERE id = :channel_id
        "#,
        ( :channel_id = channel_id )
    )
    .fetch_optional(pool)
    .await?;

    row.map(|r| r.try_into()).transpose()
}

/// Get a channel by id, including its actual metadata (not hardcoded '{}').
pub async fn get_channel_by_id(pool: &PgPool, channel_id: i64) -> anyhow::Result<Option<Channel>> {
    let row: Option<ChannelDb> = sql_forge!(
        ChannelDb,
        r#"
        SELECT
            id, name,
            COALESCE(platform, '') AS "platform",
            resource_identifier,
            COALESCE(external_id, '') AS "external_id",
            cause,
            current_profile, current_model, current_provider,
            readonly,
            COALESCE(closed, false) as "closed",
            COALESCE(metadata::text, '{}') AS "metadata",
            COALESCE(TO_CHAR(created_at, 'YYYY-MM-DD"T"HH24' || CHR(58) || 'MI' || CHR(58) || 'SS.US"Z"'), '') AS "created_at",
            COALESCE(TO_CHAR(updated_at, 'YYYY-MM-DD"T"HH24' || CHR(58) || 'MI' || CHR(58) || 'SS.US"Z"'), '') AS "updated_at"
        FROM channels
        WHERE id = :id
        "#,
        ( :id = channel_id )
    )
    .fetch_optional(pool)
    .await?;
    row.map(|r| r.try_into()).transpose()
}

pub async fn create_channel(
    pool: &PgPool,
    name: &str,
    platform: &str,
    external_id: &str,
    cause: &str,
    resource_identifier: &str,
) -> anyhow::Result<Channel> {
    let row: ChannelDb = sql_forge!(
        ChannelDb,
        r#"
        INSERT INTO channels (name, platform, external_id, cause, resource_identifier)
        VALUES (:name, NULLIF(:platform, '')::text, :external_id, :cause, NULLIF(:resource_identifier, '')::text)
        ON CONFLICT (platform, external_id)
        DO UPDATE SET updated_at = NOW()
        RETURNING
            id, name,
            COALESCE(platform, '') AS "platform",
            resource_identifier,
            COALESCE(external_id, '') AS "external_id",
            cause,
            current_profile, current_model, current_provider,
            readonly,
            COALESCE(closed, false) as "closed",
            COALESCE(metadata::text, '{}') AS "metadata",
            COALESCE(TO_CHAR(created_at, 'YYYY-MM-DD"T"HH24' || CHR(58) || 'MI' || CHR(58) || 'SS.US"Z"'), '') AS "created_at",
            COALESCE(TO_CHAR(updated_at, 'YYYY-MM-DD"T"HH24' || CHR(58) || 'MI' || CHR(58) || 'SS.US"Z"'), '') AS "updated_at"
        "#,
        ( :name = name, :platform = platform, :external_id = external_id, :cause = cause, :resource_identifier = resource_identifier )
    )
    .fetch_one(pool)
    .await?;

    row.try_into()
}

/// Look up a channel by (platform, resource_identifier).
pub async fn get_channel_by_platform_and_resource(
    pool: &PgPool,
    platform: &str,
    resource_identifier: &str,
) -> anyhow::Result<Option<Channel>> {
    let row: Option<ChannelDb> = sql_forge!(
        ChannelDb,
        r#"
        SELECT
            id, name,
            COALESCE(platform, '') AS "platform",
            resource_identifier,
            COALESCE(external_id, '') AS "external_id",
            cause,
            current_profile, current_model, current_provider,
            readonly,
            COALESCE(closed, false) as "closed",
            COALESCE(metadata::text, '{}') AS "metadata",
            COALESCE(TO_CHAR(created_at, 'YYYY-MM-DD"T"HH24' || CHR(58) || 'MI' || CHR(58) || 'SS.US"Z"'), '') AS "created_at",
            COALESCE(TO_CHAR(updated_at, 'YYYY-MM-DD"T"HH24' || CHR(58) || 'MI' || CHR(58) || 'SS.US"Z"'), '') AS "updated_at"
        FROM channels
        WHERE platform = :platform AND resource_identifier = :resource_identifier
        "#,
        ( :platform = platform, :resource_identifier = resource_identifier )
    )
    .fetch_optional(pool)
    .await?;

    row.map(|r| r.try_into()).transpose()
}

/// Old channel info returned by `update_channel_platform`.
#[derive(Debug, Clone)]
pub struct OldChannelInfo {
    pub old_platform: Option<String>,
    pub old_resource_identifier: Option<String>,
}

/// Update a channel's platform + resource_identifier by its stable channel ID.
///
/// This is used when a channel's connection changes (e.g., from telegram:chat1
/// to discord:server1). The channel is found by its stable `channel_id`, not
/// by platform + external_id (which just changed).
///
/// Returns the old platform and resource_identifier values so callers can
/// notify the old platform that the channel is no longer active there.
pub async fn update_channel_platform(
    pool: &PgPool,
    channel_id: i64,
    new_platform: &str,
    new_resource_identifier: &str,
    new_external_id: &str,
) -> anyhow::Result<OldChannelInfo> {
    // Query old values first
    let old: Option<ChannelDb> = sql_forge!(
        ChannelDb,
        r#"
        SELECT
            id, name,
            COALESCE(platform, '') AS "platform",
            resource_identifier,
            COALESCE(external_id, '') AS "external_id",
            cause,
            current_profile, current_model, current_provider,
            readonly,
            COALESCE(closed, false) as "closed",
            '{}'::text AS "metadata",
            ''::text AS "created_at",
            ''::text AS "updated_at"
        FROM channels
        WHERE id = :id
        "#,
        ( :id = channel_id )
    )
    .fetch_optional(pool)
    .await?;

    let old_platform = old.as_ref().and_then(|c| {
        let p = c.platform.as_deref().unwrap_or("");
        if p.is_empty() { None } else { Some(p.to_string()) }
    });
    let old_resource_identifier = old.as_ref().and_then(|c| c.resource_identifier.clone());

    // Update the row
    sql_forge!(
        r#"
        UPDATE channels
        SET platform = NULLIF(:platform, '')::text,
            resource_identifier = NULLIF(:resource_identifier, '')::text,
            external_id = NULLIF(:external_id, '')::text,
            updated_at = NOW()
        WHERE id = :id
        "#,
        ( :platform = new_platform, :resource_identifier = new_resource_identifier, :external_id = new_external_id, :id = channel_id )
    )
    .execute(pool)
    .await?;

    Ok(OldChannelInfo {
        old_platform,
        old_resource_identifier,
    })
}

/// Update a channel's provider and/or model by its stable channel ID.
///
/// Only non-None fields are updated (partial update). Pass `None` to leave
/// the current value unchanged, or `Some("")` to clear it to NULL.
pub async fn update_channel_model(
    pool: &PgPool,
    channel_id: i64,
    provider: Option<&str>,
    model: Option<&str>,
) -> anyhow::Result<()> {
    // Build a dynamic UPDATE using COALESCE to preserve existing values
    // for any parameter that is None.
    let set_provider = provider.map(|p| {
        if p.is_empty() {
            "NULL::text".to_string()
        } else {
            format!("'{}'", p.replace('\'', "''"))
        }
    });
    let set_model = model.map(|m| {
        if m.is_empty() {
            "NULL::text".to_string()
        } else {
            format!("'{}'", m.replace('\'', "''"))
        }
    });

    let provider_sql = set_provider
        .map(|v| format!("current_provider = {}", v))
        .unwrap_or_default();
    let model_sql = set_model
        .map(|v| format!("current_model = {}", v))
        .unwrap_or_default();

    let mut sets = Vec::new();
    if !provider_sql.is_empty() {
        sets.push(provider_sql);
    }
    if !model_sql.is_empty() {
        sets.push(model_sql);
    }

    if sets.is_empty() {
        return Ok(());
    }

    let sql = format!(
        "UPDATE channels SET {}, updated_at = NOW() WHERE id = $1",
        sets.join(", ")
    );

    sqlx::query(sqlx::AssertSqlSafe(sql.as_str()))
        .bind(channel_id)
        .execute(pool)
        .await?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Channel open/close/status queries
// ---------------------------------------------------------------------------

/// Close a channel — sets closed=true and skips pending/processing threads.
pub async fn close_channel(pool: &PgPool, channel_id: i64) -> anyhow::Result<()> {
    sql_forge!(
        "UPDATE channels SET closed = true, updated_at = NOW() WHERE id = :id",
        ( :id = channel_id )
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Open a channel — sets closed=false so the supervisor spawns a handler.
pub async fn open_channel(pool: &PgPool, channel_id: i64) -> anyhow::Result<()> {
    sql_forge!(
        "UPDATE channels SET closed = false, updated_at = NOW() WHERE id = :id",
        ( :id = channel_id )
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Check if a channel is closed.
pub async fn is_channel_closed(pool: &PgPool, channel_id: i64) -> anyhow::Result<bool> {
    let row: Option<ChannelDb> = sql_forge!(
        ChannelDb,
        r#"
        SELECT
            id, name,
            COALESCE(platform, '') AS "platform",
            resource_identifier,
            COALESCE(external_id, '') AS "external_id",
            cause,
            current_profile, current_model, current_provider,
            readonly,
            COALESCE(closed, false) as "closed",
            '{}'::text AS "metadata",
            COALESCE(TO_CHAR(created_at, 'YYYY-MM-DD"T"HH24' || CHR(58) || 'MI' || CHR(58) || 'SS.US"Z"'), '') AS "created_at",
            COALESCE(TO_CHAR(updated_at, 'YYYY-MM-DD"T"HH24' || CHR(58) || 'MI' || CHR(58) || 'SS.US"Z"'), '') AS "updated_at"
        FROM channels
        WHERE id = :channel_id
        "#,
        ( :channel_id = channel_id )
    )
    .fetch_optional(pool)
    .await?;

    Ok(row.and_then(|r| r.closed).unwrap_or(false))
}

/// Status info for a channel: open/closed, thread counts, config.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ChannelStatus {
    pub channel_id: i64,
    pub name: String,
    pub platform: String,
    pub closed: bool,
    pub current_profile: String,
    pub current_model: Option<String>,
    pub current_provider: Option<String>,
    pub pending_threads: i64,
    pub processing_threads: i64,
}

/// Get channel status with thread counts.
pub async fn get_channel_status(pool: &PgPool, channel_id: i64) -> anyhow::Result<Option<ChannelStatus>> {
    let ch = find_channel_by_id(pool, channel_id).await?;
    let ch = match ch {
        Some(c) => c,
        None => return Ok(None),
    };

    let pending: Option<i64> = sql_forge!(
        scalar Option<i64>,
        "SELECT COUNT(*) FROM threads WHERE channel_id = :cid AND status = 'pending'",
        ( :cid = channel_id )
    )
    .fetch_one(pool)
    .await?;

    let processing: Option<i64> = sql_forge!(
        scalar Option<i64>,
        "SELECT COUNT(*) FROM threads WHERE channel_id = :cid AND status = 'processing'",
        ( :cid = channel_id )
    )
    .fetch_one(pool)
    .await?;

    Ok(Some(ChannelStatus {
        channel_id: ch.id,
        name: ch.name,
        platform: ch.platform.unwrap_or_default(),
        closed: ch.closed,
        current_profile: ch.current_profile,
        current_model: ch.current_model,
        current_provider: ch.current_provider,
        pending_threads: pending.unwrap_or(0),
        processing_threads: processing.unwrap_or(0),
    }))
}

// ---------------------------------------------------------------------------
// Channel seq-0 message query — for recent channel context
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ChannelSeq0Message {
    pub id: i64,
    pub content: String,
    pub role: String,
    pub msg_type: String,
}

/// Get the most recent seq-0 (thread root) messages for a channel.
pub async fn get_recent_channel_seq0_messages(
    pool: &PgPool,
    channel_id: i64,
    limit: i64,
) -> anyhow::Result<Vec<ChannelSeq0Message>> {
    let rows: Vec<ChannelSeq0Message> = sql_forge!(
        ChannelSeq0Message,
        r#"
        SELECT id, content, role, msg_type
        FROM messages
        WHERE thread_id IN (SELECT id FROM threads WHERE channel_id = :channel_id)
          AND thread_sequence = 0
        ORDER BY created_at DESC
        LIMIT :limit
        "#,
        ( :channel_id = channel_id, :limit = limit )
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

// ---------------------------------------------------------------------------
// Context retrieval helper functions — updated for new message schema
// ---------------------------------------------------------------------------

/// Get recent messages from a thread for context assembly.
pub async fn get_recent_thread_messages(
    pool: &PgPool,
    thread_id: i64,
    limit: i64,
) -> anyhow::Result<Vec<Message>> {
    let rows: Vec<MessageDb> = sql_forge!(
        MessageDb,
        r#"
        SELECT
            id, thread_id, role, content, thread_sequence, external_id,
            metadata::text AS "metadata", embedding, summary_text, is_summary,
            msg_type, msg_subtype,
            token_usage::text AS "token_usage", processing_time_ms,
            COALESCE(TO_CHAR(created_at, 'YYYY-MM-DD"T"HH24' || CHR(58) || 'MI' || CHR(58) || 'SS.US"Z"'), '') AS "created_at"
        FROM messages
        WHERE thread_id = :thread_id
          AND role IN ('user', 'agent')
          AND msg_type IN ('message', 'reasoning')
        ORDER BY created_at DESC
        LIMIT :limit
        "#,
        ( :thread_id = thread_id, :limit = limit )
    )
    .fetch_all(pool)
    .await?;

    rows.into_iter().map(|r| r.try_into()).collect()
}

/// Search messages by text content (ILIKE) for context retrieval.
pub async fn search_messages_text(
    pool: &PgPool,
    query: &str,
    channel_id: i64,
    limit: i64,
) -> anyhow::Result<Vec<Message>> {
    let pattern = format!("%{}%", query.replace('%', "\\%").replace('_', "\\_"));
    let rows: Vec<MessageDb> = sql_forge!(
        MessageDb,
        r#"
        SELECT
            m.id, m.thread_id, m.role, m.content, m.thread_sequence, m.external_id,
            m.metadata::text AS "metadata", m.embedding, m.summary_text, m.is_summary,
            m.msg_type, m.msg_subtype,
            m.token_usage::text AS "token_usage", m.processing_time_ms,
            COALESCE(TO_CHAR(m.created_at, 'YYYY-MM-DD"T"HH24' || CHR(58) || 'MI' || CHR(58) || 'SS.US"Z"'), '') AS "created_at"
        FROM messages m
        JOIN threads t ON t.id = m.thread_id
        WHERE t.channel_id = :channel_id
          AND m.content ILIKE :pattern
          AND m.msg_type IN ('cause', 'summary', 'plan', 'message', 'reasoning', 'error', 'tool', 'tool_result')
        ORDER BY m.created_at DESC
        LIMIT :limit
        "#,
        ( :channel_id = channel_id, :pattern = &pattern, :limit = limit )
    )
    .fetch_all(pool)
    .await?;

    rows.into_iter().map(|r| r.try_into()).collect()
}

/// Find messages where embedding IS NULL, ordered by created_at, with a limit.
pub async fn find_messages_without_embeddings(
    pool: &PgPool,
    limit: usize,
) -> anyhow::Result<Vec<crate::vectorizer::MessageEmbeddingRow>> {
    let rows: Vec<crate::vectorizer::MessageEmbeddingRow> = sql_forge!(
        crate::vectorizer::MessageEmbeddingRow,
        r#"
        SELECT id, content
        FROM messages
        WHERE embedding IS NULL
          AND msg_type IN ('cause', 'summary', 'plan', 'message', 'reasoning', 'error')
        ORDER BY created_at ASC
        LIMIT :limit
        "#,
        ( :limit = limit as i64 )
    )
    .fetch_all(pool)
    .await?;

    Ok(rows)
}

/// Update the embedding column for a given message and keep embedding_vec in sync.
pub async fn update_message_embedding(
    pool: &PgPool,
    message_id: i64,
    embedding_string: &str,
) -> anyhow::Result<()> {
    // Update both the TEXT column (for backward compat / Phase 2 migration) and
    // the native vector column (for HNSW-indexed two-stage search).
    // The vector cast is safe because embedding_string is always in `[0.1,0.2,...]` format.
    // sql_forge! cannot handle the vector cast expression, so use raw sqlx.
    sqlx::query(
        r#"
        UPDATE messages
        SET embedding = $1,
            embedding_vec = CASE WHEN $1 IS NOT NULL AND $1 != '' THEN $1::vector(1536) ELSE NULL END
        WHERE id = $2
        "#,
    )
    .bind(embedding_string)
    .bind(message_id)
    .execute(pool)
    .await?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Hybrid retrieval: pgvector semantic search — two-stage recency-weighted
// ---------------------------------------------------------------------------

/// Search messages by embedding similarity with recency-weighted two-stage decay.
///
/// Stage 1: Fetch top N candidates using HNSW-indexed ANN search (cosine distance).
/// Stage 2: Re-rank candidates by `distance × (1 + days_old)` so recent messages
/// rank higher than older ones with similar semantic match.
///
/// The `embedding_vec` column and HNSW index are created by the startup migration.
/// No fallback to TEXT-cast search — the two-stage decay is the only path.
pub async fn search_messages_semantic(
    pool: &PgPool,
    embedding_str: &str,
    channel_id: i64,
    limit: i64,
) -> anyhow::Result<Vec<Message>> {
    let rows: Vec<MessageDb> = sqlx::query_as(
        r#"
        WITH vector_candidates AS (
            SELECT m.id, m.created_at,
                   (m.embedding_vec <=> $2::vector(1536)) AS distance_raw
            FROM messages m
            JOIN threads t ON t.id = m.thread_id
            WHERE t.channel_id = $1
              AND m.embedding_vec IS NOT NULL
              AND m.msg_type IN ('cause', 'summary', 'plan', 'message', 'reasoning', 'error', 'tool', 'tool_result')
            ORDER BY m.embedding_vec <=> $2::vector(1536)
            LIMIT 100
        )
        SELECT
            m.id, m.thread_id, m.role, m.content, m.thread_sequence, m.external_id,
            m.metadata::text AS "metadata", m.embedding, m.summary_text, m.is_summary,
            m.msg_type, m.msg_subtype,
            COALESCE(TO_CHAR(m.created_at, 'YYYY-MM-DD"T"HH24' || CHR(58) || 'MI' || CHR(58) || 'SS.US"Z"'), '') AS "created_at"
        FROM messages m
        JOIN vector_candidates vc ON vc.id = m.id
        ORDER BY vc.distance_raw * (1 + EXTRACT(EPOCH FROM (NOW() - vc.created_at)) / 86400)
        LIMIT $3
        "#,
    )
    .bind(channel_id)
    .bind(embedding_str)
    .bind(limit)
    .fetch_all(pool)
    .await?;

    rows.into_iter().map(|r| r.try_into()).collect()
}

// ---------------------------------------------------------------------------
// Delete old messages/summaries
// ---------------------------------------------------------------------------

pub async fn delete_old_messages(
    pool: &PgPool,
    before: chrono::DateTime<chrono::Utc>,
) -> anyhow::Result<u64> {
    let result = sql_forge!(
        "DELETE FROM messages WHERE created_at < :cutoff",
        ( :cutoff = before )
    )
    .execute(pool)
    .await?;

    Ok(result.rows_affected())
}

pub async fn delete_old_summaries(
    pool: &PgPool,
    before: chrono::DateTime<chrono::Utc>,
) -> anyhow::Result<u64> {
    let result = sql_forge!(
        "DELETE FROM summaries WHERE created_at < :cutoff",
        ( :cutoff = before )
    )
    .execute(pool)
    .await?;

    Ok(result.rows_affected())
}

/// Get the latest seq-0 message in a channel (for context preview).
pub async fn get_latest_seq0_message(
    pool: &PgPool,
    channel_id: i64,
) -> anyhow::Result<Option<Message>> {
    #[derive(Debug, sqlx::FromRow)]
    struct IdContent {
        id: i64,
        content: String,
    }
    let row: Option<IdContent> = sqlx::query_as(
        r#"
        SELECT m.id, m.content
        FROM messages m
        JOIN threads t ON t.id = m.thread_id
        WHERE t.channel_id = $1
          AND m.thread_sequence = 0
        ORDER BY m.id DESC
        LIMIT 1
        "#,
    )
    .bind(channel_id)
    .fetch_optional(pool)
    .await?;

    match row {
        Some(r) => Ok(Some(Message {
            id: r.id,
            content: r.content,
            thread_id: 0,
            role: String::new(),
            thread_sequence: 0,
            external_id: None,
            metadata: serde_json::Value::Null,
            embedding: None,
            summary_text: None,
            is_summary: false,
            msg_type: String::new(),
            msg_subtype: None,
            created_at: chrono::Utc::now(),
            processing_time_ms: None,
            token_usage: None,
        })),
        None => Ok(None),
    }
}

/// Get the thread ID for a given message.
pub async fn get_message_thread(
    pool: &PgPool,
    message_id: i64,
) -> anyhow::Result<Option<i64>> {
    let row: Option<(i64,)> = sqlx::query_as(
        "SELECT thread_id FROM messages WHERE id = $1",
    )
    .bind(message_id)
    .fetch_optional(pool)
    .await?;

    Ok(row.map(|r| r.0))
}

pub fn search_wiki_text(wiki_dir: &str, query: &str, limit: usize) -> Vec<(String, String, String)> {
    use std::fs;
    let query_lower = query.to_lowercase();

    // Split query into individual search terms
    let terms: Vec<&str> = query_lower
        .split_whitespace()
        .filter(|t| t.len() > 2) // ignore very short words
        .collect();

    // If no meaningful terms, fall back to whole-query matching
    let use_terms = !terms.is_empty();
    let search_terms: Vec<&str> = if use_terms { terms } else { vec![&query_lower] };

    let mut scored: Vec<(String, String, usize, String)> = Vec::new();
    // results: Vec<(relative_path, title, max_score, Vec<(snippet_line, matching_term)>)>

    let walker = walkdir::WalkDir::new(wiki_dir)
        .follow_links(true)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file() && e.path().extension().map(|ext| ext == "md").unwrap_or(false));

    for entry in walker {
        let path = entry.path();
        let relative = path.strip_prefix(wiki_dir).unwrap_or(path).display().to_string();
        let content = match fs::read_to_string(path) {
            Ok(c) => c,
            Err(_) => continue,
        };

        let title = content.lines()
            .find(|l| l.starts_with("# "))
            .map(|l| l.trim_start_matches("# ").to_string())
            .unwrap_or_else(|| path.file_stem().unwrap_or_default().to_string_lossy().to_string());
        // Count how many unique terms match at least one line
        let content_lower = content.to_lowercase();
        let match_count = search_terms
            .iter()
            .filter(|term| content_lower.contains(*term))
            .count();

        if match_count == 0 {
            continue;
        }

        // Find the best snippet (line with the most matching terms)
        let mut best_snippet = String::new();
        let mut best_snippet_score = 0usize;
        for line in content.lines() {
            let line_lower = line.to_lowercase();
            let line_score = search_terms
                .iter()
                .filter(|term| line_lower.contains(*term))
                .count();
            if line_score > best_snippet_score {
                best_snippet = line.trim().chars().take(200).collect();
                best_snippet_score = line_score;
            }
        }
        // Score = unique term matches + bonus for best snippet score
        let score = match_count * 100 + best_snippet_score;
        scored.push((relative, title, score, best_snippet));
    }

    // Sort by score descending, take top `limit`
    scored.sort_by_key(|b| std::cmp::Reverse(b.2));
    scored.truncate(limit);

    scored.into_iter()
        .map(|(path, title, _score, snippet)| (path, title, snippet))
        .collect()
}

pub async fn search_wiki_qdrant(
    qdrant_url: &str,
    embedding: &[f32],
    limit: usize,
) -> anyhow::Result<Vec<(String, String, f64)>> {
    use serde_json::json;

    let client = reqwest::Client::new();
    let payload = json!({
        "vector": embedding,
        "limit": limit as u64,
        "with_payload": true,
    });

    let resp = client
        .post(format!("{}/collections/wiki/points/search", qdrant_url))
        .json(&payload)
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("Qdrant search request failed: {}", e))?;

    let body: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| anyhow::anyhow!("Qdrant search response parse failed: {}", e))?;

    let mut results = Vec::new();
    if let Some(points) = body["result"].as_array() {
        for point in points {
            let score = point["score"].as_f64().unwrap_or(0.0);
            let payload = &point["payload"];
            let path = payload["path"].as_str().unwrap_or("").to_string();
            let title = payload["title"].as_str().unwrap_or("").to_string();
            results.push((path, title, score));
        }
    }

    Ok(results)
}

// ---------------------------------------------------------------------------
// Summary query functions — unchanged
// ---------------------------------------------------------------------------

/// Get the latest (most recent) summary for a channel.
pub async fn get_latest_summary(
    pool: &PgPool,
    channel_id: i64,
) -> anyhow::Result<Option<SummaryDb>> {
    let row: Option<SummaryDb> = sql_forge!(
        SummaryDb,
        r#"
        SELECT
            id, channel_id, next_thread_id, content,
            COALESCE(TO_CHAR(created_at, 'YYYY-MM-DD"T"HH24' || CHR(58) || 'MI' || CHR(58) || 'SS.US"Z"'), '') AS "created_at"
        FROM summaries
        WHERE channel_id = :channel_id
        ORDER BY id DESC
        LIMIT 1
        "#,
        ( :channel_id = channel_id )
    )
    .fetch_optional(pool)
    .await?;

    Ok(row)
}

/// Get the last N summaries for a channel (newest first).
#[expect(dead_code)]
pub async fn get_recent_summaries(
    pool: &PgPool,
    channel_id: i64,
    limit: i64,
) -> anyhow::Result<Vec<SummaryDb>> {
    let rows: Vec<SummaryDb> = sql_forge!(
        SummaryDb,
        r#"
        SELECT
            id, channel_id, next_thread_id, content,
            COALESCE(TO_CHAR(created_at, 'YYYY-MM-DD"T"HH24' || CHR(58) || 'MI' || CHR(58) || 'SS.US"Z"'), '') AS "created_at"
        FROM summaries
        WHERE channel_id = :channel_id
        ORDER BY id DESC
        LIMIT :limit
        "#,
        ( :channel_id = channel_id, :limit = limit )
    )
    .fetch_all(pool)
    .await?;

    Ok(rows)
}

/// Get completed seq-0 threads (thread roots) with id > since_id,
/// ordered by id ASC, limited to `limit` rows.
/// Now queries the threads table instead of messages.
pub async fn get_completed_seq0_threads_since(
    pool: &PgPool,
    channel_id: i64,
    since_id: i64,
    limit: i64,
) -> anyhow::Result<Vec<ThreadDb>> {
    let rows: Vec<ThreadDb> = sql_forge!(
        ThreadDb,
        r#"
        SELECT
            id, status, cause, channel_id, profile, provider, model, task_id, schedule_task_id,
            input_tokens, cached_tokens, output_tokens, duration_ms,
            COALESCE(TO_CHAR(created_at, 'YYYY-MM-DD"T"HH24' || CHR(58) || 'MI' || CHR(58) || 'SS.US"Z"'), '') AS "created_at",
            COALESCE(TO_CHAR(started_at, 'YYYY-MM-DD"T"HH24' || CHR(58) || 'MI' || CHR(58) || 'SS.US"Z"'), '') AS "started_at",
            COALESCE(TO_CHAR(ended_at, 'YYYY-MM-DD"T"HH24' || CHR(58) || 'MI' || CHR(58) || 'SS.US"Z"'), '') AS "ended_at",
            terminal,
            planning_mode
        FROM threads
        WHERE channel_id = :channel_id
          AND status = 'completed'
          AND id > :since_id
        ORDER BY id ASC
        LIMIT :limit
        "#,
        ( :channel_id = channel_id, :since_id = since_id, :limit = limit )
    )
    .fetch_all(pool)
    .await?;

    Ok(rows)
}

/// Get all messages for a given thread, ordered by thread_sequence ASC.
pub async fn get_thread_messages(
    pool: &PgPool,
    thread_id: i64,
) -> anyhow::Result<Vec<MessageDb>> {
    let rows: Vec<MessageDb> = sql_forge!(
        MessageDb,
        r#"
        SELECT
            id, thread_id, role, content, thread_sequence, external_id,
            metadata::text AS "metadata", embedding, summary_text, is_summary,
            msg_type, msg_subtype,
            token_usage::text AS "token_usage", processing_time_ms,
            COALESCE(TO_CHAR(created_at, 'YYYY-MM-DD"T"HH24' || CHR(58) || 'MI' || CHR(58) || 'SS.US"Z"'), '') AS "created_at"
        FROM messages
        WHERE thread_id = :thread_id
        ORDER BY thread_sequence ASC, created_at ASC
        "#,
        ( :thread_id = thread_id )
    )
    .fetch_all(pool)
    .await?;

    Ok(rows)
}

/// Create a new summary record.
pub async fn create_summary(
    pool: &PgPool,
    channel_id: i64,
    next_thread_id: i64,
    content: &str,
) -> anyhow::Result<SummaryDb> {
    let row: SummaryDb = sql_forge!(
        SummaryDb,
        r#"
        INSERT INTO summaries (channel_id, next_thread_id, content)
        VALUES (:channel_id, :next_thread_id, :content)
        RETURNING
            id, channel_id, next_thread_id, content,
            COALESCE(TO_CHAR(created_at, 'YYYY-MM-DD"T"HH24' || CHR(58) || 'MI' || CHR(58) || 'SS.US"Z"'), '') AS "created_at"
        "#,
        ( :channel_id = channel_id, :next_thread_id = next_thread_id, :content = content )
    )
    .fetch_one(pool)
    .await?;

    Ok(row)
}

// ---------------------------------------------------------------------------
// Subscription CRUD functions
// ---------------------------------------------------------------------------

/// Add a subscription: a channel subscriber (platform+resource) will receive
/// summaries from the given channel.
pub async fn add_subscription(
    pool: &PgPool,
    channel_id: i64,
    subscriber_platform: &str,
    subscriber_resource: &str,
) -> anyhow::Result<i64> {
    #[derive(Debug, sqlx::FromRow)]
    struct SubId {
        id: i64,
    }
    let row: SubId = sql_forge!(
        SubId,
        r#"
        INSERT INTO channel_subscriptions (channel_id, subscriber_platform, subscriber_resource)
        VALUES (:channel_id, :subscriber_platform, :subscriber_resource)
        ON CONFLICT (channel_id, subscriber_platform, subscriber_resource)
        DO UPDATE SET created_at = NOW()
        RETURNING id
        "#,
        ( :channel_id = channel_id, :subscriber_platform = subscriber_platform, :subscriber_resource = subscriber_resource )
    )
    .fetch_one(pool)
    .await?;
    Ok(row.id)
}

/// Remove a subscription. Returns true if a row was actually deleted.
pub async fn remove_subscription(
    pool: &PgPool,
    channel_id: i64,
    subscriber_platform: &str,
    subscriber_resource: &str,
) -> anyhow::Result<bool> {
    let result = sql_forge!(
        "DELETE FROM channel_subscriptions WHERE channel_id = :channel_id AND subscriber_platform = :subscriber_platform AND subscriber_resource = :subscriber_resource",
        ( :channel_id = channel_id, :subscriber_platform = subscriber_platform, :subscriber_resource = subscriber_resource )
    )
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// Get all subscribers for a given channel (the channel whose summaries are
/// being subscribed to).
pub async fn get_subscribers_for_channel(
    pool: &PgPool,
    channel_id: i64,
) -> anyhow::Result<Vec<SubscriptionDb>> {
    let rows: Vec<SubscriptionDb> = sql_forge!(
        SubscriptionDb,
        r#"
        SELECT
            id, channel_id, subscriber_platform, subscriber_resource,
            COALESCE(TO_CHAR(created_at, 'YYYY-MM-DD"T"HH24' || CHR(58) || 'MI' || CHR(58) || 'SS.US"Z"'), '') AS "created_at"
        FROM channel_subscriptions
        WHERE channel_id = :channel_id
        ORDER BY id ASC
        "#,
        ( :channel_id = channel_id )
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Get all subscriptions for a given subscriber (what channels does this
/// subscriber receive summaries from).
pub async fn get_subscriptions_for_subscriber(
    pool: &PgPool,
    subscriber_platform: &str,
    subscriber_resource: &str,
) -> anyhow::Result<Vec<SubscriptionDb>> {
    let rows: Vec<SubscriptionDb> = sql_forge!(
        SubscriptionDb,
        r#"
        SELECT
            id, channel_id, subscriber_platform, subscriber_resource,
            COALESCE(TO_CHAR(created_at, 'YYYY-MM-DD"T"HH24' || CHR(58) || 'MI' || CHR(58) || 'SS.US"Z"'), '') AS "created_at"
        FROM channel_subscriptions
        WHERE subscriber_platform = :subscriber_platform AND subscriber_resource = :subscriber_resource
        ORDER BY id ASC
        "#,
        ( :subscriber_platform = subscriber_platform, :subscriber_resource = subscriber_resource )
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Get all summaries from a channel with id > since_id (for polling new summaries).
pub async fn get_summaries_since(
    pool: &PgPool,
    channel_id: i64,
    since_id: i64,
) -> anyhow::Result<Vec<SummaryDb>> {
    let rows: Vec<SummaryDb> = sql_forge!(
        SummaryDb,
        r#"
        SELECT
            id, channel_id, next_thread_id, content,
            COALESCE(TO_CHAR(created_at, 'YYYY-MM-DD"T"HH24' || CHR(58) || 'MI' || CHR(58) || 'SS.US"Z"'), '') AS "created_at"
        FROM summaries
        WHERE channel_id = :channel_id AND id > :since_id
        ORDER BY id ASC
        "#,
        ( :channel_id = channel_id, :since_id = since_id )
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}
// ---------------------------------------------------------------------------
// Usage stats query — channel-level token usage
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ChannelUsageStats {
    pub channel_id: i64,
    pub channel_name: String,
    pub model: Option<String>,
    pub total_input_tokens: Option<i64>,
    pub total_cached_tokens: Option<i64>,
    pub total_output_tokens: Option<i64>,
    pub total_threads: Option<i64>,
    pub total_duration_ms: Option<i64>,
}

/// Get token usage stats aggregated per channel.
/// Shows model, input_tokens, cached_tokens, output_tokens for each channel.
pub async fn get_channel_usage_stats(pool: &PgPool) -> anyhow::Result<Vec<ChannelUsageStats>> {
    let rows: Vec<ChannelUsageStats> = sql_forge!(
        ChannelUsageStats,
        r#"
        SELECT
            c.id AS channel_id,
            c.name AS channel_name,
            COALESCE(NULLIF(t.model, ''), '(not set)') AS model,
            COALESCE(SUM(t.input_tokens), 0)::bigint AS total_input_tokens,
            COALESCE(SUM(t.cached_tokens), 0)::bigint AS total_cached_tokens,
            COALESCE(SUM(t.output_tokens), 0)::bigint AS total_output_tokens,
            COUNT(t.id)::bigint AS total_threads,
            COALESCE(SUM(t.duration_ms), 0)::bigint AS total_duration_ms
        FROM channels c
        LEFT JOIN threads t ON t.channel_id = c.id
        WHERE t.status IN ('completed', 'failed', 'interrupted', 'skipped')
        GROUP BY c.id, c.name, t.model
        ORDER BY c.name ASC, t.model ASC
        "#,
    )
    .fetch_all(pool)
    .await?;

    Ok(rows)
}

// ---------------------------------------------------------------------------
// Action DB struct and CRUD functions
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ActionDb {
    pub id: String,
    pub name: String,
    pub tool_name: String,
    pub params: Option<String>,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
    pub is_builtin: Option<bool>,
}

impl TryFrom<ActionDb> for Action {
    type Error = anyhow::Error;

    fn try_from(db: ActionDb) -> Result<Self, Self::Error> {
        Ok(Self {
            id: db.id,
            name: db.name,
            tool_name: db.tool_name,
            params: db.params.as_deref().map(|s| serde_json::from_str(s).unwrap_or_default()).unwrap_or_default(),
            created_at: db.created_at.unwrap_or_default(),
            updated_at: db.updated_at.unwrap_or_default(),
            is_builtin: db.is_builtin.unwrap_or(false),
        })
    }
}

/// Create a new action.
pub async fn create_action(
    pool: &PgPool,
    name: &str,
    tool_name: &str,
    params: &serde_json::Value,
) -> anyhow::Result<Action> {
    let row: ActionDb = sql_forge!(
        ActionDb,
        r#"
        INSERT INTO actions (id, name, tool_name, params)
        VALUES (CAST(nextval('actions_id_seq') AS TEXT), :name, :tool_name, NULLIF(:params, '{}')::jsonb)
        RETURNING
            id, name, tool_name,
            params::text AS "params",
            COALESCE(TO_CHAR(created_at, 'YYYY-MM-DD"T"HH24' || CHR(58) || 'MI' || CHR(58) || 'SS.US"Z"'), '') AS "created_at",
            COALESCE(TO_CHAR(updated_at, 'YYYY-MM-DD"T"HH24' || CHR(58) || 'MI' || CHR(58) || 'SS.US"Z"'), '') AS "updated_at",
            is_builtin
        "#,
        ( :name = name, :tool_name = tool_name, :params = &params.to_string() )
    )
    .fetch_one(pool)
    .await?;

    row.try_into()
}

/// List all actions ordered by creation time.
pub async fn list_actions(pool: &PgPool) -> anyhow::Result<Vec<Action>> {
    let rows: Vec<ActionDb> = sql_forge!(
        ActionDb,
        r#"
        SELECT
            id, name, tool_name,
            params::text AS "params",
            COALESCE(TO_CHAR(created_at, 'YYYY-MM-DD"T"HH24' || CHR(58) || 'MI' || CHR(58) || 'SS.US"Z"'), '') AS "created_at",
            COALESCE(TO_CHAR(updated_at, 'YYYY-MM-DD"T"HH24' || CHR(58) || 'MI' || CHR(58) || 'SS.US"Z"'), '') AS "updated_at",
            is_builtin
        FROM actions
        ORDER BY created_at ASC
        "#
    )
    .fetch_all(pool)
    .await?;

    rows.into_iter().map(|r| r.try_into()).collect()
}

/// Get an action by id.
pub async fn get_action(pool: &PgPool, id: &str) -> anyhow::Result<Option<Action>> {
    let row: Option<ActionDb> = sql_forge!(
        ActionDb,
        r#"
        SELECT
            id, name, tool_name,
            params::text AS "params",
            COALESCE(TO_CHAR(created_at, 'YYYY-MM-DD"T"HH24' || CHR(58) || 'MI' || CHR(58) || 'SS.US"Z"'), '') AS "created_at",
            COALESCE(TO_CHAR(updated_at, 'YYYY-MM-DD"T"HH24' || CHR(58) || 'MI' || CHR(58) || 'SS.US"Z"'), '') AS "updated_at",
            is_builtin
        FROM actions
        WHERE id = :id
        "#,
        ( :id = id )
    )
    .fetch_optional(pool)
    .await?;

    row.map(|r| r.try_into()).transpose()
}

/// Update an action by id.
pub async fn update_action(
    pool: &PgPool,
    id: &str,
    name: &str,
    tool_name: &str,
    params: &serde_json::Value,
) -> anyhow::Result<Action> {
    let row: ActionDb = sql_forge!(
        ActionDb,
        r#"
        UPDATE actions
        SET name = :name,
            tool_name = :tool_name,
            params = NULLIF(:params, '{}')::jsonb,
            updated_at = NOW()
        WHERE id = :id
        RETURNING
            id, name, tool_name,
            params::text AS "params",
            COALESCE(TO_CHAR(created_at, 'YYYY-MM-DD"T"HH24' || CHR(58) || 'MI' || CHR(58) || 'SS.US"Z"'), '') AS "created_at",
            COALESCE(TO_CHAR(updated_at, 'YYYY-MM-DD"T"HH24' || CHR(58) || 'MI' || CHR(58) || 'SS.US"Z"'), '') AS "updated_at",
            is_builtin
        "#,
        ( :id = id, :name = name, :tool_name = tool_name, :params = &params.to_string() )
    )
    .fetch_one(pool)
    .await?;

    row.try_into()
}

/// Delete an action by id. Returns the number of rows affected (0 or 1).
pub async fn delete_action(pool: &PgPool, id: &str) -> anyhow::Result<u64> {
    let result = sql_forge!(
        "DELETE FROM actions WHERE id = :id",
        ( :id = id )
    )
    .execute(pool)
    .await?;

    Ok(result.rows_affected())
}
