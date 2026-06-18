//! DB-focused structs using only primitive types compatible with sql-forge's
//! compile-time validation. Each struct mirrors a domain model but stores
//! complex types (DateTime, JSON, enums) as plain strings. Conversion to
//! domain types is done explicitly in Rust — no SQL type casting.
//!
//! Currently uses `sql_forge!(...)` macros for compile-time SQL validation.
//! DB structs use only primitive types (Strings for DateTime/JSON/enums) with
//! Rust-side conversion. SQL columns that can return NULL use `Option<T>` in
//! the struct with `AS "column?"` in the query.

use chrono::{DateTime, Utc};
use sql_forge::sql_forge;
use sqlx::PgPool;

use crate::models::{Channel, ChannelStop, Message, MessageNew, MessageStatus};

// ---------------------------------------------------------------------------
// Message DB struct (for SELECT results)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct MessageDb {
    pub id: i64,
    pub channel_id: i64,
    pub role: String,
    pub content: String,
    pub status: String,
    pub thread_id: Option<i64>,
    pub thread_sequence: i32,
    pub external_id: Option<String>,
    pub metadata: Option<String>,
    pub embedding: Option<String>,
    pub summary_text: Option<String>,
    pub is_summary: bool,
    pub msg_type: String,
    pub msg_subtype: Option<String>,
    pub iteration_count: i32,
    pub profile: String,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub processing_time_ms: Option<i32>,
    pub token_usage: Option<String>,
    pub iterations: i32,
    pub created_at: String,
}

impl TryFrom<MessageDb> for Message {
    type Error = anyhow::Error;

    fn try_from(db: MessageDb) -> Result<Self, Self::Error> {
        Ok(Self {
            id: db.id,
            channel_id: db.channel_id,
            role: db.role,
            content: db.content,
            status: db
                .status
                .parse::<MessageStatus>()
                .map_err(|_| anyhow::anyhow!("Invalid status: {}", db.status))?,
            thread_id: db.thread_id.unwrap_or(db.id),
            thread_sequence: db.thread_sequence,
            external_id: db.external_id,
            metadata: db.metadata.as_deref().map(|s| serde_json::from_str(s).unwrap_or_default()).unwrap_or_default(),
            embedding: db.embedding,
            summary_text: db.summary_text,
            is_summary: db.is_summary,
            msg_type: db.msg_type,
            msg_subtype: db.msg_subtype,
            iteration_count: db.iteration_count,
            profile: db.profile,
            provider: db.provider,
            model: db.model,
            processing_time_ms: db.processing_time_ms,
            token_usage: db.token_usage.and_then(|v| serde_json::from_str(&v).ok()),
            iterations: db.iterations,
            created_at: db
                .created_at
                .parse::<DateTime<Utc>>()
                .map_err(|e| anyhow::anyhow!("Invalid timestamp '{}': {}", db.created_at, e))?,
        })
    }
}

// ---------------------------------------------------------------------------
// MessageNew DB struct (for INSERT params)
// ---------------------------------------------------------------------------

/// Intermediate result type for create_message INSERT RETURNING.
/// Mirrors the RETURNING columns exactly, used because `sql_forge!`
/// compile-time validation via sqlx::query_as! can't infer nullability
/// for computed expressions without a DB connection.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct CreateMessageRow {
    pub id: i64,
    pub channel_id: i64,
    pub role: String,
    pub content: String,
    pub status: String,
    pub thread_id: Option<i64>,
    pub thread_sequence: i32,
    pub external_id: Option<String>,
    pub metadata: Option<String>,
    pub embedding: Option<String>,
    pub summary_text: Option<String>,
    pub is_summary: bool,
    pub msg_type: String,
    pub msg_subtype: Option<String>,
    pub iteration_count: i32,
    pub profile: String,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub processing_time_ms: Option<i32>,
    pub token_usage: Option<String>,
    pub iterations: i32,
    pub created_at: String,
}

impl From<CreateMessageRow> for MessageDb {
    fn from(r: CreateMessageRow) -> Self {
        Self {
            id: r.id,
            channel_id: r.channel_id,
            role: r.role,
            content: r.content,
            status: r.status,
            thread_id: r.thread_id,
            thread_sequence: r.thread_sequence,
            external_id: r.external_id,
            metadata: r.metadata,
            embedding: r.embedding,
            summary_text: r.summary_text,
            is_summary: r.is_summary,
            msg_type: r.msg_type,
            msg_subtype: r.msg_subtype,
            iteration_count: r.iteration_count,
            profile: r.profile,
            provider: r.provider,
            model: r.model,
            processing_time_ms: r.processing_time_ms,
            token_usage: r.token_usage,
            iterations: r.iterations,
            created_at: r.created_at,
        }
    }
}

pub struct MessageNewDb {
    pub channel_id: i64,
    pub role: String,
    pub content: String,
    pub status: String,
    pub thread_id: Option<i64>,
    pub thread_sequence: i32,
    pub external_id: Option<String>,
    pub metadata: String,
    pub embedding: Option<String>,
    pub summary_text: Option<String>,
    pub is_summary: bool,
    pub msg_type: String,
    pub msg_subtype: Option<String>,
    pub iteration_count: i32,
    pub profile: String,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub processing_time_ms: Option<i32>,
    pub token_usage: Option<String>,
    pub iterations: i32,
}

impl From<&MessageNew> for MessageNewDb {
    fn from(msg: &MessageNew) -> Self {
        Self {
            channel_id: msg.channel_id,
            role: msg.role.clone(),
            content: msg.content.clone(),
            status: msg.status.to_string(),
            thread_id: msg.thread_id,
            thread_sequence: msg.thread_sequence,
            external_id: msg.external_id.clone(),
            metadata: msg.metadata.to_string(),
            embedding: msg.embedding.clone(),
            summary_text: msg.summary_text.clone(),
            is_summary: msg.is_summary,
            msg_type: msg.msg_type.clone(),
            msg_subtype: msg.msg_subtype.clone(),
            iteration_count: msg.iteration_count,
            profile: msg.profile.clone(),
            provider: msg.provider.clone(),
            model: msg.model.clone(),
            processing_time_ms: msg.processing_time_ms,
            token_usage: msg.token_usage.as_ref().map(|v| v.to_string()),
            iterations: msg.iterations,
        }
    }
}

// ---------------------------------------------------------------------------
// Channel DB struct (for SELECT results)
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
    pub metadata: Option<String>,
    pub created_at: String,
    pub updated_at: String,
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
            metadata: db.metadata.as_deref().map(|s| serde_json::from_str(s).unwrap_or_default()).unwrap_or_default(),
            created_at: db
                .created_at
                .parse::<DateTime<Utc>>()
                .map_err(|e| anyhow::anyhow!("Invalid timestamp '{}': {}", db.created_at, e))?,
            updated_at: db
                .updated_at
                .parse::<DateTime<Utc>>()
                .map_err(|e| anyhow::anyhow!("Invalid timestamp '{}': {}", db.updated_at, e))?,
        })
    }
}

// ---------------------------------------------------------------------------
// ChannelStop DB struct (for SELECT results)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ChannelStopDb {
    pub id: i64,
    pub channel_id: i64,
    pub stopped_at: String,
}

impl TryFrom<ChannelStopDb> for ChannelStop {
    type Error = anyhow::Error;

    fn try_from(db: ChannelStopDb) -> Result<Self, Self::Error> {
        Ok(Self {
            id: db.id,
            channel_id: db.channel_id,
            stopped_at: db
                .stopped_at
                .parse::<DateTime<Utc>>()
                .map_err(|e| anyhow::anyhow!("Invalid timestamp '{}': {}", db.stopped_at, e))?,
        })
    }
}

// ---------------------------------------------------------------------------
// Query functions using raw sqlx (runtime-only validation)
// Replace with sql_forge!(...) after upgrading sqlx to 0.9
// ---------------------------------------------------------------------------

pub async fn find_pending_messages(pool: &PgPool, channel_id: i64) -> anyhow::Result<Vec<Message>> {
    let rows: Vec<MessageDb> = sql_forge!(
        MessageDb,
        r#"
        SELECT
            id, channel_id, role, content, status,
            thread_id, thread_sequence, external_id,
            metadata::text AS "metadata?", embedding, summary_text, is_summary,
            msg_type, msg_subtype, iteration_count,
            profile, provider, model, processing_time_ms, token_usage::text AS "token_usage?",
            iterations,
            COALESCE(TO_CHAR(created_at, 'YYYY-MM-DD"T"HH24' || CHR(58) || 'MI' || CHR(58) || 'SS.US"Z"'), '') AS "created_at!"
        FROM messages
        WHERE channel_id = :channel_id AND status = 'pending'
        ORDER BY created_at ASC
        "#,
        ( :channel_id = channel_id )
    )
    .fetch_all(pool)
    .await?;

    rows.into_iter().map(|r| r.try_into()).collect()
}

pub async fn create_message(pool: &PgPool, msg: &MessageNew) -> anyhow::Result<Message> {
    let db = MessageNewDb::from(msg);
    let metadata_val: serde_json::Value = serde_json::from_str(&db.metadata).unwrap_or_default();
    let processing_time_ms_val: i32 = db.processing_time_ms.unwrap_or(0);
    let token_usage_val: serde_json::Value = db
        .token_usage
        .as_ref()
        .and_then(|s| serde_json::from_str(s).ok())
        .unwrap_or(serde_json::Value::Null);
    let row: CreateMessageRow = sql_forge!(
        CreateMessageRow,
        r#"
        INSERT INTO messages (
            channel_id, role, content, status,
            thread_id, thread_sequence, external_id,
            metadata, embedding, summary_text, is_summary,
            msg_type, msg_subtype, iteration_count,
            profile, provider, model, processing_time_ms, token_usage,
            iterations
        )
        VALUES (:channel_id, :role, :content, :status, :thread_id, :thread_sequence, :external_id, :metadata, :embedding, :summary_text, :is_summary, :msg_type, :msg_subtype, :iteration_count, :profile, :provider, :model, :processing_time_ms, :token_usage, :iterations)
        RETURNING
            id, channel_id, role, content, status,
            thread_id, thread_sequence, external_id,
            metadata::text AS "metadata?", embedding, summary_text, is_summary,
            msg_type, msg_subtype, iteration_count,
            profile, provider, model, processing_time_ms, token_usage::text AS "token_usage?",
            iterations,
            COALESCE(TO_CHAR(created_at, 'YYYY-MM-DD"T"HH24' || CHR(58) || 'MI' || CHR(58) || 'SS.US"Z"'), '') AS "created_at!"
        "#,
        ( :channel_id = db.channel_id, :role = &db.role, :content = &db.content, :status = &db.status, :thread_id = db.thread_id.unwrap_or(0), :thread_sequence = db.thread_sequence, :external_id = db.external_id.as_deref().unwrap_or(""), :metadata = &metadata_val, :embedding = db.embedding.as_deref().unwrap_or(""), :summary_text = db.summary_text.as_deref().unwrap_or(""), :is_summary = db.is_summary, :msg_type = &db.msg_type, :msg_subtype = db.msg_subtype.as_deref().unwrap_or(""), :iteration_count = db.iteration_count, :profile = &db.profile, :provider = db.provider.as_deref().unwrap_or(""), :model = db.model.as_deref().unwrap_or(""), :processing_time_ms = processing_time_ms_val, :token_usage = &token_usage_val, :iterations = db.iterations )
    )
    .fetch_one(pool)
    .await?;

    MessageDb::from(row).try_into()
}

/// Insert a seq-0 message with thread_id=NULL, then immediately backfill
/// thread_id = id so subsequent messages can reference this thread.
/// Uses two atomic statements (the window where thread_id is NULL is
/// recovered by the safety pass in skip_all_pending_processing).
pub async fn init_thread_root(pool: &PgPool, msg: &MessageNew) -> anyhow::Result<Message> {
    // Insert with thread_id=None (column is nullable after migration)
    let inserted = create_message(pool, msg).await?;

    // Backfill: SET thread_id = id for the root message
    sql_forge!(
        "UPDATE messages SET thread_id = id WHERE id = :id AND thread_id IS NULL",
        ( :id = inserted.id )
    )
    .execute(pool)
    .await?;

    // Re-read with thread_id now populated
    let row: MessageDb = sql_forge!(
        MessageDb,
        r#"
        SELECT
            id, channel_id, role, content, status,
            thread_id, thread_sequence, external_id,
            metadata::text AS "metadata?", embedding, summary_text, is_summary,
            msg_type, msg_subtype, iteration_count,
            profile, provider, model, processing_time_ms, token_usage::text AS "token_usage?",
            iterations,
            COALESCE(TO_CHAR(created_at, 'YYYY-MM-DD"T"HH24' || CHR(58) || 'MI' || CHR(58) || 'SS.US"Z"'), '') AS "created_at!"
        FROM messages
        WHERE id = :id
        "#,
        ( :id = inserted.id )
    )
    .fetch_one(pool)
    .await?;

    row.try_into()
}

pub async fn update_message_status(
    pool: &PgPool,
    id: i64,
    status: &MessageStatus,
) -> anyhow::Result<()> {
    let status_str = status.to_string();
    sql_forge!(
        "UPDATE messages SET status = :status WHERE id = :id",
        ( :status = &status_str, :id = id )
    )
    .execute(pool)
    .await?;

    Ok(())
}

pub async fn find_all_channels(pool: &PgPool) -> anyhow::Result<Vec<Channel>> {
    let rows: Vec<ChannelDb> = sql_forge!(
        ChannelDb,
        r#"
        SELECT
            id, name, platform, external_id, cause,
            current_profile, current_model, current_provider,
            metadata::text AS "metadata?", COALESCE(TO_CHAR(created_at, 'YYYY-MM-DD"T"HH24' || CHR(58) || 'MI' || CHR(58) || 'SS.US"Z"'), '') AS "created_at!", COALESCE(TO_CHAR(updated_at, 'YYYY-MM-DD"T"HH24' || CHR(58) || 'MI' || CHR(58) || 'SS.US"Z"'), '') AS "updated_at!"
        FROM channels
        ORDER BY name ASC
        "#
    )
    .fetch_all(pool)
    .await?;

    rows.into_iter().map(|r| r.try_into()).collect()
}

pub async fn count_thread_iterations(pool: &PgPool, thread_id: i64) -> anyhow::Result<i32> {
    let count: Option<i64> = sql_forge!(
        scalar Option<i64>,
        r#"
        SELECT COALESCE(MAX(iterations), 0) FROM messages
        WHERE thread_id = :thread_id
        "#,
        ( :thread_id = thread_id )
    )
    .fetch_one(pool)
    .await?;

    Ok(count.unwrap_or(0) as i32)
}

pub async fn skip_pending_messages(pool: &PgPool, channel_id: i64) -> anyhow::Result<u64> {
    let result = sql_forge!(
        "UPDATE messages SET status = 'skipped' WHERE channel_id = :channel_id AND status = 'pending'",
        ( :channel_id = channel_id )
    )
    .execute(pool)
    .await?;

    Ok(result.rows_affected())
}

/// Mark ALL pending and processing messages as skipped (run on startup).
/// Also aggregates thread-level stats (processing time, token usage, message count)
/// and writes them back to the sequence-0 message before skipping.
pub async fn skip_all_pending_processing(pool: &PgPool) -> anyhow::Result<u64> {
    // Safety pass: normalize any orphaned rows where thread_id IS NULL
    // (brief window between INSERT and init_thread_root UPDATE)
    sql_forge!(r#"UPDATE messages SET thread_id = id WHERE thread_id IS NULL"#)
        .execute(pool)
        .await?;

    // First pass: update sequence-0 messages with aggregated thread stats, then mark skipped
    let result = sql_forge!(
        r#"
        WITH affected_threads AS (
            SELECT DISTINCT channel_id, thread_id
            FROM messages
            WHERE status IN ('pending', 'processing') AND thread_sequence = 0
        ),
        aggregates AS (
            SELECT
                m.channel_id,
                m.thread_id,
                SUM(m.processing_time_ms) AS total_time,
                COUNT(*) AS msg_count,
                jsonb_build_object(
                    'prompt_tokens',
                    SUM(COALESCE((token_usage->>'prompt_tokens')::int, 0)),
                    'completion_tokens',
                    SUM(COALESCE((token_usage->>'completion_tokens')::int, 0))
                ) AS total_tokens
            FROM messages m
            INNER JOIN affected_threads t
                ON m.channel_id = t.channel_id AND m.thread_id = t.thread_id
            GROUP BY m.channel_id, m.thread_id
        )
        UPDATE messages m
        SET
            status = 'skipped',
            processing_time_ms = a.total_time,
            iteration_count = a.msg_count,
            token_usage = a.total_tokens
        FROM aggregates a
        WHERE m.channel_id = a.channel_id
          AND m.thread_id = a.thread_id
          AND m.thread_sequence = 0
          AND m.status IN ('pending', 'processing')
        "#
    )
    .execute(pool)
    .await?;

    let seq0_count = result.rows_affected();

    // Second pass: skip remaining pending/processing messages (non-sequence-0)
    let remaining = sql_forge!(
        r#"
        UPDATE messages
        SET status = 'skipped'
        WHERE status IN ('pending', 'processing')
        "#
    )
    .execute(pool)
    .await?;

    Ok(seq0_count + remaining.rows_affected())
}

pub async fn stop_channel(pool: &PgPool, channel_id: i64) -> anyhow::Result<()> {
    sql_forge!(
        r#"
        INSERT INTO channel_stops (channel_id)
        VALUES (:channel_id)
        ON CONFLICT (channel_id) DO UPDATE SET stopped_at = NOW()
        "#,
        ( :channel_id = channel_id )
    )
    .execute(pool)
    .await?;

    Ok(())
}

pub async fn find_stopped_channel(
    pool: &PgPool,
    channel_id: i64,
) -> anyhow::Result<Option<ChannelStop>> {
    let row: Option<ChannelStopDb> = sql_forge!(
        ChannelStopDb,
        r#"
        SELECT id, channel_id, COALESCE(TO_CHAR(stopped_at, 'YYYY-MM-DD"T"HH24' || CHR(58) || 'MI' || CHR(58) || 'SS.US"Z"'), '') AS "stopped_at!"
        FROM channel_stops
        WHERE channel_id = :channel_id
        "#,
        ( :channel_id = channel_id )
    )
    .fetch_optional(pool)
    .await?;

    row.map(|r| r.try_into()).transpose()
}

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

// ---------------------------------------------------------------------------
// Context retrieval helper functions
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
            id, channel_id, role, content, status,
            thread_id, thread_sequence, external_id,
            metadata::text AS "metadata?", embedding, summary_text, is_summary,
            msg_type, msg_subtype, iteration_count,
            profile, provider, model, processing_time_ms, token_usage::text AS "token_usage?",
            iterations,
            COALESCE(TO_CHAR(created_at, 'YYYY-MM-DD"T"HH24' || CHR(58) || 'MI' || CHR(58) || 'SS.US"Z"'), '') AS "created_at!"
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
            id, channel_id, role, content, status,
            thread_id, thread_sequence, external_id,
            metadata::text AS "metadata?", embedding, summary_text, is_summary,
            msg_type, msg_subtype, iteration_count,
            profile, provider, model, processing_time_ms, token_usage::text AS "token_usage?",
            iterations,
            COALESCE(TO_CHAR(created_at, 'YYYY-MM-DD"T"HH24' || CHR(58) || 'MI' || CHR(58) || 'SS.US"Z"'), '') AS "created_at!"
        FROM messages
        WHERE channel_id = :channel_id
          AND content ILIKE :pattern
          AND role IN ('user', 'agent')
        ORDER BY created_at DESC
        LIMIT :limit
        "#,
        ( :channel_id = channel_id, :pattern = &pattern, :limit = limit )
    )
    .fetch_all(pool)
    .await?;

    rows.into_iter().map(|r| r.try_into()).collect()
}

/// Find messages where embedding IS NULL, ordered by created_at, with a limit.
/// Only returns messages with role='user' or role='agent'.
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

pub async fn get_channel_by_name(pool: &PgPool, name: &str) -> anyhow::Result<Option<Channel>> {
    let row: Option<ChannelDb> = sql_forge!(
        ChannelDb,
        r#"
        SELECT
            id, name, platform, external_id, cause,
            current_profile, current_model, current_provider,
            metadata::text AS "metadata?", COALESCE(TO_CHAR(created_at, 'YYYY-MM-DD"T"HH24' || CHR(58) || 'MI' || CHR(58) || 'SS.US"Z"'), '') AS "created_at!", COALESCE(TO_CHAR(updated_at, 'YYYY-MM-DD"T"HH24' || CHR(58) || 'MI' || CHR(58) || 'SS.US"Z"'), '') AS "updated_at!"
        FROM channels
        WHERE name = :name
        "#,
        ( :name = name )
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
            metadata::text AS "metadata?", COALESCE(TO_CHAR(created_at, 'YYYY-MM-DD"T"HH24' || CHR(58) || 'MI' || CHR(58) || 'SS.US"Z"'), '') AS "created_at!", COALESCE(TO_CHAR(updated_at, 'YYYY-MM-DD"T"HH24' || CHR(58) || 'MI' || CHR(58) || 'SS.US"Z"'), '') AS "updated_at!"
        "#,
        ( :name = name, :platform = platform, :external_id = external_id, :cause = cause )
    )
    .fetch_one(pool)
    .await?;

    row.try_into()
}

// ---------------------------------------------------------------------------
// Hybrid retrieval: pgvector semantic search for messages
// ---------------------------------------------------------------------------

/// Search messages by embedding similarity (pgvector cosine distance).
/// Uses raw sqlx for vector operator support (pgvector <=>).
/// Returns messages sorted by similarity (closest first).
pub async fn search_messages_semantic(
    pool: &PgPool,
    embedding_str: &str,
    channel_id: i64,
    limit: i64,
) -> anyhow::Result<Vec<Message>> {
    // Use raw sqlx query since pgvector's <=> operator isn't supported by sql_forge!
    let rows: Vec<MessageDb> = sqlx::query_as(
        r#"
        SELECT
            id, channel_id, role, content, status,
            thread_id, thread_sequence, external_id,
            metadata::text AS "metadata?", embedding, summary_text, is_summary,
            msg_type, msg_subtype, iteration_count,
            profile, provider, model, processing_time_ms, token_usage::text AS "token_usage?",
            iterations,
            COALESCE(TO_CHAR(created_at, 'YYYY-MM-DD"T"HH24' || CHR(58) || 'MI' || CHR(58) || 'SS.US"Z"'), '') AS "created_at!"
        FROM messages
        WHERE channel_id = $1
          AND embedding IS NOT NULL
          AND role IN ('user', 'agent')
        ORDER BY embedding::vector(1536) <=> $2::vector(1536)
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
// Wiki text search (walkdir-based)
// ---------------------------------------------------------------------------

/// Search wiki markdown files by text content using walkdir.
/// Searches for the query string in file contents (case-insensitive).
pub fn search_wiki_text(wiki_dir: &str, query: &str, limit: usize) -> Vec<(String, String, String)> {
    let query_lower = query.to_lowercase();
    let wiki_path = std::path::Path::new(wiki_dir);
    if !wiki_path.exists() {
        return vec![];
    }

    let mut results = Vec::new();
    let mut count = 0usize;

    for entry in walkdir::WalkDir::new(wiki_path)
        .follow_links(true)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()).map(|e| e.eq_ignore_ascii_case("md")).unwrap_or(false) {
            match std::fs::read_to_string(path) {
                Ok(content) => {
                    if content.to_lowercase().contains(&query_lower) {
                        let file_path = path.to_string_lossy().to_string();
                        let title = path.file_stem()
                            .and_then(|s| s.to_str())
                            .unwrap_or("untitled")
                            .to_string();
                        // Find the matching section (first ~200 chars around match)
                        let snippet = if let Some(idx) = content.to_lowercase().find(&query_lower) {
                            let start = idx.saturating_sub(100);
                            let end = (idx + query.len() + 200).min(content.len());
                            let snippet: String = content[start..end].chars().collect();
                            format!("...{}...", snippet.trim())
                        } else {
                            content.chars().take(300).collect()
                        };
                        results.push((file_path, title, snippet));
                        count += 1;
                        if count >= limit {
                            break;
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!("Failed to read wiki file {:?}: {}", path, e);
                }
            }
        }
    }
    results
}

// ---------------------------------------------------------------------------
// Qdrant semantic search for wiki
// ---------------------------------------------------------------------------

/// Search wiki documents in Qdrant by vector similarity.
pub async fn search_wiki_qdrant(
    qdrant_url: &str,
    embedding: &[f32],
    limit: usize,
) -> anyhow::Result<Vec<(String, String, f32)>> {
    let url = format!("{}/collections/wiki/points/search", qdrant_url);
    let client = reqwest::Client::new();

    let body = serde_json::json!({
        "vector": embedding,
        "limit": limit,
        "with_payload": true,
    });

    let resp = client
        .post(&url)
        .json(&body)
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("Qdrant search request failed: {}", e))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        anyhow::bail!("Qdrant search failed ({}): {}", status, text);
    }

    let data: serde_json::Value = resp.json().await?;
    let mut results = Vec::new();

    if let Some(result_array) = data.get("result").and_then(|r| r.as_array()) {
        for point in result_array {
            let score = point.get("score").and_then(|s| s.as_f64()).unwrap_or(0.0) as f32;
            let payload = point.get("payload").and_then(|p| p.as_object());
            let file_path = payload
                .and_then(|p| p.get("path"))
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string();
            let title = payload
                .and_then(|p| p.get("title"))
                .and_then(|v| v.as_str())
                .unwrap_or("untitled")
                .to_string();
            results.push((file_path, title, score));
        }
    }

    Ok(results)
}

#[expect(dead_code)]
pub async fn clear_channel_stop(pool: &PgPool, channel_id: i64) -> anyhow::Result<()> {
    sql_forge!("DELETE FROM channel_stops WHERE channel_id = :channel_id", ( :channel_id = channel_id ))
        .execute(pool)
        .await?;

    Ok(())
}

#[expect(dead_code)]
pub async fn find_all_stopped_channels(pool: &PgPool) -> anyhow::Result<Vec<ChannelStop>> {
    let rows: Vec<ChannelStopDb> = sql_forge!(
        ChannelStopDb,
        r#"
        SELECT id, channel_id, COALESCE(TO_CHAR(stopped_at, 'YYYY-MM-DD"T"HH24' || CHR(58) || 'MI' || CHR(58) || 'SS.US"Z"'), '') AS "stopped_at!"
        FROM channel_stops
        ORDER BY stopped_at DESC
        "#
    )
    .fetch_all(pool)
    .await?;

    rows.into_iter().map(|r| r.try_into()).collect()
}
