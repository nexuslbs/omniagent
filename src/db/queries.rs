use anyhow::Result;
use sqlx::PgPool;

use crate::models::{Channel, ChannelStop, Message, MessageNew, MessageStatus};

/// Find the oldest pending messages for a channel, ordered by created_at.
pub async fn find_pending_messages(
    pool: &PgPool,
    channel_id: i64,
) -> Result<Vec<Message>> {
    let rows = sqlx::query_as::<_, Message>(
        r#"
        SELECT
            id,
            channel_id,
            role,
            content,
            status,
            thread_id,
            thread_sequence,
            external_id,
            metadata,
            embedding,
            summary_text,
            is_summary,
            msg_type,
            msg_subtype,
            iteration_count,
            created_at
        FROM messages
        WHERE channel_id = $1 AND status = 'pending'
        ORDER BY created_at ASC
        "#,
    )
    .bind(channel_id)
    .fetch_all(pool)
    .await?;

    Ok(rows)
}

/// Insert a new message into the database.
pub async fn create_message(pool: &PgPool, msg: &MessageNew) -> Result<Message> {
    let row = sqlx::query_as::<_, Message>(
        r#"
        INSERT INTO messages (
            channel_id, role, content, status,
            thread_id, thread_sequence, external_id,
            metadata, embedding, summary_text, is_summary,
            msg_type, msg_subtype, iteration_count
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14)
        RETURNING
            id,
            channel_id,
            role,
            content,
            status,
            thread_id,
            thread_sequence,
            external_id,
            metadata,
            embedding,
            summary_text,
            is_summary,
            msg_type,
            msg_subtype,
            iteration_count,
            created_at
        "#,
    )
    .bind(msg.channel_id)
    .bind(&msg.role)
    .bind(&msg.content)
    .bind(&msg.status)
    .bind(msg.thread_id)
    .bind(msg.thread_sequence)
    .bind(&msg.external_id)
    .bind(&msg.metadata)
    .bind(&msg.embedding)
    .bind(&msg.summary_text)
    .bind(msg.is_summary)
    .bind(&msg.msg_type)
    .bind(&msg.msg_subtype)
    .bind(msg.iteration_count)
    .fetch_one(pool)
    .await?;

    Ok(row)
}

/// Update the status of a message by its id.
pub async fn update_message_status(
    pool: &PgPool,
    id: i64,
    status: &MessageStatus,
) -> Result<()> {
    sqlx::query("UPDATE messages SET status = $1 WHERE id = $2")
        .bind(status)
        .bind(id)
        .execute(pool)
        .await?;

    Ok(())
}

/// Find a channel by its name.
pub async fn get_channel_by_name(pool: &PgPool, name: &str) -> Result<Option<Channel>> {
    let row = sqlx::query_as::<_, Channel>(
        r#"
        SELECT
            id,
            name,
            platform,
            external_id,
            cause,
            metadata,
            created_at,
            updated_at
        FROM channels
        WHERE name = $1
        "#,
    )
    .bind(name)
    .fetch_optional(pool)
    .await?;

    Ok(row)
}

/// Find all channels.
pub async fn find_all_channels(pool: &PgPool) -> Result<Vec<Channel>> {
    let rows = sqlx::query_as::<_, Channel>(
        r#"
        SELECT
            id,
            name,
            platform,
            external_id,
            cause,
            metadata,
            created_at,
            updated_at
        FROM channels
        ORDER BY name ASC
        "#,
    )
    .fetch_all(pool)
    .await?;

    Ok(rows)
}

/// Find messages with status='processing' created before the given timestamp.
pub async fn find_processing_older_than(
    pool: &PgPool,
    before: chrono::DateTime<chrono::Utc>,
) -> Result<Vec<Message>> {
    let rows = sqlx::query_as::<_, Message>(
        r#"
        SELECT
            id,
            channel_id,
            role,
            content,
            status,
            thread_id,
            thread_sequence,
            external_id,
            metadata,
            embedding,
            summary_text,
            is_summary,
            msg_type,
            msg_subtype,
            iteration_count,
            created_at
        FROM messages
        WHERE status = 'processing' AND created_at < $1
        ORDER BY created_at ASC
        "#,
    )
    .bind(before)
    .fetch_all(pool)
    .await?;

    Ok(rows)
}

/// Create a new channel, or return the existing one if a channel with the
/// same (platform, external_id) already exists.
pub async fn create_channel(
    pool: &PgPool,
    name: &str,
    platform: &str,
    external_id: &str,
    cause: &str,
) -> Result<Channel> {
    let row = sqlx::query_as::<_, Channel>(
        r#"
        INSERT INTO channels (name, platform, external_id, cause)
        VALUES ($1, $2, $3, $4)
        ON CONFLICT (platform, external_id)
        DO UPDATE SET
            updated_at = NOW()
        RETURNING
            id,
            name,
            platform,
            external_id,
            cause,
            metadata,
            created_at,
            updated_at
        "#,
    )
    .bind(name)
    .bind(platform)
    .bind(external_id)
    .bind(cause)
    .fetch_one(pool)
    .await?;

    Ok(row)
}

/// Find a stopped channel by its channel_id.
pub async fn find_stopped_channel(
    pool: &PgPool,
    channel_id: i64,
) -> Result<Option<ChannelStop>> {
    let row = sqlx::query_as::<_, ChannelStop>(
        r#"
        SELECT id, channel_id, stopped_at
        FROM channel_stops
        WHERE channel_id = $1
        "#,
    )
    .bind(channel_id)
    .fetch_optional(pool)
    .await?;

    Ok(row)
}

/// Stop a channel — insert or update the channel_stops entry.
///
/// If the channel is already stopped, its `stopped_at` timestamp is refreshed.
pub async fn stop_channel(pool: &PgPool, channel_id: i64) -> Result<()> {
    sqlx::query(
        r#"
        INSERT INTO channel_stops (channel_id)
        VALUES ($1)
        ON CONFLICT (channel_id) DO UPDATE SET stopped_at = NOW()
        "#,
    )
    .bind(channel_id)
    .execute(pool)
    .await?;

    Ok(())
}

/// Clear a channel stop — remove the entry from channel_stops.
///
/// After this, new pending messages for this channel will be processed again.
pub async fn clear_channel_stop(pool: &PgPool, channel_id: i64) -> Result<()> {
    sqlx::query("DELETE FROM channel_stops WHERE channel_id = $1")
        .bind(channel_id)
        .execute(pool)
        .await?;

    Ok(())
}

/// Skip all pending messages for a channel by marking them as `skipped`.
///
/// Returns the number of messages that were skipped.
pub async fn skip_pending_messages(pool: &PgPool, channel_id: i64) -> Result<u64> {
    let result = sqlx::query(
        "UPDATE messages SET status = 'skipped' WHERE channel_id = $1 AND status = 'pending'",
    )
    .bind(channel_id)
    .execute(pool)
    .await?;

    Ok(result.rows_affected())
}

/// Delete messages created before the given timestamp.
///
/// Returns the number of rows deleted.
pub async fn delete_old_messages(
    pool: &PgPool,
    before: chrono::DateTime<chrono::Utc>,
) -> Result<u64> {
    let result = sqlx::query("DELETE FROM messages WHERE created_at < $1")
        .bind(before)
        .execute(pool)
        .await?;

    Ok(result.rows_affected())
}

/// Find all stopped channels, ordered by most recently stopped first.
pub async fn find_all_stopped_channels(pool: &PgPool) -> Result<Vec<ChannelStop>> {
    let rows = sqlx::query_as::<_, ChannelStop>(
        r#"
        SELECT id, channel_id, stopped_at
        FROM channel_stops
        ORDER BY stopped_at DESC
        "#,
    )
    .fetch_all(pool)
    .await?;

    Ok(rows)
}

/// Count how many agent 'message'-type iterations have occurred in a thread.
/// Used to enforce the per-thread iteration limit.
pub async fn count_thread_iterations(
    pool: &PgPool,
    thread_id: i64,
) -> Result<i32> {
    let count: Option<i64> = sqlx::query_scalar(
        r#"
        SELECT COUNT(*) FROM messages
        WHERE thread_id = $1
          AND role = 'agent'
          AND msg_type = 'message'
        "#,
    )
    .bind(thread_id)
    .fetch_one(pool)
    .await?;

    Ok(count.unwrap_or(0) as i32)
}
