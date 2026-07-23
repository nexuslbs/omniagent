use sql_forge::sql_forge;
use sqlx::PgPool;

use crate::db::types::{
    Channel, ChannelDb, ChannelSeq0Message, ChannelStatus, CreateChannelParams, OldChannelInfo,
};
use crate::error::AppResult;

// ---------------------------------------------------------------------------
// Channel query functions
// ---------------------------------------------------------------------------

pub async fn find_all_channels(pool: &PgPool) -> AppResult<Vec<Channel>> {
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
            COALESCE(plan, true) as "plan",
            '{}'::text AS "metadata",
            COALESCE(template, '') AS "template",
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

pub async fn get_channel_by_name(pool: &PgPool, name: &str) -> AppResult<Option<Channel>> {
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
            COALESCE(plan, true) as "plan",
            '{}'::text AS "metadata",
            COALESCE(template, '') AS "template",
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
) -> AppResult<Option<Channel>> {
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
            COALESCE(plan, true) as "plan",
            '{}'::text AS "metadata",
            COALESCE(template, '') AS "template",
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

pub async fn find_channel_by_id(pool: &PgPool, channel_id: i64) -> AppResult<Option<Channel>> {
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
            COALESCE(plan, true) as "plan",
            '{}'::text AS "metadata",
            COALESCE(template, '') AS "template",
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
pub async fn get_channel_by_id(pool: &PgPool, channel_id: i64) -> AppResult<Option<Channel>> {
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
            COALESCE(plan, true) as "plan",
            COALESCE(metadata::text, '{}') AS "metadata",
            COALESCE(template, '') AS "template",
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

/// Get a channel's plan setting directly from the DB column.
/// Returns None if the channel is not found.
pub async fn get_channel_plan(pool: &PgPool, channel_id: i64) -> AppResult<Option<bool>> {
    // Use sql_forge to get the nullable boolean column
    match sql_forge!(
        scalar Option<bool>,
        "SELECT plan FROM channels WHERE id = :channel_id",
        ( :channel_id = channel_id )
    )
    .fetch_optional(pool)
    .await
    {
        Ok(Some(val)) => Ok(val),
        Ok(None) => Ok(None),
        Err(e) => {
            tracing::warn!(
                "get_channel_plan failed for channel {}: {:?}",
                channel_id,
                e
            );
            Ok(None)
        }
    }
}

pub async fn create_channel(pool: &PgPool, p: CreateChannelParams) -> AppResult<Channel> {
    let default_profile = crate::profile::default_profile_name();
    let row: ChannelDb = sql_forge!(
        ChannelDb,
        r#"
        INSERT INTO channels (name, platform, external_id, cause, resource_identifier, current_profile)
        VALUES (:name, NULLIF(:platform, '')::text, :external_id, :cause, NULLIF(:resource_identifier, '')::text, :current_profile)
        ON CONFLICT (name)
        DO UPDATE SET
            resource_identifier = NULLIF(:resource_identifier, '')::text,
            closed = false,
            updated_at = NOW()
        RETURNING
            id, name,
            COALESCE(platform, '') AS "platform",
            resource_identifier,
            COALESCE(external_id, '') AS "external_id",
            cause,
            current_profile, current_model, current_provider,
            readonly,
            COALESCE(closed, false) as "closed",
            COALESCE(plan, true) as "plan",
            COALESCE(metadata::text, '{}') AS "metadata",
            COALESCE(template, '') AS "template",
            COALESCE(TO_CHAR(created_at, 'YYYY-MM-DD"T"HH24' || CHR(58) || 'MI' || CHR(58) || 'SS.US"Z"'), '') AS "created_at",
            COALESCE(TO_CHAR(updated_at, 'YYYY-MM-DD"T"HH24' || CHR(58) || 'MI' || CHR(58) || 'SS.US"Z"'), '') AS "updated_at"
        "#,
        ( :name = p.name.as_str(), :platform = p.platform.as_str(), :external_id = p.external_id.as_str(), :cause = p.cause.as_str(), :resource_identifier = p.resource_identifier.as_str(), :current_profile = default_profile.as_str() )
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
) -> AppResult<Option<Channel>> {
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
            COALESCE(plan, true) as "plan",
            COALESCE(metadata::text, '{}') AS "metadata",
            COALESCE(template, '') AS "template",
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

/// Update a channel's platform + resource_identifier by its stable channel ID.
///
/// This is used when a channel's connection changes (e.g., from telegram:chat1
/// to discord:server1). The channel is found by its stable `channel_id`, not
/// by platform + external_id (which just changed).
///
/// Returns the old platform and resource_identifier values so callers can
/// notify the old platform that the channel is no longer active there.
#[allow(dead_code)]
pub async fn update_channel_platform(
    pool: &PgPool,
    channel_id: i64,
    new_platform: &str,
    new_resource_identifier: &str,
    new_external_id: &str,
) -> AppResult<OldChannelInfo> {
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
            COALESCE(plan, true) as "plan",
            '{}'::text AS "metadata",
            COALESCE(template, '') AS "template",
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
        if p.is_empty() {
            None
        } else {
            Some(p.to_string())
        }
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
) -> AppResult<()> {
    match (provider, model) {
        (Some(p), Some(m)) => {
            sql_forge!(
                r#"
                UPDATE channels
                SET current_provider = NULLIF(:provider, '')::text,
                    current_model = NULLIF(:model, '')::text,
                    updated_at = NOW()
                WHERE id = :id
                "#,
                ( :provider = p, :model = m, :id = channel_id )
            )
            .execute(pool)
            .await?;
        }
        (Some(p), None) => {
            sql_forge!(
                r#"
                UPDATE channels
                SET current_provider = NULLIF(:provider, '')::text,
                    updated_at = NOW()
                WHERE id = :id
                "#,
                ( :provider = p, :id = channel_id )
            )
            .execute(pool)
            .await?;
        }
        (None, Some(m)) => {
            sql_forge!(
                r#"
                UPDATE channels
                SET current_model = NULLIF(:model, '')::text,
                    updated_at = NOW()
                WHERE id = :id
                "#,
                ( :model = m, :id = channel_id )
            )
            .execute(pool)
            .await?;
        }
        (None, None) => {}
    }
    Ok(())
}

/// Claim a channel for a session by updating its resource_identifier.
/// Returns the old resource_identifier (if any) so the caller can notify the
/// previous session.
pub async fn claim_channel_resource(
    pool: &PgPool,
    channel_id: i64,
    session_id: &str,
) -> AppResult<Option<String>> {
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

// ---------------------------------------------------------------------------
// Channel open/close/status queries
// ---------------------------------------------------------------------------

/// Close a channel: sets closed=true and skips pending/processing threads.
pub async fn close_channel(pool: &PgPool, channel_id: i64) -> AppResult<()> {
    sql_forge!(
        "UPDATE channels SET closed = true, updated_at = NOW() WHERE id = :id",
        ( :id = channel_id )
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Open a channel: sets closed=false so the supervisor spawns a handler.
pub async fn open_channel(pool: &PgPool, channel_id: i64) -> AppResult<()> {
    sql_forge!(
        "UPDATE channels SET closed = false, updated_at = NOW() WHERE id = :id",
        ( :id = channel_id )
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Check if a channel is closed.
pub async fn is_channel_closed(pool: &PgPool, channel_id: i64) -> AppResult<bool> {
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
            COALESCE(plan, true) as "plan",
            '{}'::text AS "metadata",
            COALESCE(template, '') AS "template",
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

/// Get channel status with thread counts.
pub async fn get_channel_status(
    pool: &PgPool,
    channel_id: i64,
) -> AppResult<Option<ChannelStatus>> {
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
// Channel seq-0 message query: for recent channel context
// ---------------------------------------------------------------------------

/// Get the most recent seq-0 (thread root) messages for a channel.
/// Filters out cron and kanban system messages: only user-facing conversations.
pub async fn get_recent_channel_seq0_messages(
    pool: &PgPool,
    channel_id: i64,
    limit: i64,
) -> AppResult<Vec<ChannelSeq0Message>> {
    let rows: Vec<ChannelSeq0Message> = sql_forge!(
        ChannelSeq0Message,
        r#"
        SELECT id, content, role, msg_type
        FROM messages
        WHERE thread_id IN (SELECT id FROM threads WHERE channel_id = :channel_id)
          AND thread_sequence = 0
          AND (msg_type IS NULL OR msg_type NOT IN ('cron', 'kanban'))
        ORDER BY created_at DESC
        LIMIT :limit
        "#,
        ( :channel_id = channel_id, :limit = limit )
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}
