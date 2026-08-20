use sqlx::PgPool;

use crate::channels_yaml::{self, ChannelDef, ChannelsFile};
use crate::db::types::{Channel, ChannelSeq0Message, ChannelStatus, CreateChannelParams};
use crate::error::{AppResult, Error};

// ---------------------------------------------------------------------------
// Channel store: channels.yml (the `channels` table is DROPPED — see
// db-migrations). All definitions AND runtime state live in the yml; the
// functions below keep their historical signatures (pool first — unused for
// pure yml reads, still needed for thread-count / message queries) so call
// sites change only in id type: numeric channel ids are now channel NAMES.
// ---------------------------------------------------------------------------

fn def_to_channel(name: &str, def: &ChannelDef) -> Channel {
    // Resolve identity fields (profile/provider/model) AT LOAD TIME with
    // fallback — the loader returns resolved data, never shallow yml fields.
    // A channels.yml edit (e.g. switching a channel's provider) therefore
    // takes effect on the very next load, with no restart and no cache.
    let resolved = crate::resolution::resolve_channel_identity(
        crate::channels_yaml::data_dir().unwrap_or_default(),
        def,
    );
    let rid = def.resource_identifier.clone().unwrap_or_default();
    Channel {
        id: name.to_string(),
        name: name.to_string(),
        platform: def.platform.clone(),
        resource_identifier: def.resource_identifier.clone(),
        // external_id is NOT a stored yml field — it was always equal to
        // resource_identifier at every creation site; derive for compat.
        external_id: (!rid.is_empty()).then_some(rid),
        current_profile: resolved.profile,
        current_model: resolved.model,
        current_provider: resolved.provider,
        readonly: def.readonly.unwrap_or(false),
        closed: def.closed.unwrap_or(false),
        plan: def.plan.unwrap_or(true),
        metadata: serde_json::json!({}),
        template: def.template.clone().filter(|t| !t.is_empty()),
        created_at: chrono::DateTime::UNIX_EPOCH,
        updated_at: chrono::DateTime::UNIX_EPOCH,
    }
}

pub async fn find_all_channels(_pool: &PgPool) -> AppResult<Vec<Channel>> {
    Ok(channels_yaml::find_all()?
        .into_iter()
        .map(|(name, def)| def_to_channel(&name, &def))
        .collect())
}

pub async fn get_channel_by_name(_pool: &PgPool, name: &str) -> AppResult<Option<Channel>> {
    Ok(channels_yaml::get_by_name(name)?.map(|def| def_to_channel(name, &def)))
}

pub async fn get_channel_by_platform_name(
    _pool: &PgPool,
    platform: &str,
    name: &str,
) -> AppResult<Option<Channel>> {
    let def = channels_yaml::get_by_name(name)?;
    let def = def.filter(|d| {
        d.platform
            .as_deref()
            .map(|p| p == platform)
            .unwrap_or(false)
    });
    Ok(def.map(|d| def_to_channel(name, &d)))
}

/// Look up a channel by its name (yml key). The old numeric id is gone:
/// id == name. Kept as the historical entry point — callers pass the yml key.
pub async fn find_channel_by_id(_pool: &PgPool, name: &str) -> AppResult<Option<Channel>> {
    Ok(channels_yaml::get_by_name(name)?.map(|def| def_to_channel(name, &def)))
}

/// Get a channel by id (== name) including its runtime state.
pub async fn get_channel_by_id(_pool: &PgPool, name: &str) -> AppResult<Option<Channel>> {
    Ok(channels_yaml::get_by_name(name)?.map(|def| def_to_channel(name, &def)))
}

/// Get a channel's plan setting directly from the yml `plan` field.
/// Returns None if the channel is not found or no plan is set.
pub async fn get_channel_plan(_pool: &PgPool, name: &str) -> AppResult<Option<bool>> {
    Ok(channels_yaml::get_by_name(name)?.and_then(|d| d.plan))
}

/// Upsert a channel BY NAME (yml key). When a channel with that name exists
/// it is UPDATED to the incoming platform + resource_identifier — a channel
/// is not pinned to the platform that first created it (same semantics as the
/// old `ON CONFLICT (name) DO UPDATE`). Auto-created channels APPEND to
/// channels.yml and become visible immediately (the yml IS the runtime store).
pub async fn create_channel(_pool: &PgPool, p: CreateChannelParams) -> AppResult<Channel> {
    let default_profile = crate::profile::default_profile_name();
    let name = p.name.clone();
    let def = channels_yaml::update_channel(&name, |existing| {
        let mut d = existing.cloned().unwrap_or_default();
        // Definition fields: rewritten on every upsert (name conflict wins).
        d.platform = (!p.platform.is_empty()).then(|| p.platform.clone());
        d.resource_identifier =
            (!p.resource_identifier.is_empty()).then(|| p.resource_identifier.clone());
        // Runtime fields: keep existing values on conflict (like the old
        // ON CONFLICT DO UPDATE which only rewrote resource_identifier and
        // reopened the channel); set the default profile on first creation.
        if d.profile.is_none() {
            d.profile = Some(default_profile.clone());
        }
        d.closed = Some(false);
        if let Err(e) = channels_yaml::validate_channel(&name, &d) {
            return Err(Error::Message(e));
        }
        Ok(d)
    })?;
    Ok(def_to_channel(&name, &def))
}

/// Look up a channel by (platform, resource_identifier).
pub async fn get_channel_by_platform_and_resource(
    _pool: &PgPool,
    platform: &str,
    resource_identifier: &str,
) -> AppResult<Option<Channel>> {
    Ok(
        channels_yaml::get_by_platform_and_resource(platform, resource_identifier)?
            .map(|(name, def)| def_to_channel(&name, &def)),
    )
}

/// Update a channel's provider and/or model by its name (yml key).
///
/// Only non-None fields are updated (partial update). Pass `None` to leave
/// the current value unchanged, or `Some("")` to clear it.
pub async fn update_channel_model(
    _pool: &PgPool,
    name: &str,
    provider: Option<&str>,
    model: Option<&str>,
) -> AppResult<()> {
    channels_yaml::update_channel(name, |existing| {
        let mut d = existing
            .cloned()
            .ok_or_else(|| Error::Message(format!("Channel '{}' not found", name)))?;
        if let Some(p) = provider {
            d.provider = (!p.is_empty()).then(|| p.to_string());
        }
        if let Some(m) = model {
            d.model = (!m.is_empty()).then(|| m.to_string());
        }
        Ok(d)
    })?;
    Ok(())
}

/// Claim a channel for a session by rewriting its resource_identifier in the
/// yml. Returns the old resource_identifier (if any) so the caller can notify
/// the previous session.
pub async fn claim_channel_resource(
    _pool: &PgPool,
    name: &str,
    session_id: &str,
) -> AppResult<Option<String>> {
    let old_rid = channels_yaml::get_by_name(name)?.and_then(|d| d.resource_identifier);
    channels_yaml::update_channel(name, |existing| {
        let mut d = existing
            .cloned()
            .ok_or_else(|| Error::Message(format!("Channel '{}' not found", name)))?;
        d.resource_identifier = Some(session_id.to_string());
        Ok(d)
    })?;
    Ok(old_rid)
}

// ---------------------------------------------------------------------------
// Channel open/close/status queries
// ---------------------------------------------------------------------------

/// Close a channel: sets closed=true and skips pending/processing threads.
pub async fn close_channel(_pool: &PgPool, name: &str) -> AppResult<()> {
    channels_yaml::update_channel(name, |existing| {
        let mut d = existing
            .cloned()
            .ok_or_else(|| Error::Message(format!("Channel '{}' not found", name)))?;
        d.closed = Some(true);
        Ok(d)
    })?;
    Ok(())
}

/// Open a channel: sets closed=false so the supervisor spawns a handler.
pub async fn open_channel(_pool: &PgPool, name: &str) -> AppResult<()> {
    channels_yaml::update_channel(name, |existing| {
        let mut d = existing
            .cloned()
            .ok_or_else(|| Error::Message(format!("Channel '{}' not found", name)))?;
        d.closed = Some(false);
        Ok(d)
    })?;
    Ok(())
}

/// Check if a channel is closed (from the yml).
pub async fn is_channel_closed(_pool: &PgPool, name: &str) -> AppResult<bool> {
    Ok(channels_yaml::get_by_name(name)?
        .and_then(|d| d.closed)
        .unwrap_or(false))
}

/// Get channel status with thread counts (thread counts still come from DB).
pub async fn get_channel_status(pool: &PgPool, name: &str) -> AppResult<Option<ChannelStatus>> {
    let ch = match get_channel_by_name(pool, name).await? {
        Some(c) => c,
        None => return Ok(None),
    };

    let pending: Option<i64> = sqlx::query_scalar(
        "SELECT COUNT(*) FROM threads WHERE channel_id = $1 AND status = 'pending'",
    )
    .bind(name)
    .fetch_one(pool)
    .await
    .ok();

    let processing: Option<i64> = sqlx::query_scalar(
        "SELECT COUNT(*) FROM threads WHERE channel_id = $1 AND status = 'processing'",
    )
    .bind(name)
    .fetch_one(pool)
    .await
    .ok();

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
    name: &str,
    limit: i64,
) -> AppResult<Vec<ChannelSeq0Message>> {
    let rows: Vec<ChannelSeq0Message> = sqlx::query_as(
        r#"
        SELECT id, content, role, msg_type
        FROM messages
        WHERE thread_id IN (SELECT id FROM threads WHERE channel_id = $1)
          AND thread_sequence = 0
          AND (msg_type IS NULL OR msg_type NOT IN ('cron', 'kanban'))
        ORDER BY created_at DESC
        LIMIT $2
        "#,
    )
    .bind(name)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

// Convenience accessors on the raw yml store (used by server handlers that
// need the bare yml fields rather than the compat `Channel` view).

/// Load the raw ChannelsFile (server API + dashboard compat).
pub fn raw_channels_file() -> AppResult<ChannelsFile> {
    channels_yaml::load_channels()
}

/// Mutate + persist a single channel entry (server PATCH handler).
pub fn mutate_channel<F>(name: &str, mutate: F) -> AppResult<ChannelDef>
where
    F: FnOnce(Option<&ChannelDef>) -> AppResult<ChannelDef>,
{
    channels_yaml::update_channel(name, mutate)
}
