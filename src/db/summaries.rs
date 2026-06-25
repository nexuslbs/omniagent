use sql_forge::sql_forge;
use sqlx::PgPool;

use crate::db::types::SummaryDb;

// ---------------------------------------------------------------------------
// Summary query functions
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

/// Delete old summaries.
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

// (end of file)
