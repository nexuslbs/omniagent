use sql_forge::sql_forge;
use sqlx::PgPool;

use crate::db::types::{Message, MessageDb, MessageNew};

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
            msg_type, msg_subtype, processing_time_ms, token_usage, iteration_number
        )
        VALUES (:thread_id, :role, :content, :thread_sequence, NULLIF(:external_id, '')::text,
            :metadata, NULLIF(:embedding, '')::text, NULLIF(:summary_text, '')::text, :is_summary,
            :msg_type, NULLIF(:msg_subtype, '')::text, NULLIF(:processing_time_ms, -1)::int, NULLIF(:token_usage, 'null')::jsonb, :iteration_number)
        RETURNING
            id, thread_id, role, content, thread_sequence, external_id,
            metadata::text AS "metadata", embedding, summary_text, is_summary,
            msg_type, msg_subtype,
            token_usage::text AS "token_usage", processing_time_ms, iteration_number,
            COALESCE(TO_CHAR(created_at, 'YYYY-MM-DD"T"HH24' || CHR(58) || 'MI' || CHR(58) || 'SS.US"Z"'), '') AS "created_at"
        "#,
        ( :thread_id = msg.thread_id, :role = &msg.role, :content = &msg.content, :thread_sequence = msg.thread_sequence, :external_id = msg.external_id.as_deref().unwrap_or(""), :metadata = &metadata_val, :embedding = msg.embedding.as_deref().unwrap_or(""), :summary_text = msg.summary_text.as_deref().unwrap_or(""), :is_summary = msg.is_summary, :msg_type = &msg.msg_type, :msg_subtype = msg.msg_subtype.as_deref().unwrap_or(""), :processing_time_ms = msg.processing_time_ms.unwrap_or(-1), :token_usage = &token_usage_val.to_string(), :iteration_number = msg.iteration_number )
    )
    .fetch_one(pool)
    .await?;

    row.try_into()
}

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
            msg_type, msg_subtype, iteration_number,
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
            m.msg_type, m.msg_subtype, m.iteration_number,
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

/// Delete old messages.
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
            iteration_number: 0,
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
            msg_type, msg_subtype, iteration_number,
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
