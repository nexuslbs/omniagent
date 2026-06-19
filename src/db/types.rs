//! DB-focused structs using only primitive types compatible with sql-forge's
//! compile-time validation. Each struct mirrors a domain model but stores
//! complex types (DateTime, JSON) as plain strings. Conversion to
//! domain types is done explicitly in Rust — no SQL type casting.

use chrono::{DateTime, Utc};
use sql_forge::sql_forge;
use sqlx::PgPool;

use crate::models::{Channel, ChannelStop, Message, MessageNew, Thread};

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
    pub platform: String,
    pub external_id: String,
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
) -> anyhow::Result<Thread> {
    let row: ThreadDb = sql_forge!(
        ThreadDb,
        r#"
        INSERT INTO threads (status, cause, channel_id, profile, provider, model)
        VALUES ('created', :cause, :channel_id, :profile, NULLIF(:provider, '')::text, NULLIF(:model, '')::text)
        RETURNING
            id, status, cause, channel_id, profile, provider, model,
            input_tokens, cached_tokens, output_tokens, duration_ms,
            COALESCE(TO_CHAR(created_at, 'YYYY-MM-DD"T"HH24' || CHR(58) || 'MI' || CHR(58) || 'SS.US"Z"'), '') AS "created_at",
            ''::text AS "started_at",
            ''::text AS "ended_at",
            terminal
        "#,
        ( :cause = cause, :channel_id = channel_id, :profile = profile, :provider = provider.unwrap_or(""), :model = model.unwrap_or("") )
    )
    .fetch_one(pool)
    .await?;

    row.try_into()
}

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

    sql_forge!(
        "UPDATE threads SET status = 'pending' WHERE id = :id AND NOT terminal",
        ( :id = msg.thread_id )
    )
    .execute(&mut *tx)
    .await?;

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
            id, status, cause, channel_id, profile, provider, model,
            input_tokens, cached_tokens, output_tokens, duration_ms,
            COALESCE(TO_CHAR(created_at, 'YYYY-MM-DD"T"HH24' || CHR(58) || 'MI' || CHR(58) || 'SS.US"Z"'), '') AS "created_at",
            COALESCE(TO_CHAR(started_at, 'YYYY-MM-DD"T"HH24' || CHR(58) || 'MI' || CHR(58) || 'SS.US"Z"'), '') AS "started_at",
            COALESCE(TO_CHAR(ended_at, 'YYYY-MM-DD"T"HH24' || CHR(58) || 'MI' || CHR(58) || 'SS.US"Z"'), '') AS "ended_at",
            terminal
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
    let result = sql_forge!(
        "UPDATE threads SET status = 'skipped', ended_at = NOW(), terminal = true WHERE channel_id = :channel_id AND status IN ('pending', 'processing') AND NOT terminal",
        ( :channel_id = channel_id )
    )
    .execute(pool)
    .await?;

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
// Channel query functions — mostly unchanged
// ---------------------------------------------------------------------------

pub async fn find_all_channels(pool: &PgPool) -> anyhow::Result<Vec<Channel>> {
    let rows: Vec<ChannelDb> = sql_forge!(
        ChannelDb,
        r#"
        SELECT
            id, name, platform, external_id, cause,
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
            id, name, platform, external_id, cause,
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
            id, name, platform, external_id, cause,
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
            id, name, platform, external_id, cause,
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
            id, name, platform, external_id, cause,
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
) -> anyhow::Result<Channel> {
    let row: ChannelDb = sql_forge!(
        ChannelDb,
        r#"
        INSERT INTO channels (name, platform, external_id, cause)
        VALUES (:name, :platform, :external_id, :cause)
        ON CONFLICT (platform, external_id)
        DO UPDATE SET updated_at = NOW()
        RETURNING
            id, name, platform, external_id, cause,
            current_profile, current_model, current_provider,
            readonly,
            COALESCE(closed, false) as "closed",
            COALESCE(metadata::text, '{}') AS "metadata",
            COALESCE(TO_CHAR(created_at, 'YYYY-MM-DD"T"HH24' || CHR(58) || 'MI' || CHR(58) || 'SS.US"Z"'), '') AS "created_at",
            COALESCE(TO_CHAR(updated_at, 'YYYY-MM-DD"T"HH24' || CHR(58) || 'MI' || CHR(58) || 'SS.US"Z"'), '') AS "updated_at"
        "#,
        ( :name = name, :platform = platform, :external_id = external_id, :cause = cause )
    )
    .fetch_one(pool)
    .await?;

    row.try_into()
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
            id, name, platform, external_id, cause,
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
        platform: ch.platform,
        closed: ch.closed,
        current_profile: ch.current_profile,
        current_model: ch.current_model,
        current_provider: ch.current_provider,
        pending_threads: pending.unwrap_or(0),
        processing_threads: processing.unwrap_or(0),
    }))
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
          AND m.role IN ('user', 'agent')
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
          AND role IN ('user', 'agent')
        ORDER BY created_at ASC
        LIMIT :limit
        "#,
        ( :limit = limit as i64 )
    )
    .fetch_all(pool)
    .await?;

    Ok(rows)
}

/// Update the embedding column for a given message.
pub async fn update_message_embedding(
    pool: &PgPool,
    message_id: i64,
    embedding_string: &str,
) -> anyhow::Result<()> {
    sql_forge!(
        "UPDATE messages SET embedding = :embedding WHERE id = :id",
        ( :embedding = embedding_string, :id = message_id )
    )
    .execute(pool)
    .await?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Hybrid retrieval: pgvector semantic search for messages — unchanged
// ---------------------------------------------------------------------------

/// Search messages by embedding similarity (pgvector cosine distance).
pub async fn search_messages_semantic(
    pool: &PgPool,
    embedding_str: &str,
    channel_id: i64,
    limit: i64,
) -> anyhow::Result<Vec<Message>> {
    let rows: Vec<MessageDb> = sqlx::query_as(
        r#"
        SELECT
            m.id, m.thread_id, m.role, m.content, m.thread_sequence, m.external_id,
            m.metadata::text AS "metadata", m.embedding, m.summary_text, m.is_summary,
            m.msg_type, m.msg_subtype,
            COALESCE(TO_CHAR(m.created_at, 'YYYY-MM-DD"T"HH24' || CHR(58) || 'MI' || CHR(58) || 'SS.US"Z"'), '') AS "created_at"
        FROM messages m
        JOIN threads t ON t.id = m.thread_id
        WHERE t.channel_id = $1
          AND m.embedding IS NOT NULL
          AND m.role IN ('user', 'agent')
        ORDER BY m.embedding::vector(1536) <=> $2::vector(1536)
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

pub fn search_wiki_text(wiki_dir: &str, query: &str, limit: usize) -> Vec<(String, String, String)> {
    use std::fs;
    let query_lower = query.to_lowercase();
    let mut results = Vec::new();

    let walker = walkdir::WalkDir::new(wiki_dir)
        .follow_links(true)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file() && e.path().extension().map(|ext| ext == "md").unwrap_or(false));

    for entry in walker.take(limit * 10) {
        if results.len() >= limit {
            break;
        }
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

        for line in content.lines() {
            if line.to_lowercase().contains(&query_lower) {
                let snippet = line.trim().chars().take(200).collect::<String>();
                results.push((relative.clone(), title.clone(), snippet));
                if results.len() >= limit {
                    break;
                }
            }
        }
    }

    results
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
            id, status, cause, channel_id, profile, provider, model,
            input_tokens, cached_tokens, output_tokens, duration_ms,
            COALESCE(TO_CHAR(created_at, 'YYYY-MM-DD"T"HH24' || CHR(58) || 'MI' || CHR(58) || 'SS.US"Z"'), '') AS "created_at",
            COALESCE(TO_CHAR(started_at, 'YYYY-MM-DD"T"HH24' || CHR(58) || 'MI' || CHR(58) || 'SS.US"Z"'), '') AS "started_at",
            COALESCE(TO_CHAR(ended_at, 'YYYY-MM-DD"T"HH24' || CHR(58) || 'MI' || CHR(58) || 'SS.US"Z"'), '') AS "ended_at",
            terminal
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


