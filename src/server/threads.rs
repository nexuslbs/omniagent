//! Threads API - threads list with filters, statuses, causes, subtasks.
//!
//! - `GET /threads` - paginated threads list with optional filters
//! - `GET /threads/filters` - distinct statuses and causes for filter dropdowns
//! - `GET /threads/{id}/subtasks` - subtasks for a specific thread

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
    routing::get,
    Router,
};
use serde::{Deserialize, Serialize};
use sql_forge::sql_forge;
use sqlx::FromRow;
use std::sync::Arc;
use tracing::error;

use super::{err_json, ok_json, AppState};

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

pub fn threads_router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/threads", get(list_threads_handler))
        .route("/threads/filters", get(thread_filters_handler))
        .route("/threads/{id}/subtasks", get(thread_subtasks_handler))
}

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct ThreadsQueryParams {
    pub status: Option<String>,
    pub cause: Option<String>,
    pub channel_id: Option<i64>,
    pub id: Option<i64>,
    pub parent_id: Option<i64>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct ThreadEntry {
    pub id: i64,
    pub channel_id: i64,
    pub channel_name: Option<String>,
    pub status: Option<String>,
    pub cause: Option<String>,
    pub profile: Option<String>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub created_at: String,
    pub ended_at: Option<String>,
    pub duration_ms: Option<i64>,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub cached_tokens: Option<i64>,
    pub iterations: Option<i64>,
    pub parent_id: Option<i64>,
    pub msg_count: i64,
    pub last_message: Option<String>,
    pub plan: Option<bool>,
    pub started_at: Option<String>,
    pub cause_content_preview: Option<String>,
    pub cause_msg_type: Option<String>,
    pub cause_msg_subtype: Option<String>,
    pub channel_closed: bool,
}

#[derive(Debug, Serialize)]
pub struct ThreadsListResponse {
    pub threads: Vec<ThreadEntry>,
    pub total: i64,
    pub offset: i64,
    pub limit: i64,
}

#[derive(Debug, Serialize)]
pub struct ThreadFiltersResponse {
    pub statuses: Vec<String>,
    pub causes: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct SubtaskEntry {
    pub id: i64,
    pub description: String,
    pub status: Option<String>,
    pub created_at: Option<String>,
}

// ---------------------------------------------------------------------------
// Row types
// ---------------------------------------------------------------------------

#[derive(FromRow)]
struct ThreadListRow {
    id: i64,
    channel_id: i64,
    channel_name: Option<String>,
    status: Option<String>,
    cause: Option<String>,
    profile: Option<String>,
    provider: Option<String>,
    model: Option<String>,
    created_at: chrono::DateTime<chrono::Utc>,
    ended_at: Option<chrono::DateTime<chrono::Utc>>,
    duration_ms: Option<i32>,
    input_tokens: Option<i32>,
    output_tokens: Option<i32>,
    cached_tokens: Option<i32>,
    iterations: Option<i32>,
    parent_id: Option<i64>,
    msg_count: Option<i64>,
    last_message: Option<String>,
    plan: Option<bool>,
    started_at: Option<chrono::DateTime<chrono::Utc>>,
    cause_content_preview: Option<String>,
    cause_msg_type: Option<String>,
    cause_msg_subtype: Option<String>,
    channel_closed: Option<bool>,
}

#[derive(FromRow)]
struct CountRow {
    total: Option<i64>,
}

#[derive(FromRow)]
struct StatusRow {
    status: Option<String>,
}

#[derive(FromRow)]
struct SubtaskRow {
    id: i64,
    description: String,
    status: Option<String>,
    created_at: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(FromRow)]
struct CauseRow {
    cause: Option<String>,
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// GET /threads - list threads with optional filters and pagination
async fn list_threads_handler(
    State(state): State<Arc<AppState>>,
    Query(params): Query<ThreadsQueryParams>,
) -> impl IntoResponse {
    let limit = params.limit.unwrap_or(50).clamp(1, 500);
    let offset = params.offset.unwrap_or(0).max(0);

    let status = params.status.unwrap_or_default();
    let cause = params.cause.unwrap_or_default();

    // ── Count ──
    let total = match sql_forge!(
        CountRow,
        r#"
        SELECT COUNT(*) AS total
        FROM threads t
        LEFT JOIN channels c ON c.id = t.channel_id
        WHERE 1=1
          AND (:status = '' OR t.status = ANY(string_to_array(:status, ',')))
          AND (:cause = '' OR t.cause = :cause)
          AND (:channel_id = 0::bigint OR t.channel_id = :channel_id)
          AND (:id = 0::bigint OR t.id = :id)
          AND (:parent_id = 0::bigint OR t.parent_id = :parent_id)
        "#,
        ( :status = &status,
          :cause = &cause,
          :channel_id = params.channel_id.unwrap_or(0),
          :id = params.id.unwrap_or(0),
          :parent_id = params.parent_id.unwrap_or(0) )
    )
    .fetch_one(&state.pool)
    .await
    {
        Ok(row) => row.total.unwrap_or(0),
        Err(e) => {
            error!("[threads] count query failed: {:?}", e);
            return err_json(StatusCode::INTERNAL_SERVER_ERROR, "Failed to count threads");
        }
    };

    // ── Data (two ORDER BY variants) ──
    let threads = match sql_forge!(
        ThreadListRow,
        r#"
        SELECT
            t.id,
            t.channel_id,
            c.name AS channel_name,
            t.status,
            t.cause,
            t.profile,
            t.provider,
            t.model,
            t.created_at,
            t.ended_at,
            t.duration_ms,
            t.input_tokens,
            t.output_tokens,
            t.cached_tokens,
            t.iterations,
            t.parent_id,
            t.plan,
            t.started_at,
            m0.content AS cause_content_preview,
            m0.msg_type AS cause_msg_type,
            m0.msg_subtype AS cause_msg_subtype,
            COALESCE(c.closed, false) AS channel_closed,
            COALESCE((SELECT COUNT(*) FROM messages sub WHERE sub.thread_id = t.id), 0) AS msg_count,
            (SELECT content FROM messages sub2 WHERE sub2.thread_id = t.id ORDER BY sub2.id DESC LIMIT 1) AS last_message
        FROM threads t
        LEFT JOIN channels c ON c.id = t.channel_id
        LEFT JOIN messages m0 ON m0.thread_id = t.id AND m0.thread_sequence = 0
        WHERE 1=1
          AND (:status = '' OR t.status = ANY(string_to_array(:status, ',')))
          AND (:cause = '' OR t.cause = :cause)
          AND (:channel_id = 0::bigint OR t.channel_id = :channel_id)
          AND (:id = 0::bigint OR t.id = :id)
          AND (:parent_id = 0::bigint OR t.parent_id = :parent_id)
        ORDER BY t.id DESC
        LIMIT :limit_val OFFSET :offset_val
        "#,
        ( :status = &status,
          :cause = &cause,
          :channel_id = params.channel_id.unwrap_or(0),
          :id = params.id.unwrap_or(0),
          :parent_id = params.parent_id.unwrap_or(0),
          :limit_val = limit,
          :offset_val = offset )
    )
    .fetch_all(&state.pool)
    .await
    {
        Ok(rows) => rows,
        Err(e) => {
            error!("[threads] data query failed: {:?}", e);
            return err_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to fetch threads",
            );
        }
    };

    let threads: Vec<ThreadEntry> = threads
        .into_iter()
        .map(|r| ThreadEntry {
            id: r.id,
            channel_id: r.channel_id,
            channel_name: r.channel_name,
            status: r.status,
            cause: r.cause,
            profile: r.profile,
            provider: r.provider,
            model: r.model,
            created_at: r.created_at.format("%Y-%m-%dT%H:%M:%SZ").to_string(),
            ended_at: r
                .ended_at
                .map(|dt| dt.format("%Y-%m-%dT%H:%M:%SZ").to_string()),
            duration_ms: r.duration_ms.map(|v| v as i64),
            input_tokens: r.input_tokens.map(|v| v as i64),
            output_tokens: r.output_tokens.map(|v| v as i64),
            cached_tokens: r.cached_tokens.map(|v| v as i64),
            iterations: r.iterations.map(|v| v as i64),
            parent_id: r.parent_id,
            msg_count: r.msg_count.unwrap_or(0),
            last_message: r.last_message,
            plan: r.plan,
            started_at: r
                .started_at
                .map(|dt| dt.format("%Y-%m-%dT%H:%M:%SZ").to_string()),
            cause_content_preview: r.cause_content_preview,
            cause_msg_type: r.cause_msg_type,
            cause_msg_subtype: r.cause_msg_subtype,
            channel_closed: r.channel_closed.unwrap_or(false),
        })
        .collect();

    ok_json(ThreadsListResponse {
        threads,
        total,
        offset,
        limit,
    })
}

/// GET /threads/filters - distinct statuses and causes
async fn thread_filters_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let statuses = match sql_forge!(
        StatusRow,
        r#"SELECT DISTINCT status FROM threads WHERE status IS NOT NULL ORDER BY status"#,
    )
    .fetch_all(&state.pool)
    .await
    {
        Ok(rows) => rows
            .into_iter()
            .filter_map(|r| r.status)
            .collect::<Vec<_>>(),
        Err(e) => {
            error!("[threads/filters] statuses query failed: {:?}", e);
            return err_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to fetch status filters",
            );
        }
    };

    let causes = match sql_forge!(
        CauseRow,
        r#"SELECT DISTINCT cause FROM threads WHERE cause IS NOT NULL ORDER BY cause"#,
    )
    .fetch_all(&state.pool)
    .await
    {
        Ok(rows) => rows.into_iter().filter_map(|r| r.cause).collect::<Vec<_>>(),
        Err(e) => {
            error!("[threads/filters] causes query failed: {:?}", e);
            return err_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to fetch cause filters",
            );
        }
    };

    ok_json(ThreadFiltersResponse { statuses, causes })
}

/// GET /threads/{id}/subtasks - subtasks for a specific thread
async fn thread_subtasks_handler(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let subtasks = match sql_forge!(
        SubtaskRow,
        r#"
        SELECT
            s.id,
            s.description,
            s.status,
            s.created_at
        FROM thread_subtasks s
        WHERE s.thread_id = :thread_id
        ORDER BY s.id
        "#,
        ( :thread_id = id )
    )
    .fetch_all(&state.pool)
    .await
    {
        Ok(rows) => rows
            .into_iter()
            .map(|r| SubtaskEntry {
                id: r.id,
                description: r.description,
                status: r.status,
                created_at: r
                    .created_at
                    .map(|dt| dt.format("%Y-%m-%dT%H:%M:%SZ").to_string()),
            })
            .collect::<Vec<_>>(),
        Err(e) => {
            error!("[threads/{}/subtasks] query failed: {:?}", id, e);
            return err_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to fetch subtasks",
            );
        }
    };

    ok_json(subtasks)
}
