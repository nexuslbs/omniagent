use sql_forge::sql_forge;
use sqlx::PgPool;

use crate::db::types::SubscriptionDb;

// ---------------------------------------------------------------------------
// Subscription CRUD functions
// ---------------------------------------------------------------------------

/// Get all subscribers for a given channel (the channel whose summaries are
/// being subscribed to).
pub async fn get_subscribers_for_channel(
    pool: &PgPool,
    channel_id: i64,
) -> crate::error::AppResult<Vec<SubscriptionDb>> {
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

// (end of file)
