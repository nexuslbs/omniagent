//! Messages API: provides filter options and paginated message events.
//!
//! These endpoints replace the dashboard's direct PostgreSQL queries,
//! so the dashboard no longer needs DB credentials.
//!
//! - `GET /messages/filters`: filter options (channels, roles, types, etc.)
//! - `GET /messages/events`: paginated messages with optional filters

use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::IntoResponse,
    routing::get,
    Json, Router,
};
use serde::{Deserialize, Serialize};
use sql_forge::sql_forge;
use sqlx::FromRow;
use std::sync::Arc;
use tracing::error;

use super::AppState;

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

pub fn messages_router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/messages/filters", get(filters_handler))
        .route("/messages/events", get(events_handler))
}

// ---------------------------------------------------------------------------
// Types: Filters response
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct ChannelFilterEntry {
    pub id: i64,
    pub name: String,
    pub count: i64,
}

#[derive(Debug, Serialize)]
pub struct MessagesFiltersResponse {
    pub channels: Vec<ChannelFilterEntry>,
    pub roles: Vec<String>,
    pub providers: Vec<String>,
    pub models: Vec<String>,
    pub types: Vec<String>,
    pub subtypes: Vec<String>,
}

// ---------------------------------------------------------------------------
// Types: Events request / response
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct EventsQueryParams {
    pub channel_id: Option<String>,
    pub thread_id: Option<String>,
    pub role: Option<String>,
    pub provider: Option<String>,
    pub model: Option<String>,
    #[serde(rename = "type")]
    pub msg_type: Option<String>,
    pub subtype: Option<String>,
    pub seq0: Option<String>,
    pub order: Option<String>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct MessageEventEntry {
    pub id: i64,
    pub thread_id: Option<String>,
    pub role: String,
    pub content: Option<String>,
    pub thread_sequence: Option<i32>,
    pub external_id: Option<String>,
    pub metadata: Option<String>,
    pub created_at: String,
    #[serde(rename = "type")]
    pub msg_type: Option<String>,
    #[serde(rename = "subtype")]
    pub msg_subtype: Option<String>,
    pub iteration_number: Option<i32>,
    pub channel_id: Option<i64>,
    pub status: Option<String>,
    pub profile: Option<String>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub thread_duration_ms: Option<i64>,
    pub thread_input_tokens: Option<i64>,
    pub thread_output_tokens: Option<i64>,
    pub thread_cached_tokens: Option<i64>,
    pub channel_name: Option<String>,
    pub processing_time_ms: Option<i64>,
    pub token_usage: Option<serde_json::Value>,
}

#[derive(Debug, Serialize)]
pub struct MessagesEventsResponse {
    pub messages: Vec<MessageEventEntry>,
    pub total: i64,
    pub offset: i64,
    pub limit: i64,
}

// ---------------------------------------------------------------------------
// Row types for sqlx (DB struct pattern: all primitive types)
// ---------------------------------------------------------------------------

#[derive(FromRow)]
struct ChannelCountRow {
    id: i64,
    name: String,
    count: Option<i64>,
}

#[derive(FromRow)]
struct RoleRow {
    role: Option<String>,
}
#[derive(FromRow)]
struct TypeRow {
    msg_type: Option<String>,
}
#[derive(FromRow)]
struct SubtypeRow {
    msg_subtype: Option<String>,
}
#[derive(FromRow)]
struct ProviderRow {
    provider: Option<String>,
}
#[derive(FromRow)]
struct ModelRow {
    model: Option<String>,
}

#[derive(FromRow)]
struct CountRow {
    total: Option<i64>,
}

#[derive(FromRow)]
struct MessageEventRow {
    id: i64,
    thread_id: Option<i64>,
    role: String,
    content: Option<String>,
    thread_sequence: Option<i32>,
    external_id: Option<String>,
    metadata: Option<String>,
    created_at: chrono::DateTime<chrono::Utc>,
    msg_type: Option<String>,
    msg_subtype: Option<String>,
    iteration_number: Option<i32>,
    channel_id: Option<i64>,
    status: Option<String>,
    profile: Option<String>,
    provider: Option<String>,
    model: Option<String>,
    thread_duration_ms: Option<i64>,
    thread_input_tokens: Option<i64>,
    thread_output_tokens: Option<i64>,
    thread_cached_tokens: Option<i64>,
    channel_name: Option<String>,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn ok_json<T: Serialize>(data: T) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::OK,
        Json(serde_json::json!({ "success": true, "data": data })),
    )
}

fn err_json(status: StatusCode, msg: &str) -> (StatusCode, Json<serde_json::Value>) {
    (
        status,
        Json(serde_json::json!({ "success": false, "error": msg })),
    )
}

fn fmt_ts(ts: &chrono::DateTime<chrono::Utc>) -> String {
    ts.format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

// ---------------------------------------------------------------------------
// Handler: GET /messages/filters
// ---------------------------------------------------------------------------

async fn filters_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    // Channels with message count
    let channels = match sql_forge!(
        ChannelCountRow,
        r#"
        SELECT c.id, c.name, COUNT(t.id) AS count
        FROM channels c
        JOIN threads t ON t.channel_id = c.id
        GROUP BY c.id, c.name
        ORDER BY c.name
        "#,
    )
    .fetch_all(&state.pool)
    .await
    {
        Ok(rows) => rows
            .into_iter()
            .map(|r| ChannelFilterEntry {
                id: r.id,
                name: r.name,
                count: r.count.unwrap_or(0),
            })
            .collect::<Vec<_>>(),
        Err(e) => {
            error!("[messages/filters] channels query failed: {:?}", e);
            return err_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to fetch channels filter",
            );
        }
    };

    // Roles
    let roles = match sql_forge!(
        RoleRow,
        r#"SELECT DISTINCT role FROM messages WHERE role IS NOT NULL ORDER BY role"#,
    )
    .fetch_all(&state.pool)
    .await
    {
        Ok(rows) => rows.into_iter().filter_map(|r| r.role).collect::<Vec<_>>(),
        Err(e) => {
            error!("[messages/filters] roles query failed: {:?}", e);
            return err_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to fetch roles filter",
            );
        }
    };

    // Message types
    let types = match sql_forge!(
        TypeRow,
        r#"SELECT DISTINCT msg_type FROM messages WHERE msg_type IS NOT NULL ORDER BY msg_type"#,
    )
    .fetch_all(&state.pool)
    .await
    {
        Ok(rows) => rows
            .into_iter()
            .filter_map(|r| r.msg_type)
            .collect::<Vec<_>>(),
        Err(e) => {
            error!("[messages/filters] types query failed: {:?}", e);
            return err_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to fetch types filter",
            );
        }
    };

    // Subtypes
    let subtypes = match sql_forge!(
        SubtypeRow,
        r#"SELECT DISTINCT msg_subtype FROM messages WHERE msg_subtype IS NOT NULL AND msg_subtype != '' ORDER BY msg_subtype"#,
    )
    .fetch_all(&state.pool)
    .await
    {
        Ok(rows) => rows
            .into_iter()
            .filter_map(|r| r.msg_subtype)
            .collect::<Vec<_>>(),
        Err(e) => {
            error!("[messages/filters] subtypes query failed: {:?}", e);
            return err_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to fetch subtypes filter",
            );
        }
    };

    // Providers
    let providers = match sql_forge!(
        ProviderRow,
        r#"SELECT DISTINCT provider FROM threads WHERE provider IS NOT NULL ORDER BY provider"#,
    )
    .fetch_all(&state.pool)
    .await
    {
        Ok(rows) => rows
            .into_iter()
            .filter_map(|r| r.provider)
            .collect::<Vec<_>>(),
        Err(e) => {
            error!("[messages/filters] providers query failed: {:?}", e);
            return err_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to fetch providers filter",
            );
        }
    };

    // Models
    let models = match sql_forge!(
        ModelRow,
        r#"SELECT DISTINCT model FROM threads WHERE model IS NOT NULL ORDER BY model"#,
    )
    .fetch_all(&state.pool)
    .await
    {
        Ok(rows) => rows.into_iter().filter_map(|r| r.model).collect::<Vec<_>>(),
        Err(e) => {
            error!("[messages/filters] models query failed: {:?}", e);
            return err_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to fetch models filter",
            );
        }
    };

    ok_json(MessagesFiltersResponse {
        channels,
        roles,
        providers,
        models,
        types,
        subtypes,
    })
}

// ---------------------------------------------------------------------------
// Handler: GET /messages/events
//
// Uses sql_forge!() with conditional WHERE patterns for all optional filters.
// ORDER BY direction uses two if-branches since it can't be a bind parameter.
// ---------------------------------------------------------------------------

async fn events_handler(
    State(state): State<Arc<AppState>>,
    Query(params): Query<EventsQueryParams>,
) -> impl IntoResponse {
    let limit = params.limit.unwrap_or(50).clamp(1, 500);
    let offset = params.offset.unwrap_or(0).max(0);
    let order_desc = params.order.as_deref() != Some("asc");

    // Coalesce optional params to empty string: the WHERE clause pattern
    // `(:param = '' OR ...)` short-circuits when the param is empty.
    let channel_id = params.channel_id.unwrap_or_default();
    let thread_id = params.thread_id.unwrap_or_default();
    let role = params.role.unwrap_or_default();
    let provider = params.provider.unwrap_or_default();
    let model = params.model.unwrap_or_default();
    let msg_type = params.msg_type.unwrap_or_default();
    let seq0 = params.seq0.unwrap_or_default();
    let subtype = params.subtype.unwrap_or_default();
    let subtype_pattern = if subtype.trim().is_empty() {
        String::new()
    } else {
        format!("%{}%", subtype)
    };

    // Parse string IDs to i64 for bigint SQL params
    let channel_int = channel_id.parse::<i64>().unwrap_or(0);
    let thread_int = thread_id.parse::<i64>().unwrap_or(0);

    // ── Count query ──
    let total = match sql_forge!(
        CountRow,
        r#"
        SELECT COUNT(*) AS total
        FROM messages m
        JOIN threads t ON t.id = m.thread_id
        JOIN channels c ON c.id = t.channel_id
        WHERE 1=1
          AND (:channel_id = '' OR :channel_id = 'all' OR t.channel_id = :channel_int)
          AND (:thread_id = '' OR m.thread_id = :thread_int)
          AND (:role = '' OR :role = 'all' OR m.role = :role)
          AND (:provider = '' OR :provider = 'all' OR t.provider = :provider)
          AND (:model = '' OR :model = 'all' OR t.model = :model)
          AND (:msg_type = '' OR :msg_type = 'all' OR m.msg_type = ANY(string_to_array(:msg_type, ',')))
          AND (:seq0 != 'true' OR m.thread_sequence = 0)
          AND (:subtype_pattern = '' OR m.msg_subtype LIKE :subtype_pattern)
        "#,
        ( :channel_id = &channel_id,
          :channel_int = channel_int,
          :thread_id = &thread_id,
          :thread_int = thread_int,
          :role = &role,
          :provider = &provider,
          :model = &model,
          :msg_type = &msg_type,
          :seq0 = &seq0,
          :subtype_pattern = &subtype_pattern )
    )
    .fetch_one(&state.pool)
    .await
    {
        Ok(row) => row.total.unwrap_or(0),
        Err(e) => {
            error!("[messages/events] count query failed: {:?}", e);
            return err_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to count messages",
            );
        }
    };

    // ── Data query ──
    // Two compile-time string literals for ORDER BY direction.
    // sql_forge!() requires a string literal: can't format!() at runtime.
    // The rest of the SQL (SELECT columns, JOINs, WHERE conditions) is identical.

    let rows = if order_desc {
        match sql_forge!(
            MessageEventRow,
            r#"
            SELECT
                m.id,
                m.thread_id,
                m.role,
                m.content,
                m.thread_sequence,
                m.external_id,
                m.metadata::text AS metadata,
                m.created_at,
                m.msg_type,
                m.msg_subtype,
                m.iteration_number AS iteration_number,
                t.channel_id,
                t.status,
                t.profile,
                t.provider,
                t.model,
                t.duration_ms::bigint AS thread_duration_ms,
                t.input_tokens::bigint AS thread_input_tokens,
                t.output_tokens::bigint AS thread_output_tokens,
                t.cached_tokens::bigint AS thread_cached_tokens,
                c.name AS channel_name
            FROM messages m
            JOIN threads t ON t.id = m.thread_id
            JOIN channels c ON c.id = t.channel_id
            WHERE 1=1
              AND (:channel_id = '' OR :channel_id = 'all' OR t.channel_id = :channel_int)
              AND (:thread_id = '' OR m.thread_id = :thread_int)
              AND (:role = '' OR :role = 'all' OR m.role = :role)
              AND (:provider = '' OR :provider = 'all' OR t.provider = :provider)
              AND (:model = '' OR :model = 'all' OR t.model = :model)
              AND (:msg_type = '' OR :msg_type = 'all' OR m.msg_type = ANY(string_to_array(:msg_type, ',')))
              AND (:seq0 != 'true' OR m.thread_sequence = 0)
              AND (:subtype_pattern = '' OR m.msg_subtype LIKE :subtype_pattern)
            ORDER BY m.id DESC
            LIMIT :limit_val OFFSET :offset_val
            "#,
            ( :channel_id = &channel_id,
              :channel_int = channel_int,
              :thread_id = &thread_id,
              :thread_int = thread_int,
              :role = &role,
              :provider = &provider,
              :model = &model,
              :msg_type = &msg_type,
              :seq0 = &seq0,
              :subtype_pattern = &subtype_pattern,
              :limit_val = limit,
              :offset_val = offset )
        )
        .fetch_all(&state.pool)
        .await
        {
            Ok(rows) => rows,
            Err(e) => {
                error!("[messages/events] data query failed: {:?}", e);
                return err_json(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Failed to fetch messages",
                );
            }
        }
    } else {
        match sql_forge!(
            MessageEventRow,
            r#"
            SELECT
                m.id,
                m.thread_id,
                m.role,
                m.content,
                m.thread_sequence,
                m.external_id,
                m.metadata::text AS metadata,
                m.created_at,
                m.msg_type,
                m.msg_subtype,
                m.iteration_number AS iteration_number,
                t.channel_id,
                t.status,
                t.profile,
                t.provider,
                t.model,
                t.duration_ms::bigint AS thread_duration_ms,
                t.input_tokens::bigint AS thread_input_tokens,
                t.output_tokens::bigint AS thread_output_tokens,
                t.cached_tokens::bigint AS thread_cached_tokens,
                c.name AS channel_name
            FROM messages m
            JOIN threads t ON t.id = m.thread_id
            JOIN channels c ON c.id = t.channel_id
            WHERE 1=1
              AND (:channel_id = '' OR :channel_id = 'all' OR t.channel_id = :channel_int)
              AND (:thread_id = '' OR m.thread_id = :thread_int)
              AND (:role = '' OR :role = 'all' OR m.role = :role)
              AND (:provider = '' OR :provider = 'all' OR t.provider = :provider)
              AND (:model = '' OR :model = 'all' OR t.model = :model)
              AND (:msg_type = '' OR :msg_type = 'all' OR m.msg_type = ANY(string_to_array(:msg_type, ',')))
              AND (:seq0 != 'true' OR m.thread_sequence = 0)
              AND (:subtype_pattern = '' OR m.msg_subtype LIKE :subtype_pattern)
            ORDER BY m.id ASC
            LIMIT :limit_val OFFSET :offset_val
            "#,
            ( :channel_id = &channel_id,
              :channel_int = channel_int,
              :thread_id = &thread_id,
              :thread_int = thread_int,
              :role = &role,
              :provider = &provider,
              :model = &model,
              :msg_type = &msg_type,
              :seq0 = &seq0,
              :subtype_pattern = &subtype_pattern,
              :limit_val = limit,
              :offset_val = offset )
        )
        .fetch_all(&state.pool)
        .await
        {
            Ok(rows) => rows,
            Err(e) => {
                error!("[messages/events] data query failed: {:?}", e);
                return err_json(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Failed to fetch messages",
                );
            }
        }
    };

    let messages: Vec<MessageEventEntry> = rows
        .into_iter()
        .map(|r| MessageEventEntry {
            id: r.id,
            thread_id: r.thread_id.map(|v| v.to_string()),
            role: r.role,
            content: r.content,
            thread_sequence: r.thread_sequence,
            external_id: r.external_id,
            metadata: r.metadata,
            created_at: fmt_ts(&r.created_at),
            msg_type: r.msg_type,
            msg_subtype: r.msg_subtype,
            iteration_number: r.iteration_number,
            channel_id: r.channel_id,
            status: r.status,
            profile: r.profile,
            provider: r.provider,
            model: r.model,
            thread_duration_ms: r.thread_duration_ms,
            thread_input_tokens: r.thread_input_tokens,
            thread_output_tokens: r.thread_output_tokens,
            thread_cached_tokens: r.thread_cached_tokens,
            channel_name: r.channel_name,
            processing_time_ms: r.thread_duration_ms,
            token_usage: if r.thread_input_tokens.is_some()
                || r.thread_output_tokens.is_some()
                || r.thread_cached_tokens.is_some()
            {
                Some(serde_json::json!({
                    "prompt_tokens": r.thread_input_tokens,
                    "completion_tokens": r.thread_output_tokens,
                    "cached_tokens": r.thread_cached_tokens,
                }))
            } else {
                None
            },
        })
        .collect();

    ok_json(MessagesEventsResponse {
        messages,
        total,
        offset,
        limit,
    })
}
